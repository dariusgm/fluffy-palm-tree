use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinSet;
use walkdir::WalkDir;

use crate::db::models::{self, Details, FileKind, FileRecord, SummaryStatus};
use crate::detect::{self, Detected};
use crate::extract;
use crate::jobs::Job;
use crate::security::ResolvedPath;
use crate::state::AppState;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IndexRequest {
    pub path: PathBuf,
    #[serde(default = "default_traverse")]
    pub traverse: bool,
}

fn default_traverse() -> bool {
    true
}

enum Outcome {
    Indexed,
    Unchanged,
    TooLarge,
}

pub async fn run(
    state: AppState,
    job: Arc<Job>,
    target: ResolvedPath,
    traverse: bool,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<PathBuf>(256);

    let walker = {
        let job = job.clone();
        let start = target.path.clone();
        tokio::task::spawn_blocking(move || walk(&start, traverse, &job, tx))
    };

    let rx = Arc::new(Mutex::new(rx));
    let target = Arc::new(target);
    let mut workers = JoinSet::new();
    for _ in 0..state.config.staging.index_workers {
        let (state, job, rx, target) = (state.clone(), job.clone(), rx.clone(), target.clone());
        workers.spawn(async move {
            loop {
                if job.is_cancelled() {
                    break;
                }
                let next = rx.lock().await.recv().await;
                let Some(path) = next else { break };
                match process_file(&state, &target, &path).await {
                    Ok(Outcome::Indexed) => job.inc_processed(),
                    Ok(Outcome::Unchanged | Outcome::TooLarge) => job.inc_skipped(),
                    Err(e) => job.record_failure(path.display().to_string(), &e),
                }
            }
        });
    }
    drop(rx);

    walker.await.context("walker panicked")?;
    while let Some(res) = workers.join_next().await {
        res.context("index worker panicked")?;
    }
    crate::db::fts::rebuild(&state.db).await;
    Ok(())
}

/// Walks the tree without following symlinks and skips hidden entries.
fn walk(start: &Path, traverse: bool, job: &Job, tx: mpsc::Sender<PathBuf>) {
    let mut walker = WalkDir::new(start).follow_links(false);
    if !traverse {
        walker = walker.max_depth(1);
    }
    let entries = walker
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'));
    for entry in entries {
        if job.is_cancelled() {
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                let path = e
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                job.record_error(path, &anyhow::Error::new(e));
                continue;
            }
        };
        if !entry.file_type().is_file() || detect::detect(entry.path()).is_none() {
            continue;
        }
        job.inc_found();
        if tx.blocking_send(entry.into_path()).is_err() {
            break;
        }
    }
}

async fn process_file(
    state: &AppState,
    target: &ResolvedPath,
    path: &Path,
) -> anyhow::Result<Outcome> {
    let cfg = &state.config.staging;
    let detected = detect::detect(path).context("unsupported file type")?;
    let meta = tokio::fs::symlink_metadata(path).await?;
    let abs_path = path
        .to_str()
        .context("path is not valid UTF-8")?
        .to_string();
    let rel_path = target
        .relative(path)
        .and_then(|p| p.to_str().map(str::to_string))
        .context("path outside of root")?;
    let mtime = file_mtime(&meta);
    let size = meta.len();

    let (root, rel) = (target.root_name.clone(), rel_path.clone());
    let existing = state
        .db
        .call(move |c| models::find_stat(c, &root, &rel))
        .await?;
    if existing.is_some_and(|s| s.size_bytes == size && s.mtime == mtime) {
        return Ok(Outcome::Unchanged);
    }

    let mut rec = FileRecord {
        root: target.root_name.clone(),
        rel_path,
        file_name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        extension: detect::extension(path),
        abs_path,
        kind: detected.kind,
        mime: None,
        size_bytes: size,
        mode: meta.mode() & 0o7777,
        uid: meta.uid(),
        gid: meta.gid(),
        mtime,
        sha256: None,
    };

    if size > cfg.max_file_bytes {
        save(state, rec, Details::None, SummaryStatus::Skipped).await?;
        return Ok(Outcome::TooLarge);
    }

    let staged = if cfg.copy_on_index {
        let (staged, sha) = state.staging.copy_in(path, size).await?;
        rec.sha256 = Some(sha);
        Some(staged)
    } else {
        None
    };
    let work_path = staged.as_ref().map_or(path, |s| s.path()).to_path_buf();

    rec.mime = {
        let p = work_path.clone();
        tokio::task::spawn_blocking(move || detect::sniff_mime(&p, detected)).await?
    };
    let details = extract_details(&work_path, detected, rec.mime.clone()).await?;
    save(state, rec, details, SummaryStatus::Pending).await?;
    Ok(Outcome::Indexed)
}

async fn extract_details(
    path: &Path,
    detected: Detected,
    mime: Option<String>,
) -> anyhow::Result<Details> {
    Ok(match detected.kind {
        FileKind::Image => {
            let p = path.to_path_buf();
            let meta =
                tokio::task::spawn_blocking(move || extract::image::extract(&p, mime.as_deref()))
                    .await??;
            Details::Image(meta)
        }
        FileKind::Document => {
            let doc_type = detected.doc_type.context("document without doc type")?;
            Details::Document(extract::document::extract(path, doc_type).await?)
        }
        FileKind::Video => Details::Video(extract::video::extract(path).await?),
    })
}

async fn save(
    state: &AppState,
    rec: FileRecord,
    details: Details,
    status: SummaryStatus,
) -> anyhow::Result<()> {
    state
        .db
        .call(move |c| models::save_indexed(c, &rec, &details, status))
        .await?;
    Ok(())
}

/// Modification time truncated to microseconds, the precision DuckDB stores.
fn file_mtime(meta: &std::fs::Metadata) -> Option<DateTime<Utc>> {
    let micros = meta
        .mtime()
        .checked_mul(1_000_000)?
        .checked_add(meta.mtime_nsec() / 1_000)?;
    DateTime::from_timestamp_micros(micros)
}

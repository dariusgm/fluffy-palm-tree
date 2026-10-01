use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinSet;
use walkdir::WalkDir;

use crate::db::models::{
    self, Details, FileKind, FileRecord, META_VERSION, StatFields, SummaryStatus,
};
use crate::detect::{self, Detected};
use crate::extract;
use crate::jobs::Job;
use crate::security::ResolvedPath;
use crate::staging::Staging;
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
    /// Same content, stored metadata refreshed (LLM results kept).
    Updated,
    TooLarge,
    /// Not a supported file type; `removed` if an existing record had to be dropped.
    Unsupported {
        removed: bool,
    },
}

/// Result of walking the tree: relative paths of all regular files, and whether the walk
/// saw everything (no read errors, not cancelled). Only a complete walk may be used to
/// delete records of files that disappeared.
struct WalkResult {
    seen: HashSet<String>,
    complete: bool,
}

pub async fn run(
    state: AppState,
    job: Arc<Job>,
    target: ResolvedPath,
    traverse: bool,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<PathBuf>(256);
    let target = Arc::new(target);

    let walker = {
        let (job, target) = (job.clone(), target.clone());
        tokio::task::spawn_blocking(move || walk(&target, traverse, &job, tx))
    };

    let rx = Arc::new(Mutex::new(rx));
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
                let outcome = process_file(&state, &target, &path).await;
                if !matches!(outcome, Ok(Outcome::Unsupported { .. })) {
                    job.inc_found();
                }
                match outcome {
                    Ok(Outcome::Indexed) => job.inc_processed(),
                    Ok(Outcome::Updated) => job.inc_updated(),
                    Ok(Outcome::Unchanged | Outcome::TooLarge) => job.inc_skipped(),
                    Ok(Outcome::Unsupported { removed }) => {
                        if removed {
                            job.add_removed(1);
                        }
                    }
                    Err(e) => job.record_failure(path.display().to_string(), &e),
                }
            }
        });
    }
    drop(rx);

    let walked = walker.await.context("walker panicked")?;
    while let Some(res) = workers.join_next().await {
        res.context("index worker panicked")?;
    }

    if walked.complete && !job.is_cancelled() && target.path.is_dir() {
        let scope = target
            .relative(&target.path)
            .and_then(|p| p.to_str().map(str::to_string))
            .context("index path is not valid UTF-8")?;
        let root = target.root_name.clone();
        let removed = state
            .db
            .call(move |c| models::delete_unseen(c, &root, &scope, traverse, &walked.seen))
            .await?;
        if removed > 0 {
            tracing::info!(removed, "removed records of files no longer present");
        }
        job.add_removed(removed as u64);
    }
    crate::db::fts::rebuild(&state.db).await;
    Ok(())
}

/// Walks the tree without following symlinks and skips hidden entries. Every regular
/// file is passed on; the workers decide by content whether it is supported.
fn walk(target: &ResolvedPath, traverse: bool, job: &Job, tx: mpsc::Sender<PathBuf>) -> WalkResult {
    let mut result = WalkResult {
        seen: HashSet::new(),
        complete: true,
    };
    let mut walker = WalkDir::new(&target.path).follow_links(false);
    if !traverse {
        walker = walker.max_depth(1);
    }
    let entries = walker
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'));
    for entry in entries {
        if job.is_cancelled() {
            result.complete = false;
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                result.complete = false;
                let path = e
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                job.record_error(path, &anyhow::Error::new(e));
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if let Some(rel) = target
            .relative(entry.path())
            .and_then(|p| p.to_str().map(str::to_string))
        {
            result.seen.insert(rel);
        }
        if tx.blocking_send(entry.into_path()).is_err() {
            result.complete = false;
            break;
        }
    }
    result
}

async fn process_file(
    state: &AppState,
    target: &ResolvedPath,
    path: &Path,
) -> anyhow::Result<Outcome> {
    let cfg = &state.config.staging;
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
    let stat = StatFields {
        mode: meta.mode() & 0o7777,
        uid: meta.uid(),
        gid: meta.gid(),
        created: meta.created().ok().and_then(to_micros),
    };

    let (root, rel) = (target.root_name.clone(), rel_path.clone());
    let existing = state
        .db
        .call(move |c| models::find_stat(c, &root, &rel))
        .await?;
    // Fast path: same size and mtime means unchanged, without reading the file
    // (re-reading a large share on every run would be far too slow over SMB).
    if let Some(old) = existing
        .as_ref()
        .filter(|s| s.size_bytes == size && s.mtime == mtime)
    {
        if old.meta_version >= META_VERSION {
            return Ok(Outcome::Unchanged);
        }
        match refresh_derived(state, old, path, size).await? {
            Refreshed::Kept => {
                let (id, stat) = (old.id.clone(), stat);
                state
                    .db
                    .call(move |c| models::refresh_stat(c, &id, &stat))
                    .await?;
                return Ok(Outcome::Updated);
            }
            Refreshed::Removed => return Ok(Outcome::Unsupported { removed: true }),
            // The type changed: index it from scratch below.
            Refreshed::Reclassified => {}
        }
    }
    // A reclassified record was deleted above; treat the file as new.
    let existing = match existing {
        Some(old)
            if old.meta_version < META_VERSION && old.size_bytes == size && old.mtime == mtime =>
        {
            None
        }
        other => other,
    };

    let detected = {
        let p = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            detect::read_header(&p).map(|h| detect::classify(&p, &h))
        })
        .await??
    };
    let Some(detected) = detected else {
        // Unsupported now; drop a stale record if the content type changed.
        let removed = match existing {
            Some(old) => {
                state
                    .db
                    .call(move |c| models::delete_file(c, &old.id))
                    .await?;
                true
            }
            None => false,
        };
        return Ok(Outcome::Unsupported { removed });
    };

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
        mime: Some(detected.mime.clone()),
        size_bytes: size,
        mode: stat.mode,
        uid: stat.uid,
        gid: stat.gid,
        mtime,
        created: stat.created,
        sha256: None,
    };

    if size > cfg.max_file_bytes {
        save(state, rec, Details::None, SummaryStatus::Skipped).await?;
        return Ok(Outcome::TooLarge);
    }

    let (staged, sha) = if cfg.copy_on_index {
        let (staged, sha) = state.staging.copy_in(path, size).await?;
        (Some(staged), sha)
    } else {
        (None, Staging::hash_file(path).await?)
    };
    rec.sha256 = Some(sha.clone());

    if let Some(old) = existing {
        if old.sha256.as_deref() == Some(sha.as_str()) {
            // Only stat data changed (e.g. touched): keep metadata and LLM results.
            state
                .db
                .call(move |c| models::touch_unchanged(c, &old.id, &rec))
                .await?;
            return Ok(Outcome::Updated);
        }
        // New content: drop the old record now, so a failed extraction leaves no stale data.
        state
            .db
            .call(move |c| models::delete_file(c, &old.id))
            .await?;
    }

    let work_path = staged.as_ref().map_or(path, |s| s.path()).to_path_buf();
    let details = extract_details(&work_path, &detected).await?;
    let status = match detected.kind {
        // Documents stay pending for the planned LLM document summaries.
        FileKind::Image | FileKind::Video | FileKind::Document => SummaryStatus::Pending,
        // No LLM step planned for archives.
        FileKind::Archive => SummaryStatus::Skipped,
    };
    let id = save(state, rec, details, status).await?;
    let reused = state
        .db
        .call(move |c| models::reuse_analysis(c, &id, &sha))
        .await?;
    if reused {
        tracing::debug!(path = %path.display(), "reused LLM results of identical content");
    }
    Ok(Outcome::Indexed)
}

enum Refreshed {
    Kept,
    Removed,
    Reclassified,
}

/// Unchanged file from an older index version. Videos keep everything (their LLM results
/// are expensive); images get their metadata (e.g. the perceptual hash) re-extracted
/// without touching the description; documents and archives are re-classified and their
/// metadata re-extracted, which picks up new fields and detection fixes.
async fn refresh_derived(
    state: &AppState,
    old: &models::StoredStat,
    path: &Path,
    size: u64,
) -> anyhow::Result<Refreshed> {
    let refreshable = [
        FileKind::Image.as_str(),
        FileKind::Document.as_str(),
        FileKind::Archive.as_str(),
    ];
    if !refreshable.contains(&old.kind.as_str()) {
        return Ok(Refreshed::Kept);
    }
    let p = path.to_path_buf();
    let detected = tokio::task::spawn_blocking(move || {
        detect::read_header(&p).map(|h| detect::classify(&p, &h))
    })
    .await??;
    let id = old.id.clone();
    let unsupported = detected.is_none();
    let Some(detected) = detected.filter(|d| d.kind.as_str() == old.kind) else {
        let removed = unsupported;
        state.db.call(move |c| models::delete_file(c, &id)).await?;
        return Ok(if removed {
            Refreshed::Removed
        } else {
            Refreshed::Reclassified
        });
    };
    match detected.kind {
        FileKind::Archive => {
            let meta = detected.archive.context("archive without details")?;
            state
                .db
                .call(move |c| models::replace_archive(c, &id, &meta))
                .await?;
        }
        _ => {
            let (staged, _sha) = state.staging.copy_in(path, size).await?;
            match extract_details(staged.path(), &detected).await? {
                Details::Document(meta) => {
                    state
                        .db
                        .call(move |c| models::replace_document(c, &id, &meta))
                        .await?;
                }
                Details::Image(meta) => {
                    state
                        .db
                        .call(move |c| models::replace_image(c, &id, &meta))
                        .await?;
                }
                _ => {}
            }
        }
    }
    Ok(Refreshed::Kept)
}

async fn extract_details(path: &Path, detected: &Detected) -> anyhow::Result<Details> {
    Ok(match detected.kind {
        FileKind::Image => {
            let (p, mime) = (path.to_path_buf(), detected.mime.clone());
            let meta =
                tokio::task::spawn_blocking(move || extract::image::extract(&p, Some(&mime)))
                    .await??;
            Details::Image(meta)
        }
        FileKind::Document => {
            let doc_type = detected.doc_type.context("document without doc type")?;
            Details::Document(
                extract::document::extract(path, doc_type, detected.language.clone()).await?,
            )
        }
        FileKind::Video => Details::Video(extract::video::extract(path).await?),
        FileKind::Archive => Details::Archive(
            detected
                .archive
                .clone()
                .context("archive without details")?,
        ),
    })
}

async fn save(
    state: &AppState,
    rec: FileRecord,
    details: Details,
    status: SummaryStatus,
) -> anyhow::Result<String> {
    state
        .db
        .call(move |c| models::save_indexed(c, &rec, &details, status))
        .await
}

/// Modification time truncated to microseconds, the precision DuckDB stores.
fn file_mtime(meta: &std::fs::Metadata) -> Option<DateTime<Utc>> {
    let micros = meta
        .mtime()
        .checked_mul(1_000_000)?
        .checked_add(meta.mtime_nsec() / 1_000)?;
    DateTime::from_timestamp_micros(micros)
}

fn to_micros(t: std::time::SystemTime) -> Option<DateTime<Utc>> {
    let micros = t.duration_since(std::time::UNIX_EPOCH).ok()?.as_micros();
    DateTime::from_timestamp_micros(i64::try_from(micros).ok()?)
}

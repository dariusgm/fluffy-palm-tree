use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use crate::db::media::{self, AnalysisRecord, CandidateFilter, MediaCandidate};
use crate::db::models::{FileKind, SummaryStatus};
use crate::extract::{frames, prepare};
use crate::jobs::Job;
use crate::llm::{Part, prompts};
use crate::security::resolve_in_roots;
use crate::state::AppState;

const MAX_TOKENS: u32 = 1536;
/// Per-frame text passed to the merge prompt, to keep long videos within the context size.
const MERGE_FRAME_CHARS: usize = 600;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRequest {
    #[serde(default)]
    pub kind: Vec<FileKind>,
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub ids: Vec<String>,
    pub limit: Option<u32>,
    #[serde(default)]
    pub force: bool,
}

impl ImportRequest {
    pub fn filter(&self) -> Result<CandidateFilter, String> {
        let kinds = if self.kind.is_empty() {
            vec![FileKind::Image, FileKind::Video]
        } else {
            self.kind.clone()
        };
        if kinds
            .iter()
            .any(|k| matches!(k, FileKind::Document | FileKind::Archive))
        {
            return Err("only image and video are supported; use kind image and/or video".into());
        }
        Ok(CandidateFilter {
            kinds,
            path_prefix: self.path_prefix.clone(),
            ids: self.ids.clone(),
            limit: self.limit,
            force: self.force,
        })
    }
}

pub async fn run(state: AppState, job: Arc<Job>, filter: CandidateFilter) -> anyhow::Result<()> {
    let candidates = state
        .db
        .call(move |c| media::select_candidates(c, &filter))
        .await?;
    job.add_found(candidates.len() as u64);

    let queue = Arc::new(Mutex::new(VecDeque::from(candidates)));
    let mut workers = JoinSet::new();
    for _ in 0..state.config.llm.workers {
        let (state, job, queue) = (state.clone(), job.clone(), queue.clone());
        workers.spawn(async move {
            loop {
                if job.is_cancelled() {
                    break;
                }
                let next = queue.lock().expect("queue lock").pop_front();
                let Some(item) = next else { break };
                process(&state, &job, item).await;
            }
        });
    }
    while let Some(res) = workers.join_next().await {
        res.context("import worker panicked")?;
    }
    crate::db::fts::rebuild(&state.db).await;
    Ok(())
}

async fn process(state: &AppState, job: &Job, item: MediaCandidate) {
    let id = item.id.clone();
    if let Err(e) = update_status(state, &id, SummaryStatus::Running).await {
        job.record_failure(item.abs_path.clone(), &e);
        return;
    }

    let result = analyze(state, job, &item).await;
    let outcome = match result {
        Ok(summary) => {
            let id = id.clone();
            state
                .db
                .call(move |c| media::set_summary(c, &id, &summary))
                .await
                .map(|()| job.inc_processed())
        }
        Err(_) if job.is_cancelled() => update_status(state, &id, SummaryStatus::Pending).await,
        Err(e) => {
            job.record_failure(item.abs_path.clone(), &e);
            update_status(state, &id, SummaryStatus::Failed).await
        }
    };
    if let Err(e) = outcome {
        job.record_error(item.abs_path, &e);
    }
}

async fn update_status(state: &AppState, id: &str, status: SummaryStatus) -> anyhow::Result<()> {
    let id = id.to_string();
    state
        .db
        .call(move |c| media::set_status(c, &id, status))
        .await
}

/// Fetches the file again from the source and returns the searchable summary.
async fn analyze(state: &AppState, job: &Job, item: &MediaCandidate) -> anyhow::Result<String> {
    if item.size_bytes > state.config.staging.max_file_bytes {
        bail!("file exceeds staging.max_file_bytes");
    }
    // The source must still be inside a configured root (roots may have changed).
    let roots = state.config.roots.clone();
    let source = PathBuf::from(&item.abs_path);
    let resolved = tokio::task::spawn_blocking(move || resolve_in_roots(&roots, &source))
        .await?
        .map_err(|e| anyhow!("{e}"))?;
    let (staged, _sha) = state
        .staging
        .copy_in(&resolved.path, item.size_bytes)
        .await?;

    match item.kind {
        FileKind::Image => describe_image(state, &item.id, staged.path(), None).await,
        FileKind::Video => analyze_video(state, job, item, staged).await,
        FileKind::Document | FileKind::Archive => {
            bail!("only images and videos are supported by import_media")
        }
    }
}

async fn analyze_video(
    state: &AppState,
    job: &Job,
    item: &MediaCandidate,
    staged: crate::staging::StagedFile,
) -> anyhow::Result<String> {
    let cfg = &state.config.llm;
    let interval = frames::effective_interval(
        item.duration_secs,
        cfg.video_frame_interval_secs,
        cfg.video_max_frames,
    );
    let dir = state.staging.temp_dir()?;
    let sampled = frames::sample(
        staged.path(),
        dir.path(),
        interval,
        item.duration_secs,
        cfg.video_max_frames,
        cfg.image_max_edge,
    )
    .await?;
    drop(staged);
    if sampled.is_empty() {
        bail!("ffmpeg produced no frames");
    }

    let mut described = Vec::with_capacity(sampled.len());
    for (ts, frame) in &sampled {
        if job.is_cancelled() {
            bail!("cancelled");
        }
        match describe_image(state, &item.id, frame, Some(*ts)).await {
            Ok(text) => described.push((*ts, text)),
            Err(e) => job.record_error(format!("{} @ {ts:.0}s", item.abs_path), &e),
        }
    }
    if described.is_empty() {
        bail!("no frame could be described ({} sampled)", sampled.len());
    }

    let (id, frames_for_db) = (item.id.clone(), described.clone());
    state
        .db
        .call(move |c| media::replace_frames(c, &id, &frames_for_db))
        .await?;

    let condensed: Vec<(f64, String)> = described
        .into_iter()
        .map(|(ts, t)| (ts, t.chars().take(MERGE_FRAME_CHARS).collect()))
        .collect();
    call_llm(
        state,
        &item.id,
        None,
        prompts::VIDEO_MERGE_PROMPT_VERSION,
        prompts::VIDEO_MERGE_SYSTEM,
        vec![Part::Text(prompts::video_merge_user(&condensed))],
    )
    .await
}

async fn describe_image(
    state: &AppState,
    file_id: &str,
    path: &Path,
    ts: Option<f64>,
) -> anyhow::Result<String> {
    let edge = state.config.llm.image_max_edge;
    let p = path.to_path_buf();
    let jpeg = tokio::task::spawn_blocking(move || prepare::to_llm_jpeg(&p, edge)).await??;
    call_llm(
        state,
        file_id,
        ts,
        prompts::IMAGE_PROMPT_VERSION,
        prompts::IMAGE_SYSTEM,
        vec![
            Part::Text(prompts::IMAGE_USER.to_string()),
            Part::Jpeg(jpeg),
        ],
    )
    .await
}

/// Calls the LLM, stores the attempt in `analyses`, and returns searchable text.
async fn call_llm(
    state: &AppState,
    file_id: &str,
    ts: Option<f64>,
    prompt_version: &str,
    system: &str,
    parts: Vec<Part>,
) -> anyhow::Result<String> {
    let mut rec = AnalysisRecord {
        file_id: file_id.to_string(),
        ts_secs: ts,
        model: state.llm.model().to_string(),
        prompt_version: prompt_version.to_string(),
        ..Default::default()
    };
    let result = match state.llm.chat(system, parts, MAX_TOKENS).await {
        Ok(c) => {
            let parsed = prompts::parse_json(&c.text);
            let text = parsed
                .as_ref()
                .map(prompts::searchable_text)
                .filter(|t| !t.is_empty())
                .or_else(|| prompts::salvage_summary(&c.text))
                .unwrap_or_else(|| c.text.clone());
            if c.truncated {
                // Still usable (salvaged summary), but flagged for quality statistics.
                rec.error = Some(format!("output truncated at max_tokens={MAX_TOKENS}"));
            }
            rec.raw_response = Some(c.text);
            rec.parsed = parsed;
            rec.latency_ms = Some(c.latency_ms);
            rec.prompt_tokens = c.prompt_tokens;
            rec.completion_tokens = c.completion_tokens;
            if text.trim().is_empty() {
                rec.error = Some("empty response".into());
                Err(anyhow!("LLM returned an empty response"))
            } else {
                Ok(text)
            }
        }
        Err(e) => {
            rec.error = Some(format!("{e:#}"));
            Err(e)
        }
    };
    state
        .db
        .call(move |c| media::insert_analysis(c, &rec))
        .await?;
    result
}

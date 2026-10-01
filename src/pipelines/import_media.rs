use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::JoinSet;

use crate::config::OcrMode;
use crate::db::media::{self, AnalysisRecord, CandidateFilter, MediaCandidate};
use crate::db::models::{FileKind, SummaryStatus};
use crate::extract::{frames, pdf_pages, prepare};
use crate::jobs::Job;
use crate::llm::{Part, prompts};
use crate::security::resolve_in_roots;
use crate::state::AppState;

const MAX_TOKENS: u32 = 1536;
/// OCR output can be long (dense document pages).
const OCR_MAX_TOKENS: u32 = 4096;
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
            vec![FileKind::Image, FileKind::Video, FileKind::Document]
        } else {
            self.kind.clone()
        };
        if kinds.contains(&FileKind::Archive) {
            return Err("archives have no LLM step; use kind image, video and/or document".into());
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

/// What happened to one file; each step is `None` if it was not requested or not run.
#[derive(Default)]
struct ItemOutcome {
    summary: Option<anyhow::Result<String>>,
    ocr: Option<anyhow::Result<OcrResult>>,
}

enum OcrResult {
    /// Transcribed text per page (images use page 1). Empty if no text was found.
    Pages(Vec<(u32, String)>),
    /// OCR disabled for images (`ocr_images = "never"`).
    Skipped,
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
    if item.describe
        && let Err(e) = update_status(state, &item.id, SummaryStatus::Running).await
    {
        job.record_failure(item.abs_path.clone(), &e);
        return;
    }

    let outcome = match analyze(state, job, &item).await {
        Ok(o) => o,
        // Fetching the file failed: every requested step failed.
        Err(e) => ItemOutcome {
            summary: item.describe.then(|| Err(anyhow!("{e:#}"))),
            ocr: item.ocr.then(|| Err(e)),
        },
    };

    let mut errors: Vec<anyhow::Error> = Vec::new();
    match outcome.summary {
        Some(Ok(summary)) if summary.is_empty() => {
            if let Err(e) = update_status(state, &item.id, SummaryStatus::Skipped).await {
                errors.push(e);
            }
        }
        Some(Ok(summary)) => {
            let id = item.id.clone();
            if let Err(e) = state
                .db
                .call(move |c| media::set_summary(c, &id, &summary))
                .await
            {
                errors.push(e);
            }
        }
        Some(Err(e)) => {
            let status = if job.is_cancelled() {
                SummaryStatus::Pending
            } else {
                errors.push(e);
                SummaryStatus::Failed
            };
            if let Err(e) = update_status(state, &item.id, status).await {
                errors.push(e);
            }
        }
        None if item.describe => {
            // Describe was requested but not attempted (cancelled before it ran).
            let _ = update_status(state, &item.id, SummaryStatus::Pending).await;
        }
        None => {}
    }
    let ocr_update = match outcome.ocr {
        Some(Ok(OcrResult::Pages(pages))) => {
            let status = if pages.iter().any(|(_, t)| !t.is_empty()) {
                "done"
            } else {
                "none"
            };
            Some((pages, status))
        }
        Some(Ok(OcrResult::Skipped)) => Some((Vec::new(), "skipped")),
        Some(Err(_)) if job.is_cancelled() => None,
        Some(Err(e)) => {
            errors.push(e.context("text recognition"));
            Some((Vec::new(), "failed"))
        }
        None => None,
    };
    if let Some((pages, status)) = ocr_update {
        let id = item.id.clone();
        if let Err(e) = state
            .db
            .call(move |c| media::set_ocr(c, &id, &pages, status))
            .await
        {
            errors.push(e);
        }
    }

    let mut errors = errors.into_iter();
    match errors.next() {
        None if !job.is_cancelled() => job.inc_processed(),
        None => {}
        Some(first) => {
            job.record_failure(item.abs_path.clone(), &first);
            for e in errors {
                job.record_error(item.abs_path.clone(), &e);
            }
        }
    }
}

async fn update_status(state: &AppState, id: &str, status: SummaryStatus) -> anyhow::Result<()> {
    let id = id.to_string();
    state
        .db
        .call(move |c| media::set_status(c, &id, status))
        .await
}

/// Fetches the file again from the source and runs the requested steps.
async fn analyze(
    state: &AppState,
    job: &Job,
    item: &MediaCandidate,
) -> anyhow::Result<ItemOutcome> {
    if item.kind == FileKind::Document && item.doc_type.as_deref() != Some("pdf") {
        // Text and code are summarized from the text extracted at index time (kept
        // current by the hash check), so the file is not fetched again.
        let summary = if item.describe {
            Some(summarize_document(state, item).await)
        } else {
            None
        };
        return Ok(ItemOutcome { summary, ocr: None });
    }
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

    Ok(match item.kind {
        FileKind::Image => analyze_image(state, item, staged.path()).await,
        FileKind::Video if item.describe => ItemOutcome {
            summary: Some(analyze_video(state, job, item, staged).await),
            ocr: None,
        },
        FileKind::Video => ItemOutcome::default(),
        FileKind::Document => analyze_pdf(state, job, item, staged.path()).await?,
        FileKind::Archive => bail!("archives have no LLM step"),
    })
}

async fn analyze_image(state: &AppState, item: &MediaCandidate, path: &Path) -> ItemOutcome {
    let mut out = ItemOutcome::default();
    let mut visible_text = None;
    if item.describe {
        let described = describe_image(state, &item.id, path, None).await;
        if let Ok((_, v)) = &described {
            visible_text = Some(v.clone().unwrap_or_default());
        }
        out.summary = Some(described.map(|(t, _)| t));
    }
    if !item.ocr {
        return out;
    }
    out.ocr = match state.config.llm.ocr_images {
        OcrMode::Never => Some(Ok(OcrResult::Skipped)),
        OcrMode::Always => Some(ocr_page(state, &item.id, path, 1).await.map(single_page)),
        OcrMode::Auto => {
            let visible = match visible_text {
                Some(v) => Ok(v),
                // Described in an earlier run: use what that description reported.
                None if !item.describe => {
                    let id = item.id.clone();
                    state
                        .db
                        .call(move |c| media::latest_visible_text(c, &id))
                        .await
                        .map(Option::unwrap_or_default)
                }
                // The description failed in this run; decide next time.
                None => return out,
            };
            Some(match visible {
                Ok(v) if v.trim().is_empty() => Ok(OcrResult::Pages(Vec::new())),
                Ok(_) => ocr_page(state, &item.id, path, 1).await.map(single_page),
                Err(e) => Err(e),
            })
        }
    };
    out
}

fn single_page(text: String) -> OcrResult {
    OcrResult::Pages(vec![(1, text)])
}

/// PDFs: the first page is described like an image; scanned PDFs (no text layer) are
/// transcribed page by page.
async fn analyze_pdf(
    state: &AppState,
    job: &Job,
    item: &MediaCandidate,
    path: &Path,
) -> anyhow::Result<ItemOutcome> {
    let cfg = &state.config.llm;
    let pages = if item.ocr {
        item.page_count
            .unwrap_or(cfg.pdf_ocr_max_pages)
            .clamp(1, cfg.pdf_ocr_max_pages.max(1))
    } else {
        1
    };
    let dir = state.staging.temp_dir()?;
    let rendered = pdf_pages::render(path, dir.path(), pages, cfg.pdf_render_dpi).await?;

    let mut out = ItemOutcome::default();
    if item.describe {
        let (_, first) = &rendered[0];
        out.summary = Some(
            describe_image(state, &item.id, first, None)
                .await
                .map(|(t, _)| t),
        );
    }
    if item.ocr {
        let mut texts = Vec::with_capacity(rendered.len());
        let mut last_error = None;
        for (page, image) in &rendered {
            if job.is_cancelled() {
                last_error = Some(anyhow!("cancelled"));
                break;
            }
            match ocr_page(state, &item.id, image, *page).await {
                Ok(text) => texts.push((*page, text)),
                Err(e) => {
                    job.record_error(format!("{} page {page}", item.abs_path), &e);
                    last_error = Some(e);
                }
            }
        }
        out.ocr = Some(match last_error {
            Some(e) if texts.is_empty() || job.is_cancelled() => Err(e),
            _ => Ok(OcrResult::Pages(texts)),
        });
    }
    Ok(out)
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
            Ok((text, _)) => described.push((*ts, text)),
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
        MAX_TOKENS,
        describe_text,
    )
    .await
    .map(|(text, _)| text)
}

/// Minimum non-whitespace characters for a document to be worth summarizing.
const MIN_DOC_CHARS: usize = 20;

/// Summarizes a text/markdown/code document. Returns an empty string for (nearly)
/// empty documents, which are then marked `skipped` without an LLM call.
async fn summarize_document(state: &AppState, item: &MediaCandidate) -> anyhow::Result<String> {
    let id = item.id.clone();
    let doc = state.db.call(move |c| media::document_text(c, &id)).await?;
    if doc.content.chars().filter(|c| !c.is_whitespace()).count() < MIN_DOC_CHARS {
        return Ok(String::new());
    }
    let total = doc.content.chars().count();
    let head: String = doc
        .content
        .chars()
        .take(state.config.llm.doc_summary_max_chars)
        .collect();
    let prompt = prompts::doc_user(
        &doc.file_name,
        &doc.doc_type,
        doc.language.as_deref(),
        &head,
        total,
    );
    let (text, _) = call_llm(
        state,
        &item.id,
        None,
        prompts::DOC_PROMPT_VERSION,
        prompts::DOC_SYSTEM,
        vec![Part::Text(prompt)],
        MAX_TOKENS,
        describe_text,
    )
    .await?;
    Ok(text)
}

/// Describes an image; returns the searchable text and the `visible_text` it reported.
async fn describe_image(
    state: &AppState,
    file_id: &str,
    path: &Path,
    ts: Option<f64>,
) -> anyhow::Result<(String, Option<String>)> {
    let edge = state.config.llm.image_max_edge;
    let p = path.to_path_buf();
    let jpeg = tokio::task::spawn_blocking(move || prepare::to_llm_jpeg(&p, edge)).await??;
    let (text, parsed) = call_llm(
        state,
        file_id,
        ts,
        prompts::IMAGE_PROMPT_VERSION,
        prompts::IMAGE_SYSTEM,
        vec![
            Part::Text(prompts::IMAGE_USER.to_string()),
            Part::Jpeg(jpeg),
        ],
        MAX_TOKENS,
        describe_text,
    )
    .await?;
    let visible = parsed
        .as_ref()
        .and_then(|v| v["visible_text"].as_str())
        .map(str::to_string);
    Ok((text, visible))
}

/// Transcribes the text in an image (higher resolution than for descriptions).
async fn ocr_page(
    state: &AppState,
    file_id: &str,
    path: &Path,
    page: u32,
) -> anyhow::Result<String> {
    let edge = state.config.llm.ocr_max_edge;
    let p = path.to_path_buf();
    let jpeg = tokio::task::spawn_blocking(move || prepare::to_llm_jpeg(&p, edge)).await??;
    let (text, _) = call_llm(
        state,
        file_id,
        None,
        prompts::OCR_PROMPT_VERSION,
        prompts::OCR_SYSTEM,
        vec![Part::Text(prompts::OCR_USER.to_string()), Part::Jpeg(jpeg)],
        OCR_MAX_TOKENS,
        |raw, _| prompts::parse_ocr(raw).or_else(|| Some(raw.trim().to_string())),
    )
    .await
    .with_context(|| format!("page {page}"))?;
    Ok(text)
}

/// Searchable text of a description answer; `None` if the answer is empty.
fn describe_text(raw: &str, parsed: Option<&Value>) -> Option<String> {
    parsed
        .map(prompts::searchable_text)
        .filter(|t| !t.is_empty())
        .or_else(|| prompts::salvage_summary(raw))
        .or_else(|| Some(raw.trim().to_string()))
        .filter(|t| !t.is_empty())
}

/// Calls the LLM, stores the attempt in `analyses`, and returns the text produced by
/// `interpret` together with the parsed JSON answer.
#[allow(clippy::too_many_arguments)]
async fn call_llm(
    state: &AppState,
    file_id: &str,
    ts: Option<f64>,
    prompt_version: &str,
    system: &str,
    parts: Vec<Part>,
    max_tokens: u32,
    interpret: impl Fn(&str, Option<&Value>) -> Option<String>,
) -> anyhow::Result<(String, Option<Value>)> {
    let mut rec = AnalysisRecord {
        file_id: file_id.to_string(),
        ts_secs: ts,
        model: state.llm.model().to_string(),
        prompt_version: prompt_version.to_string(),
        ..Default::default()
    };
    let result = match state.llm.chat(system, parts, max_tokens).await {
        Ok(c) => {
            let parsed = prompts::parse_json(&c.text);
            let text = interpret(&c.text, parsed.as_ref());
            if c.truncated {
                // Still usable (salvaged), but flagged for quality statistics.
                rec.error = Some(format!("output truncated at max_tokens={max_tokens}"));
            }
            rec.raw_response = Some(c.text);
            rec.parsed = parsed.clone();
            rec.latency_ms = Some(c.latency_ms);
            rec.prompt_tokens = c.prompt_tokens;
            rec.completion_tokens = c.completion_tokens;
            match text {
                Some(t) => Ok((t, parsed)),
                None => {
                    rec.error = Some("empty response".into());
                    Err(anyhow!("LLM returned an empty response"))
                }
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

use chrono::Utc;
use duckdb::{Connection, OptionalExt, params, params_from_iter};
use serde_json::Value;

use super::models::{FileKind, SummaryStatus};

/// One LLM call, kept for comparing extraction quality across prompts and models.
#[derive(Debug, Clone, Default)]
pub struct AnalysisRecord {
    pub file_id: String,
    pub ts_secs: Option<f64>,
    pub model: String,
    pub prompt_version: String,
    pub raw_response: Option<String>,
    pub parsed: Option<Value>,
    pub latency_ms: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MediaCandidate {
    pub id: String,
    pub abs_path: String,
    pub kind: FileKind,
    pub size_bytes: u64,
    pub duration_secs: Option<f64>,
    pub page_count: Option<u32>,
    pub doc_type: Option<String>,
    /// Needs an LLM description ("what we see") or, for text/code, a summary.
    pub describe: bool,
    /// Needs text recognition ("what text is inside").
    pub ocr: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CandidateFilter {
    pub kinds: Vec<FileKind>,
    pub path_prefix: Option<String>,
    pub ids: Vec<String>,
    pub limit: Option<u32>,
    /// Also re-process `done` and `failed` items.
    pub force: bool,
}

/// Selects work for `/import_media`:
/// - describe: images, videos and documents whose summary is pending (or done/failed with
///   `force`); PDFs are described from their first page, text/code is summarized
/// - ocr: scanned PDFs and images without an OCR result yet (or any with `force`); for
///   images only together with or after a successful description, which decides in
///   `auto` mode whether there is text at all
pub fn select_candidates(
    conn: &Connection,
    f: &CandidateFilter,
) -> anyhow::Result<Vec<MediaCandidate>> {
    let describe_status = if f.force {
        "f.summary_status IN ('pending', 'done', 'failed')"
    } else {
        "f.summary_status = 'pending'"
    };
    let ocr_status = if f.force {
        "true"
    } else {
        "f.ocr_status IS NULL"
    };
    let mut sql = format!(
        r#"SELECT * FROM (
             SELECT f.id, f.abs_path, f.kind, f.size_bytes, v.duration_secs, d.page_count,
                    ({describe_status} AND f.kind IN ('image', 'video', 'document')) AS do_describe,
                    ({ocr_status} AND f.summary_status <> 'skipped'
                     AND ((f.kind = 'image' AND ({describe_status} OR f.summary_status = 'done'))
                          OR coalesce(d.needs_ocr, false))) AS do_ocr,
                    d.doc_type
             FROM files f
             LEFT JOIN videos v ON v.file_id = f.id
             LEFT JOIN documents d ON d.file_id = f.id
             WHERE f.summary_status <> 'running' AND f.kind IN ({kinds})"#,
        kinds = vec!["?"; f.kinds.len()].join(", ")
    );
    let mut params: Vec<String> = f.kinds.iter().map(|k| k.as_str().to_string()).collect();
    if let Some(prefix) = &f.path_prefix {
        sql.push_str(" AND starts_with(f.abs_path, ?)");
        params.push(prefix.clone());
    }
    if !f.ids.is_empty() {
        sql.push_str(&format!(
            " AND f.id IN ({})",
            vec!["?"; f.ids.len()].join(", ")
        ));
        params.extend(f.ids.iter().cloned());
    }
    sql.push_str(") WHERE do_describe OR do_ocr ORDER BY abs_path");
    if let Some(limit) = f.limit {
        sql.push_str(&format!(" LIMIT {limit}"));
    }

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params.iter()), |r| {
        let kind: String = r.get(2)?;
        Ok(MediaCandidate {
            id: r.get(0)?,
            abs_path: r.get(1)?,
            kind: kind.parse().unwrap_or(FileKind::Image),
            size_bytes: r.get::<_, i64>(3)? as u64,
            duration_secs: r.get(4)?,
            page_count: r.get::<_, Option<i64>>(5)?.map(|p| p as u32),
            describe: r.get(6)?,
            ocr: r.get(7)?,
            doc_type: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Extracted text of a document together with what the summary prompt needs.
pub struct DocumentText {
    pub file_name: String,
    pub doc_type: String,
    pub language: Option<String>,
    pub content: String,
}

pub fn document_text(conn: &Connection, id: &str) -> anyhow::Result<DocumentText> {
    Ok(conn.query_row(
        r#"SELECT f.file_name, d.doc_type, d.language, coalesce(d.content, '')
           FROM files f JOIN documents d ON d.file_id = f.id WHERE f.id = ?"#,
        params![id],
        |r| {
            Ok(DocumentText {
                file_name: r.get(0)?,
                doc_type: r.get(1)?,
                language: r.get(2)?,
                content: r.get(3)?,
            })
        },
    )?)
}

/// `visible_text` reported by the latest description of a file (used to decide whether
/// an already described image needs OCR).
pub fn latest_visible_text(conn: &Connection, id: &str) -> anyhow::Result<Option<String>> {
    let text: Option<Option<String>> = conn
        .query_row(
            r#"SELECT parsed->>'$.visible_text' FROM analyses
               WHERE file_id = ? AND ts_secs IS NULL AND parsed IS NOT NULL
                 AND prompt_version LIKE 'img-%'
               ORDER BY created_at DESC LIMIT 1"#,
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(text.flatten())
}

/// Stores OCR text per page (images use page 1) and the resulting status:
/// `done` (text found), `none` (no text) or `failed`.
pub fn set_ocr(
    conn: &mut Connection,
    id: &str,
    pages: &[(u32, String)],
    status: &str,
) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM ocr WHERE file_id = ?", params![id])?;
    for (page, text) in pages.iter().filter(|(_, t)| !t.is_empty()) {
        tx.execute(
            "INSERT INTO ocr (file_id, page, text) VALUES (?, ?, ?)",
            params![id, page, text],
        )?;
    }
    tx.execute(
        "UPDATE files SET ocr_status = ? WHERE id = ?",
        params![status, id],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn set_status(conn: &Connection, id: &str, status: SummaryStatus) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE files SET summary_status = ? WHERE id = ?",
        params![status.as_str(), id],
    )?;
    Ok(())
}

pub fn set_summary(conn: &Connection, id: &str, summary: &str) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE files SET summary = ?, summary_status = 'done' WHERE id = ?",
        params![summary, id],
    )?;
    Ok(())
}

pub fn insert_analysis(conn: &Connection, a: &AnalysisRecord) -> anyhow::Result<()> {
    conn.execute(
        r#"INSERT INTO analyses (id, file_id, ts_secs, model, prompt_version, raw_response, parsed,
                                 latency_ms, prompt_tokens, completion_tokens, created_at, error)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        params![
            uuid::Uuid::new_v4().to_string(),
            a.file_id,
            a.ts_secs,
            a.model,
            a.prompt_version,
            a.raw_response,
            a.parsed.as_ref().map(Value::to_string),
            a.latency_ms,
            a.prompt_tokens,
            a.completion_tokens,
            Utc::now(),
            a.error,
        ],
    )?;
    Ok(())
}

pub fn replace_frames(
    conn: &mut Connection,
    id: &str,
    frames: &[(f64, String)],
) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM video_frames WHERE file_id = ?", params![id])?;
    for (ts, desc) in frames {
        tx.execute(
            "INSERT INTO video_frames (file_id, ts_secs, description) VALUES (?, ?, ?)",
            params![id, ts, desc],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Items left `running` by a killed process go back to the queue.
pub fn reset_running(conn: &Connection) -> anyhow::Result<usize> {
    Ok(conn.execute(
        "UPDATE files SET summary_status = 'pending' WHERE summary_status = 'running'",
        [],
    )?)
}

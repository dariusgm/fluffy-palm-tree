use chrono::Utc;
use duckdb::{Connection, params, params_from_iter};
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

pub fn select_candidates(
    conn: &Connection,
    f: &CandidateFilter,
) -> anyhow::Result<Vec<MediaCandidate>> {
    let mut sql = String::from(
        "SELECT f.id, f.abs_path, f.kind, f.size_bytes, v.duration_secs
         FROM files f LEFT JOIN videos v ON v.file_id = f.id WHERE ",
    );
    let mut params: Vec<String> = Vec::new();

    sql.push_str(&format!(
        "f.kind IN ({})",
        vec!["?"; f.kinds.len()].join(", ")
    ));
    params.extend(f.kinds.iter().map(|k| k.as_str().to_string()));

    if f.force {
        sql.push_str(" AND f.summary_status IN ('pending', 'done', 'failed')");
    } else {
        sql.push_str(" AND f.summary_status = 'pending'");
    }
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
    sql.push_str(" ORDER BY f.abs_path");
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
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
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

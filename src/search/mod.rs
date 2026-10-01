pub mod query;

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use duckdb::{Row, params_from_iter};
use serde::Serialize;

use crate::db::Db;
use query::{BuiltQuery, SearchRequest};

const MAX_FRAMES_PER_RESULT: usize = 10;

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub total: u64,
    pub limit: u32,
    pub offset: u32,
    pub text_mode: Option<&'static str>,
    pub results: Vec<SearchHit>,
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub id: String,
    pub kind: String,
    pub root: String,
    pub rel_path: String,
    pub path: String,
    pub name: String,
    pub extension: Option<String>,
    pub mime: Option<String>,
    pub size_bytes: i64,
    /// Octal permission bits, e.g. "0644".
    pub mode: String,
    pub mode_str: String,
    pub uid: Option<i64>,
    pub gid: Option<i64>,
    /// Last modification time.
    pub mtime: Option<DateTime<Utc>>,
    /// Creation (birth) time, if the filesystem reports it.
    pub created: Option<DateTime<Utc>>,
    pub sha256: Option<String>,
    pub summary: String,
    pub summary_status: String,
    pub indexed_at: DateTime<Utc>,
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<DocumentHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive: Option<ArchiveHit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_frames: Vec<FrameHit>,
}

#[derive(Debug, Serialize)]
pub struct ImageHit {
    pub format: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct DocumentHit {
    pub doc_type: String,
    pub page_count: Option<i64>,
    pub snippet: Option<String>,
    pub language: Option<String>,
    pub encoding: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ArchiveHit {
    pub compression: String,
    pub format: String,
}

#[derive(Debug, Serialize)]
pub struct VideoHit {
    pub duration_secs: Option<f64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub fps: Option<f64>,
    pub container: Option<String>,
    pub bitrate: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct FrameHit {
    pub ts_secs: f64,
    pub description: String,
}

pub async fn search(db: &Db, req: SearchRequest) -> Result<anyhow::Result<SearchResponse>, String> {
    let built = query::build(&req, db.fts_available())?;
    Ok(db.call(move |c| execute(c, built)).await)
}

fn execute(conn: &mut duckdb::Connection, q: BuiltQuery) -> anyhow::Result<SearchResponse> {
    let mut stmt = conn.prepare(&q.sql)?;
    let mut total = 0u64;
    let mut results = stmt
        .query_map(params_from_iter(q.params.iter()), |r| {
            let (hit, t) = hit_from_row(r)?;
            Ok((hit, t))
        })?
        .map(|row| {
            row.map(|(hit, t)| {
                total = t;
                hit
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    if !q.terms.is_empty() {
        attach_frames(conn, &mut results, &q.terms)?;
    }

    Ok(SearchResponse {
        total,
        limit: q.limit,
        offset: q.offset,
        text_mode: q.text_mode.map(|m| m.as_str()),
        results,
    })
}

/// Adds the video frames whose descriptions contain any search term.
fn attach_frames(
    conn: &duckdb::Connection,
    results: &mut [SearchHit],
    terms: &[String],
) -> anyhow::Result<()> {
    let ids: Vec<String> = results
        .iter()
        .filter(|h| h.kind == "video")
        .map(|h| h.id.clone())
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let id_marks = vec!["?"; ids.len()].join(", ");
    let term_conds = vec!["description ILIKE ?"; terms.len()].join(" OR ");
    let sql = format!(
        "SELECT file_id, ts_secs, description FROM video_frames
         WHERE file_id IN ({id_marks}) AND ({term_conds}) ORDER BY file_id, ts_secs"
    );
    let params = ids
        .iter()
        .cloned()
        .chain(terms.iter().map(|t| format!("%{t}%")));
    let mut stmt = conn.prepare(&sql)?;
    let mut frames: HashMap<String, Vec<FrameHit>> = HashMap::new();
    let rows = stmt.query_map(params_from_iter(params), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    for row in rows {
        let (id, ts_secs, description) = row?;
        let list = frames.entry(id).or_default();
        if list.len() < MAX_FRAMES_PER_RESULT {
            list.push(FrameHit {
                ts_secs,
                description: description.unwrap_or_default(),
            });
        }
    }
    for hit in results {
        if let Some(f) = frames.remove(&hit.id) {
            hit.matched_frames = f;
        }
    }
    Ok(())
}

fn hit_from_row(r: &Row<'_>) -> duckdb::Result<(SearchHit, u64)> {
    let kind: String = r.get(6)?;
    let mode: i64 = r.get(9)?;
    let image = (kind == "image").then(|| -> duckdb::Result<ImageHit> {
        Ok(ImageHit {
            format: r.get(18)?,
            width: r.get(19)?,
            height: r.get(20)?,
        })
    });
    let doc_type: Option<String> = r.get(21)?;
    let document = doc_type.map(|doc_type| -> duckdb::Result<DocumentHit> {
        Ok(DocumentHit {
            doc_type,
            page_count: r.get(22)?,
            snippet: r.get(23)?,
            language: r.get(33)?,
            encoding: r.get(34)?,
        })
    });
    let archive = (kind == "archive").then(|| -> duckdb::Result<ArchiveHit> {
        Ok(ArchiveHit {
            compression: r.get(35)?,
            format: r.get(36)?,
        })
    });
    let video = (kind == "video").then(|| -> duckdb::Result<VideoHit> {
        Ok(VideoHit {
            duration_secs: r.get(24)?,
            width: r.get(25)?,
            height: r.get(26)?,
            video_codec: r.get(27)?,
            audio_codec: r.get(28)?,
            fps: r.get(29)?,
            container: r.get(30)?,
            bitrate: r.get(31)?,
        })
    });
    let hit = SearchHit {
        id: r.get(0)?,
        root: r.get(1)?,
        rel_path: r.get(2)?,
        path: r.get(3)?,
        name: r.get(4)?,
        extension: r.get(5)?,
        mime: r.get(7)?,
        size_bytes: r.get(8)?,
        mode: format!("{mode:04o}"),
        mode_str: r.get(10)?,
        uid: r.get(11)?,
        gid: r.get(12)?,
        mtime: r.get(13)?,
        sha256: r.get(14)?,
        summary: r.get(15)?,
        summary_status: r.get(16)?,
        indexed_at: r.get(17)?,
        created: r.get(32)?,
        archive: archive.transpose()?,
        score: r.get(37)?,
        image: image.transpose()?,
        document: document.transpose()?,
        video: video.transpose()?,
        matched_frames: Vec::new(),
        kind,
    };
    let total: i64 = r.get(38)?;
    Ok((hit, total as u64))
}

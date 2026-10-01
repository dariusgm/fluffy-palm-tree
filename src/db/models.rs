use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use duckdb::{Connection, OptionalExt, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Image,
    Document,
    Video,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::Image => "image",
            FileKind::Document => "document",
            FileKind::Video => "video",
        }
    }
}

impl fmt::Display for FileKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FileKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "image" => Ok(Self::Image),
            "document" => Ok(Self::Document),
            "video" => Ok(Self::Video),
            other => Err(format!("unknown kind {other:?}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocType {
    Text,
    Markdown,
    Pdf,
}

impl DocType {
    pub fn as_str(self) -> &'static str {
        match self {
            DocType::Text => "text",
            DocType::Markdown => "markdown",
            DocType::Pdf => "pdf",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl SummaryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SummaryStatus::Pending => "pending",
            SummaryStatus::Running => "running",
            SummaryStatus::Done => "done",
            SummaryStatus::Failed => "failed",
            SummaryStatus::Skipped => "skipped",
        }
    }
}

/// Base record stored for every indexed file.
#[derive(Debug, Clone)]
pub struct FileRecord {
    pub root: String,
    pub rel_path: String,
    pub abs_path: String,
    pub file_name: String,
    pub extension: Option<String>,
    pub kind: FileKind,
    pub mime: Option<String>,
    pub size_bytes: u64,
    /// Permission bits only (`mode & 0o7777`).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: Option<DateTime<Utc>>,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageMeta {
    pub format: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocumentMeta {
    pub doc_type: DocType,
    pub page_count: Option<u32>,
    pub content: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoMeta {
    pub duration_secs: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub fps: Option<f64>,
    pub container: Option<String>,
    pub bitrate: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum Details {
    Image(ImageMeta),
    Document(DocumentMeta),
    Video(VideoMeta),
    /// Base record only (e.g. file too large or extraction failed).
    None,
}

/// What the DB knows about a file, used to skip unchanged files on re-index.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredStat {
    pub id: String,
    pub size_bytes: u64,
    pub mtime: Option<DateTime<Utc>>,
}

/// Renders permission bits like `ls -l`, e.g. `rw-r--r--`.
pub fn mode_string(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

pub fn find_stat(
    conn: &Connection,
    root: &str,
    rel_path: &str,
) -> anyhow::Result<Option<StoredStat>> {
    let row = conn
        .query_row(
            "SELECT id, size_bytes, mtime FROM files WHERE root = ? AND rel_path = ?",
            params![root, rel_path],
            |r| {
                Ok(StoredStat {
                    id: r.get(0)?,
                    size_bytes: r.get::<_, i64>(1)? as u64,
                    mtime: r.get(2)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Inserts or updates a file and its type-specific metadata in one transaction.
/// Re-indexing a changed file resets its summary, since the content may differ.
pub fn save_indexed(
    conn: &mut Connection,
    rec: &FileRecord,
    details: &Details,
    status: SummaryStatus,
) -> anyhow::Result<String> {
    let tx = conn.transaction()?;
    let new_id = uuid::Uuid::new_v4().to_string();
    let id: String = tx.query_row(
        r#"INSERT INTO files (id, root, rel_path, abs_path, file_name, extension, kind, mime,
                              size_bytes, mode, mode_str, uid, gid, mtime, sha256,
                              summary, summary_status, indexed_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '', ?, ?)
           ON CONFLICT (root, rel_path) DO UPDATE SET
               abs_path = excluded.abs_path,
               file_name = excluded.file_name,
               extension = excluded.extension,
               kind = excluded.kind,
               mime = excluded.mime,
               size_bytes = excluded.size_bytes,
               mode = excluded.mode,
               mode_str = excluded.mode_str,
               uid = excluded.uid,
               gid = excluded.gid,
               mtime = excluded.mtime,
               sha256 = excluded.sha256,
               summary = '',
               summary_status = excluded.summary_status,
               indexed_at = excluded.indexed_at
           RETURNING id"#,
        params![
            new_id,
            rec.root,
            rec.rel_path,
            rec.abs_path,
            rec.file_name,
            rec.extension,
            rec.kind.as_str(),
            rec.mime,
            rec.size_bytes as i64,
            rec.mode,
            mode_string(rec.mode),
            rec.uid,
            rec.gid,
            rec.mtime,
            rec.sha256,
            status.as_str(),
            Utc::now(),
        ],
        |r| r.get(0),
    )?;

    for table in ["images", "documents", "videos", "video_frames"] {
        tx.execute(
            &format!("DELETE FROM {table} WHERE file_id = ?"),
            params![id],
        )?;
    }
    match details {
        Details::Image(m) => {
            tx.execute(
                "INSERT INTO images (file_id, format, width, height) VALUES (?, ?, ?, ?)",
                params![id, m.format, m.width, m.height],
            )?;
        }
        Details::Document(m) => {
            tx.execute(
                "INSERT INTO documents (file_id, doc_type, page_count, content) VALUES (?, ?, ?, ?)",
                params![id, m.doc_type.as_str(), m.page_count, m.content],
            )?;
        }
        Details::Video(m) => {
            tx.execute(
                r#"INSERT INTO videos (file_id, duration_secs, width, height, video_codec,
                                       audio_codec, fps, container, bitrate)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
                params![
                    id,
                    m.duration_secs,
                    m.width,
                    m.height,
                    m.video_codec,
                    m.audio_codec,
                    m.fps,
                    m.container,
                    m.bitrate.map(|b| b as i64),
                ],
            )?;
        }
        Details::None => {}
    }
    tx.commit()?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn record(size: u64) -> FileRecord {
        FileRecord {
            root: "r".into(),
            rel_path: "a/b.png".into(),
            abs_path: "/x/a/b.png".into(),
            file_name: "b.png".into(),
            extension: Some("png".into()),
            kind: FileKind::Image,
            mime: Some("image/png".into()),
            size_bytes: size,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            mtime: Some(DateTime::from_timestamp_micros(1_700_000_000_123_456).unwrap()),
            sha256: None,
        }
    }

    #[test]
    fn mode_strings() {
        assert_eq!(mode_string(0o644), "rw-r--r--");
        assert_eq!(mode_string(0o755), "rwxr-xr-x");
        assert_eq!(mode_string(0o000), "---------");
    }

    #[tokio::test]
    async fn upsert_keeps_id_and_replaces_details() {
        let db = Db::open_in_memory().unwrap();
        let (id1, id2, stat, height, count) = db
            .call(|c| {
                let img = |h| {
                    Details::Image(ImageMeta {
                        format: Some("png".into()),
                        width: Some(10),
                        height: Some(h),
                    })
                };
                let id1 = save_indexed(c, &record(10), &img(20), SummaryStatus::Pending)?;
                c.execute(
                    "UPDATE files SET summary = 'old', summary_status = 'done'",
                    [],
                )?;
                let id2 = save_indexed(c, &record(99), &img(30), SummaryStatus::Pending)?;
                let stat = find_stat(c, "r", "a/b.png")?.unwrap();
                let height: u32 = c.query_row("SELECT height FROM images", [], |r| r.get(0))?;
                let count: i64 =
                    c.query_row("SELECT count(*) FROM files WHERE summary = ''", [], |r| {
                        r.get(0)
                    })?;
                Ok((id1, id2, stat, height, count))
            })
            .await
            .unwrap();
        assert_eq!(id1, id2);
        assert_eq!(stat.size_bytes, 99);
        assert_eq!(stat.mtime, record(0).mtime);
        assert_eq!(height, 30);
        assert_eq!(count, 1, "summary is reset on re-index");
    }
}

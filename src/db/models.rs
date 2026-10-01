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
    Archive,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::Image => "image",
            FileKind::Document => "document",
            FileKind::Video => "video",
            FileKind::Archive => "archive",
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
            "archive" => Ok(Self::Archive),
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
    /// Source code, markup and config files (language in `DocumentMeta::language`).
    Code,
}

impl DocType {
    pub fn as_str(self) -> &'static str {
        match self {
            DocType::Text => "text",
            DocType::Markdown => "markdown",
            DocType::Pdf => "pdf",
            DocType::Code => "code",
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
    /// Birth time, if the filesystem reports it (not on every mount, e.g. not GVFS).
    pub created: Option<DateTime<Utc>>,
    pub sha256: Option<String>,
}

/// Stat-derived fields that can be refreshed without reading the file.
#[derive(Debug, Clone)]
pub struct StatFields {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub created: Option<DateTime<Utc>>,
}

/// Bump when new per-file metadata is extracted, so re-indexing fills it in for
/// files that are otherwise unchanged (without touching their LLM results).
pub const META_VERSION: i32 = 3;

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
    pub language: Option<String>,
    /// Character encoding of text documents: ascii, utf-8, utf-16le/be, windows-1252.
    pub encoding: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveMeta {
    /// gzip, bzip2, xz, zstd, lz4, lzip, compress, zip, 7z, rar or none (plain tar).
    pub compression: String,
    /// e.g. gz, tar.gz, zip, 7z, tar.
    pub format: String,
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
    Archive(ArchiveMeta),
    /// Base record only (e.g. file too large or extraction failed).
    None,
}

/// What the DB knows about a file, used to skip unchanged files on re-index.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredStat {
    pub id: String,
    pub size_bytes: u64,
    pub mtime: Option<DateTime<Utc>>,
    pub sha256: Option<String>,
    pub meta_version: i32,
    pub kind: String,
}

/// Tables holding rows that belong to a file. All of them are cleared when the file's
/// record is removed, so the database only describes the current content.
const FILE_CHILD_TABLES: &[&str] = &[
    "images",
    "documents",
    "videos",
    "archives",
    "video_frames",
    "analyses",
    "tags",
];

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
            "SELECT id, size_bytes, mtime, sha256, meta_version, kind FROM files WHERE root = ? AND rel_path = ?",
            params![root, rel_path],
            |r| {
                Ok(StoredStat {
                    id: r.get(0)?,
                    size_bytes: r.get::<_, i64>(1)? as u64,
                    mtime: r.get(2)?,
                    sha256: r.get(3)?,
                    meta_version: r.get::<_, Option<i32>>(4)?.unwrap_or(1),
                    kind: r.get(5)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Removes a file and everything derived from it (metadata, frames, LLM history, tags).
pub fn delete_file(conn: &mut Connection, id: &str) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    for table in FILE_CHILD_TABLES {
        tx.execute(
            &format!("DELETE FROM {table} WHERE file_id = ?"),
            params![id],
        )?;
    }
    tx.execute("DELETE FROM files WHERE id = ?", params![id])?;
    tx.commit()?;
    Ok(())
}

/// Same content (hash) but new stat data, e.g. after a `touch` or a copy that kept the
/// bytes: refresh the stat columns and keep metadata and LLM results.
pub fn touch_unchanged(conn: &Connection, id: &str, rec: &FileRecord) -> anyhow::Result<()> {
    conn.execute(
        r#"UPDATE files SET size_bytes = ?, mode = ?, mode_str = ?, uid = ?, gid = ?,
                  mtime = ?, created = ?, sha256 = ?, meta_version = ?, indexed_at = ?
           WHERE id = ?"#,
        params![
            rec.size_bytes as i64,
            rec.mode,
            mode_string(rec.mode),
            rec.uid,
            rec.gid,
            rec.mtime,
            rec.created,
            rec.sha256,
            META_VERSION,
            Utc::now(),
            id
        ],
    )?;
    Ok(())
}

/// Unchanged file indexed by an older version: fill in the stat-based metadata added
/// since (e.g. `created`) without reading the file and without touching other data.
pub fn refresh_stat(conn: &Connection, id: &str, stat: &StatFields) -> anyhow::Result<()> {
    conn.execute(
        r#"UPDATE files SET mode = ?, mode_str = ?, uid = ?, gid = ?, created = ?,
                  meta_version = ?, indexed_at = ?
           WHERE id = ?"#,
        params![
            stat.mode,
            mode_string(stat.mode),
            stat.uid,
            stat.gid,
            stat.created,
            META_VERSION,
            Utc::now(),
            id
        ],
    )?;
    Ok(())
}

/// Replaces only the document metadata of an existing record (keeps id, summary, tags).
pub fn replace_document(conn: &mut Connection, id: &str, m: &DocumentMeta) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM documents WHERE file_id = ?", params![id])?;
    tx.execute(
        "INSERT INTO documents (file_id, doc_type, page_count, content, language, encoding)
         VALUES (?, ?, ?, ?, ?, ?)",
        params![
            id,
            m.doc_type.as_str(),
            m.page_count,
            m.content,
            m.language,
            m.encoding
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// Copies the LLM summary and frame descriptions from another file with identical
/// content (duplicate or moved file), so it does not need another LLM run.
/// Returns true if something was reused.
pub fn reuse_analysis(conn: &mut Connection, id: &str, sha256: &str) -> anyhow::Result<bool> {
    let donor: Option<(String, String)> = conn
        .query_row(
            r#"SELECT id, summary FROM files
               WHERE sha256 = ? AND id <> ? AND summary_status = 'done'
               ORDER BY indexed_at DESC LIMIT 1"#,
            params![sha256, id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((donor_id, summary)) = donor else {
        return Ok(false);
    };
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE files SET summary = ?, summary_status = 'done' WHERE id = ?",
        params![summary, id],
    )?;
    tx.execute("DELETE FROM video_frames WHERE file_id = ?", params![id])?;
    tx.execute(
        r#"INSERT INTO video_frames (file_id, ts_secs, description)
           SELECT ?, ts_secs, description FROM video_frames WHERE file_id = ?"#,
        params![id, donor_id],
    )?;
    tx.commit()?;
    Ok(true)
}

/// Deletes records inside an indexed directory scope whose files were not seen during
/// the walk (deleted or moved at the source). `scope` is the directory relative to the
/// root ("" for the root itself); with `recursive = false` only direct children count.
pub fn delete_unseen(
    conn: &mut Connection,
    root: &str,
    scope: &str,
    recursive: bool,
    seen: &std::collections::HashSet<String>,
) -> anyhow::Result<usize> {
    let prefix = if scope.is_empty() {
        String::new()
    } else {
        format!("{scope}/")
    };
    let candidates: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, rel_path FROM files WHERE root = ? AND starts_with(rel_path, ?)",
        )?;
        let rows = stmt.query_map(params![root, prefix], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut removed = 0;
    for (id, rel) in candidates {
        let rest = &rel[prefix.len()..];
        let in_scope = recursive || !rest.contains('/');
        if in_scope && !seen.contains(&rel) {
            delete_file(conn, &id)?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Stores a file and its type-specific metadata. A record that already exists at the
/// same path is removed first, including everything derived from it, so the database
/// only ever describes the file's current content.
pub fn save_indexed(
    conn: &mut Connection,
    rec: &FileRecord,
    details: &Details,
    status: SummaryStatus,
) -> anyhow::Result<String> {
    if let Some(old) = find_stat(conn, &rec.root, &rec.rel_path)? {
        delete_file(conn, &old.id)?;
    }
    let tx = conn.transaction()?;
    let id = uuid::Uuid::new_v4().to_string();
    tx.execute(
        r#"INSERT INTO files (id, root, rel_path, abs_path, file_name, extension, kind, mime,
                              size_bytes, mode, mode_str, uid, gid, mtime, created, sha256,
                              summary, summary_status, meta_version, indexed_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '', ?, ?, ?)"#,
        params![
            id,
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
            rec.created,
            rec.sha256,
            status.as_str(),
            META_VERSION,
            Utc::now(),
        ],
    )?;

    match details {
        Details::Image(m) => {
            tx.execute(
                "INSERT INTO images (file_id, format, width, height) VALUES (?, ?, ?, ?)",
                params![id, m.format, m.width, m.height],
            )?;
        }
        Details::Document(m) => {
            tx.execute(
                "INSERT INTO documents (file_id, doc_type, page_count, content, language, encoding)
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    id,
                    m.doc_type.as_str(),
                    m.page_count,
                    m.content,
                    m.language,
                    m.encoding
                ],
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
        Details::Archive(m) => {
            tx.execute(
                "INSERT INTO archives (file_id, compression, format) VALUES (?, ?, ?)",
                params![id, m.compression, m.format],
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
            created: None,
            sha256: None,
        }
    }

    #[test]
    fn mode_strings() {
        assert_eq!(mode_string(0o644), "rw-r--r--");
        assert_eq!(mode_string(0o755), "rwxr-xr-x");
        assert_eq!(mode_string(0o000), "---------");
    }

    fn img(h: u32) -> Details {
        Details::Image(ImageMeta {
            format: Some("png".into()),
            width: Some(10),
            height: Some(h),
        })
    }

    fn count(c: &Connection, sql: &str) -> i64 {
        c.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[tokio::test]
    async fn changed_content_replaces_record_and_derived_rows() {
        let db = Db::open_in_memory().unwrap();
        db.call(|c| {
            let id1 = save_indexed(c, &record(10), &img(20), SummaryStatus::Pending)?;
            c.execute_batch(&format!(
                "UPDATE files SET summary = 'old', summary_status = 'done';
                 INSERT INTO video_frames VALUES ('{id1}', 0, 'old frame');
                 INSERT INTO tags VALUES ('{id1}', 'old', 'manual');
                 INSERT INTO analyses (id, file_id, created_at) VALUES ('a1', '{id1}', now());"
            ))?;
            let id2 = save_indexed(c, &record(99), &img(30), SummaryStatus::Pending)?;
            assert_ne!(id1, id2, "a new record replaces the old one");
            let stat = find_stat(c, "r", "a/b.png")?.unwrap();
            assert_eq!(stat.id, id2);
            assert_eq!(stat.size_bytes, 99);
            assert_eq!(count(c, "SELECT count(*) FROM files"), 1);
            assert_eq!(count(c, "SELECT count(*) FROM files WHERE summary = ''"), 1);
            assert_eq!(count(c, "SELECT count(*) FROM images"), 1);
            assert_eq!(count(c, "SELECT height FROM images"), 30);
            for t in ["video_frames", "tags", "analyses"] {
                assert_eq!(count(c, &format!("SELECT count(*) FROM {t}")), 0, "{t}");
            }
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn touch_keeps_results_and_reuse_copies_them() {
        let db = Db::open_in_memory().unwrap();
        db.call(|c| {
            let mut rec = record(10);
            rec.sha256 = Some("abc".into());
            let id1 = save_indexed(c, &rec, &img(20), SummaryStatus::Pending)?;
            c.execute_batch(&format!(
                "UPDATE files SET summary = 'a red square', summary_status = 'done';
                 INSERT INTO video_frames VALUES ('{id1}', 5, 'frame text');"
            ))?;

            rec.mtime = None;
            touch_unchanged(c, &id1, &rec)?;
            assert_eq!(find_stat(c, "r", "a/b.png")?.unwrap().mtime, None);
            assert_eq!(count(c, "SELECT count(*) FROM files WHERE summary = 'a red square'"), 1);

            let mut copy = rec.clone();
            copy.rel_path = "copy.png".into();
            let id2 = save_indexed(c, &copy, &img(20), SummaryStatus::Pending)?;
            assert!(reuse_analysis(c, &id2, "abc")?);
            assert_eq!(
                count(c, "SELECT count(*) FROM files WHERE summary = 'a red square' AND summary_status = 'done'"),
                2
            );
            assert_eq!(count(c, "SELECT count(*) FROM video_frames"), 2);
            assert!(!reuse_analysis(c, &id2, "other-hash")?);
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn delete_unseen_respects_scope() {
        let db = Db::open_in_memory().unwrap();
        db.call(|c| {
            for rel in [
                "top.png",
                "dir/a.png",
                "dir/b.png",
                "dir/sub/c.png",
                "dirx/d.png",
            ] {
                let mut r = record(1);
                r.rel_path = rel.into();
                save_indexed(c, &r, &img(1), SummaryStatus::Pending)?;
            }
            let seen = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
            // non-recursive in "dir": only direct children are candidates
            assert_eq!(
                delete_unseen(c, "r", "dir", false, &seen(&["dir/a.png"]))?,
                1
            );
            assert!(find_stat(c, "r", "dir/b.png")?.is_none());
            assert!(find_stat(c, "r", "dir/sub/c.png")?.is_some());
            assert!(
                find_stat(c, "r", "dirx/d.png")?.is_some(),
                "sibling prefix untouched"
            );
            // recursive from the root
            assert_eq!(delete_unseen(c, "r", "", true, &seen(&["top.png"]))?, 3);
            assert_eq!(count(c, "SELECT count(*) FROM files"), 1);
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn database_file_survives_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/search.duckdb");
        {
            let db = Db::open(&path).unwrap();
            db.call(|c| save_indexed(c, &record(7), &img(1), SummaryStatus::Pending))
                .await
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let stat = db
            .call(|c| find_stat(c, "r", "a/b.png"))
            .await
            .unwrap()
            .expect("record persisted");
        assert_eq!(stat.size_bytes, 7);
    }
}

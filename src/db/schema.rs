use anyhow::Context;
use duckdb::{Connection, params};

/// Ordered migrations. Never edit an applied entry; append a new one instead.
const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE files (
    id             TEXT PRIMARY KEY,
    root           TEXT NOT NULL,
    rel_path       TEXT NOT NULL,
    abs_path       TEXT NOT NULL,
    file_name      TEXT NOT NULL,
    extension      TEXT,
    kind           TEXT NOT NULL,
    mime           TEXT,
    size_bytes     BIGINT NOT NULL,
    mode           INTEGER NOT NULL,
    mode_str       TEXT NOT NULL,
    uid            INTEGER,
    gid            INTEGER,
    mtime          TIMESTAMP,
    sha256         TEXT,
    summary        TEXT NOT NULL DEFAULT '',
    summary_status TEXT NOT NULL DEFAULT 'pending',
    indexed_at     TIMESTAMP NOT NULL,
    UNIQUE (root, rel_path)
);
CREATE TABLE images (
    file_id TEXT PRIMARY KEY,
    format  TEXT,
    width   INTEGER,
    height  INTEGER
);
CREATE TABLE documents (
    file_id    TEXT PRIMARY KEY,
    doc_type   TEXT NOT NULL,
    page_count INTEGER,
    content    TEXT
);
CREATE TABLE videos (
    file_id       TEXT PRIMARY KEY,
    duration_secs DOUBLE,
    width         INTEGER,
    height        INTEGER,
    video_codec   TEXT,
    audio_codec   TEXT,
    fps           DOUBLE,
    container     TEXT,
    bitrate       BIGINT
);
CREATE TABLE video_frames (
    file_id     TEXT NOT NULL,
    ts_secs     DOUBLE NOT NULL,
    description TEXT,
    PRIMARY KEY (file_id, ts_secs)
);
CREATE TABLE analyses (
    id                TEXT PRIMARY KEY,
    file_id           TEXT NOT NULL,
    model             TEXT,
    prompt_version    TEXT,
    raw_response      TEXT,
    parsed            JSON,
    latency_ms        BIGINT,
    prompt_tokens     INTEGER,
    completion_tokens INTEGER,
    created_at        TIMESTAMP NOT NULL,
    error             TEXT
);
CREATE TABLE jobs (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    params      JSON,
    status      TEXT NOT NULL,
    found       BIGINT NOT NULL DEFAULT 0,
    processed   BIGINT NOT NULL DEFAULT 0,
    failed      BIGINT NOT NULL DEFAULT 0,
    skipped     BIGINT NOT NULL DEFAULT 0,
    started_at  TIMESTAMP NOT NULL,
    finished_at TIMESTAMP,
    error       TEXT
);
CREATE TABLE tags (
    file_id TEXT NOT NULL,
    tag     TEXT NOT NULL,
    source  TEXT,
    PRIMARY KEY (file_id, tag)
);
"#];

pub fn migrate(conn: &mut Connection) -> anyhow::Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);")?;
    let current: i64 = conn.query_row(
        "SELECT coalesce(max(version), 0) FROM schema_version",
        [],
        |r| r.get(0),
    )?;
    for (idx, sql) in MIGRATIONS.iter().enumerate() {
        let version = idx as i64 + 1;
        if version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .with_context(|| format!("applying migration {version}"))?;
        tx.execute(
            "INSERT INTO schema_version (version) VALUES (?)",
            params![version],
        )?;
        tx.commit()?;
        tracing::info!(version, "applied database migration");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        let v: i64 = conn
            .query_row("SELECT max(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, MIGRATIONS.len() as i64);
    }
}

use duckdb::Connection;

use super::Db;

/// Concatenated searchable text per file. Shared by the FTS index and the ILIKE fallback.
pub const BODY_SQL: &str = r#"concat_ws(' ',
    f.file_name,
    replace(f.rel_path, '/', ' '),
    f.summary,
    d.content,
    (SELECT string_agg(vf.description, ' ') FROM video_frames vf WHERE vf.file_id = f.id),
    (SELECT string_agg(t.tag, ' ') FROM tags t WHERE t.file_id = f.id))"#;

/// Rebuilds `search_docs` and its BM25 index. DuckDB FTS indexes are static, so this
/// must run after data changes (end of index/import jobs, startup).
pub fn rebuild_sync(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(&format!(
        r#"CREATE OR REPLACE TABLE search_docs AS
               SELECT f.id AS file_id, {BODY_SQL} AS body
               FROM files f LEFT JOIN documents d ON d.file_id = f.id;"#
    ))?;
    // Must be a separate call: DuckDB binds the PRAGMA before earlier statements in
    // the same batch have run. The custom `ignore` keeps digits (default drops them).
    conn.execute_batch(
        r#"PRAGMA create_fts_index('search_docs', 'file_id', 'body',
               stemmer = 'porter', stopwords = 'english',
               ignore = '(\.|[^a-z0-9])+', strip_accents = 1, lower = 1,
               overwrite = 1);"#,
    )?;
    Ok(())
}

/// Rebuilds the FTS index if the extension is available. Errors are logged, not returned,
/// because a stale index must never fail the job that triggered the rebuild.
pub async fn rebuild(db: &Db) {
    if !db.fts_available() {
        return;
    }
    let started = std::time::Instant::now();
    match db.call(|c| rebuild_sync(c)).await {
        Ok(()) => tracing::info!(
            ms = started.elapsed().as_millis() as u64,
            "rebuilt FTS index"
        ),
        Err(e) => tracing::error!(error = format!("{e:#}"), "FTS rebuild failed"),
    }
}

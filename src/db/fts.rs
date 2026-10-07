use duckdb::Connection;

use super::Db;

/// Concatenated searchable text per file. Shared by the FTS index and the ILIKE fallback.
pub const BODY_SQL: &str = r#"concat_ws(' ',
    f.file_name,
    replace(f.rel_path, '/', ' '),
    f.summary,
    d.content,
    (SELECT string_agg(vf.description, ' ') FROM video_frames vf WHERE vf.file_id = f.id),
    (SELECT string_agg(t.tag, ' ') FROM tags t WHERE t.file_id = f.id),
    (SELECT string_agg(o.text, ' ' ORDER BY o.page) FROM ocr o WHERE o.file_id = f.id))"#;

/// Rebuilds the BM25 index. DuckDB FTS indexes are static, so this must run after data
/// changes (end of index/import jobs, startup).
///
/// The indexed text lives only in a TEMP table: the FTS extension needs it while building
/// the index, but `match_bm25` reads the index tables alone (`fts_main_search_docs`),
/// so persisting a second copy of every document's text would only grow the database.
pub fn rebuild_sync(conn: &Connection) -> anyhow::Result<()> {
    // Older databases persisted this table; its body column was never read by a search.
    conn.execute_batch("DROP TABLE IF EXISTS main.search_docs;")?;
    conn.execute_batch(&format!(
        r#"CREATE OR REPLACE TEMP TABLE search_docs AS
               SELECT f.id AS file_id, {BODY_SQL} AS body
               FROM files f LEFT JOIN documents d ON d.file_id = f.id;"#
    ))?;
    // Must be a separate call: DuckDB binds the PRAGMA before earlier statements in
    // the same batch have run. The custom `ignore` keeps digits (default drops them).
    let indexed = conn.execute_batch(
        r#"PRAGMA create_fts_index('search_docs', 'file_id', 'body',
               stemmer = 'porter', stopwords = 'english',
               ignore = '(\.|[^a-z0-9])+', strip_accents = 1, lower = 1,
               overwrite = 1);"#,
    );
    conn.execute_batch("DROP TABLE IF EXISTS temp.search_docs;")?;
    Ok(indexed?)
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

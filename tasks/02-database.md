# Phase 2: Database layer

**Status:** done

## Goal
DuckDB schema, migrations on startup, and safe concurrent access from async code.

## Design
- Use the `duckdb` crate with the `bundled` feature. The DB file is at `database.path`
  (parent dir created on startup).
- DuckDB allows only one writer process, and connections are blocking:
  - `Db` wraps a `duckdb::Connection` behind a `std::sync::Mutex`. Each async call runs
    in `tokio::task::spawn_blocking` and uses `try_clone()`d connections where useful.
  - Writes from pipelines are small transactions per file (upsert files + detail table).
- Schema version table: `schema_version(version INTEGER)`. Migrations are an ordered
  list of SQL strings, applied in a transaction.
- Install and load the FTS extension at startup (`INSTALL fts; LOAD fts;`). This needs
  internet once, and the extension is then cached in `~/.duckdb`. Document this in
  INSTALLATION.md.

## Schema (v1)
```sql
files(id UUID PK, root TEXT, rel_path TEXT, abs_path TEXT, file_name TEXT, extension TEXT,
      kind TEXT, mime TEXT, size_bytes BIGINT, mode INTEGER, mode_str TEXT, uid INTEGER,
      gid INTEGER, mtime TIMESTAMP, sha256 TEXT, summary TEXT DEFAULT '',
      summary_status TEXT DEFAULT 'pending', indexed_at TIMESTAMP, UNIQUE(root, rel_path))
images(file_id UUID PK, format TEXT, width INT, height INT)
documents(file_id UUID PK, doc_type TEXT, page_count INT, content TEXT)
videos(file_id UUID PK, duration_secs DOUBLE, width INT, height INT, video_codec TEXT,
       audio_codec TEXT, fps DOUBLE, container TEXT, bitrate BIGINT)
video_frames(file_id UUID, ts_secs DOUBLE, description TEXT, PK(file_id, ts_secs))
analyses(id UUID PK, file_id UUID, model TEXT, prompt_version TEXT, raw_response TEXT,
         parsed JSON, latency_ms BIGINT, prompt_tokens INT, completion_tokens INT,
         created_at TIMESTAMP, error TEXT)
jobs(id UUID PK, kind TEXT, params JSON, status TEXT, found BIGINT, processed BIGINT,
     failed BIGINT, skipped BIGINT, started_at TIMESTAMP, finished_at TIMESTAMP, error TEXT)
tags(file_id UUID, tag TEXT, source TEXT, PK(file_id, tag))
```
- `summary_status` values are `pending | running | done | failed | skipped`.
- `kind` values are `image | document | video`.

## Checklist
- [x] Add the `duckdb` (bundled), `uuid`, `chrono` crates
- [x] `src/db/mod.rs`: `Db` handle (open, migrate, `call(|conn| ...)` helper via spawn_blocking)
- [x] `src/db/schema.rs`: migrations
- [x] `src/db/models.rs`: `FileRecord`, `ImageMeta`, `DocumentMeta`, `VideoMeta`, `save_indexed`, `find_stat`
- [x] Wire `Db` into `AppState`, open it on startup
- [x] Tests: migrate on a temp DB is idempotent; an upsert by `(root, rel_path)` updates
      in place and keeps the `id`
- [x] Update README (layout) and tasks/README.md

## Implementation notes
- IDs are stored as `TEXT` (UUID v4 strings) to avoid UUID type mapping issues.
- `save_indexed` upserts the file, deletes all detail rows (images/documents/videos/
  video_frames) for that id and inserts the new ones, all in one transaction. Re-indexing
  resets `summary` to `''`.
- The FTS extension is not part of the bundled build. `Db::init` runs `INSTALL fts; LOAD fts;`
  (downloads once to `~/.duckdb/extensions`). If that fails, `Db::fts_available()` is
  `false` and search must fall back to `ILIKE`.
- One `Mutex<Connection>` serializes all access. If search latency during big index jobs
  becomes a problem, add a second read connection via `try_clone()`.

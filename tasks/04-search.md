# Phase 4: `/search`

**Status:** done

## Goal
Search over extracted text, summaries, paths and all metadata through a JSON body,
without exposing SQL.

## Request format
```json
{ "q": { "text": "free text" } }
{ "q": { "height": "300" } }
{ "q": { "kind": "video", "height": { "gte": 1080, "lte": 2160 }, "video_codec": "h264" },
  "limit": 20, "offset": 0 }
```
- `text`: full-text search (BM25) over `files.summary`, `files.file_name`, `files.rel_path`,
  `documents.content` and `video_frames.description`.
- Other keys come from a fixed whitelist (unknown key → 400):

| key | column | match |
|---|---|---|
| `kind`, `extension`, `mime`, `mode_str`, `summary_status`, `root` | files.* | exact (case-insensitive) |
| `path` | files.abs_path | contains (`ILIKE '%' \|\| ? \|\| '%'`) |
| `name` | files.file_name | contains |
| `doc_type` | documents.doc_type | exact |
| `format` | images.format | exact |
| `video_codec`, `audio_codec`, `container` | videos.* | exact |
| `width`, `height` | images or videos (COALESCE) | number or `{gte,lte,gt,lt}` |
| `duration_secs`, `fps` | videos.* | number or range |
| `size_bytes` | files.size_bytes | number or range |
| `page_count` | documents.page_count | number or range |
| `mtime` | files.mtime | ISO timestamp range |
| `tag` | tags.tag | exact (phase 7) |

- Values may be given as a string or a number: `"300"` and `300` mean the same.
- `limit` defaults to 20 (max 200), `offset` defaults to 0.

## Design
- `src/search/query.rs` parses the request into a typed `SearchQuery` (enum per field
  type), and a builder produces SQL text plus a `Vec<duckdb::types::Value>` of
  parameters. Column names only come from the whitelist, never from input.
- FTS: DuckDB's FTS index does not update itself, so `fts::rebuild()` builds a
  `search_docs(file_id, body)` table (concatenated text per file) plus
  `PRAGMA create_fts_index('search_docs', 'file_id', 'body', overwrite=1)`.
  This runs after index and import jobs and at startup (not debounced; the DB mutex serializes rebuilds). Score with `fts_main_search_docs.match_bm25`.
- Response: `{ total, results: [{ file, image?, document?, video?, score, matched_frames? }] }`.
  Document `content` is not returned in full, only a snippet (first 300 chars).

## Checklist
- [x] `src/search/query.rs` (parsing + SQL builder) with unit tests for every field type
- [x] `src/db/fts.rs`: rebuild function, called at the end of index jobs
- [x] `src/api/search.rs` + route
- [x] Integration test: index fixtures, search by text, by height, with a range, unknown field → 400
- [x] Update README and tasks/README.md

## Implementation notes
- The `ignore` regex is `(\.|[^a-z0-9])+`, so digits are kept (e.g. invoice numbers).
  DuckDB's default drops them.
- `PRAGMA create_fts_index` must run in its own `execute_batch`. In the same batch as
  the `CREATE TABLE`, it is bound before the table exists.
- English stemmer and stopword list. Short words like `sub` are stopwords and cannot be
  searched via `text` (use `path` instead). A configurable language (e.g. German) is in the backlog.
- Substring fallback (`text_mode: "substring"`): each term is an `ILIKE` on the
  concatenated body. A file matches if any term matches, and the score is the number of
  matching terms.
- Additional fields beyond the plan: `id`, `sha256`, `summary`, `uid`, `gid`, `indexed_at`, `mode`.
- `Db::set_fts_available(false)` lets tests exercise the fallback against real DuckDB.

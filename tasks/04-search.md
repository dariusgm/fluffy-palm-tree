# Phase 4: `/search`

**Status:** todo

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
  This runs after index and import jobs (debounced). Score with `fts_main_search_docs.match_bm25`.
- Response: `{ total, results: [{ file, image?, document?, video?, score, matched_frames? }] }`.
  Document `content` is not returned in full, only a snippet (first 300 chars).

## Checklist
- [ ] `src/search/query.rs` (parsing + SQL builder) with unit tests for every field type
- [ ] `src/db/fts.rs`: rebuild function, called at the end of index jobs
- [ ] `src/api/search.rs` + route
- [ ] Integration test: index fixtures, search by text, by height, with a range, unknown field → 400
- [ ] Update README and tasks/README.md

# media-search

A self-hosted search service for documents, images, PDFs and videos on local disks
and SMB network shares. It is written in Rust with [axum](https://github.com/tokio-rs/axum).
Metadata goes into a local [DuckDB](https://duckdb.org) database, and a local
[llama.cpp](https://github.com/ggml-org/llama.cpp) vision model describes the content
of images and videos, so everything stays searchable by text.

> Status: all planned features are implemented and verified against a live llama.cpp
> vision model. Future work is listed under [Roadmap](#roadmap).

## How it works

```
client ──► axum (private-network allowlist)
             ├─ POST /index         → job: walk tree → copy to staging → extract metadata → DuckDB
             ├─ POST /import_media  → job: copy to staging → frames/resize → llama.cpp → DuckDB
             ├─ POST /search        → DuckDB full-text + metadata filters
             └─ GET  /jobs/{id}     → progress (found / processed / failed / skipped)
```

- **Source data is never modified.** Files are copied into a local staging directory
  for analysis, and the copy is deleted afterwards. If a later step needs the file
  again, it copies it from the source again.
- **Long-running work runs as background jobs.** Clients poll `/jobs/{id}`. Both
  `found` and `processed` keep growing while a job runs.
- **Indexing is restricted** to the root directories configured in `config.toml`.
- **Only clients from the configured networks are accepted** (default `192.168.0.0/16`
  plus loopback).
- **Search accepts JSON only.** There is no SQL interface, and every filter maps to a
  whitelisted column.

### Stored metadata

| Kind | Fields |
|---|---|
| All files | root, relative/absolute path, name, extension, size, unix mode, uid/gid, mtime, sha256, summary, summary status |
| Image | format, width, height |
| Document | type (`text`, `markdown`, `pdf`; office later), page count, extracted text |
| Video | duration, width, height, video/audio codec, fps, container, bitrate, per-frame descriptions |

## API

| Endpoint | Status |
|---|---|
| `GET /health` | implemented |
| `POST /index` | implemented |
| `GET /jobs`, `GET /jobs/{id}`, `DELETE /jobs/{id}` | implemented |
| `POST /search` | implemented |
| `POST /import_media` | implemented |
| `GET /health/llm` | implemented |

### `POST /index`

```json
{ "path": "/mnt/share/photos/2024", "traverse": true }
```
`traverse` defaults to `true`. If it is `false`, only the files directly inside `path`
are indexed. The response is `202` with `{ "job_id": "...", "status_url": "/jobs/..." }`.

Re-indexing keeps the database at the current state of the source:

| Situation | Result |
|---|---|
| Same size and modification time | skipped without reading the file |
| Stat changed, same SHA-256 (e.g. touched) | stat columns updated; metadata and LLM results kept |
| Same path, different SHA-256 | old record and everything derived from it (metadata, frames, LLM history, tags) deleted, new record created |
| New file with the same SHA-256 as an analysed file (copy or move) | LLM summary and frame descriptions reused, no new LLM run |
| Record whose file no longer exists | deleted (`removed` counter), only for the indexed folder (direct children if `traverse` is `false`) and only if the walk completed without errors and was not cancelled |

The hash is computed for every indexed file, while copying it to staging or by
reading it in place when `staging.copy_on_index = false`. Files above
`staging.max_file_bytes` are recorded without a hash.

### `POST /import_media`

```json
{}
{ "kind": ["image", "video"], "path_prefix": "/mnt/share/photos", "ids": [], "limit": 50, "force": false }
```
All fields are optional (send `{}` for the defaults). The job processes images and
videos whose summary is still `pending`. With `force: true` it also re-processes
`done` and `failed` items.

For each item, the job:
1. copies the file from the source into staging again (the path must still be inside
   a configured root);
2. for images, applies EXIF rotation, downscales to `llm.image_max_edge` and sends a JPEG
   to llama.cpp (`/v1/chat/completions`);
3. for videos, samples the first frame, one frame every `llm.video_frame_interval_secs`
   (default 60 s, widened for long videos so that `video_max_frames` covers the whole
   duration) and the last frame (skipped if it is within 2 s of the previous sample), describes each frame
   (stored with its timestamp), then merges the descriptions into one video summary;
4. stores the summary, including people, objects, scene, visible text and tags, as
   searchable text, and records every LLM call (model, prompt version, raw output,
   latency, tokens, errors) in the `analyses` table for quality comparison.

Documents are not supported yet (`400`).

### `GET /health/llm`

Checks llama.cpp via `/v1/models` and `/props`. Returns
`200 {"reachable": true, "configured_model_loaded": ..., "models": [...], "vision": true|false|null}`
or `503` with the error. `vision: false` means llama-server was started without `--mmproj`,
so image and video analysis will fail.

### `POST /search`

```json
{ "q": { "text": "person with red jacket at the beach" } }
{ "q": { "height": "300" } }
{ "q": { "kind": "video", "height": { "gte": 1080 }, "video_codec": "h264" }, "limit": 20, "offset": 0 }
```

- **`text`** runs a BM25 full-text search over the file name, directory names, LLM summary,
  document text, video frame descriptions and tags. It uses English stemming, so `fox`
  also matches `foxes`, and stopwords are ignored.
- All other keys filter on metadata. Values can be strings or numbers, and numeric
  and time fields also accept a range like `{"gte": .., "gt": .., "lte": .., "lt": .., "eq": ..}`.

| Field | Match |
|---|---|
| `kind`, `extension`, `mime`, `root`, `doc_type`, `format`, `video_codec`, `audio_codec`, `summary_status`, `mode_str`, `sha256`, `id`, `tag` | exact, case-insensitive; a list means any of |
| `path`, `name`, `summary`, `container` | substring, case-insensitive |
| `width`, `height` (image or video), `duration_secs`, `fps`, `size_bytes`, `page_count`, `uid`, `gid` | number or range |
| `mtime`, `indexed_at` | `"2024-05-01"` (whole day), RFC 3339 timestamp, or range |
| `mode` | octal permissions, e.g. `"644"` |

- An unknown field or invalid value returns `400`. `limit` defaults to 20 (max 200).
- Each result contains the file metadata, `score`, and an `image`/`document`/`video`
  object depending on its kind. Documents return a 300-character `snippet`. For text
  queries, videos list the `matched_frames` (timestamp and description).
- `text_mode` is `fts`, or `substring` if the DuckDB FTS extension is unavailable.
- The full-text index is rebuilt when an index or import job finishes. Files that a
  running job has just added can already be found by metadata filters, but not by `text` yet.

### `GET /jobs/{id}`

```json
{ "id": "...", "kind": "index", "status": "running", "params": { "path": "...", "traverse": true },
  "found": 1200, "processed": 850, "failed": 2, "skipped": 300, "removed": 4,
  "started_at": "...", "finished_at": null, "error": null,
  "recent_errors": [ { "path": "/mnt/share/x/broken.png", "error": "..." } ] }
```
- `status` is one of `running | completed | failed | cancelled | interrupted`.
- `skipped` counts unchanged files and files above `staging.max_file_bytes`; `removed`
  counts records deleted because their file no longer exists.
- `GET /jobs` lists the last 100 jobs. `DELETE /jobs/{id}` cancels a running job
  (`202`), or returns `409` if the job has already finished.

## Getting started

See [INSTALLATION.md](INSTALLATION.md) for the system packages, SMB mounts and the
llama.cpp setup. In short:

```bash
cp config.example.toml config.toml   # adapt roots and LLM URL
cargo run --release
curl http://127.0.0.1:8080/health
```

## Development

Every step must pass these checks before it is committed:

```bash
cargo fmt --check
cargo build
cargo clippy --all-targets -- -D warnings
cargo test
```

Tests generate their own fixtures, such as synthetic images and test videos. Tests
that need `ffmpeg` or `pdftotext` skip themselves if those tools are missing.

### Project layout

```
src/
  main.rs        startup, signal handling
  lib.rs         module wiring
  config.rs      config.toml loading and validation
  security.rs    IP allowlist middleware, index-root validation
  error.rs       API error type
  state.rs       shared application state
  db/            DuckDB handle, migrations, models/upserts
  jobs.rs        background job registry (counters, cancel, persistence)
  staging.rs     copy-to-staging with hashing, byte budget, auto-cleanup
  detect.rs      file kind / doc type / MIME detection
  extract/       metadata extractors (image header, text/pdftotext, ffprobe),
                 frame sampling (ffmpeg), image preparation for the LLM
  pipelines/     job implementations (index, import_media)
  llm/           llama.cpp client, versioned prompts, JSON parsing
  search/        JSON query → parameterized SQL, result mapping
  api/           HTTP handlers
tests/           integration tests
```

### Design decisions

- SMB shares are mounted on the host (CIFS or GVFS) and configured as `[[roots]]`. The
  service only sees local paths, and files are identified by `(root name, relative path)`.
- Video and PDF tooling are system binaries (`ffprobe`, `ffmpeg`, `pdftotext`),
  always called with argument vectors, never through a shell.
- All state (files, metadata, LLM results, jobs) lives in the DuckDB file at
  `database.path` (default `./data/search.duckdb`) and survives restarts. Back up that
  file to keep the LLM results. Keep it on a persistent disk, not in `/tmp`.
- DuckDB is accessed through one mutex-protected connection on the blocking thread
  pool (DuckDB has a single writer). Schema changes are append-only migrations in
  `src/db/schema.rs`.
- DuckDB FTS indexes are static. The `search_docs` table and its BM25 index are rebuilt
  at startup and after every index and import job. The tokenizer keeps digits, so
  invoice numbers are searchable.
- Every LLM call is stored in `analyses` with its prompt version (`src/llm/prompts.rs`).
  Bump the version when changing a prompt, then compare runs with SQL, e.g.
  `SELECT prompt_version, count(*), avg(latency_ms), count(error) FROM analyses GROUP BY 1`.
- The LLM is asked for JSON (summary, people, people_count, objects, scene, visible
  text, tags), and the answer is flattened into searchable text with one labelled line
  per person. The model is told never to guess identities. People can be found through
  their description and through visible name tags.

### LLM evaluation (llama.cpp, Qwen vision model, single slot)

- About 50 s per image and about 55 s per video frame, dominated by output tokens
  (roughly 1,700 images per day).
- Videos: 1 frame per 60 s plus the first and last frame, so a 10-minute video takes
  11 frames, roughly 10 minutes.
- Prompt v1 hit the output limit on crowds and code screenshots. Prompt v2 (at most 6
  listed people, crowds as one entry, bounded text) produced valid JSON for every call
  in the test set.
- `image_max_edge = 1024` keeps name tags and UI text readable. Lowering it saves
  little, because image tokens are not the bottleneck.
- llama-server must run with a vision projector (`--mmproj ...` or `-hf ... --mmproj-auto`).
  `GET /health/llm` reports `vision: false` otherwise.

## Roadmap

- Document summaries through the LLM (`/import_media` with `kind: ["document"]`,
  chunking long documents)
- Office documents (docx/xlsx/pptx)
- OCR for scanned PDFs (no text layer), e.g. render pages and send them to the vision model
- `people_count` as a search filter, since a text query like "person" misses many images
- Tagging API (`POST /files/{id}/tags`, `DELETE /files/{id}/tags/{tag}`); `tag` is
  already a search field
- Person search beyond descriptions: face detection and embeddings (e.g. ONNX model plus
  DuckDB `vss`), clustered into named persons through tags
- HEIC/RAW images, EXIF metadata (capture date, camera; GPS optional)
- Re-derive `files.summary` from stored `analyses.parsed` when the flattening changes
- Configurable FTS language (e.g. German), periodic FTS rebuilds during long jobs
- Optional API token in addition to the IP allowlist; systemd unit file

## Public repository hygiene

This repository is public. **Never commit:**
- `config.toml`, `.env`, or anything else with real hosts, IPs, share names or credentials
- `data/`, staging directories, or `*.duckdb` files (they contain your file paths and content descriptions)
- real media or documents (`.gitignore` blocks common media extensions)

`config.example.toml` uses placeholders only. Check `git status` and `git diff --cached`
before every commit.

## License

See [LICENSE](LICENSE).

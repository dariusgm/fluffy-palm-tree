# Phase 3: `/index` and the job system

**Status:** todo

## Goal
`POST /index {"path": "...", "traverse": true}` starts a background job that walks the
tree, copies files to staging, extracts metadata and stores it in DuckDB. Progress can
be polled.

## Design
### Jobs (`src/jobs.rs`)
- `JobRegistry` keeps live counters as `AtomicU64` per job (`found`, `processed`,
  `failed`, `skipped`), a status and a `CancellationToken` (`tokio-util`).
- Counters are flushed to the `jobs` table periodically (every ~2 s) and at the end, so
  `GET /jobs/{id}` works for live and historical jobs.
- On startup, jobs left in `running` are set to `interrupted`.
- Endpoints: `GET /jobs`, `GET /jobs/{id}`, `DELETE /jobs/{id}` (cancel).

### Index pipeline (`src/pipelines/index.rs`)
1. Validate the path with `security::resolve_in_roots`. A path to a single file is allowed too.
2. The walker task (`walkdir`, in `spawn_blocking`) uses `max_depth(1)` when `traverse=false`,
   does not follow symlinks, and skips hidden files and dirs (`.`-prefixed) and
   unsupported extensions. Each supported file increments `found` and is sent over a
   bounded mpsc channel.
3. `staging.index_workers` workers process each file:
   - `symlink_metadata` gives mode, uid, gid, size and mtime.
   - Skip check: if the DB has the same `(root, rel_path)` with equal size and mtime,
     increment `skipped`.
   - If `size > max_file_bytes`, store the base record only, with status `skipped`.
   - If `copy_on_index`, copy to `staging.dir/<uuid>.<ext>` while hashing (sha256).
     A `StagedFile` guard deletes the copy on `Drop`. A staging budget
     (semaphore on bytes, `max_bytes`) applies back-pressure.
   - Detect the kind with the `infer` crate (magic bytes), falling back to the extension.
   - Extract metadata (below) and upsert. Increment `processed`, or `failed` with a log entry.
4. When the job finishes, rebuild the FTS index (phase 4 provides `fts::rebuild`).

### Extractors (`src/extract/`)
- `image.rs`: `imagesize` for width and height, format from `infer`.
  Types: jpg, png, webp, gif, bmp, tiff.
- `document.rs`: `.txt` → `text`, `.md`/`.markdown` → `markdown` (read as UTF-8, lossy,
  capped at e.g. 5 MB of text). `.pdf` → `pdf` via `pdftotext -layout file -` and
  `pdfinfo` for the page count. Use `tokio::process::Command` with a timeout.
- `video.rs`: `ffprobe -v error -print_format json -show_format -show_streams`. Parse
  duration, first video stream (width, height, codec_name, avg_frame_rate),
  first audio codec, format_name, bit_rate.
  Types: mp4, mkv, mov, webm, avi, m4v.
- External tools are invoked with argument vectors (never a shell), and paths are
  passed after `--` where supported.

## Checklist
- [ ] Crates: `walkdir`, `infer`, `imagesize`, `sha2`, `hex`, `tokio-util`
- [ ] `src/staging.rs`: copy + hash, `StagedFile` drop guard, byte budget
- [ ] `src/detect.rs`: file kind and doc type detection
- [ ] `src/extract/{image,document,video}.rs`
- [ ] `src/jobs.rs` + `GET /jobs`, `GET /jobs/{id}`, `DELETE /jobs/{id}`
- [ ] `src/pipelines/index.rs` + `POST /index` (`traverse` defaults to `true`)
- [ ] Startup: mark stale running jobs as `interrupted`, clean leftover staging files
- [ ] Tests: generated PNG, text and markdown files; ffmpeg `testsrc` video if ffmpeg is
      available (skip otherwise); `traverse=false`; path outside roots → 403; re-index skips unchanged files
- [ ] Update README (API status) and tasks/README.md

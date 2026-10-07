# API reference

HTTP/JSON API of the media search service. Replace `localhost:8081` by the `server.bind`
address from `config.toml`. How the pipelines work internally is described in
[README.md](README.md).

| Endpoint | Purpose |
|---|---|
| `GET /` | minimal web UI (jobs and search), all assets served by the binary |
| [`GET /health`](#get-health) | liveness and version |
| [`GET /health/llm`](#get-healthllm) | is the vision LLM reachable |
| [`POST /index`](#post-index) | scan a folder and store file metadata (job) |
| [`POST /import_media`](#post-import_media) | analyse images, videos and documents with the LLM (job) |
| [`POST /search`](#post-search) | full-text and metadata search |
| [`GET /files/{id}`](#get-filesid) | metadata of one indexed file |
| [`POST /files/delete`](#post-filesdelete) | remove files from the index, optionally from the share |
| [`GET /jobs`](#get-jobs) | list recent jobs |
| [`GET /jobs/{id}`](#get-jobsid) | job status and progress |
| [`DELETE /jobs/{id}`](#delete-jobsid) | cancel a running job |

## Conventions

- Request and response bodies are JSON (`Content-Type: application/json`).
- There is no authentication. Access is limited by network: clients outside
  `server.allowed_cidrs` (default `192.168.0.0/16`, `127.0.0.1/32`) get `403` with the
  plain-text body `forbidden`. Request bodies are limited to `server.max_body_bytes`
  (64 KiB by default).
- `/index` and `/import_media` start a background job and return `202` immediately.
  Poll `GET /jobs/{id}` for progress. Jobs run independently of the HTTP connection.
- Errors from the service are `{"error": "message"}` with one of these statuses:

  | Status | Meaning |
  |---|---|
  | `400` | invalid request: unparsable JSON, relative or missing path, unknown search field, invalid value, `archive` as import kind |
  | `403` | path is not inside a configured index root, or client network not allowed |
  | `404` | job not found |
  | `409` | job is not running (cancel) |
  | `422` | JSON is valid but has an unknown field or a wrong type (plain-text body from the JSON parser) |
  | `500` | internal error; details are only in the server log |
  | `503` | `/health/llm` only: the LLM is not reachable |

- Timestamps are RFC 3339 in UTC.

## `GET /health`

```bash
curl localhost:8081/health
```
```json
{ "status": "ok", "version": "0.1.0" }
```

## `GET /health/llm`

Asks the configured llama.cpp server for its models. `200` if reachable, otherwise `503`.

```json
{
  "reachable": true,
  "configured_model": "unsloth/Qwen3.8-Flash-Next-GGUF:UD-IQ4_XS",
  "configured_model_loaded": true,
  "models": ["unsloth/Qwen3.8-Flash-Next-GGUF:UD-IQ4_XS"],
  "model": {
    "name": "unsloth/Qwen3.8-Flash-Next-GGUF:UD-IQ4_XS",
    "n_ctx": 180224,
    "n_ctx_train": 262144,
    "n_params": 176943899520,
    "size_bytes": 93671559680,
    "quantization": "IQ4_XS - 4.25 bpw",
    "build_info": "b11269-cee37ffea"
  },
  "model_error": null,
  "vision": true
}
```

`configured_model` is `llm.model` or, when that is empty, the model the server reports;
`model` holds what the server says about it (fields are null if it does not expose them).
`vision` tells whether the server has a vision projector loaded; without it
`/import_media` cannot describe images. When unreachable the body is
`{ "reachable": false, "configured_model": "...", "error": "..." }`.

## `POST /index`

Scans a directory, stores the metadata of every supported file in the database and
removes records of files that no longer exist. Sources are only read, never modified.

```bash
curl -X POST localhost:8081/index -H 'content-type: application/json' \
  -d '{"path": "/mnt/share/photos/2024", "traverse": true}'
```

| Field | Type | Default | Description |
|---|---|---|---|
| `path` | string, required | | absolute path inside one of the `[[roots]]` of the config |
| `traverse` | bool | `true` | `false` indexes only the files directly inside `path` |

Response `202`:

```json
{ "job_id": "849dd9a4-a207-49f2-830f-45f625ebc4c8", "status_url": "/jobs/849dd9a4-a207-49f2-830f-45f625ebc4c8" }
```

Errors: `400` (path not absolute or not accessible), `403` (outside the roots).

What is stored per file, how re-indexing treats changed, moved and deleted files, and the
job counters are described under `POST /index` in the README.

## `POST /import_media`

Runs the LLM steps for files that still need them: a description of images, videos and
PDFs, a summary of text and code documents, and text recognition (OCR) for images and
scanned PDFs. Results are searchable once the job finishes.

```bash
curl -X POST localhost:8081/import_media -H 'content-type: application/json' -d '{}'
curl -X POST localhost:8081/import_media -H 'content-type: application/json' \
  -d '{"kind": ["image"], "path_prefix": "/mnt/share/photos", "limit": 50}'
```

| Field | Type | Default | Description |
|---|---|---|---|
| `kind` | array of `image`, `video`, `document` | all three | which file kinds to process; `archive` is rejected with `400` |
| `path_prefix` | string | none | only files whose absolute path starts with this |
| `ids` | array of strings | none | only these file ids (`id` in search results) |
| `limit` | integer | none | process at most this many files (in path order) |
| `force` | bool | `false` | also redo files that are `done` or `failed`; never reuses descriptions of similar images |

Unknown fields are rejected with `422`. Response `202`, same shape as `/index`.

Each file only gets the steps it is missing, so a repeated call continues where the last
one stopped (after a restart or cancel, too). Files are processed in path order.
In the job counters, `found` is the number of selected files, `processed` the ones
finished, `skipped` the images that reused the description of a similar image
(`llm.phash_reuse_distance`), and `failed` the ones with an error (see
`recent_errors`; they can be retried with `force`).

## `POST /search`

```bash
curl -X POST localhost:8081/search -H 'content-type: application/json' \
  -d '{"q": {"text": "person with red jacket at the beach"}, "limit": 10}'
curl -X POST localhost:8081/search -H 'content-type: application/json' \
  -d '{"q": {"kind": "video", "height": {"gte": 1080}, "video_codec": "h264"}}'
```

| Field | Type | Default | Description |
|---|---|---|---|
| `q` | object | `{}` | search conditions, all combined with AND |
| `limit` | integer | 20 | results per page, 1 to 200 |
| `offset` | integer | 0 | number of results to skip |

`q.text` is a BM25 full-text search over file names, directory names, LLM summaries,
document text, OCR text, video frame descriptions and tags (English stemming, stopwords
ignored); results are ordered by relevance. All other keys filter on metadata:

| Field | Match |
|---|---|
| `kind`, `extension`, `mime`, `root`, `doc_type`, `language`, `encoding`, `format`, `video_codec`, `audio_codec`, `compression`, `archive_format`, `summary_status`, `ocr_status`, `summary_model`, `ocr_model`, `mode_str`, `sha256`, `id`, `tag` | exact, case-insensitive; a list means any of |
| `path`, `name`, `summary`, `container` | substring, case-insensitive |
| `width`, `height` (image or video), `duration_secs`, `fps`, `size_bytes`, `page_count`, `uid`, `gid` | number or range |
| `modified` (alias `mtime`), `created`, `indexed_at` | `"2024-05-01"` (whole day), RFC 3339 timestamp, or range |
| `mode` | octal permissions, e.g. `"644"` |

A range is an object with any of `gt`, `gte`, `lt`, `lte`, `eq`, e.g. `{"gte": 1080}`.
Numbers may also be sent as strings (`"height": "300"`). An unknown field or an invalid
value returns `400`; the error message lists the allowed fields.

Response `200`:

```json
{
  "total": 1,
  "limit": 20,
  "offset": 0,
  "text_mode": "fts",
  "results": [
    {
      "id": "0d0c5a52-7a47-4d79-9a0e-5a0f4a3e1f0b",
      "kind": "image",
      "root": "nas",
      "rel_path": "2019/Beach/IMG_0412.jpg",
      "path": "/mnt/share/photos/2019/Beach/IMG_0412.jpg",
      "name": "IMG_0412.jpg",
      "extension": "jpg",
      "mime": "image/jpeg",
      "size_bytes": 4231876,
      "mode": "0644",
      "mode_str": "rw-r--r--",
      "uid": 1000,
      "gid": 1000,
      "mtime": "2019-07-14T15:02:11Z",
      "created": null,
      "sha256": "d6e296012ddef0fbc541a1def14ef1d93325f57c5cf7a41ddbf759eb4d538c3d",
      "summary": "A person in a red jacket stands on a beach ... Tags: beach, red jacket.",
      "summary_status": "done",
      "summary_model": "unsloth/Qwen3.8-Flash-Next-GGUF:UD-IQ4_XS",
      "ocr_status": "none",
      "ocr_model": "unsloth/Qwen3.8-Flash-Next-GGUF:UD-IQ4_XS",
      "indexed_at": "2026-10-01T22:40:03Z",
      "score": 7.31,
      "image": { "format": "jpeg", "width": 4032, "height": 3024 }
    }
  ]
}
```

- `total` is the number of matches, independent of `limit` and `offset`.
- `text_mode` is `fts`, `substring` if the DuckDB FTS extension is unavailable, or `null`
  without a `text` condition. `score` is only set (otherwise `null`) for text queries.
- Depending on the kind, a result has one of `image` (`format`, `width`, `height`),
  `document` (`doc_type`, `page_count`, `language`, `encoding`, `snippet`),
  `video` (`duration_secs`, `width`, `height`, `video_codec`, `audio_codec`, `fps`,
  `container`, `bitrate`) or `archive` (`compression`, `format`).
- `summary_status` is `pending`, `running`, `done`, `failed` or `skipped`. `ocr_status`
  is `done`, `none` (no text found), `failed`, `skipped`, or null (not run).
  `summary_model` and `ocr_model` name the LLM (`llm.model`) that produced them; null if
  the step has not run.
- Files with recognised text have `ocr_text` (first 1000 characters). For text queries,
  videos list `matched_frames` (`ts_secs`, `description`).
- The full-text index is rebuilt when an index or import job finishes. Files added by a
  running job can already be found by metadata filters, but not by `text` yet.

## `GET /files/{id}`

Stored metadata of one file (`id` as in search results): `root`, `rel_path`, `abs_path`,
`name`, `extension`, `kind`, `mime`, `size_bytes`, `sha256`, `summary`, `summary_status`,
`ocr_status`. `404` if the id is unknown.

## `POST /files/delete`

Removes files and everything derived from them (metadata, frames, LLM results, tags) from
the index, without stopping the server. By default only database records are removed and
the share is untouched, so a later `/index` adds the files back unless they match
`staging.exclude` in the config.

```bash
# what would be removed?  (dry run is the default)
curl -X POST localhost:8081/files/delete -H 'content-type: application/json' \
  -d '{"path_prefix": "/mnt/share/old-data"}'
# remove the records
curl -X POST localhost:8081/files/delete -H 'content-type: application/json' \
  -d '{"path_prefix": "/mnt/share/old-data", "dry_run": false}'
# remove the records and the files on the share
curl -X POST localhost:8081/files/delete -H 'content-type: application/json' \
  -d '{"ids": ["..."], "delete_source": true, "dry_run": false}'
```

| Field | Type | Default | Description |
|---|---|---|---|
| `ids` | array of strings | `[]` | file ids to select (at most 10000) |
| `path_prefix` | string | none | absolute path inside a root; selects that file or everything below that directory. It may no longer exist. A whole root is refused with `400` |
| `delete_source` | bool | `false` | also delete the files on the share |
| `dry_run` | bool | `true` | only report; send `false` to act |

At least one of `ids` and `path_prefix` is required; nothing is selected by default.
Both are combined (union). Unknown fields are rejected with `422`.

Response `200`: `matched`, `bytes`, `by_kind` and a `sample` of up to 20 relative paths.
When acting it also has `removed_records` and, with `delete_source`, `deleted_source_files`,
`already_missing`, `error_count` and up to 20 `errors`.

With `delete_source` only regular files known to the index are deleted, and only inside a
configured root (symlinks and directories are left alone and reported as errors).
Directories that become empty along the way are removed, never the root. A record is only
dropped once its file is gone or was already missing; files that fail stay in the index.
Deleting on the share cannot be undone. Use the dry run first.

Search results omit removed files immediately; the full-text index itself is rebuilt
when the next index or import job finishes.

## `GET /jobs`

The last 100 jobs, newest first, each in the shape of `GET /jobs/{id}`.

## `GET /jobs/{id}`

```json
{
  "id": "6ba31907-15a0-4580-8e06-f151219b21b3",
  "kind": "import_media",
  "status": "running",
  "params": { "kind": [], "path_prefix": null, "ids": [], "limit": null, "force": false },
  "found": 42569,
  "processed": 43,
  "failed": 0,
  "skipped": 2,
  "updated": 0,
  "removed": 0,
  "started_at": "2026-10-02T02:24:30Z",
  "finished_at": null,
  "error": null,
  "recent_errors": [ { "path": "/mnt/share/x/broken.png", "error": "..." } ]
}
```

- `kind` is `index` or `import_media`.
- `status` is `running`, `completed`, `failed` (the job itself aborted, see `error`),
  `cancelled`, or `interrupted` (the service stopped while the job was running).
- Counters: `found` supported files (index) or selected files (import); `processed`
  finished successfully; `failed` with an error; `skipped` unchanged files and files above
  `staging.max_file_bytes` (index) or images that reused a similar image's description
  (import); `updated` unchanged content whose stored metadata was refreshed; `removed`
  records deleted because the file no longer exists or is no longer a supported type.
- `recent_errors` holds the latest errors with the affected path. Single files that fail
  do not fail the job.

`404` if the id is unknown.

## `DELETE /jobs/{id}`

Cancels a running job. Response `202` with `{ "status": "cancelling" }`; the job stops
after the files it is working on and then has the status `cancelled`. Already stored results
are kept, and `/import_media` can be started again to continue. `409` if the job has
already finished, `404` if it does not exist.

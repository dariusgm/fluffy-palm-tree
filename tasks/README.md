# Tasks

The implementation is split into phases, one file per phase. Each file has the goal,
the design decisions taken and a checklist. When you resume work, read this file
first, then the first phase that is not `done`.

| Phase | File | Status |
|---|---|---|
| 1 | [01-scaffold.md](01-scaffold.md) | done |
| 2 | [02-database.md](02-database.md) | done |
| 3 | [03-index.md](03-index.md) | done |
| 4 | [04-search.md](04-search.md) | done |
| 5 | [05-import-media.md](05-import-media.md) | done (mocked LLM) |
| 6 | [06-live-llm.md](06-live-llm.md) | todo (needs the LLM server running) |
| 7 | [07-later.md](07-later.md) | backlog |

## Rules for every step

1. `cargo fmt --check`, `cargo build`, `cargo clippy --all-targets -- -D warnings`
   and `cargo test` must pass.
2. Update the phase file's checklist, the table above, and README.md (API status, layout).
3. Create one commit per completed step. The owner reviews commits before pushing,
   because the repository is public.
4. Never commit real hosts, IPs, share paths, credentials, databases or media.
   See "Public repository hygiene" in README.md.

## Decisions log

- Rust (edition 2024), axum 0.8, tokio, and DuckDB through the `duckdb` crate with the `bundled` feature.
- Ubuntu 24.04 is the only target OS.
- SMB shares are mounted via CIFS on the host. The service only sees local paths,
  configured as `[[roots]]`. Files are identified by `(root name, relative path)`.
- Source files are never modified. They are copied into `staging.dir` for analysis
  and the copy is deleted afterwards (`staging.copy_on_index = true` by default; set
  it to `false` to read metadata in place).
- Video and PDF tooling are system binaries (`ffprobe`, `ffmpeg`, `pdftotext`), not linked libraries.
- Long-running work runs as background jobs with polling (`found`, `processed`,
  `failed`, `skipped` can all grow while running).
- Search takes JSON only (`{"q": {"text": ..., <field>: ...}}`). Fields are
  whitelisted and values bound as parameters, so no SQL is exposed.
- Network access is restricted to `server.allowed_cidrs` (default `192.168.0.0/16`
  plus loopback). Only the TCP peer address counts; `X-Forwarded-For` is ignored.
- Video sampling starts at 1 frame per 10 s (configurable) with a max-frames cap.
- Person search: first collect raw descriptions (structured `people` field in the
  LLM output, per-frame descriptions). Tags and/or face embeddings come later (phase 7).
- Every LLM run is stored in `analyses` with model, prompt version, latency and raw
  output, so extraction quality can be compared across prompts and models.

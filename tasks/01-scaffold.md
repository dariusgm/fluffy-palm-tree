# Phase 1: Scaffold

**Status:** done

## Goal
A buildable axum service with config, security primitives, docs and checks in place.

## Checklist
- [x] `cargo init` (binary `media-search`, plus a library crate so integration tests can use it)
- [x] Hardened `.gitignore`: config, env, data, DuckDB files, media extensions, IDE files
- [x] `config.example.toml` with placeholders only; real config in `config.toml` (gitignored)
- [x] `src/config.rs`: TOML loading, env overrides (`MEDIA_SEARCH_CONFIG`,
      `MEDIA_SEARCH_LLM_URL`, `MEDIA_SEARCH_LLM_API_KEY`), validation
- [x] `src/security.rs`: IP allowlist middleware (handles IPv4-mapped IPv6),
      `resolve_in_roots()` that canonicalizes and rejects `..` and symlink escapes
- [x] `src/error.rs`: `ApiError`, with internal errors hidden from clients
- [x] `GET /health`
- [x] Graceful shutdown (SIGINT/SIGTERM)
- [x] Tests: config parsing, allowlist, root resolution, health endpoint with allowed and denied peers
- [x] README.md, INSTALLATION.md, tasks/

## Notes
- The server must be started with `into_make_service_with_connect_info::<SocketAddr>()`,
  otherwise the IP middleware has no peer address. Tests inject `ConnectInfo` manually
  (`tests/common/mod.rs`).

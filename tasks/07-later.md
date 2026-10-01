# Phase 7: Backlog

**Status:** backlog

- [ ] Document summaries through the LLM (`/import_media` with `kind: ["document"]`, text-only prompt, chunking for long documents)
- [ ] Office documents (docx/xlsx/pptx), e.g. text extraction via `libreoffice --headless --convert-to txt` or pure-Rust zip/XML parsing
- [ ] Tagging API: `POST /files/{id}/tags`, `DELETE /files/{id}/tags/{tag}`, search by `tag`
- [ ] Person search: evaluate face detection and embeddings (e.g. an ONNX face model),
      store vectors in DuckDB (`vss` extension), and cluster them into named persons through tags
- [ ] HEIC/RAW image support (convert via ffmpeg or libheif before sending to the LLM)
- [ ] EXIF metadata (capture date, camera; GPS optional because it is privacy sensitive)
- [ ] Detect files deleted at the source during re-index (mark `missing`)
- [ ] Optional API token in addition to the IP allowlist
- [ ] systemd unit file

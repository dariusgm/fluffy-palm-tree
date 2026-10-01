# Phase 6: Live LLM test and extraction quality

**Status:** in progress. **Blocked:** llama-server must be restarted with `--mmproj` (vision projector).

## Goal
Verify the real llama.cpp setup and evaluate extraction quality on a small sample.

## Checklist
- [x] `sudo apt install ffmpeg`, then `cargo test` (runs the skipped video tests: ffprobe + frame sampling)
- [ ] Owner starts `llama-server` with the model and `--mmproj` (vision projector)
- [x] `GET /health/llm` reports the model as reachable (now also reports `vision`)
- [ ] Confirm the model accepts images. If not, choose a vision model (e.g. a Qwen VL variant)
- [ ] Confirm the server accepts `response_format: json_object` together with images
- [x] Run `/index` on a small folder (~20 images, 2–3 short videos)
- [ ] Run `/import_media` and record the timing per image and per frame (`analyses.latency_ms`)
- [ ] Review the quality: are people described consistently enough for later person search?
- [ ] Tune the prompts and bump `IMAGE_PROMPT_VERSION` / `VIDEO_MERGE_PROMPT_VERSION` in `src/llm/prompts.rs`, then compare runs in the `analyses` table
- [ ] Decide whether the frame interval (10 s) and `image_max_edge` (1024) are right for the hardware
- [ ] Write the findings into this file (no real file names or personal data, the repo is public)

## Useful queries
```sql
SELECT prompt_version, count(*), avg(latency_ms), sum(error IS NOT NULL) FROM analyses GROUP BY 1;
```

## Test set
Kept outside the repository (scratch directory). It is rebuilt from:
- 9 photos of people from Wikimedia Commons (public domain NASA portraits and group
  photos, plus CC-licensed street, cycling and football scenes), downloaded as
  1920px thumbnails
- 3 short videos of people from Wikimedia Commons: 32 s Theora/OGV 720x480,
  14 s VP9 1080p at 59 fps, and 37 s VP8 320x240
- Ubuntu wallpapers (`/usr/share/backgrounds`), screenshots from package docs,
  2 PDFs from package docs, and a hand-written markdown and text file
- one hidden directory (must be skipped)

## Findings
### Indexing (2026-10-01)
- 24 files indexed in 0.35 s (release build, local disk). The hidden directory was skipped. No failures.
- ffprobe handles VP8, VP9 and Theora. `.ogv` was not in the supported extension list;
  it has been added.
- Metadata and file-name search work before any LLM run (e.g. `bicycle` finds the
  invoice text and `IMG_milan_bicycle.jpg`).

### LLM
- The model is multimodal (`image-text-to-text`) and its GGUF repo ships `mmproj-F16.gguf`.
  llama-server was started without it (`/props` → `modalities.vision = false`). Image
  requests then fail with HTTP 500 `image input is not supported - hint: ... mmproj`.
  The job reports this in `recent_errors` and marks the item `failed`.
- Context: 32768 tokens, 1 slot. Keep `llm.workers = 1`.


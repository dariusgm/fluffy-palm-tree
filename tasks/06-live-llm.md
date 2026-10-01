# Phase 6: Live LLM test and extraction quality

**Status:** todo. **Blocked:** the owner has to start the model on the LLM host first.

## Goal
Verify the real llama.cpp setup and evaluate extraction quality on a small sample.

## Checklist
- [ ] `sudo apt install ffmpeg`, then `cargo test` (runs the skipped video tests: ffprobe + frame sampling)
- [ ] Owner starts `llama-server` with the model and `--mmproj` (vision projector)
- [ ] `GET /health/llm` reports the model as reachable
- [ ] Confirm the model accepts images. If not, choose a vision model (e.g. a Qwen VL variant)
- [ ] Confirm the server accepts `response_format: json_object` together with images
- [ ] Run `/index` on a small folder (~20 images, 2–3 short videos)
- [ ] Run `/import_media` and record the timing per image and per frame (`analyses.latency_ms`)
- [ ] Review the quality: are people described consistently enough for later person search?
- [ ] Tune the prompts and bump `IMAGE_PROMPT_VERSION` / `VIDEO_MERGE_PROMPT_VERSION` in `src/llm/prompts.rs`, then compare runs in the `analyses` table
- [ ] Decide whether the frame interval (10 s) and `image_max_edge` (1024) are right for the hardware
- [ ] Write the findings into this file (no real file names or personal data, the repo is public)

## Useful queries
```sql
SELECT prompt_version, count(*), avg(latency_ms), sum(error IS NOT NULL) FROM analyses GROUP BY 1;
```

## Findings
_(fill in)_

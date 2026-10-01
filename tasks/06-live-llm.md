# Phase 6: Live LLM test and extraction quality

**Status:** done (first evaluation round, prompts `img-v2` / `video-merge-v2`). Open decisions are listed at the end.

## Goal
Verify the real llama.cpp setup and evaluate extraction quality on a small sample.

## Checklist
- [x] `sudo apt install ffmpeg`, then `cargo test` (runs the skipped video tests: ffprobe + frame sampling)
- [x] Owner starts `llama-server` with the model and `--mmproj` (vision projector); `--mmproj-auto` works with `-hf`
- [x] `GET /health/llm` reports the model as reachable (now also reports `vision`)
- [x] Confirm the model accepts images
- [x] Confirm the server accepts `response_format: json_object` together with images
- [x] Run `/index` on a small folder (~20 images, 2–3 short videos)
- [x] Run `/import_media` and record the timing per image and per frame (`analyses.latency_ms`)
- [x] Review the quality: are people described consistently enough for later person search?
- [x] Tune the prompts and bump `IMAGE_PROMPT_VERSION` / `VIDEO_MERGE_PROMPT_VERSION` in `src/llm/prompts.rs`, then compare runs in the `analyses` table
- [ ] Decide whether the frame interval (10 s) and `image_max_edge` (1024) are right for the hardware (see open decisions)
- [x] Write the findings into this file (no real file names or personal data, the repo is public)

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
- After a restart with `--mmproj-auto`, `/props` reports `vision: true` and
  `response_format: json_object` works together with images.
- llama-server logs `W find_slot: non-consecutive token position ...` for image requests.
  This is most likely because Qwen-VL image tokens advance positions differently from
  text tokens. Outputs were correct, so it is treated as harmless. If garbled output ever
  appears, try `--no-cache-prompt` first.

### Run 1: prompt `img-v1` / `video-merge-v1`, max_tokens 1024
16 images + 3 videos (8 frames) in 24.5 min, 0 failed items.

| calls | avg s | min s | max s | avg prompt tok | avg output tok |
|---|---|---|---|---|---|
| 17 images | 52.4 | 32.7 | 108.6 | 924 | 467 |
| 8 frames | 59.6 | 36.3 | 80.0 | 503 | 662 |
| 3 video merges | 42.8 | 32.8 | 50.6 | 570 | 446 |

Output tokens dominate the latency (roughly 10 tokens/s generation).

**Quality: good enough to build person search on.**
- Clothing colors, hair, skin tone, age group, pose and position are consistently present.
- Name tags and labels are read (e.g. a name on a spacesuit), so people with visible
  names can be found by text.
- No identity guessing from faces. Landmarks are named (a famous castle).
- OCR of UI text (including German) and diagram labels is accurate.
- Low-resolution video (320x240) still gives usable descriptions.
- Text queries for described people rank the right file first in all 10 test queries
  (e.g. "bald man orange suit dogs", "teal top plaid shirt", "curly hair headset").

**Problems found**
1. 4/29 answers hit `max_tokens` and were cut off (invalid JSON): crowds (the model lists
   every visible person) and a terminal screenshot (`visible_text` fell into a repetition
   loop). Cut-off frames were stored as raw JSON fragments.
2. `ffmpeg -vf fps=1/10` drops the last sample if it falls within 5 s of the end
   (a 14 s video got 1 frame instead of 2, a 32 s video got 3 instead of 4).
3. Flattened `People:` text listed fields in alphabetical key order without labels.

### Run 2: prompt `img-v2` / `video-merge-v2`, max_tokens 1536
Fixes: at most 6 people listed, crowds as one group entry, a new `people_count`
estimate, `visible_text` capped and "never repeat lines", truncation flagged in
`analyses.error` with a salvaged `summary`, frame sampling via
`select='isnan(prev_selected_t)+gte(t-prev_selected_t,N)'`, and one labelled line per person.

Rerun of the 5 problem items (13 calls) in 10 min:
- 13/13 valid JSON, max 727 output tokens (v1: 4 truncated at 1024)
- the crowd photo went from 84 s to 62 s, the screenshot from 109 s to 48 s
- all videos now have their final frame
- `people_count` gives plausible estimates (0, 8–12, 20, 30, 50)
- the model does not strictly keep `visible_text` under 300 characters (about 900 on a
  code screenshot), but there is no repetition anymore

### Throughput estimate for this hardware
About 50 s per image and about 55 s per video frame, plus one merge call per video:
- ~1,700 images per day, running continuously
- a 10-minute video at 1 frame per 10 s means 60 frames, roughly 1 hour

### Open decisions
- For large photo libraries, consider a faster first pass: shorter output (fewer tags,
  shorter summary) or a smaller vision model, then a detailed pass on demand.
- Raise `video_frame_interval_secs` (e.g. 30 s) for long videos, or keep 10 s for quality.
- `image_max_edge = 1024` gives good detail (name tags are readable). Lowering it would
  save prompt tokens, but prompt tokens are not the bottleneck.


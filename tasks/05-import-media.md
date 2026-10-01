# Phase 5: `/import_media` (LLM content extraction, mocked)

**Status:** done (mocked; live test is phase 6)

## Goal
Background job that sends images and video frames to llama.cpp and stores content
descriptions. Developed and tested against a mock server (`wiremock`). The live test
happens in phase 6.

## Request
```json
{ "kind": ["image", "video"], "path_prefix": "/mnt/share/photos", "ids": [], "limit": 50, "force": false }
```
All fields are optional. Default: all images and videos with `summary_status = 'pending'`.
`force=true` re-processes `done` and `failed` items too. The response is `202` with a job id.

## Design
### LLM client (`src/llm/client.rs`)
- OpenAI-compatible `POST {base_url}/v1/chat/completions` with `reqwest` and the
  timeout `llm.timeout_secs`. Sends a Bearer token if `api_key` is set.
- Image content part: `{"type":"image_url","image_url":{"url":"data:image/jpeg;base64,..."}}`.
- Sets `"chat_template_kwargs": {"enable_thinking": false}` (Qwen3) and strips
  `<think>…</think>` from the response defensively.
- Reads `usage.prompt_tokens` and `usage.completion_tokens` when present.
- `GET /health/llm` calls `GET {base_url}/v1/models` and returns model ids and reachability.

### Prompts (`src/llm/prompts.rs`)
- Versioned constant `PROMPT_VERSION = "img-v1"`. Ask for JSON only:
```json
{ "summary": "", "people": [{"apparent_age": "", "gender_presentation": "", "appearance": "",
  "clothing": "", "action": ""}], "objects": [], "scene": "", "visible_text": "", "tags": [] }
```
- Video merge prompt (`video-merge-v1`): a text-only call with the timestamped frame
  descriptions that asks for an overall summary in the same JSON shape.
- If parsing fails, use the raw text as the summary, set `parsed = NULL`, and still mark it `done`.

### Pipeline (`src/pipelines/import_media.rs`)
1. Select the candidates and set `found`. Mark each one `running` when it is picked up.
2. Fetch it again: copy from `abs_path` into staging (it must still be inside a configured root).
3. Image: decode with the `image` crate, downscale to `image_max_edge`, encode as JPEG q85,
   then call the LLM.
4. Video: run `ffmpeg -i in -vf fps=1/{interval},scale='min({edge},iw)':-2 -frames:v {max} out_%05d.jpg`
   into a temp dir. Call the LLM per frame and store each result in `video_frames(ts_secs)`,
   then do the merge call.
5. Store `files.summary`, `summary_status`, and an `analyses` row (model, prompt version,
   raw response, parsed JSON, latency, tokens, error).
6. Delete the staging copy and frames. Increment the counters, honour cancellation, and
   rebuild FTS at the end.
- Use `llm.workers` concurrent items (default 1).

## Checklist
- [x] Crates: `reqwest` (json, rustls), `base64`, `image`, dev `wiremock`
- [x] `src/llm/{client,prompts}.rs` with unit tests (think-stripping, JSON parsing, fallback)
- [x] `src/extract/frames.rs` (ffmpeg frame sampling)
- [x] `src/pipelines/import_media.rs` + `POST /import_media`
- [x] `GET /health/llm`
- [x] Integration test with wiremock: index a generated PNG, import, then search finds the mocked description
- [x] Update README and tasks/README.md

## Implementation notes
- Requests send `response_format: {"type": "json_object"}` and
  `chat_template_kwargs.enable_thinking = false`. If the live server rejects either,
  remove it in `src/llm/client.rs`.
- `files.summary` holds flattened text (summary, people, objects, tags, scene, visible
  text), which is better for FTS than raw JSON. The raw JSON is in `analyses.parsed`.
- Each frame is one `analyses` row with `ts_secs` (migration 3). The merge call has `ts_secs = NULL`.
- The frame interval is `max(video_frame_interval_secs, duration / video_max_frames)`.
- A failed frame is logged in `recent_errors` and the video continues. The video only
  fails if no frame could be described or the merge call fails.
- On cancel, the current item goes back to `pending`. On startup, items left `running` are reset to `pending`.
- Default selection is `pending` only. `force` adds `done` and `failed`. `skipped`
  (too large) and `running` items are never selected.
- The video test is skipped without `ffmpeg`, so the ffmpeg frame extraction
  (`src/extract/frames.rs`) has **not been run yet**. After `sudo apt install ffmpeg`,
  run `cargo test` before the live LLM test.

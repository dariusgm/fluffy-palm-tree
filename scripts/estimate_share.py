#!/usr/bin/env python3
"""Estimate indexing and LLM work for a directory tree (e.g. a mounted SMB share).

Only the directory structure is scanned: files are classified by extension, file
contents are never read. With --video-durations, ffprobe reads video headers to get
durations, which determine the number of sampled frames.

The service itself detects types by content (magic bytes), so these numbers are an
approximation: e.g. extensionless text files are not counted here.

Usage:
    scripts/estimate_share.py /mnt/share/photos
    scripts/estimate_share.py /mnt/share --max-depth 2 --video-durations --sec-per-call 45
    scripts/estimate_share.py /mnt/share --json > estimate.json
"""

import argparse
import json
import math
import os
import subprocess
import sys
import time
from collections import Counter, defaultdict

# Keep in sync with src/detect.rs (approximation by extension).
IMAGE = {"jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff"}
VIDEO = {"mp4", "m4v", "mov", "mkv", "webm", "avi", "ogv", "wmv", "mpg", "mpeg", "flv"}
PDF = {"pdf"}
ARCHIVE = {"gz", "tgz", "bz2", "tbz", "tbz2", "xz", "txz", "zst", "tzst", "lz4", "lz", "z",
           "zip", "7z", "rar", "tar"}
MARKDOWN = {"md", "markdown"}
TEXT = {"txt", "text", "log", "csv", "tsv", "rst", "adoc", "org", "srt", "vtt"}
CODE = {
    "py", "pyw", "pyi", "rs", "js", "mjs", "cjs", "jsx", "ts", "tsx", "mts", "cts", "java",
    "kt", "kts", "scala", "go", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx", "cs",
    "swift", "m", "mm", "rb", "php", "pl", "pm", "lua", "r", "dart", "ex", "exs", "erl", "hs",
    "clj", "cljs", "sh", "bash", "zsh", "fish", "ksh", "ps1", "psm1", "bat", "cmd", "sql",
    "html", "htm", "xhtml", "css", "scss", "sass", "less", "vue", "svelte", "xml", "xsd",
    "xsl", "xslt", "plist", "manifest", "svg", "json", "jsonc", "geojson", "gdoc", "gsheet",
    "gslides", "ipynb", "yaml", "yml", "toml", "ini", "cfg", "conf", "properties", "gradle",
    "tex", "proto", "graphql", "gql", "tf", "nix",
}
CODE_NAMES = {"dockerfile", "containerfile", "makefile", "gnumakefile", "cmakelists.txt"}

CATEGORIES = ["image", "video", "pdf", "text", "code", "archive", "other"]


def category(name: str) -> str:
    lower = name.lower()
    if lower in CODE_NAMES:
        return "code"
    if lower.endswith((".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst")):
        return "archive"
    ext = lower.rsplit(".", 1)[1] if "." in lower else ""
    for cat, exts in (("image", IMAGE), ("video", VIDEO), ("pdf", PDF), ("archive", ARCHIVE),
                      ("text", TEXT | MARKDOWN), ("code", CODE)):
        if ext in exts:
            return cat
    return "other"


def extension(name: str) -> str:
    return name.lower().rsplit(".", 1)[1] if "." in name else "(none)"


def walk(root: str, max_depth: int | None, on_file, on_error):
    """Like the indexer: no symlinks, hidden files and directories skipped."""
    stack = [(root, 0)]
    while stack:
        path, depth = stack.pop()
        try:
            with os.scandir(path) as it:
                entries = list(it)
        except OSError as e:
            on_error(path, e)
            continue
        for entry in entries:
            if entry.name.startswith("."):
                continue
            try:
                if entry.is_symlink():
                    continue
                if entry.is_dir(follow_symlinks=False):
                    if max_depth is None or depth + 1 < max_depth:
                        stack.append((entry.path, depth + 1))
                elif entry.is_file(follow_symlinks=False):
                    on_file(entry, entry.stat(follow_symlinks=False).st_size)
            except OSError as e:
                on_error(entry.path, e)


def video_duration(path: str) -> float | None:
    try:
        out = subprocess.run(
            ["ffprobe", "-v", "error", "-show_entries", "format=duration",
             "-of", "default=noprint_wrappers=1:nokey=1", path],
            capture_output=True, text=True, timeout=60, check=True,
        ).stdout.strip()
        return float(out)
    except (subprocess.SubprocessError, ValueError, OSError):
        return None


def video_frames(duration: float, interval: int, max_frames: int) -> int:
    """Mirrors src/extract/frames.rs: first frame, one per interval, last frame."""
    interval_eff = max(interval, duration / max_frames) if max_frames else interval
    periodic = min(math.floor(duration / interval_eff) + 1, max(max_frames - 1, 1))
    last_ts = (periodic - 1) * interval_eff
    return periodic + (1 if duration - last_ts >= 2.0 else 0)


def human(n: float) -> str:
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if n < 1000 or unit == "TB":
            return f"{n:.1f} {unit}" if unit != "B" else f"{int(n)} B"
        n /= 1000
    return f"{n:.1f} TB"


def duration_str(secs: float) -> str:
    hours = secs / 3600
    return f"{hours:.1f} h" if hours < 48 else f"{hours / 24:.1f} days"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("path", help="directory to scan (e.g. a mounted share)")
    ap.add_argument("--max-depth", type=int, default=None,
                    help="1 = only files directly in PATH (like traverse=false); default: unlimited")
    ap.add_argument("--video-durations", action="store_true",
                    help="read video durations with ffprobe (header only) for frame estimates")
    ap.add_argument("--frame-interval", type=int, default=60, help="llm.video_frame_interval_secs")
    ap.add_argument("--max-frames", type=int, default=120, help="llm.video_max_frames")
    ap.add_argument("--max-file-bytes", type=int, default=10_000_000_000,
                    help="staging.max_file_bytes; larger files are indexed but not analysed")
    ap.add_argument("--sec-per-call", type=float, default=None,
                    help="average seconds per LLM call, to print a time estimate (e.g. 45)")
    ap.add_argument("--json", action="store_true", help="print the result as JSON")
    args = ap.parse_args()

    root = os.path.abspath(args.path)
    if not os.path.isdir(root):
        print(f"not a directory: {root}", file=sys.stderr)
        return 2

    counts = Counter()
    sizes = Counter()
    too_large = Counter()
    other_exts = Counter()
    videos: list[str] = []
    errors: list[str] = []
    started = time.monotonic()

    def on_file(entry, size):
        cat = category(entry.name)
        counts[cat] += 1
        sizes[cat] += size
        if cat == "other":
            other_exts[extension(entry.name)] += 1
        elif size > args.max_file_bytes:
            too_large[cat] += 1
        elif cat == "video":
            videos.append(entry.path)
        total = sum(counts.values())
        if not args.json and total % 5000 == 0:
            print(f"  ... {total} files scanned", file=sys.stderr)

    def on_error(path, err):
        errors.append(f"{path}: {err}")

    walk(root, args.max_depth, on_file, on_error)

    frames = None
    unknown_durations = 0
    total_video_secs = 0.0
    if args.video_durations and videos:
        frames = 0
        for i, path in enumerate(videos, 1):
            if not args.json and i % 50 == 0:
                print(f"  ... probed {i}/{len(videos)} videos", file=sys.stderr)
            d = video_duration(path)
            if d is None or d <= 0:
                unknown_durations += 1
                continue
            total_video_secs += d
            frames += video_frames(d, args.frame_interval, args.max_frames)

    analysable = {c: counts[c] - too_large[c] for c in CATEGORIES if c != "other"}
    # LLM calls per file, see README (import_media):
    #   image: 1 description + 1 OCR if the image contains text (0..1)
    #   video: 1 per sampled frame + 1 merge
    #   pdf:   1 first-page description + pages if scanned (unknown here)
    #   text/code: 1 summary (very short files are skipped)
    calls_min = analysable["image"] + analysable["pdf"] + analysable["text"] + analysable["code"]
    calls_max = calls_min + analysable["image"]
    if frames is not None:
        video_calls = frames + (len(videos) - unknown_durations)
        calls_min += video_calls
        calls_max += video_calls

    result = {
        "path": root,
        "max_depth": args.max_depth,
        "scan_seconds": round(time.monotonic() - started, 1),
        "files": {c: counts[c] for c in CATEGORIES},
        "bytes": {c: sizes[c] for c in CATEGORIES},
        "too_large_to_analyse": dict(too_large),
        "other_top_extensions": dict(other_exts.most_common(15)),
        "videos": {
            "count": analysable["video"],
            "probed_seconds": round(total_video_secs) if frames is not None else None,
            "unknown_duration": unknown_durations if frames is not None else None,
            "frames": frames,
        },
        "llm_calls": {
            "min": calls_min,
            "max": calls_max,
            "videos_included": frames is not None,
            "note": "max assumes every image contains text (OCR call); scanned PDF pages are not included",
        },
        "errors": errors[:50],
        "error_count": len(errors),
    }
    if args.sec_per_call:
        result["estimated_llm_time_seconds"] = {
            "min": round(calls_min * args.sec_per_call),
            "max": round(calls_max * args.sec_per_call),
        }

    if args.json:
        print(json.dumps(result, indent=2))
        return 0

    print(f"\nScanned {root} in {result['scan_seconds']} s"
          + (f" (max depth {args.max_depth})" if args.max_depth else ""))
    print(f"\n{'category':<10}{'files':>10}{'size':>12}   analysed by")
    how = {
        "image": "index + LLM description (+ OCR if text)",
        "video": "index + LLM per frame + merge",
        "pdf": "index + LLM first page (+ OCR per page if scanned)",
        "text": "index + LLM summary",
        "code": "index + LLM summary",
        "archive": "index only (no LLM)",
        "other": "ignored",
    }
    for c in CATEGORIES:
        print(f"{c:<10}{counts[c]:>10}{human(sizes[c]):>12}   {how[c]}")
    print(f"{'total':<10}{sum(counts.values()):>10}{human(sum(sizes.values())):>12}")
    if too_large:
        print(f"\nAbove max-file-bytes (indexed, not analysed): {dict(too_large)}")
    if other_exts:
        top = ", ".join(f".{e} {n}" for e, n in other_exts.most_common(10))
        print(f"Ignored types (top): {top}")

    print("\nVideos:")
    if frames is None:
        print(f"  {analysable['video']} videos; frame count unknown (use --video-durations)")
    else:
        print(f"  {analysable['video']} videos, {duration_str(total_video_secs)} total, "
              f"{frames} frames to describe"
              + (f", {unknown_durations} without duration" if unknown_durations else ""))

    print(f"\nLLM calls: {calls_min} - {calls_max}"
          + ("" if frames is not None else " (videos not included)"))
    print("  max assumes every image has text (extra OCR call); scanned PDF pages not included")
    if args.sec_per_call:
        lo, hi = result["estimated_llm_time_seconds"].values()
        print(f"  at {args.sec_per_call:g} s per call: {duration_str(lo)} - {duration_str(hi)}")
    if errors:
        print(f"\n{len(errors)} paths could not be read, e.g. {errors[0]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

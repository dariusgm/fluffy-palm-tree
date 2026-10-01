#!/usr/bin/env python3
"""Measure how many images are near-duplicates of an earlier image (perceptual hash).

Answers: how many vision-model calls could be skipped if an image that looks almost
identical to an already analysed one reused that description?

Two steps; the scan is resumable because every hash is appended to a cache file:

    scripts/phash_clusters.py scan   /mnt/share --cache hashes.jsonl
    scripts/phash_clusters.py report --cache hashes.jsonl --sec-per-call 35

Requires Pillow and numpy (not part of the service), in a uv environment at the repo root:
    uv venv .venv && uv pip install -r scripts/requirements.txt
    .venv/bin/python scripts/phash_clusters.py ...

For speed over network mounts the scan hashes the EXIF thumbnail of JPEGs (only the first
160 KB of the file is read) and falls back to decoding the whole image when there is no
usable thumbnail or with --full. Thumbnail hashes differ slightly from full-image hashes
of the same picture, which the thresholds below absorb.

The hash is a 64-bit dHash: the image is reduced to 9x8 grey pixels and each bit says
whether a pixel is brighter than its right neighbour. Hamming distance 0-4 means almost
certainly the same picture (resized, re-encoded); 5-12 also catches burst shots and
similar scenes, with a growing share of false matches.

The report simulates the intended service behaviour: files are visited in path order, an
image is "skipped" if it is within the threshold of an image that was analysed (not
skipped) earlier, otherwise it becomes a new analysed representative. No transitive
chaining, so skipped counts are what a reuse rule would really save.
"""

import argparse
import json
import os
import random
import sys
import time
import warnings
from concurrent.futures import ThreadPoolExecutor, as_completed

IMAGE_EXT = {".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".tif", ".tiff"}


def walk(root, max_depth):
    stack = [(root, 0)]
    while stack:
        path, depth = stack.pop()
        try:
            with os.scandir(path) as it:
                entries = list(it)
        except OSError:
            continue
        for e in entries:
            if e.name.startswith("."):
                continue
            try:
                if e.is_symlink():
                    continue
                if e.is_dir(follow_symlinks=False):
                    if max_depth is None or depth + 1 < max_depth:
                        stack.append((e.path, depth + 1))
                elif e.is_file(follow_symlinks=False) and os.path.splitext(e.name)[1].lower() in IMAGE_EXT:
                    yield e.path
            except OSError:
                continue


def _bits(img):
    from PIL import Image

    px = img.convert("L").resize((9, 8), Image.BILINEAR).tobytes()
    bits = 0
    for row in range(8):
        for col in range(8):
            bits = (bits << 1) | (px[row * 9 + col] > px[row * 9 + col + 1])
    return bits


def _thumbnail_hash(path):
    """dHash of the EXIF thumbnail, reading only the file head. None if there is none."""
    import io

    from PIL import Image, ImageOps

    with open(path, "rb") as f:
        head = f.read(160 * 1024)
    if head[:2] != b"\xff\xd8":
        return None
    i = 2
    while i + 4 <= len(head) and head[i] == 0xFF and head[i + 1] != 0xDA:
        length = int.from_bytes(head[i + 2 : i + 4], "big")
        if head[i + 1] == 0xE1 and head[i + 4 : i + 10] == b"Exif\0\0":
            seg = head[i + 4 : i + 2 + length]
            break
        i += 2 + length
    else:
        return None
    start = seg.find(b"\xff\xd8\xff", 14)
    if start < 0:
        return None
    with Image.open(io.BytesIO(head)) as full:
        width, height = full.size
    exif = Image.Exif()
    exif.load(seg)
    with Image.open(io.BytesIO(seg[start:])) as thumb:
        thumb.draft("L", (128, 128))
        thumb.load()
        orientation = exif.get(274, 1)
        method = {2: Image.FLIP_LEFT_RIGHT, 3: Image.ROTATE_180, 4: Image.FLIP_TOP_BOTTOM,
                  5: Image.TRANSPOSE, 6: Image.ROTATE_270, 7: Image.TRANSVERSE,
                  8: Image.ROTATE_90}.get(orientation)
        if method is not None:
            thumb = thumb.transpose(method)
        if abs(thumb.width / thumb.height - width / height) > 0.1 and \
                abs(thumb.width / thumb.height - height / width) > 0.1:
            return None  # thumbnail does not match the picture (stale after editing)
        return _bits(thumb), width, height


def dhash(path, full=False):
    from PIL import Image, ImageOps

    if not full:
        try:
            hit = _thumbnail_hash(path)
        except Exception:
            hit = None
        if hit:
            return (*hit, 1)
    with warnings.catch_warnings():
        warnings.simplefilter("error", Image.DecompressionBombWarning)
        with Image.open(path) as img:
            width, height = img.size
            img.draft("L", (128, 128))  # JPEG: decode at reduced size
            img = ImageOps.exif_transpose(img)
            return _bits(img), width, height, 0


def scan(args):
    root = os.path.abspath(args.path)
    done = set()
    if os.path.exists(args.cache):
        with open(args.cache) as f:
            for line in f:
                try:
                    done.add(json.loads(line)["p"])
                except (ValueError, KeyError):
                    pass
    print(f"cache has {len(done)} entries", file=sys.stderr)

    def work(path):
        try:
            h, w, ht, thumb = dhash(path, args.full)
            return {"p": path, "h": f"{h:016x}", "w": w, "ht": ht, "t": thumb}
        except Exception as e:  # corrupt or unsupported file
            return {"p": path, "err": type(e).__name__}

    started, n, errors = time.monotonic(), 0, 0
    with open(args.cache, "a") as out, ThreadPoolExecutor(args.workers) as pool:
        pending = set()
        paths = (p for p in walk(root, args.max_depth) if p not in done)
        exhausted = False
        while pending or not exhausted:
            while not exhausted and len(pending) < args.workers * 4:
                try:
                    pending.add(pool.submit(work, next(paths)))
                except StopIteration:
                    exhausted = True
            if not pending:
                break
            finished = next(as_completed(pending))
            pending.discard(finished)
            rec = finished.result()
            errors += "err" in rec
            out.write(json.dumps(rec) + "\n")
            n += 1
            if n % 500 == 0:
                out.flush()
                rate = n / (time.monotonic() - started)
                print(f"  {n} hashed ({rate:.1f}/s), {errors} unreadable", file=sys.stderr)
            if args.limit and n >= args.limit:
                break
    print(f"done: {n} new, {errors} unreadable, {time.monotonic() - started:.0f} s", file=sys.stderr)


def report(args):
    import numpy as np

    recs = {}
    errors = 0
    with open(args.cache) as f:
        for line in f:
            r = json.loads(line)
            if "err" in r:
                errors += 1
            else:
                recs[r["p"]] = r
    paths = sorted(recs)
    n = len(paths)
    hashes = np.array([int(recs[p]["h"], 16) for p in paths], dtype=np.uint64)
    landscape = np.array([recs[p]["w"] >= recs[p]["ht"] for p in paths])
    flat = int((hashes == 0).sum())
    exact = n - len(set(hashes.tolist()))
    print(f"{n} images hashed ({errors} unreadable), {flat} with an all-zero hash (flat images)")
    print(f"identical hash (distance 0): {exact} images repeat an earlier hash\n")

    thresholds = [int(t) for t in args.thresholds.split(",")]
    top = max(thresholds)
    samples = []
    print(f"{'max dist':>8}{'analysed':>10}{'skipped':>10}{'saved':>8}{'same folder':>13}"
          + (f"{'hours saved':>13}" if args.sec_per_call else ""))
    for t in thresholds:
        reps = np.empty(n, dtype=np.uint64)
        rep_idx = np.empty(n, dtype=np.int64)
        rep_land = np.empty(n, dtype=bool)
        r = skipped = same_folder = 0
        for i in range(n):
            if r:
                d = np.bitwise_count(reps[:r] ^ hashes[i])
                if args.same_orientation:
                    d = np.where(rep_land[:r] == landscape[i], d, 64)
                j = int(d.argmin())
                if d[j] <= t:
                    skipped += 1
                    same_folder += os.path.dirname(paths[i]) == os.path.dirname(paths[rep_idx[j]])
                    if t == top:
                        samples.append((int(d[j]), paths[i], paths[rep_idx[j]]))
                    continue
            reps[r], rep_idx[r], rep_land[r] = hashes[i], i, landscape[i]
            r += 1
        row = (f"{t:>8}{r:>10}{skipped:>10}{100 * skipped / n:>7.1f}%"
               f"{(100 * same_folder / skipped if skipped else 0):>12.0f}%")
        if args.sec_per_call:
            row += f"{skipped * args.sec_per_call / 3600:>13.0f}"
        print(row)

    if args.samples:
        random.seed(1)
        with open(args.samples, "w") as f:
            for lo, hi in ((0, 2), (3, 6), (7, 10), (11, 14), (15, 64)):
                band = [s for s in samples if lo <= s[0] <= hi]
                f.write(f"# distance {lo}-{hi}: {len(band)} pairs\n")
                for d, a, b in random.sample(band, min(10, len(band))):
                    f.write(f"{d}\t{a}\t{b}\n")
        print(f"\nsample pairs per distance band written to {args.samples}")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("scan", help="hash all images below PATH into the cache")
    s.add_argument("path")
    s.add_argument("--cache", required=True)
    s.add_argument("--workers", type=int, default=8)
    s.add_argument("--max-depth", type=int, default=None)
    s.add_argument("--full", action="store_true",
                   help="always decode the whole image instead of the EXIF thumbnail (slow over SMB)")
    s.add_argument("--limit", type=int, default=0, help="stop after N new images (testing)")
    r = sub.add_parser("report", help="count skippable images per distance threshold")
    r.add_argument("--cache", required=True, action="append")
    r.add_argument("--thresholds", default="0,2,4,6,8,10,12,16")
    r.add_argument("--sec-per-call", type=float, default=None)
    r.add_argument("--same-orientation", action="store_true",
                   help="only match images that are both landscape or both portrait")
    r.add_argument("--samples", help="write random matched pairs per distance band to this file")
    args = ap.parse_args()
    if args.cmd == "scan":
        scan(args)
    else:
        if len(args.cache) > 1:
            merged = args.cache[0] + ".merged"
            with open(merged, "w") as out:
                for c in args.cache:
                    out.write(open(c).read())
            args.cache = merged
        else:
            args.cache = args.cache[0]
        report(args)


if __name__ == "__main__":
    main()

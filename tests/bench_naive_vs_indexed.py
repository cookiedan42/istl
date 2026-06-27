"""Benchmark: indexed istl query vs naive scan of a plain binary STL.

Run:  uv run python tests/bench_naive_vs_indexed.py

Compares wall-clock for several bbox sizes against `data/terrain.stl` and
`/tmp/terrain.istl`. The "naive" implementation is a numpy-mmap scan that
filters triangles by first-vertex XY and writes a fresh binary STL — about as
fast as a non-indexed approach can reasonably get without per-call sorting.
"""
from __future__ import annotations

import struct
import time
from pathlib import Path

import istl
import numpy as np

STL = Path("data/terrain.stl")
ISTL = Path("/tmp/terrain.istl")

# --- Queries ----------------------------------------------------------------
# (label, xmin, ymin, zmin, xmax, ymax, zmax)
INF = float("inf")
QUERIES = [
    ("AOI feature 1 (~200x170 m window)", 31140, 32863, -INF, 31340, 33036, INF),
    ("AOI 10x larger (~2 km window)",     30000, 32000, -INF, 32000, 34000, INF),
    ("AOI 100x larger (~20 km window)",   20000, 25000, -INF, 40000, 45000, INF),
    ("Whole dataset",                     -INF,  -INF,  -INF,  INF,   INF,  INF),
    ("Empty (far away)",                  1e7,   1e7,   -INF,  2e7,   2e7,  INF),
]


def naive_scan(stl_path: Path, bbox, out_path: Path) -> tuple[int, float]:
    """Mmap the binary STL, filter triangles by first-vertex XY, write a new STL.

    Returns (triangles_written, seconds).
    """
    t0 = time.perf_counter()
    with stl_path.open("rb") as f:
        header = f.read(80)  # noqa: F841
        n = struct.unpack("<I", f.read(4))[0]
    # mmap the triangle region as a structured array.
    triangles = np.memmap(
        stl_path,
        dtype=np.dtype([
            ("normal", "<f4", 3),
            ("v0", "<f4", 3),
            ("v1", "<f4", 3),
            ("v2", "<f4", 3),
            ("attr", "<u2"),
        ]),
        mode="r",
        offset=84,
        shape=(n,),
    )
    xmin, ymin, zmin, xmax, ymax, zmax = bbox
    v0 = triangles["v0"]
    x = v0[:, 0]
    y = v0[:, 1]
    mask = (x >= xmin) & (x <= xmax) & (y >= ymin) & (y <= ymax)
    selected = triangles[mask]
    with out_path.open("wb") as f:
        f.write(b"\x00" * 80)
        f.write(struct.pack("<I", selected.size))
        # Write the structured slice as-is — same dtype as the source.
        f.write(bytes(selected))
    dt = time.perf_counter() - t0
    return int(selected.size), dt


def indexed_query(istl_path: Path, bbox, out_path: Path) -> tuple[int, float]:
    t0 = time.perf_counter()
    stats = istl.query(str(istl_path), str(out_path), bbox=bbox)
    dt = time.perf_counter() - t0
    return int(stats["triangles_written"]), dt


def fmt_n(n: int) -> str:
    if n >= 1_000_000:
        return f"{n/1_000_000:.2f} M"
    if n >= 1_000:
        return f"{n/1_000:.1f} k"
    return str(n)


def fmt_t(s: float) -> str:
    if s >= 1.0:
        return f"{s:6.2f} s"
    if s >= 1e-3:
        return f"{s*1e3:6.1f} ms"
    return f"{s*1e6:6.1f} us"


def main() -> None:
    assert STL.exists(), f"missing {STL}"
    assert ISTL.exists(), f"missing {ISTL}; run `target/release/istl convert` first"

    print(f"input STL : {STL} ({STL.stat().st_size / 1e9:.2f} GB)")
    print(f"input ISTL: {ISTL} ({ISTL.stat().st_size / 1e9:.2f} GB)")
    print()
    print(
        f"{'query':<38} {'naive tris':>12} {'idx tris':>10}  "
        f"{'naive':>10} {'indexed':>10} {'speedup':>9}"
    )
    print("-" * 95)
    for label, *bbox in QUERIES:
        bbox = tuple(bbox)
        naive_out = Path("/tmp/naive.stl")
        idx_out = Path("/tmp/idx.stl")
        # Run naive twice to let mmap warm — report best of 2 (representative).
        naive_n, _ = naive_scan(STL, bbox, naive_out)
        naive_n, naive_t = naive_scan(STL, bbox, naive_out)
        idx_n, _ = indexed_query(ISTL, bbox, idx_out)
        idx_n, idx_t = indexed_query(ISTL, bbox, idx_out)
        speedup = naive_t / idx_t if idx_t > 0 else float("inf")
        print(
            f"{label:<38} {fmt_n(naive_n):>12} {fmt_n(idx_n):>10}  "
            f"{fmt_t(naive_t):>10} {fmt_t(idx_t):>10} {speedup:>8.1f}x"
        )

    # Sanity: indexed result must be a superset of naive (whole row groups).
    print()
    label, *bbox = QUERIES[0]
    bbox = tuple(bbox)
    n_naive, _ = naive_scan(STL, bbox, Path("/tmp/naive.stl"))
    n_idx, _ = indexed_query(ISTL, bbox, Path("/tmp/idx.stl"))
    print(
        f"sanity (AOI 1): indexed={n_idx} naive={n_naive} -> "
        f"indexed >= naive? {n_idx >= n_naive}"
    )


if __name__ == "__main__":
    main()

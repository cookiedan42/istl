# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`istl` — an indexed binary-STL format inspired by GeoParquet. Triangles are Hilbert-sorted by their first vertex's XY and grouped into row groups; a footer at the end of the file records each group's byte range and 3D bounding box. A reader can then return all triangles in row groups whose bbox intersects a query bbox without scanning the whole mesh.

Plan & rationale: `/Users/seed1091/.claude/plans/this-project-aims-to-happy-snail.md`.

## Workspace layout

Rust workspace (`Cargo.toml` at root) + uv-managed Python dev env (`pyproject.toml` at root).

- `crates/istl-core/` — format constants, Hilbert mapping, converter, reader, `RangeReader` trait + `FileSource`/`BytesSource`. Pure Rust, no deps.
- `crates/istl-cli/` — `istl` binary (`convert` / `query` / `info` / `query-remote` subcommands, via `clap`).
- `crates/istl-py/` — `pyo3` extension module exposing `convert`, `query`, `query_url`, `read_index`. Built with `maturin`.
- `crates/istl-objectstore/` — `ObjectStoreSource` implementing `RangeReader` over any [`object_store`](https://crates.io/crates/object_store) backend (S3, GCS, Azure Blob, HTTP, local file, in-memory). `ObjectStoreSource::open_url("s3://bucket/key")` is the common entrypoint; same crate Parquet/Arrow uses for remote reads.

The on-disk format spec lives in `crates/istl-core/src/format.rs`. Keep it in sync with the plan file's "Format spec" section if you change layout.

## Common commands

Rust:
- `cargo build --release -p istl-cli` — build the `istl` CLI to `target/release/istl`.
- `cargo test -p istl-core` — runs unit tests (`format`, `hilbert`) and `tests/round_trip.rs` integration tests.
- `cargo test -p istl-core --test round_trip <name>` — single integration test.

Python bindings (uv + maturin):
- `uv sync --group dev` — provision `.venv` with `maturin`, `pytest`, `meshio`, `numpy`.
- `uv run maturin develop --release --manifest-path crates/istl-py/Cargo.toml` — build the cdylib and install `istl` into the uv venv. Re-run after any change in `crates/istl-py/` or `crates/istl-core/`.
- `uv run pytest tests/` — Python verification suite. Tests build synthetic STLs in `tmp_path` (no fixtures on disk).
- `uv run python tests/bench_naive_vs_indexed.py` — benchmark indexed query vs a numpy-mmap naive scan against `data/terrain.stl` and `/tmp/terrain.istl`. Requires running `istl convert` first. (Not picked up by pytest — `bench_*` prefix.)

End-to-end smoke against the bundled 2.5 GB sample:
- `target/release/istl convert data/terrain.stl /tmp/terrain.istl` (~16 s on this machine).
- `target/release/istl info /tmp/terrain.istl`.
- `target/release/istl query /tmp/terrain.istl /tmp/out.stl --bbox xmin,ymin,zmin,xmax,ymax,zmax` (use `-inf`/`inf` to drop an axis).

## Architecture notes worth knowing before editing

- **Footer is self-locating from the tail (Parquet-style).** Bytes 0–7 are identifier `ISTL1\0\0\0`; bytes 8–79 are zero (72 B reserved). Bytes 80–83 are the normal STL `u32` triangle count, so the file is still a valid binary STL for any parser that doesn't care about identifier in the unused first bytes of the 80-byte header. The footer payload (group_count + records) lives just before the file tail; the last 16 bytes of the file are `footer_size: u64 LE` followed by `TRAILER` identifier `ISTLEND\0`. Invariant: `file_size = 84 + 50 × triangle_count + footer_size + 16`. (meshio's STL auto-detect sniffs the first bytes as UTF-8 and trips on the ISTL identifier — that's a meshio quirk, not a spec violation.)

  **Remote-reader access pattern (S3/Lambda).** Single round trip in the common case:
    1. `GET Range: bytes=-N` for an upper-bound `N` (e.g. 64 KB) — covers the trailer identifier, `footer_size`, the entire footer, and (harmlessly) some preceding triangles. Verify the last 8 bytes equal `TRAILER`; read `footer_size` from the 8 bytes before that; slice the footer payload from the end of the buffer.
    2. Only if `N < footer_size + 16` (very large index): one precise follow-up GET. Pick a big-enough `N` and this never happens.

  The head identifier (`bytes=0-7`) is still useful as a sniff/`file(1)` signature but is not required to locate the index.
- **Two-pass converter, in-memory key sort.** Pass 1 streams the input to compute the dataset XY bbox. Pass 2 builds `(hilbert_key: u32, src_index: u32)` pairs in RAM (~8 B × N — ~432 MB for the 54 M-triangle sample), `sort_unstable_by_key`, then random-reads the input in sorted order via `pread` (Unix `FileExt::read_exact_at`) and streams triangles to the output, folding each group's bbox as it goes. The footer payload is written immediately after the triangles, followed by the 16-byte tail (`footer_size` + `TRAILER`). The converter is **strict forward-only** — no seek-back-and-patch is needed.
- **Reader is generic over `RangeReader`.** `Reader<R: RangeReader>` works over any random-access byte source: `FileSource` (local), `BytesSource<B: AsRef<[u8]>>` (in-memory), and `istl_objectstore::ObjectStoreSource` (range GETs against any S3/GCS/Azure/HTTP/local backend via the `object_store` crate). `Reader::open(path)` is sugar for `Reader::open_source(FileSource::open(path)?)`. The remote path issues one `head` request (for `Content-Length`) at construction, then one range GET for the 24-byte head + one for the 16-byte tail + one for the footer payload (3 round trips to open), then one GET per matched row-group slab during `query`.
- **Reader returns whole row groups.** Per the design, `Reader::query` copies the contiguous byte slab of every intersecting group verbatim into the output STL — no per-triangle filtering. The output is a vanilla 80-byte-zero-header binary STL. `Reader::open` validates the head identifier, then reads the 16-byte tail to find the footer, then cross-checks `footer_size` against the decoded index's `encoded_size()` (catches truncation, partial writes, and format-version drift). `Reader::query_to_writer(bbox, &mut W)` is the no-temp-file variant — useful for streaming the result straight into an HTTP response or a Lambda return payload.
- **Hilbert key is 32-bit on a 65,536 × 65,536 grid** (`HILBERT_ORDER = 16` in `convert.rs`). Coordinates are normalized via `hilbert::normalize` against the pass-1 dataset bbox; collapsed axes (min == max) collapse to grid coord 0.
- **Python bindings drop the GIL** (`py.detach`, pyo3 0.29) around `convert`/`query` so big files don't block other Python threads.

## Test data

- `data/terrain.stl` (~2.5 GB, 54,103,132 triangles, EPSG:3414). gitignored.
- `data/AOI_Geometry.json` — sample polygon AOIs (also EPSG:3414). Two features: feature 1 ~199×172 m, feature 2 ~799×772 m. Feature 1's bbox (`x[31140.5..31339.5], y[32863.7..33035.9]`) is the canonical verification query.

## `group_size` tuning

Default is `DEFAULT_GROUP_SIZE = 32_768` (constant exposed from `istl-core`, re-exported from the CLI, and accessible as `istl.DEFAULT_GROUP_SIZE` in Python). This targets object-store access — each group is ~1.6 MB (the start of the efficient parallel-GET band) and the footer fits a single ~64 KB speculative tail GET.

Lower it for local NVMe / mmap (e.g. 8 K) where per-request overhead is negligible and tighter bboxes win. Raise it for HDD/NFS (128 K+) where seeks dominate, or high-latency HTTP/edge.

## Measured performance (terrain.stl, local SSD, warm cache)

Convert: ~16 s (54 M triangles).

At `group_size = 32_768` (current default): 1,652 row groups, footer 66 KB. At `group_size = 8_192` (previous default): 6,605 row groups, footer 264 KB.

Query (indexed at 8 K group_size vs numpy-mmap naive scan — the indexed numbers don't change meaningfully at 32 K on local disk because IO is page-cached):

| Query | Naive | Indexed | Speedup |
|---|---:|---:|---:|
| AOI feature 1 (~200 m) | 310 ms | 1.0 ms | 305× |
| ~2 km window | 313 ms | 6.7 ms | 47× |
| ~20 km window (~50% of dataset) | 1.06 s | 607 ms | 1.7× |
| Whole dataset | 11.8 s | 8.7 s | 1.4× |
| Empty (far away) | 298 ms | 0.18 ms | 1675× |

Indexed always returns a **superset** of the strictly-inside triangles because the unit of retrieval is a whole row group — that's the "no per-triangle filter" design choice, not a bug. Larger `group_size` ⇒ larger superset per query.

## What's intentionally out of scope (v0)

ASCII STL input, per-triangle filtering inside row groups, adaptive/nested groups, external merge sort, parallel convert/query, polygon (non-rectangular) AOI clipping. See plan file for the full list.

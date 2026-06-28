# istl — Python bindings

Python bindings for [`istl`](https://github.com/cookiedan42/istl), an indexed
binary-STL format inspired by GeoParquet. Triangles are Hilbert-sorted by their
first vertex's XY and packed into row groups; a footer at the tail of the file
records each group's byte range and 3D bounding box. Spatial bbox queries then
skip everything that can't possibly intersect.

The Python module is a thin [`pyo3`](https://pyo3.rs) wrapper over the Rust
`istl-core` crate — all the heavy lifting (sort, IO, bbox folding) happens in
Rust with the GIL released, so calls don't block other Python threads.

## What you get

```python
import istl

# Convert a plain binary STL to a Hilbert-indexed .istl file. The default
# group_size (32,768) is tuned for object-store access (S3/GCS/Azure); see
# the API reference below to tune for local NVMe, HDD, or edge.
stats = istl.convert("terrain.stl", "terrain.istl")
# stats == {
#   "triangles": 54_103_132,
#   "groups": 1_652,
#   "footer_offset": 2_705_156_684,
#   "footer_size": 66_084,
#   "dataset_bbox": (xmin, xmax, ymin, ymax, zmin, zmax),
# }

# Query: emit every triangle in any row group whose 3D bbox intersects
# (xmin, ymin, zmin, xmax, ymax, zmax). Use `-inf`/`inf` to drop an axis.
# `input` and `output` may be local paths or URLs (s3://, gs://, az://,
# http(s)://, file://, memory://). `format` defaults to "stl" (vanilla
# binary STL) — pass "istl" to emit an indexed file that can be queried again.
res = istl.query(
    "terrain.istl",
    "aoi.stl",
    bbox=(31140.0, 32863.0, float("-inf"),
          31340.0, 33036.0, float("inf")),
)
# res == {"groups_matched": 3, "triangles_written": 24576}

# Land the result straight on S3, in indexed form:
istl.query(
    "s3://my-bucket/terrain.istl",
    "s3://my-bucket/aoi.istl",
    bbox=(31140.0, 32863.0, float("-inf"),
          31340.0, 33036.0, float("inf")),
    format="istl",
)

# Inspect the footer index without copying any triangles.
groups = istl.read_index("terrain.istl")
# [{byte_start, byte_size, xmin, xmax, ymin, ymax, zmin, zmax}, ...]
```

The output of `istl.query` is a plain binary STL (80-byte zero header + `u32`
count + 50 B/triangle) consumable by any standard STL reader (`meshio`,
`numpy-stl`, Blender, MeshLab, etc.).

**Whole-row-group semantics.** A query returns every triangle in any matching
row group, not just the triangles whose own bbox falls inside the query. That
means `triangles_written` is always a *superset* of the strict per-triangle
answer. This is the design — it lets `query` byte-copy contiguous slabs of the
indexed file with no per-triangle parsing.

## Build & install

The bindings are not currently published to PyPI. Build from source with
[`uv`](https://docs.astral.sh/uv/) and
[`maturin`](https://www.maturin.rs/):

```bash
# Clone the istl repo, then from its root:
uv sync --group dev          # provisions .venv with maturin, pytest, meshio, numpy
uv run maturin develop \
  --release \
  --manifest-path crates/istl-py/Cargo.toml
```

`maturin develop` builds the Rust cdylib in release mode and installs the
`istl` module into the uv-managed `.venv`. Re-run it after any change to
`crates/istl-py/` or `crates/istl-core/`. The bindings link directly against
`istl-core` via a path dependency — there is no separate "Rust install" step.

You can then use the module via uv:

```bash
uv run python -c "import istl; print(istl.read_index('terrain.istl')[:3])"
uv run pytest tests/
```

### Build requirements

- Python 3.10+
- A Rust toolchain (1.75+; edition 2024 is used by the workspace)
- A C linker (`cc` on macOS/Linux, MSVC on Windows). `maturin` handles the
  pyo3 ↔ Python linkage; `cargo build -p istl-py` alone will fail with an
  "undefined symbols" link error because it has no Python interpreter to
  link against — always go through `maturin`.

### Building a wheel for distribution

```bash
uv run maturin build --release --manifest-path crates/istl-py/Cargo.toml
# wheel lands in target/wheels/
```

Cross-compilation, abi3, and CI examples are covered in the
[maturin docs](https://www.maturin.rs/).

## API reference

### `istl.convert(input, output, group_size=istl.DEFAULT_GROUP_SIZE) -> dict`

Sort the triangles of a binary STL along a Hilbert curve (XY of first vertex,
mapped onto a 65,536 × 65,536 grid) and write an `.istl` file with a footer
index recording each row group's byte range and 3D bbox. Returns a dict with
`triangles`, `groups`, `footer_offset`, `footer_size`, `dataset_bbox`.

`group_size` controls the number of triangles per row group. The default
(`istl.DEFAULT_GROUP_SIZE = 32,768`, ~1.6 MB per group) targets object-store
access (S3 / GCS / Azure Blob) — each group sits in the efficient
parallel-GET band, and the footer fits a single ~64 KB speculative tail
range request. Tune per deployment:

| Workload | Suggested `group_size` |
|---|---|
| Local NVMe / mmap, tight AOIs | `8_192` |
| **S3 / GCS / Azure (default)** | **`32_768`** |
| S3 Express One Zone (lower TTFB) | `16_384` |
| HDD / NFS / EFS | `131_072` |
| Generic HTTP server, edge, high-latency | `131_072+` |

Smaller groups give tighter per-group bboxes (more selective queries, fewer
wasted triangles per match) but more requests per query. Larger groups give
a smaller index and fewer requests, at the cost of more over-fetch per
match. The format itself is agnostic — this is a writer-side knob you can
re-tune per deployment by re-running `convert`.

### `istl.query(input, output, bbox, format="stl") -> dict`

`input` and `output` are each a local path or a URL. Supported URL schemes
(via the `remote` cargo feature, on by default) are listed below — the same
call works against S3, GCS, Azure Blob, generic HTTP, and the local
filesystem, backed by
[`object_store`](https://crates.io/crates/object_store). Remote reads are
range GETs (no full download); remote writes use multipart upload for large
results and a single `put` for small ones.

| URL scheme | Backend |
|---|---|
| `s3://bucket/key` | AWS S3 (env/profile/IMDS/Lambda role auth) |
| `gs://bucket/key` | Google Cloud Storage |
| `az://container/blob` | Azure Blob Storage |
| `http(s)://host/path` | Generic HTTP / WebDAV |
| `file:///abs/path` | Local filesystem |
| `memory:///path` | In-memory (mostly tests; not shared across calls) |

`bbox` is a 6-tuple `(xmin, ymin, zmin, xmax, ymax, zmax)`. `min > max` on any
axis raises `ValueError`. Use `float("-inf")` / `float("inf")` to disable an
axis (e.g. XY-only queries).

`format` selects the output:
- `"stl"` (default) — a vanilla binary STL (80-byte zero header + `u32`
  count + triangles). Consumable by any STL reader.
- `"istl"` — an indexed `.istl` file. Matched row groups are streamed
  verbatim and a new footer is written; the result is itself queryable
  and inherits the source's per-group bboxes.

Returns `{"groups_matched", "triangles_written"}`.

For builds without remote support: `uv run maturin develop --no-default-features
--features pyo3/extension-module ...`. URL inputs/outputs then raise
`OSError` with a clear "requires the `remote` feature" message.

### `istl.read_index(path) -> list[dict]`

Reads the footer without touching the triangle payload. Useful for debugging,
visualizing coverage, or driving a custom remote reader against an `.istl`
file on S3.

## On-disk format (summary)

```
bytes  0..8   : IDENTIFIER "ISTL1\0\0\0"
bytes  8..80  : reserved (zeroed, 72 B)
bytes 80..84  : triangle_count u32 LE     ← valid plain-STL count
bytes 84..    : triangles (50 B each, Hilbert-sorted)
... footer payload (just before the tail):
    4 B group_count u32
    group_count × 40 B records: byte_start u64, byte_size u64, bbox (6× f32)
last 16 B     : footer_size u64 LE + TRAILER "ISTLEND\0"
```

The footer is **self-locating from the tail** (the same pattern Parquet uses):
a reader looks at the last 16 bytes to find the trailer identifier and
`footer_size`, then reads `footer_size` bytes immediately before the tail to
get the index. Invariant: `file_size = 84 + 50 × triangle_count + footer_size + 16`.

This makes the format friendly to range-GET access on S3: one speculative
`GET Range: bytes=-N` (e.g. `N = 64 KB`) typically retrieves the trailer
identifier, `footer_size`, and the entire footer in a single round trip. Once the
index is in hand, follow-up GETs fetch only the matching row-group slabs.

Because the format keeps the original binary-STL header structure (just with
identifier in the unused-by-spec first bytes), the file remains parseable by any
strict binary-STL reader. Some readers (e.g. `meshio`) auto-detect ASCII vs
binary by UTF-8-decoding the first few bytes and will trip on the identifier —
that's a reader quirk, not a format violation.

## License

Same as the workspace: MIT OR Apache-2.0.

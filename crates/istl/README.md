# istl

Indexed binary STL — a Hilbert-sorted, bbox-indexed mesh format inspired by
GeoParquet. Triangles are reordered along a Hilbert space-filling curve and
grouped into row groups; a footer at the end of the file records each group's
byte range and 3D bounding box. A reader can return all triangles in row
groups whose bbox intersects a query bbox without scanning the whole mesh.

The on-disk file is still a valid binary STL (any STL parser that ignores the
80-byte header can read it), so you don't have to give up tooling
compatibility to get spatial indexing.

## Example

```rust
use istl::{convert, Reader, Bbox, DEFAULT_GROUP_SIZE};

// Convert a plain binary STL into an indexed .istl.
convert("terrain.stl", "terrain.istl", DEFAULT_GROUP_SIZE)?;

// Open the indexed file and pull only the row groups intersecting a bbox.
let reader = Reader::open("terrain.istl")?;
let q = Bbox::from([31140.5, 31339.5, 32863.7, 33035.9, f32::NEG_INFINITY, f32::INFINITY]);
let stats = reader.query(&q, "aoi.stl")?;
println!("{} triangles in {} groups", stats.triangles_written, stats.groups_matched);
# Ok::<(), std::io::Error>(())
```

## Sources

`Reader` is generic over [`RangeReader`], a small trait for random-access byte
sources. This crate ships [`FileSource`] (local file) and [`BytesSource`]
(in-memory). For S3, GCS, Azure Blob, and HTTP, see the companion
[`istl-objectstore`](https://crates.io/crates/istl-objectstore) crate.

## Performance

On a 54M-triangle terrain file, querying a ~200 m window returns in ~1 ms
(vs. ~310 ms for a brute-force scan) — see the [project README](https://github.com/cookiedan42/istl)
for full benchmark numbers, the format spec, and the CLI/Python bindings.

## License

MIT OR Apache-2.0

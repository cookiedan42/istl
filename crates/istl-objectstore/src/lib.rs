//! Object-store random-access source for `istl`.
//!
//! [`ObjectStoreSource`] implements [`istl_core::RangeReader`] over any
//! [`object_store::ObjectStore`] backend — AWS S3, Google Cloud Storage, Azure
//! Blob Storage, HTTP/WebDAV, the local filesystem, or in-memory. Each
//! `read_at(offset, n)` becomes a single `GetRange::Bounded` request.
//!
//! Pair it with [`istl_core::Reader::open_source`] to query `.istl` files in
//! any of those backends without downloading them.
//!
//! ```no_run
//! use istl_core::{Bbox, Reader};
//! use istl_objectstore::ObjectStoreSource;
//!
//! let src = ObjectStoreSource::open_url("s3://my-bucket/terrain.istl")?;
//! let reader = Reader::open_source(src)?;
//! reader.query(
//!     &Bbox::from([31140.0, 31340.0, 32863.0, 33036.0,
//!                  f32::NEG_INFINITY, f32::INFINITY]),
//!     "/tmp/aoi.stl",
//! )?;
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! ### Supported URL schemes
//!
//! | Scheme | Backend |
//! |---|---|
//! | `s3://bucket/key` | AWS S3 (credentials/region from env, profile, IMDS, Lambda role) |
//! | `gs://bucket/key` | Google Cloud Storage |
//! | `az://container/blob` | Azure Blob Storage |
//! | `http(s)://host/path` | Generic HTTP / WebDAV |
//! | `file:///abs/path` | Local filesystem |
//! | `memory:///path` | In-memory (mostly useful for tests) |
//!
//! ### Concurrency model
//!
//! [`RangeReader::read_at`] is synchronous. The `object_store` crate is async,
//! so we run it on an internally-owned tokio runtime and `block_on` per call.
//! Multiple threads can call `read_at` concurrently — the underlying HTTP
//! client (a shared `reqwest` connection pool inside `object_store`) handles
//! concurrent range GETs.

use std::io;
use std::sync::{Arc, OnceLock};

use object_store::{ObjectStore, parse_url, path::Path};
use tokio::runtime::Runtime;
use url::Url;

mod reader;
pub use reader::ObjectStoreSource;

mod writer;
pub use writer::ObjectStoreSink;

/// Parse a URL into an `ObjectStore` + `Path`.
///
/// Wraps [`object_store::parse_url`] with one specialization: for `s3://` URLs
/// we build the [`AmazonS3`] via [`AmazonS3Builder::from_env`] instead of the
/// default factory. This matters in **AWS Lambda**, where IMDS is disabled.
/// `object_store`'s default S3 credential chain probes IMDSv2 first, which
/// hangs/times out, and only then falls back to env vars. `from_env()` reads
/// `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN` /
/// `AWS_REGION` / `AWS_ENDPOINT_URL` eagerly and skips the IMDS probe entirely.
///
/// Non-S3 URLs (`gs://`, `az://`, `http(s)://`, `file://`, `memory://`) go
/// through `parse_url` unchanged.
///
/// [`AmazonS3`]: object_store::aws::AmazonS3
/// [`AmazonS3Builder::from_env`]: object_store::aws::AmazonS3Builder::from_env
pub(crate) fn parse_url_with_env(url: &Url) -> Result<(Arc<dyn ObjectStore>, Path), object_store::Error> {
    if url.scheme() == "s3" {
        use object_store::aws::AmazonS3Builder;
        let store = AmazonS3Builder::from_env().with_url(url.as_str()).build()?;
        let path = Path::from(url.path().trim_start_matches('/'));
        return Ok((Arc::new(store), path));
    }
    let (store, path) = parse_url(url)?;
    Ok((Arc::from(store), path))
}

// `head` and `get_range` live on `ObjectStoreExt`; we use `get_opts` directly
// (which IS on `ObjectStore`) for both the size lookup and the range reads,
// so the impl works against a `dyn ObjectStore`.

fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().expect("failed to create tokio runtime for istl-objectstore"))
}

fn map_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("object_store: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use object_store::path::Path;
    use object_store::{GetOptions, ObjectStore};
    use std::sync::Arc;

    #[test]
    fn runtime_is_singleton() {
        let a = runtime() as *const _;
        let b = runtime() as *const _;
        assert_eq!(a, b);
    }

    /// `ObjectStoreSink` writes bytes to a store and `finish()` commits them.
    #[test]
    fn sink_round_trip_in_memory() {
        use io::Write;
        use object_store::memory::InMemory;

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("blob.bin");
        let payload: Vec<u8> = (0u8..=255).cycle().take(8 * 1024).collect();

        let mut sink = ObjectStoreSink::create(store.clone(), path.clone());
        sink.write_all(&payload).unwrap();
        sink.finish().unwrap();

        let got = runtime()
            .block_on(async {
                let res = store.get_opts(&path, GetOptions::default()).await?;
                res.bytes().await
            })
            .expect("get failed");
        assert_eq!(got.as_ref(), payload.as_slice());
    }

    /// Dropping a sink without `finish()` writes nothing — documented contract.
    #[test]
    fn sink_drop_without_finish_writes_nothing() {
        use io::Write;
        use object_store::memory::InMemory;

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("ghost.bin");
        {
            let mut sink = ObjectStoreSink::create(store.clone(), path.clone());
            sink.write_all(b"never committed").unwrap();
            // drop without finish()
        }
        let err = runtime()
            .block_on(async { store.get_opts(&path, GetOptions::default()).await })
            .err()
            .expect("expected NotFound");
        // object_store returns `Error::NotFound` for missing keys.
        let msg = format!("{err}");
        assert!(
            msg.to_lowercase().contains("not found") || msg.to_lowercase().contains("nosuchkey"),
            "expected NotFound-ish error, got: {msg}"
        );
    }

    /// End-to-end: put an .istl into `InMemory`, query into `ObjectStoreSink`
    /// at a second path (ISTL format), reopen the result via a fresh
    /// `ObjectStoreSource`, and assert the index matches the query result.
    #[test]
    fn sink_round_trip_query_istl_in_memory() {
        use object_store::memory::InMemory;
        use object_store::{PutOptions, PutPayload};

        // Build a 3-group .istl in memory using the core converter via a tmp file.
        let dir =
            std::env::temp_dir().join(format!("istl-sink-test-{}-{}", std::process::id(), "istl"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stl_path = dir.join("in.stl");
        let istl_path = dir.join("in.istl");

        // 600 triangles on a 20x30 grid, group_size=128 → 5 groups.
        let n = 600u32;
        let grid = 20u32;
        {
            use io::Write as _;
            let mut f = std::fs::File::create(&stl_path).unwrap();
            f.write_all(&[0u8; istl_core::STL_HEADER_LEN]).unwrap();
            f.write_all(&n.to_le_bytes()).unwrap();
            for i in 0..n {
                let x = (i % grid) as f32;
                let y = (i / grid) as f32;
                let mut tri = [0u8; istl_core::TRI_SIZE];
                tri[8..12].copy_from_slice(&1f32.to_le_bytes());
                for v in 0..3 {
                    let off = 12 + v * 12;
                    tri[off..off + 4].copy_from_slice(&x.to_le_bytes());
                    tri[off + 4..off + 8].copy_from_slice(&y.to_le_bytes());
                }
                f.write_all(&tri).unwrap();
            }
        }
        istl_core::convert(&stl_path, &istl_path, 128).unwrap();
        let istl_bytes = std::fs::read(&istl_path).unwrap();

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let src_path = Path::from("in.istl");
        let out_path = Path::from("aoi.istl");
        runtime()
            .block_on(async {
                store
                    .put_opts(
                        &src_path,
                        PutPayload::from_bytes(istl_bytes.into()),
                        PutOptions::default(),
                    )
                    .await
            })
            .expect("put failed");

        let src = ObjectStoreSource::open(store.clone(), src_path).unwrap();
        let reader = istl_core::Reader::open_source(src).unwrap();
        let qb = istl_core::Bbox::from([3.0, 8.0, 3.0, 8.0, -1.0, 1.0]);
        let mut sink = ObjectStoreSink::create(store.clone(), out_path.clone());
        let stats = reader.query_to_writer_istl(&qb, &mut sink).unwrap();
        sink.finish().unwrap();
        assert!(stats.groups_matched > 0);

        // Reopen the output object as an .istl and verify it round-trips.
        let out_src = ObjectStoreSource::open(store, out_path).unwrap();
        let out_reader = istl_core::Reader::open_source(out_src).unwrap();
        assert_eq!(out_reader.index.groups.len() as u64, stats.groups_matched);
        assert_eq!(out_reader.triangle_count as u64, stats.triangles_written);
        assert_eq!(out_reader.footer_size, out_reader.index.encoded_size());
    }

    /// Same end-to-end but with `query_to_writer` (vanilla STL) — proves the
    /// STL dispatch path also works through the sink.
    #[test]
    fn sink_round_trip_query_stl_in_memory() {
        use object_store::memory::InMemory;
        use object_store::{PutOptions, PutPayload};

        let dir =
            std::env::temp_dir().join(format!("istl-sink-test-{}-{}", std::process::id(), "stl"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stl_path = dir.join("in.stl");
        let istl_path = dir.join("in.istl");
        let n = 200u32;
        let grid = 10u32;
        {
            use io::Write as _;
            let mut f = std::fs::File::create(&stl_path).unwrap();
            f.write_all(&[0u8; istl_core::STL_HEADER_LEN]).unwrap();
            f.write_all(&n.to_le_bytes()).unwrap();
            for i in 0..n {
                let x = (i % grid) as f32;
                let y = (i / grid) as f32;
                let mut tri = [0u8; istl_core::TRI_SIZE];
                tri[8..12].copy_from_slice(&1f32.to_le_bytes());
                for v in 0..3 {
                    let off = 12 + v * 12;
                    tri[off..off + 4].copy_from_slice(&x.to_le_bytes());
                    tri[off + 4..off + 8].copy_from_slice(&y.to_le_bytes());
                }
                f.write_all(&tri).unwrap();
            }
        }
        istl_core::convert(&stl_path, &istl_path, 64).unwrap();
        let istl_bytes = std::fs::read(&istl_path).unwrap();

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let src_path = Path::from("in.istl");
        let out_path = Path::from("aoi.stl");
        runtime()
            .block_on(async {
                store
                    .put_opts(
                        &src_path,
                        PutPayload::from_bytes(istl_bytes.into()),
                        PutOptions::default(),
                    )
                    .await
            })
            .expect("put failed");

        let src = ObjectStoreSource::open(store.clone(), src_path).unwrap();
        let reader = istl_core::Reader::open_source(src).unwrap();
        let qb = istl_core::Bbox::UNIVERSE;
        let mut sink = ObjectStoreSink::create(store.clone(), out_path.clone());
        let stats = reader.query_to_writer(&qb, &mut sink).unwrap();
        sink.finish().unwrap();
        assert_eq!(stats.triangles_written, n as u64);

        // The output object should be a valid binary STL: 80 + 4 + 50*n bytes.
        let got = runtime()
            .block_on(async {
                let res = store.get_opts(&out_path, GetOptions::default()).await?;
                res.bytes().await
            })
            .expect("get failed");
        let expected = istl_core::STL_HEADER_LEN as u64 + 4 + n as u64 * istl_core::TRI_SIZE as u64;
        assert_eq!(got.len() as u64, expected);
    }
}

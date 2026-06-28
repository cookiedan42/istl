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

use istl_core::RangeReader;
use object_store::buffered::BufWriter as OsBufWriter;
use object_store::{GetOptions, GetRange, ObjectStore, parse_url, path::Path};
use tokio::io::AsyncWriteExt;
use tokio::runtime::Runtime;
use url::Url;

// `head` and `get_range` live on `ObjectStoreExt`; we use `get_opts` directly
// (which IS on `ObjectStore`) for both the size lookup and the range reads,
// so the impl works against a `dyn ObjectStore`.

fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        Runtime::new().expect("failed to create tokio runtime for istl-objectstore")
    })
}

/// `RangeReader` over any `object_store` backend. Each `read_at` is one range GET.
pub struct ObjectStoreSource {
    store: Arc<dyn ObjectStore>,
    path: Path,
    len: u64,
}

impl ObjectStoreSource {
    /// Open an object addressed by a URL (`s3://`, `gs://`, `az://`,
    /// `http(s)://`, `file://`, `memory://`). One `head` request is issued to
    /// learn the object size.
    pub fn open_url(url: &str) -> io::Result<Self> {
        let parsed = Url::parse(url).map_err(|e| io::Error::other(format!("bad URL {url:?}: {e}")))?;
        let (store, path) = parse_url(&parsed).map_err(map_err)?;
        Self::open(Arc::from(store), path)
    }

    /// Open from an already-constructed `ObjectStore` + `Path`. Useful when
    /// the caller wants custom credentials or a shared client across many
    /// sources (one connection pool for the whole process).
    pub fn open(store: Arc<dyn ObjectStore>, path: Path) -> io::Result<Self> {
        let opts = GetOptions {
            head: true,
            ..Default::default()
        };
        let len = runtime()
            .block_on(async { store.get_opts(&path, opts).await })
            .map_err(map_err)?
            .meta
            .size;
        Ok(ObjectStoreSource { store, path, len })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl RangeReader for ObjectStoreSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let n = buf.len() as u64;
        if n == 0 {
            return Ok(());
        }
        let opts = GetOptions {
            range: Some(GetRange::Bounded(offset..offset + n)),
            ..Default::default()
        };
        let bytes = runtime()
            .block_on(async {
                let res = self.store.get_opts(&self.path, opts).await?;
                res.bytes().await
            })
            .map_err(map_err)?;
        if bytes.len() as u64 != n {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "object_store returned {} bytes for range {offset}..{} (wanted {n})",
                    bytes.len(),
                    offset + n
                ),
            ));
        }
        buf.copy_from_slice(&bytes);
        Ok(())
    }
}

fn map_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("object_store: {e}"))
}

/// `std::io::Write` sink over any `object_store` backend.
///
/// Wraps [`object_store::buffered::BufWriter`], which adaptively chooses
/// between a single `put` (small objects) and a multipart upload (large
/// objects) based on accumulated buffered size — so the sink works against
/// backends that don't support multipart (plain HTTP, some S3-compat stores)
/// as well as those that do.
///
/// **Callers MUST call [`finish`](Self::finish)** to commit the upload.
/// Dropping the sink without `finish()` abandons any in-progress multipart
/// upload and writes nothing — this is intentional, so that a panic between
/// writes never commits a half-written object. The `#[must_use]` attribute
/// surfaces a compiler warning if the sink is constructed and ignored.
#[must_use = "ObjectStoreSink must be finalized with .finish() to commit the upload"]
pub struct ObjectStoreSink {
    inner: OsBufWriter,
}

impl ObjectStoreSink {
    /// Open a sink addressed by a URL (`s3://`, `gs://`, `az://`,
    /// `http(s)://`, `file://`, `memory://`). Same scheme support as
    /// [`ObjectStoreSource::open_url`].
    pub fn create_url(url: &str) -> io::Result<Self> {
        let parsed = Url::parse(url)
            .map_err(|e| io::Error::other(format!("bad URL {url:?}: {e}")))?;
        let (store, path) = parse_url(&parsed).map_err(map_err)?;
        Ok(Self::create(Arc::from(store), path))
    }

    /// Open a sink from an already-constructed `ObjectStore` + `Path`.
    pub fn create(store: Arc<dyn ObjectStore>, path: Path) -> Self {
        ObjectStoreSink {
            inner: OsBufWriter::new(store, path),
        }
    }

    /// Commit the upload. MUST be called; `Drop` does NOT commit.
    pub fn finish(mut self) -> io::Result<()> {
        runtime()
            .block_on(self.inner.shutdown())
            .map_err(|e| io::Error::other(format!("object_store shutdown: {e}")))
    }
}

impl io::Write for ObjectStoreSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        runtime()
            .block_on(self.inner.write(buf))
            .map_err(|e| io::Error::other(format!("object_store write: {e}")))
    }

    fn flush(&mut self) -> io::Result<()> {
        runtime()
            .block_on(self.inner.flush())
            .map_err(|e| io::Error::other(format!("object_store flush: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_is_singleton() {
        let a = runtime() as *const _;
        let b = runtime() as *const _;
        assert_eq!(a, b);
    }

    #[test]
    fn implements_range_reader() {
        fn assert_range<R: RangeReader>() {}
        assert_range::<ObjectStoreSource>();
    }

    /// End-to-end: in-memory object store can hold an istl file, and
    /// `Reader::open_source` over an `ObjectStoreSource` works the same way
    /// as over `FileSource`. This is the cheapest non-network proof that the
    /// trait wiring is correct.
    #[test]
    fn reader_over_in_memory_store() {
        use object_store::memory::InMemory;
        use object_store::{PutOptions, PutPayload};

        // Build a minimal valid .istl file in memory: header + 1 triangle + footer + tail.
        // Triangle count = 1, group_count = 1, byte_size = 50.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ISTL1\0\0\0");
        bytes.extend_from_slice(&[0u8; 72]); // reserved
        bytes.extend_from_slice(&1u32.to_le_bytes()); // triangle_count
        // one zero triangle (50 B)
        bytes.extend_from_slice(&[0u8; 50]);
        // footer payload: group_count(4) + 1 record(40)
        let footer_start = bytes.len() as u64;
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&84u64.to_le_bytes()); // byte_start
        bytes.extend_from_slice(&50u64.to_le_bytes()); // byte_size
        for _ in 0..6 {
            bytes.extend_from_slice(&0f32.to_le_bytes()); // bbox = origin point
        }
        let footer_size = bytes.len() as u64 - footer_start;
        // tail: footer_size + ISTLEND
        bytes.extend_from_slice(&footer_size.to_le_bytes());
        bytes.extend_from_slice(b"ISTLEND\0");

        // `put_opts` is on `ObjectStore` itself, so it works through `&dyn`.
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("terrain.istl");
        runtime()
            .block_on(async {
                store
                    .put_opts(&path, PutPayload::from_bytes(bytes.into()), PutOptions::default())
                    .await
            })
            .expect("put failed");

        let src = ObjectStoreSource::open(store, path).unwrap();
        let reader = istl_core::Reader::open_source(src).unwrap();
        assert_eq!(reader.triangle_count, 1);
        assert_eq!(reader.index.groups.len(), 1);
        assert_eq!(reader.index.groups[0].byte_start, 84);
        assert_eq!(reader.index.groups[0].byte_size, 50);
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
        let dir = std::env::temp_dir().join(format!(
            "istl-sink-test-{}-{}",
            std::process::id(),
            "istl"
        ));
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

        let dir = std::env::temp_dir().join(format!(
            "istl-sink-test-{}-{}",
            std::process::id(),
            "stl"
        ));
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

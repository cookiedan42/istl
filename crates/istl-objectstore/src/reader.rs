use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::sync::Arc;

use futures::{StreamExt, TryStreamExt};
use istl_core::RangeReader;
use object_store::{GetOptions, GetRange, ObjectStore, parse_url, path::Path};
use url::Url;

use super::{map_err, runtime};

/// Default number of in-flight range GETs for [`ObjectStoreSource::read_to_writer_batch`].
/// 16 is the conventional S3 sweet spot — saturates one client without
/// blowing memory (peak working set ≈ 16 × largest matched group ≈ 25 MB at
/// the default `group_size`).
pub const DEFAULT_BATCH_CONCURRENCY: usize = 16;

/// `RangeReader` over any `object_store` backend. Each `read_at` is one range GET.
pub struct ObjectStoreSource {
    store: Arc<dyn ObjectStore>,
    path: Path,
    len: u64,
    concurrency: NonZeroUsize,
}

impl ObjectStoreSource {
    /// Open an object addressed by a URL (`s3://`, `gs://`, `az://`,
    /// `http(s)://`, `file://`, `memory://`). One `head` request is issued to
    /// learn the object size.
    pub fn open_url(url: &str) -> io::Result<Self> {
        let parsed =
            Url::parse(url).map_err(|e| io::Error::other(format!("bad URL {url:?}: {e}")))?;
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
        Ok(ObjectStoreSource {
            store,
            path,
            len,
            concurrency: NonZeroUsize::new(DEFAULT_BATCH_CONCURRENCY).unwrap(),
        })
    }

    /// Override the number of concurrent range GETs issued by
    /// `read_to_writer_batch`. Returns `self` for builder chaining.
    pub fn with_concurrency(mut self, concurrency: NonZeroUsize) -> Self {
        self.concurrency = concurrency;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn concurrency(&self) -> NonZeroUsize {
        self.concurrency
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

    /// Stream the range-GET response chunks straight into `out`. Avoids the
    /// `Bytes` → caller-`Vec<u8>` `copy_from_slice` that the `read_at` path
    /// needs. Chunks are refcounted `Bytes` views into the HTTP body buffer;
    /// we collect them inside the runtime (to drive the stream) and write
    /// them out afterwards (so `out`'s own `Write` impl is free to drive its
    /// own runtime — e.g. `ObjectStoreSink::write` blocks on tokio too).
    fn read_to_writer<W: Write + ?Sized>(
        &self,
        offset: u64,
        n: u64,
        out: &mut W,
    ) -> io::Result<()> {
        if n == 0 {
            return Ok(());
        }
        let opts = GetOptions {
            range: Some(GetRange::Bounded(offset..offset + n)),
            ..Default::default()
        };
        let chunks: Vec<bytes::Bytes> = runtime().block_on(async {
            let res = self
                .store
                .get_opts(&self.path, opts)
                .await
                .map_err(map_err)?;
            res.into_stream()
                .map(|c| c.map_err(map_err))
                .try_collect()
                .await
        })?;
        let mut written: u64 = 0;
        for chunk in &chunks {
            out.write_all(chunk)?;
            written += chunk.len() as u64;
        }
        if written != n {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "object_store streamed {written} bytes for range {offset}..{} (wanted {n})",
                    offset + n
                ),
            ));
        }
        Ok(())
    }

    /// Issue up to [`Self::concurrency`] range GETs in parallel and write the
    /// results to `out` in submission order. On a high-RTT link this turns a
    /// K-group query from K × RTT into roughly K/N × RTT for the network
    /// phase. Bytes for a slow request can pile up behind earlier in-flight
    /// requests that haven't completed yet — `buffered(N)` is ordered, not
    /// out-of-order — so peak memory is bounded by `N × largest-in-flight-range`.
    ///
    /// All response chunks for the whole batch are collected inside a single
    /// `block_on` (so we don't nest a writer-side `block_on` underneath an
    /// async drive — see `read_to_writer` above for the same workaround). The
    /// `Bytes` values are refcounted views into the HTTP body, not owned
    /// copies, so the intermediate `Vec<Vec<Bytes>>` is cheap.
    fn read_to_writer_batch<W: Write + ?Sized>(
        &self,
        ranges: &[(u64, u64)],
        out: &mut W,
    ) -> io::Result<()> {
        // Skip zero-length entries entirely. They're a valid input shape but
        // object_store rejects an HTTP-style `bytes=O-O` range, and the
        // default `read_to_writer` short-circuits on `n == 0` so the
        // override must too.
        let nonempty: Vec<(u64, u64)> =
            ranges.iter().copied().filter(|&(_, n)| n > 0).collect();
        if nonempty.is_empty() {
            return Ok(());
        }
        let concurrency = self.concurrency.get();
        let store = &self.store;
        let path = &self.path;
        let results: Vec<(u64, u64, Vec<bytes::Bytes>)> = runtime().block_on(async move {
            futures::stream::iter(nonempty.iter().copied())
                .map(|(offset, n)| async move {
                    let opts = GetOptions {
                        range: Some(GetRange::Bounded(offset..offset + n)),
                        ..Default::default()
                    };
                    let res = store.get_opts(path, opts).await.map_err(map_err)?;
                    let chunks: Vec<bytes::Bytes> = res
                        .into_stream()
                        .map(|c| c.map_err(map_err))
                        .try_collect()
                        .await?;
                    Ok::<_, io::Error>((offset, n, chunks))
                })
                .buffered(concurrency)
                .try_collect()
                .await
        })?;

        for (offset, expected, chunks) in &results {
            let mut written: u64 = 0;
            for chunk in chunks {
                out.write_all(chunk)?;
                written += chunk.len() as u64;
            }
            if written != *expected {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "object_store streamed {written} bytes for range \
                         {offset}..{} (wanted {expected})",
                        offset + expected
                    ),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    .put_opts(
                        &path,
                        PutPayload::from_bytes(bytes.into()),
                        PutOptions::default(),
                    )
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

    /// Put a deterministic byte payload in memory, build a list of 20
    /// non-overlapping varying-size ranges, and assert that
    /// `read_to_writer_batch` produces the same output bytes regardless of
    /// the configured concurrency (1, 8, default 16). Concretely this proves
    /// that `buffered(N)` preserves submission order and that a
    /// faster-completing later request never gets written ahead of an
    /// earlier slow one.
    #[test]
    fn read_to_writer_batch_preserves_order_across_concurrencies() {
        use object_store::memory::InMemory;
        use object_store::{PutOptions, PutPayload};

        let payload: Vec<u8> = (0..64_000u32).map(|i| (i * 31) as u8).collect();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("payload.bin");
        runtime()
            .block_on(async {
                store
                    .put_opts(
                        &path,
                        PutPayload::from_bytes(bytes::Bytes::from(payload.clone())),
                        PutOptions::default(),
                    )
                    .await
            })
            .expect("put failed");

        // Mix of large and tiny ranges so any out-of-order completion would
        // be obvious in the output diff. Offsets are deliberately scattered.
        let ranges: Vec<(u64, u64)> = vec![
            (1000, 4096),
            (50, 17),
            (32_000, 8192),
            (5, 1),
            (60_000, 999),
            (0, 250),
            (12_345, 333),
            (40_000, 5000),
            (200, 64),
            (8_000, 2048),
            (250, 0), // zero-length range — should be a no-op slot
            (15_000, 1024),
            (500, 1234),
            (45_000, 32),
            (7_000, 8),
            (20_000, 4000),
            (24_000, 64),
            (55_000, 128),
            (3_500, 2222),
            (62_000, 100),
        ];
        let expected: Vec<u8> = ranges
            .iter()
            .flat_map(|&(o, n)| payload[o as usize..(o + n) as usize].iter().copied())
            .collect();

        for c in [1usize, 8, DEFAULT_BATCH_CONCURRENCY] {
            let src = ObjectStoreSource::open(store.clone(), path.clone())
                .unwrap()
                .with_concurrency(NonZeroUsize::new(c).unwrap());
            let mut out = Vec::with_capacity(expected.len());
            src.read_to_writer_batch(&ranges, &mut out).unwrap();
            assert_eq!(
                out, expected,
                "concurrency={c} produced wrong/out-of-order bytes"
            );
        }
    }

    #[test]
    fn read_to_writer_batch_empty_is_noop() {
        use object_store::memory::InMemory;
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("empty.bin");
        // The path doesn't even need to exist — an empty batch must not GET.
        let src = ObjectStoreSource {
            store,
            path,
            len: 0,
            concurrency: NonZeroUsize::new(4).unwrap(),
        };
        let mut out = Vec::new();
        src.read_to_writer_batch(&[], &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn read_to_writer_batch_single_matches_read_to_writer() {
        use object_store::memory::InMemory;
        use object_store::{PutOptions, PutPayload};
        let payload: Vec<u8> = (0..4_096u32).map(|i| (i & 0xff) as u8).collect();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("single.bin");
        runtime()
            .block_on(async {
                store
                    .put_opts(
                        &path,
                        PutPayload::from_bytes(bytes::Bytes::from(payload.clone())),
                        PutOptions::default(),
                    )
                    .await
            })
            .unwrap();
        let src = ObjectStoreSource::open(store, path).unwrap();
        let mut via_single = Vec::new();
        src.read_to_writer(100, 1234, &mut via_single).unwrap();
        let mut via_batch = Vec::new();
        src.read_to_writer_batch(&[(100, 1234)], &mut via_batch)
            .unwrap();
        assert_eq!(via_single, via_batch);
        assert_eq!(via_single, &payload[100..1334]);
    }
}

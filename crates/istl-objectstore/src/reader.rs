use std::io;
use std::sync::Arc;

use istl_core::RangeReader;
use object_store::{GetOptions, GetRange, ObjectStore, parse_url, path::Path};
use url::Url;

use super::{map_err, runtime};

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
}

use std::io;
use std::sync::Arc;

use object_store::buffered::BufWriter as OsBufWriter;
use object_store::{ObjectStore, parse_url, path::Path};
use tokio::io::AsyncWriteExt;
use url::Url;

use super::{map_err, runtime};

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
        let parsed =
            Url::parse(url).map_err(|e| io::Error::other(format!("bad URL {url:?}: {e}")))?;
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

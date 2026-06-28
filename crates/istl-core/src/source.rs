//! Random-access byte sources for [`crate::Reader`].
//!
//! The reader doesn't care whether the bytes live on local disk, on S3, behind an
//! HTTP server, or in a `Vec<u8>`. Any type that implements [`RangeReader`] can be
//! handed to `Reader::open_source` and queried.
//!
//! `istl-core` ships exactly two implementations:
//! - [`FileSource`] — wraps a `std::fs::File` using `pread`/seek+read.
//! - [`BytesSource`] — wraps any `AsRef<[u8]>` (handy for tests and in-memory data).
//!
//! Remote-IO implementations (S3, HTTP) live in separate crates so this crate
//! stays dependency-free.

use std::fs::File;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::path::Path;

/// Random-access byte source.
///
/// Implementations must be cheap to share by reference across many `read_at`
/// calls — the reader holds one source for the lifetime of all queries and
/// fans many reads out to it (footer, then each matched row-group slab).
///
/// Implementations should be safe to call from concurrent threads where
/// possible; the file-backed impl uses `pread`, which is concurrency-safe.
pub trait RangeReader {
    /// Total size of the underlying object in bytes.
    fn len(&self) -> u64;

    /// Read exactly `buf.len()` bytes starting at absolute `offset`.
    /// Fails if `offset + buf.len() > self.len()` (i.e. short reads error,
    /// they don't silently truncate).
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;

    /// Convenience: read `n` bytes at `offset` into a fresh `Vec<u8>`.
    fn read_vec(&self, offset: u64, n: usize) -> io::Result<Vec<u8>> {
        let mut v = vec![0u8; n];
        self.read_at(offset, &mut v)?;
        Ok(v)
    }
}

/// `RangeReader` backed by a `std::fs::File`. Uses `pread` on Unix.
pub struct FileSource {
    file: File,
    len: u64,
}

impl FileSource {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = File::open(path.as_ref())?;
        let len = file.metadata()?.len();
        Ok(FileSource { file, len })
    }

    pub fn from_file(file: File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(FileSource { file, len })
    }
}

impl RangeReader for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    #[cfg(unix)]
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.read_exact_at(buf, offset)
    }

    #[cfg(not(unix))]
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = self.file.try_clone()?;
        f.seek(SeekFrom::Start(offset))?;
        f.read_exact(buf)
    }
}

/// `RangeReader` over any in-memory byte buffer. Useful for tests and for
/// callers that have already pulled the file into RAM.
pub struct BytesSource<B: AsRef<[u8]>> {
    bytes: B,
}

impl<B: AsRef<[u8]>> BytesSource<B> {
    pub fn new(bytes: B) -> Self {
        BytesSource { bytes }
    }
}

impl<B: AsRef<[u8]>> RangeReader for BytesSource<B> {
    fn len(&self) -> u64 {
        self.bytes.as_ref().len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let src = self.bytes.as_ref();
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset overflow"))?;
        if end > src.len() as u64 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read_at past end of buffer",
            ));
        }
        let start = offset as usize;
        buf.copy_from_slice(&src[start..start + buf.len()]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_source_reads_slice() {
        let s = BytesSource::new(b"0123456789".to_vec());
        assert_eq!(s.len(), 10);
        let mut buf = [0u8; 4];
        s.read_at(3, &mut buf).unwrap();
        assert_eq!(&buf, b"3456");
    }

    #[test]
    fn bytes_source_eof_errors() {
        let s = BytesSource::new(b"abc".to_vec());
        let mut buf = [0u8; 5];
        let err = s.read_at(0, &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn bytes_source_read_vec() {
        let s = BytesSource::new(b"hello world".to_vec());
        let v = s.read_vec(6, 5).unwrap();
        assert_eq!(&v, b"world");
    }
}

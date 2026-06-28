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

use std::io;

mod byte_source;
pub use byte_source::BytesSource;

mod file_source;
pub use file_source::FileSource;

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

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

use std::io::{self, Write};

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

    /// Stream `n` bytes at `offset` directly into `out`. The default impl
    /// chunks through a 64 KB scratch buffer; remote backends should override
    /// to push range-GET response bytes straight to the writer without
    /// allocating the whole slab. Used by `Reader::query_to_writer` so a
    /// matched row group never has to land in a caller-side `Vec`.
    fn read_to_writer<W: Write + ?Sized>(
        &self,
        offset: u64,
        n: u64,
        out: &mut W,
    ) -> io::Result<()> {
        const CHUNK: usize = 64 * 1024;
        if n == 0 {
            return Ok(());
        }
        let mut buf = vec![0u8; CHUNK.min(n as usize)];
        let mut remaining = n;
        let mut cur = offset;
        while remaining > 0 {
            let take = remaining.min(buf.len() as u64) as usize;
            self.read_at(cur, &mut buf[..take])?;
            out.write_all(&buf[..take])?;
            cur += take as u64;
            remaining -= take as u64;
        }
        Ok(())
    }

    /// Read every `(offset, byte_size)` range and write them to `out` in
    /// submission order — used by `Reader::query_to_writer` to dispatch all
    /// matched row-group reads in one trait call. The default impl is a serial
    /// loop over `read_to_writer`, identical to issuing the reads one by one.
    ///
    /// Remote backends (e.g. `ObjectStoreSource`) should override to issue the
    /// reads concurrently while preserving output order — on S3 this turns a
    /// `K × RTT` query into a roughly `K/N × RTT` one for large AOIs.
    fn read_to_writer_batch<W: Write + ?Sized>(
        &self,
        ranges: &[(u64, u64)],
        out: &mut W,
    ) -> io::Result<()> {
        for &(offset, n) in ranges {
            self.read_to_writer(offset, n, out)?;
        }
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

use super::RangeReader;
use std::io;

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

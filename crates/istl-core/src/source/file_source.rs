use super::RangeReader;
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::path::Path;

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

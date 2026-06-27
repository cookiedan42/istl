use std::fs::File;
use std::io::{self, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;
#[cfg(unix)]
use std::os::unix::fs::FileExt;

use crate::format::{Bbox, Index, RowGroup, STL_HEADER_LEN, TAIL_LEN, parse_istl_header, parse_tail};

#[derive(Debug, Clone, Copy)]
pub struct QueryStats {
    pub groups_matched: u64,
    pub triangles_written: u64,
}

pub struct Reader {
    file: File,
    pub index: Index,
    pub footer_offset: u64,
    pub footer_size: u64,
    pub triangle_count: u32,
}

impl Reader {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let mut file = File::open(path.as_ref())?;
        let file_len = file.metadata()?.len();

        // Sanity: header magic. We still validate the head magic so corrupted
        // files or non-istl inputs fail fast with a meaningful error.
        let mut header = [0u8; STL_HEADER_LEN];
        file.read_exact(&mut header)?;
        if !parse_istl_header(&header) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file does not start with ISTL magic",
            ));
        }
        let mut count_buf = [0u8; 4];
        file.read_exact(&mut count_buf)?;
        let triangle_count = u32::from_le_bytes(count_buf);

        // Footer is self-locating from the tail (Parquet-style):
        //   last 8 B = TRAILER magic; preceding 8 B = footer_size (u64 LE).
        if file_len < (STL_HEADER_LEN + 4 + TAIL_LEN) as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file too short to contain header + tail",
            ));
        }
        let mut tail = [0u8; TAIL_LEN];
        file.seek(SeekFrom::Start(file_len - TAIL_LEN as u64))?;
        file.read_exact(&mut tail)?;
        let footer_size = parse_tail(&tail).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "missing ISTL trailer at end of file",
            )
        })?;

        // Footer payload sits immediately before the tail.
        let footer_offset = file_len
            .checked_sub(TAIL_LEN as u64 + footer_size)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "footer_size larger than file",
                )
            })?;
        if footer_offset < (STL_HEADER_LEN + 4) as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "footer would overlap header",
            ));
        }

        // Read the footer payload in one shot, then decode in memory. This is
        // both faster on local disk (one large read) and the canonical pattern
        // for the remote case (one range request).
        let mut payload = vec![0u8; footer_size as usize];
        read_exact_at(&file, &mut payload, footer_offset)?;
        let index = Index::decode(&mut Cursor::new(&payload))?;
        if index.encoded_size() != footer_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "tail footer_size ({}) disagrees with decoded index size ({})",
                    footer_size,
                    index.encoded_size()
                ),
            ));
        }

        Ok(Reader {
            file,
            index,
            footer_offset,
            footer_size,
            triangle_count,
        })
    }

    /// Write a vanilla binary STL containing every triangle from every row group
    /// whose bbox intersects `query_bbox`. Use `Bbox::UNIVERSE` (or `±∞` on any
    /// axis) to disable filtering on that axis.
    pub fn query<P: AsRef<Path>>(
        &self,
        query_bbox: &Bbox,
        out_path: P,
    ) -> io::Result<QueryStats> {
        let matched: Vec<&RowGroup> = self
            .index
            .groups
            .iter()
            .filter(|g| g.bbox.intersects(query_bbox))
            .collect();

        let triangles_written: u64 = matched.iter().map(|g| g.triangle_count()).sum();
        let tri_count_u32: u32 = triangles_written
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "result > 2^32 triangles"))?;

        let out_file = File::create(out_path.as_ref())?;
        let mut out = BufWriter::with_capacity(8 * 1024 * 1024, out_file);
        // Standard 80-byte zero STL header — NOT istl-magic, so the result is a
        // plain binary STL consumable by any STL reader.
        out.write_all(&[0u8; STL_HEADER_LEN])?;
        out.write_all(&tri_count_u32.to_le_bytes())?;

        let mut buf = vec![0u8; 4 * 1024 * 1024];
        for g in &matched {
            let mut remaining = g.byte_size;
            let mut offset = g.byte_start;
            while remaining > 0 {
                let n = remaining.min(buf.len() as u64) as usize;
                read_exact_at(&self.file, &mut buf[..n], offset)?;
                out.write_all(&buf[..n])?;
                offset += n as u64;
                remaining -= n as u64;
            }
        }
        out.flush()?;

        Ok(QueryStats {
            groups_matched: matched.len() as u64,
            triangles_written,
        })
    }
}

/// Convenience for callers that don't want to instantiate a Reader.
pub fn read_index<P: AsRef<Path>>(path: P) -> io::Result<Index> {
    Ok(Reader::open(path)?.index)
}

#[cfg(unix)]
fn read_exact_at(f: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    f.read_exact_at(buf, offset)
}

#[cfg(not(unix))]
fn read_exact_at(f: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = f.try_clone()?;
    f.seek(SeekFrom::Start(offset))?;
    f.read_exact(buf)
}

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
#[cfg(unix)]
use std::os::unix::fs::FileExt;

use crate::format::{
    bbox_intersects, parse_istl_header, Index, RowGroup, STL_HEADER_LEN,
};

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
        let mut header = [0u8; STL_HEADER_LEN];
        file.read_exact(&mut header)?;
        let info = parse_istl_header(&header).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "file does not start with ISTL magic",
            )
        })?;
        let mut count_buf = [0u8; 4];
        file.read_exact(&mut count_buf)?;
        let triangle_count = u32::from_le_bytes(count_buf);
        file.seek(SeekFrom::Start(info.footer_offset))?;
        let mut footer_reader = std::io::BufReader::new(&file);
        let index = Index::decode(&mut footer_reader)?;
        // Sanity-check the recorded footer_size against the actually-encoded
        // index size. Mismatch ⇒ either a corrupted file or a stale writer.
        let encoded = index.encoded_size();
        if info.footer_size != encoded {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "footer_size in header ({}) disagrees with decoded index size ({})",
                    info.footer_size, encoded
                ),
            ));
        }
        Ok(Reader {
            file,
            index,
            footer_offset: info.footer_offset,
            footer_size: info.footer_size,
            triangle_count,
        })
    }

    /// Write a vanilla binary STL containing every triangle from every row group
    /// whose bbox intersects the query bbox `[xmin, xmax, ymin, ymax, zmin, zmax]`.
    pub fn query<P: AsRef<Path>>(
        &self,
        query_bbox: &[f32; 6],
        out_path: P,
    ) -> io::Result<QueryStats> {
        let matched: Vec<&RowGroup> = self
            .index
            .groups
            .iter()
            .filter(|g| bbox_intersects(&g.bbox, query_bbox))
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


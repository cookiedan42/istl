use std::fs::File;
use std::io::{self, BufWriter, Cursor, Write};
use std::path::Path;

use crate::format::{
    Bbox, Index, RowGroup, STL_HEADER_LEN, TAIL_LEN, make_istl_header, make_tail,
    parse_istl_header, parse_tail,
};
use crate::source::{FileSource, RangeReader};

#[derive(Debug, Clone, Copy)]
pub struct QueryStats {
    pub groups_matched: u64,
    pub triangles_written: u64,
}

/// Reader over any [`RangeReader`] source — local file, S3, in-memory, …
///
/// Construction reads the head magic (24 B) and the 16-byte tail to locate
/// the footer, then fetches the footer payload in one `read_at`. On S3 that's
/// typically two range GETs (front + back); a client that already grabbed a
/// speculative tail can construct a `BytesSource` over that buffer to skip the
/// second GET entirely.
pub struct Reader<R: RangeReader> {
    source: R,
    pub index: Index,
    pub footer_offset: u64,
    pub footer_size: u64,
    pub triangle_count: u32,
}

impl Reader<FileSource> {
    /// Convenience: open a local `.istl` file. Equivalent to
    /// `Reader::open_source(FileSource::open(path)?)`.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Reader::open_source(FileSource::open(path)?)
    }
}

impl<R: RangeReader> Reader<R> {
    /// Open a `Reader` over an arbitrary `RangeReader` source.
    pub fn open_source(source: R) -> io::Result<Self> {
        let file_len = source.len();
        if file_len < (STL_HEADER_LEN + 4 + TAIL_LEN) as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file too short to contain header + tail",
            ));
        }

        // Header magic + STL triangle count (84 bytes from the front).
        let mut head = [0u8; STL_HEADER_LEN + 4];
        source.read_at(0, &mut head)?;
        if !parse_istl_header(&head[..STL_HEADER_LEN]) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file does not start with ISTL magic",
            ));
        }
        let triangle_count = u32::from_le_bytes(
            head[STL_HEADER_LEN..STL_HEADER_LEN + 4].try_into().unwrap(),
        );

        // Footer is self-locating from the tail (Parquet-style):
        //   last 8 B = TRAILER magic; preceding 8 B = footer_size (u64 LE).
        let mut tail = [0u8; TAIL_LEN];
        source.read_at(file_len - TAIL_LEN as u64, &mut tail)?;
        let footer_size = parse_tail(&tail).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "missing ISTL trailer at end of file",
            )
        })?;

        // Footer payload sits immediately before the 16-byte tail.
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

        let payload = source.read_vec(footer_offset, footer_size as usize)?;
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
            source,
            index,
            footer_offset,
            footer_size,
            triangle_count,
        })
    }

    /// Borrow the underlying source — useful for callers that want to issue
    /// extra `read_at` calls of their own without re-opening.
    pub fn source(&self) -> &R {
        &self.source
    }

    /// Filter the index to the groups whose 3D bbox intersects `query_bbox`.
    pub fn matched_groups<'a>(&'a self, query_bbox: &Bbox) -> Vec<&'a RowGroup> {
        self.index
            .groups
            .iter()
            .filter(|g| g.bbox.intersects(query_bbox))
            .collect()
    }

    /// Write a vanilla binary STL containing every triangle from every row group
    /// whose bbox intersects `query_bbox`, into the given `Write`.
    ///
    /// Use `Bbox::UNIVERSE` (or `±∞` on any axis) to disable filtering.
    pub fn query_to_writer<W: Write + ?Sized>(
        &self,
        query_bbox: &Bbox,
        out: &mut W,
    ) -> io::Result<QueryStats> {
        let matched = self.matched_groups(query_bbox);
        let triangles_written: u64 = matched.iter().map(|g| g.triangle_count()).sum();
        let tri_count_u32: u32 = triangles_written.try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "result > 2^32 triangles")
        })?;

        // Plain 80-byte zero STL header — keeps the result a vanilla binary STL.
        out.write_all(&[0u8; STL_HEADER_LEN])?;
        out.write_all(&tri_count_u32.to_le_bytes())?;

        let mut buf = vec![0u8; 4 * 1024 * 1024];
        for g in &matched {
            self.stream_group(g, &mut buf, out)?;
        }

        Ok(QueryStats {
            groups_matched: matched.len() as u64,
            triangles_written,
        })
    }

    /// Write a valid `.istl` file containing every triangle from every row
    /// group whose bbox intersects `query_bbox`, into the given `Write`.
    ///
    /// Matched row groups are streamed verbatim (no triangle decode, no
    /// re-Hilbert). The rebuilt footer carries new contiguous `byte_start`
    /// offsets but the same `byte_size` and `bbox` as the source groups.
    /// Triangle grouping is therefore inherited from the source — the output
    /// is a valid ISTL whose per-group bboxes are exactly those of the
    /// matched subset.
    pub fn query_to_writer_istl<W: Write + ?Sized>(
        &self,
        query_bbox: &Bbox,
        out: &mut W,
    ) -> io::Result<QueryStats> {
        let matched = self.matched_groups(query_bbox);
        let triangles_written: u64 = matched.iter().map(|g| g.triangle_count()).sum();
        let tri_count_u32: u32 = triangles_written.try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "result > 2^32 triangles")
        })?;

        out.write_all(&make_istl_header())?;
        out.write_all(&tri_count_u32.to_le_bytes())?;

        let mut cur_offset: u64 = (STL_HEADER_LEN + 4) as u64;
        let mut groups: Vec<RowGroup> = Vec::with_capacity(matched.len());
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        for g in &matched {
            let byte_start = cur_offset;
            self.stream_group(g, &mut buf, out)?;
            cur_offset += g.byte_size;
            groups.push(RowGroup {
                byte_start,
                byte_size: g.byte_size,
                bbox: g.bbox,
            });
        }

        debug_assert_eq!(
            cur_offset,
            (STL_HEADER_LEN + 4) as u64
                + matched.iter().map(|g| g.byte_size).sum::<u64>()
        );

        let index = Index { groups };
        let footer_size = index.encoded_size();
        index.encode(out)?;
        out.write_all(&make_tail(footer_size))?;

        Ok(QueryStats {
            groups_matched: matched.len() as u64,
            triangles_written,
        })
    }

    /// Sugar: write the matched STL to a path on disk.
    pub fn query<P: AsRef<Path>>(
        &self,
        query_bbox: &Bbox,
        out_path: P,
    ) -> io::Result<QueryStats> {
        let f = File::create(out_path.as_ref())?;
        let mut out = BufWriter::with_capacity(8 * 1024 * 1024, f);
        let stats = self.query_to_writer(query_bbox, &mut out)?;
        out.flush()?;
        Ok(stats)
    }

    /// Sugar: write the matched ISTL to a path on disk.
    pub fn query_istl<P: AsRef<Path>>(
        &self,
        query_bbox: &Bbox,
        out_path: P,
    ) -> io::Result<QueryStats> {
        let f = File::create(out_path.as_ref())?;
        let mut out = BufWriter::with_capacity(8 * 1024 * 1024, f);
        let stats = self.query_to_writer_istl(query_bbox, &mut out)?;
        out.flush()?;
        Ok(stats)
    }

    /// Stream a single row group's triangle bytes from `self.source` into `out`,
    /// reusing the caller's scratch `buf`.
    fn stream_group<W: Write + ?Sized>(
        &self,
        g: &RowGroup,
        buf: &mut [u8],
        out: &mut W,
    ) -> io::Result<()> {
        let mut remaining = g.byte_size;
        let mut offset = g.byte_start;
        while remaining > 0 {
            let n = remaining.min(buf.len() as u64) as usize;
            self.source.read_at(offset, &mut buf[..n])?;
            out.write_all(&buf[..n])?;
            offset += n as u64;
            remaining -= n as u64;
        }
        Ok(())
    }
}

/// Convenience for callers that just want the index from a local file.
pub fn read_index<P: AsRef<Path>>(path: P) -> io::Result<Index> {
    Ok(Reader::open(path)?.index)
}

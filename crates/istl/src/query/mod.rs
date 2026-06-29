use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use crate::format::{
    Bbox, Index, IstlFormat, IstlTrait, MIN_ISTL_SIZE, RowGroup, STL_HEADER_LEN, StlFormat,
    TAIL_LEN, make_tail, parse_tail,
};
use crate::source::{FileSource, RangeReader};

/// Default size of the speculative tail fetch issued by [`Reader::open_source`].
///
/// 64 KB is large enough that the entire footer payload fits in one round trip
/// for any realistic file (1,632 row groups at the default `group_size = 32_768`
/// → 66 KB footer; that's already ~54 M triangles). Smaller hints cost extra
/// round trips on huge indexes; larger hints waste bandwidth on tiny ones.
pub const DEFAULT_TAIL_HINT: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct QueryStats {
    pub groups_matched: u64,
    pub triangles_written: u64,
}

/// Reader over any [`RangeReader`] source — local file, S3, in-memory, …
///
/// Construction issues one [`RangeReader::prefetch_tail`] for the last
/// [`DEFAULT_TAIL_HINT`] bytes. The 16-byte trailer at the end of that buffer
/// locates the footer; in the common case the footer payload already lives
/// inside the same buffer and no further I/O is needed. Only when the footer
/// is larger than the hint does construction issue a second precise `read_at`.
///
/// On S3-style backends that means one range GET to open (versus the previous
/// HEAD + 3 GETs). The head identifier is no longer consulted at open — the
/// trailer plus the `footer_size == index.encoded_size()` cross-check are what
/// validate the file.
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
    /// Open a `Reader` over an arbitrary `RangeReader` source using the default
    /// tail hint ([`DEFAULT_TAIL_HINT`]).
    pub fn open_source(source: R) -> io::Result<Self> {
        Self::open_source_with_tail_hint(source, DEFAULT_TAIL_HINT)
    }

    /// `open_source` with a caller-chosen speculative-tail size. Useful for
    /// callers that want to tune the trade-off (tiny files want a smaller
    /// hint, very-large indexes want a bigger one to avoid the follow-up GET).
    pub fn open_source_with_tail_hint(source: R, tail_hint: u64) -> io::Result<Self> {
        let hint = tail_hint.max(TAIL_LEN as u64);
        let (buf, file_len) = source.prefetch_tail(hint)?;
        if file_len < MIN_ISTL_SIZE as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file too short to contain header + tail",
            ));
        }
        if (buf.len() as u64) < TAIL_LEN as u64 || (buf.len() as u64) > file_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "prefetch_tail returned a buffer with an unexpected size",
            ));
        }

        // Footer is self-locating from the tail (Parquet-style):
        //   last 8 B = TRAILER identifier; preceding 8 B = footer_size (u64 LE).
        let tail_start_in_buf = buf.len() - TAIL_LEN;
        let footer_size = parse_tail(&buf[tail_start_in_buf..]).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "missing ISTL trailer at end of file",
            )
        })?;

        // Footer payload sits immediately before the 16-byte tail.
        let footer_offset = file_len
            .checked_sub(TAIL_LEN as u64 + footer_size)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "footer_size larger than file")
            })?;
        if footer_offset < (STL_HEADER_LEN + 4) as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "footer would overlap header",
            ));
        }

        // Common case: the speculative tail already covers the whole footer.
        // Slice out of `buf`; fall back to one precise GET only if it doesn't.
        let need = (footer_size as usize).saturating_add(TAIL_LEN);
        let payload: Vec<u8> = if buf.len() >= need {
            let start = buf.len() - need;
            buf[start..start + footer_size as usize].to_vec()
        } else {
            source.read_vec(footer_offset, footer_size as usize)?
        };

        let index = Index::decode(&payload)?;
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
        let triangle_count = index.triangle_count_u32()?;

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
}

// write methods
impl<R: RangeReader> Reader<R> {
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
        let tri_count_u32: u32 = triangles_written
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "result > 2^32 triangles"))?;

        out.write_all(StlFormat::HEADER)?;
        out.write_all(&tri_count_u32.to_le_bytes())?;

        let ranges: Vec<(u64, u64)> =
            matched.iter().map(|g| (g.byte_start, g.byte_size)).collect();
        self.source.read_to_writer_batch(&ranges, out)?;

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
        let tri_count_u32: u32 = triangles_written
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "result > 2^32 triangles"))?;

        out.write_all(IstlFormat::HEADER)?;
        out.write_all(&tri_count_u32.to_le_bytes())?;

        let offset_head: u64 = (STL_HEADER_LEN + 4) as u64;

        // Rebuilt RowGroups carry the SAME byte_size and bbox as the source
        // groups but contiguous byte_starts beginning right after the header.
        // Computed off the read path so the batch read below can fan out.
        let mut cur_offset: u64 = offset_head;
        let mut groups: Vec<RowGroup> = Vec::with_capacity(matched.len());
        let mut ranges: Vec<(u64, u64)> = Vec::with_capacity(matched.len());
        for g in &matched {
            groups.push(RowGroup {
                byte_start: cur_offset,
                byte_size: g.byte_size,
                bbox: g.bbox,
            });
            ranges.push((g.byte_start, g.byte_size));
            cur_offset += g.byte_size;
        }
        debug_assert_eq!(
            cur_offset,
            (STL_HEADER_LEN + 4) as u64 + matched.iter().map(|g| g.byte_size).sum::<u64>()
        );

        self.source.read_to_writer_batch(&ranges, out)?;

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
    pub fn query<P: AsRef<Path>>(&self, query_bbox: &Bbox, out_path: P) -> io::Result<QueryStats> {
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
}

//! Convert a plain binary STL to an indexed binary STL.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::format::{
    Bbox, Index, IstlFormat, IstlTrait, RowGroup, STL_HEADER_LEN, STL_NUM_TRIANGLES_SIZE, TRI_SIZE,
    Triangle, make_tail, parse_istl_header,
};
use crate::hilbert;

#[derive(Debug, Clone, Copy)]
pub struct ConvertStats {
    pub triangles: u64,
    pub groups: u64,
    pub dataset_bbox: Bbox,
    pub footer_offset: u64,
    pub footer_size: u64,
}

/// Hilbert grid side = 2^16. Anything finer doesn't fit in a u32 key.
const HILBERT_ORDER: u32 = 16;
const HILBERT_SIDE: u32 = 1u32 << HILBERT_ORDER;
const ONE_MB: usize = 1024 * 1024;

/// Default triangles per row group.
///
/// At 50 B/triangle this is ~1.6 MB per group, which sits at the start of the
/// efficient parallel-range-GET band for object stores (S3, GCS, Azure Blob),
/// while keeping the footer small enough (~2 KB per 1,000 groups) to fit in a
/// single ~64-96 KB speculative tail GET. For terrain-scale data (~50 M
/// triangles) this yields ~1.5 K groups and a ~60 KB footer.
///
/// Tune per deployment:
/// - Local NVMe / mmap'd reads: 8,192 (smaller, tighter bboxes).
/// - HDD / NFS / EFS: 131,072+ (fewer seeks).
/// - High-latency edge / generic HTTP: 131,072+ (overhead dominates).
/// - S3 Express One Zone: 16,384 (lower TTFB pays back smaller groups).
///
/// The format itself doesn't care about this number — it's a writer-side knob.
pub const DEFAULT_GROUP_SIZE: usize = 32_768;

/// Convert a plain binary STL to an indexed binary STL.
pub fn convert<P: AsRef<Path>, Q: AsRef<Path>>(
    input: P,
    output: Q,
    group_size: usize,
) -> io::Result<ConvertStats> {
    if group_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "group_size must be > 0",
        ));
    }

    let mut in_file = File::open(input.as_ref())?;
    let mut header = [0u8; STL_HEADER_LEN];
    in_file.read_exact(&mut header)?;

    if parse_istl_header(&header) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "input already starts with ISTL identifier; refusing to double-convert",
        ));
    }

    let mut count_buf = [0u8; STL_NUM_TRIANGLES_SIZE];
    in_file.read_exact(&mut count_buf)?;
    let n_triangles = u32::from_le_bytes(count_buf) as usize;

    // Pass 1: stream every triangle, collect first-vertex XY for the sort key,
    // and accumulate the full dataset bbox over all 3 vertices.
    let mut first_xy: Vec<[f32; 2]> = Vec::with_capacity(n_triangles);
    let mut bbox = Bbox::EMPTY;

    let mut reader = BufReader::with_capacity(ONE_MB, in_file);
    let mut tri = [0u8; TRI_SIZE];
    for _ in 0..n_triangles {
        reader.read_exact(&mut tri)?;
        let tri = Triangle::ref_from(&tri);
        first_xy.push(tri.first_xy());
        tri.fold_triangle(&mut bbox);
    }
    drop(reader);

    // Build sort keys and free the (x,y) buffer.
    let mut keys: Vec<(u32, u32)> = Vec::with_capacity(n_triangles);
    for (i, &[x, y]) in first_xy.iter().enumerate() {
        let qx = hilbert::normalize(x, bbox.xmin, bbox.xmax, HILBERT_SIDE);
        let qy = hilbert::normalize(y, bbox.ymin, bbox.ymax, HILBERT_SIDE);
        keys.push((hilbert::xy2d(HILBERT_ORDER, qx, qy), i as u32));
    }
    drop(first_xy);
    keys.sort_unstable_by_key(|p| p.0);

    // Pass 2: random reads in sorted order, into a freshly-created output.
    let in_file = File::open(input.as_ref())?;
    let out_file = File::create(output.as_ref())?;
    let mut out = BufWriter::with_capacity(ONE_MB, out_file);

    // Header is identifier only — footer is self-locating from the tail.
    out.write_all(IstlFormat::HEADER)?;
    out.write_all(&(n_triangles as u32).to_le_bytes())?;
    let mut cur_offset: u64 = (STL_HEADER_LEN + 4) as u64;

    let mut groups: Vec<RowGroup> = Vec::with_capacity(n_triangles.div_ceil(group_size));
    let mut buf = [0u8; TRI_SIZE];

    // write triangles and build groups
    for chunk in keys.chunks(group_size) {
        let byte_start = cur_offset;
        let mut g_bbox = Bbox::EMPTY;
        for &(_, idx) in chunk {
            let src_off = (STL_HEADER_LEN + 4) as u64 + (idx as u64) * TRI_SIZE as u64;

            read_exact_at(&in_file, &mut buf, src_off)?;
            let tri = Triangle::ref_from(&buf);
            tri.fold_triangle(&mut g_bbox);
            out.write_all(&buf)?;
        }
        let byte_size = chunk.len() as u64 * TRI_SIZE as u64;
        cur_offset += byte_size;
        groups.push(RowGroup {
            byte_start,
            byte_size,
            bbox: g_bbox,
        });
    }

    let footer_offset = cur_offset;
    let index = Index { groups };
    let group_count = index.groups.len() as u64;
    let footer_size = index.encoded_size();

    // write index and footer
    index.encode(&mut out)?;
    out.write_all(&make_tail(footer_size))?;
    out.flush()?;

    Ok(ConvertStats {
        triangles: n_triangles as u64,
        groups: group_count,
        dataset_bbox: bbox,
        footer_offset,
        footer_size,
    })
}

#[cfg(unix)]
fn read_exact_at(f: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    f.read_exact_at(buf, offset)
}

#[cfg(not(unix))]
fn read_exact_at(f: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = f;
    f.seek(SeekFrom::Start(offset))?;
    f.read_exact(buf)
}

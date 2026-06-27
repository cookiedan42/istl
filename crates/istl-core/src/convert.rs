use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::format::{make_istl_header, Index, RowGroup, MAGIC, STL_HEADER_LEN, TRI_SIZE};
use crate::hilbert;

#[derive(Debug, Clone, Copy)]
pub struct ConvertStats {
    pub triangles: u64,
    pub groups: u64,
    /// [xmin, xmax, ymin, ymax, zmin, zmax]
    pub dataset_bbox: [f32; 6],
    pub footer_offset: u64,
    pub footer_size: u64,
}

#[inline]
fn read_f32(b: &[u8]) -> f32 {
    f32::from_le_bytes(b.try_into().expect("4-byte slice"))
}

/// Hilbert grid side = 2^16. Anything finer doesn't fit in a u32 key.
const HILBERT_ORDER: u32 = 16;
const HILBERT_SIDE: u32 = 1u32 << HILBERT_ORDER;

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
    if &header[0..8] == MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "input already starts with ISTL magic; refusing to double-convert",
        ));
    }
    let mut count_buf = [0u8; 4];
    in_file.read_exact(&mut count_buf)?;
    let n_triangles = u32::from_le_bytes(count_buf) as usize;

    // Pass 1: stream every triangle, collect first-vertex XY for the sort key,
    // and accumulate the full dataset bbox over all 3 vertices.
    let mut first_xy: Vec<(f32, f32)> = Vec::with_capacity(n_triangles);
    let mut bbox = [
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
    ];

    let mut reader = BufReader::with_capacity(8 * 1024 * 1024, in_file);
    let mut tri = [0u8; TRI_SIZE];
    for _ in 0..n_triangles {
        reader.read_exact(&mut tri)?;
        first_xy.push((read_f32(&tri[12..16]), read_f32(&tri[16..20])));
        fold_bbox(&mut bbox, &tri);
    }
    drop(reader);

    // Build sort keys and free the (x,y) buffer.
    let mut keys: Vec<(u32, u32)> = Vec::with_capacity(n_triangles);
    for (i, &(x, y)) in first_xy.iter().enumerate() {
        let qx = hilbert::normalize(x, bbox[0], bbox[1], HILBERT_SIDE);
        let qy = hilbert::normalize(y, bbox[2], bbox[3], HILBERT_SIDE);
        keys.push((hilbert::xy2d(HILBERT_ORDER, qx, qy), i as u32));
    }
    drop(first_xy);
    keys.sort_unstable_by_key(|p| p.0);

    // Pass 2: random reads in sorted order, into a freshly-created output.
    let in_file = File::open(input.as_ref())?;
    let out_file = File::create(output.as_ref())?;
    let mut out = BufWriter::with_capacity(8 * 1024 * 1024, out_file);

    // Placeholder header — footer_offset / footer_size patched in at the end.
    out.write_all(&make_istl_header(0, 0))?;
    out.write_all(&(n_triangles as u32).to_le_bytes())?;
    let mut cur_offset: u64 = (STL_HEADER_LEN + 4) as u64;

    let mut groups: Vec<RowGroup> = Vec::with_capacity(n_triangles.div_ceil(group_size));
    let mut buf = [0u8; TRI_SIZE];

    for chunk in keys.chunks(group_size) {
        let byte_start = cur_offset;
        let mut g_bbox = [
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ];
        for &(_, idx) in chunk {
            let src_off = (STL_HEADER_LEN + 4) as u64 + (idx as u64) * TRI_SIZE as u64;
            read_exact_at(&in_file, &mut buf, src_off)?;
            fold_bbox(&mut g_bbox, &buf);
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
    index.encode(&mut out)?;
    out.flush()?;

    // Patch footer_offset (8..16) and footer_size (16..24) into the header.
    let mut out_file = out.into_inner().map_err(|e| e.into_error())?;
    out_file.seek(SeekFrom::Start(8))?;
    out_file.write_all(&footer_offset.to_le_bytes())?;
    out_file.write_all(&footer_size.to_le_bytes())?;
    out_file.flush()?;

    Ok(ConvertStats {
        triangles: n_triangles as u64,
        groups: group_count,
        dataset_bbox: bbox,
        footer_offset,
        footer_size,
    })
}

fn fold_bbox(bbox: &mut [f32; 6], tri: &[u8; TRI_SIZE]) {
    for v in 0..3 {
        let off = 12 + v * 12;
        let vx = read_f32(&tri[off..off + 4]);
        let vy = read_f32(&tri[off + 4..off + 8]);
        let vz = read_f32(&tri[off + 8..off + 12]);
        if vx < bbox[0] {
            bbox[0] = vx;
        }
        if vx > bbox[1] {
            bbox[1] = vx;
        }
        if vy < bbox[2] {
            bbox[2] = vy;
        }
        if vy > bbox[3] {
            bbox[3] = vy;
        }
        if vz < bbox[4] {
            bbox[4] = vz;
        }
        if vz > bbox[5] {
            bbox[5] = vz;
        }
    }
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

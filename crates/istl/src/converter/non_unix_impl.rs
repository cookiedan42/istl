use super::{ConvertStats, HEAD_LEN, HILBERT_ORDER, HILBERT_SIDE};
use std::fs::File;
use std::io::{self, Read};
use std::io::{BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use crate::format::{Bbox, Index, IstlFormat, IstlTrait, RowGroup, TRI_SIZE, Triangle, make_tail};
use crate::hilbert;

const ONE_MB: usize = 1024 * 1024;

pub(crate) fn convert_impl(
    input: &Path,
    output: &Path,
    n_triangles: usize,
    group_size: usize,
) -> io::Result<ConvertStats> {
    // Non-Unix fallback: serial seek+read+write. mmap isn't available portably
    // in the form we use here, and the Unix path is the supported deployment.
    let mut in_file = File::open(input)?;
    in_file.seek(SeekFrom::Start(HEAD_LEN as u64))?;
    let mut reader = BufReader::with_capacity(ONE_MB, in_file);

    let mut first_xy: Vec<[f32; 2]> = Vec::with_capacity(n_triangles);
    let mut bbox = Bbox::EMPTY;
    let mut tri = [0u8; TRI_SIZE];
    for _ in 0..n_triangles {
        reader.read_exact(&mut tri)?;
        let t = Triangle::ref_from(&tri);
        first_xy.push(t.first_xy());
        t.fold_triangle(&mut bbox);
    }
    drop(reader);

    let mut keys: Vec<(u32, u32)> = first_xy
        .iter()
        .enumerate()
        .map(|(i, &[x, y])| {
            let qx = hilbert::normalize(x, bbox.xmin, bbox.xmax, HILBERT_SIDE);
            let qy = hilbert::normalize(y, bbox.ymin, bbox.ymax, HILBERT_SIDE);
            (hilbert::xy2d(HILBERT_ORDER, qx, qy), i as u32)
        })
        .collect();
    drop(first_xy);
    keys.sort_unstable_by_key(|p| p.0);

    let mut in_file = File::open(input)?;
    let out_file = File::create(output)?;
    let mut out = BufWriter::with_capacity(ONE_MB, out_file);

    out.write_all(IstlFormat::HEADER)?;
    out.write_all(&(n_triangles as u32).to_le_bytes())?;
    let mut cur_offset: u64 = HEAD_LEN as u64;
    let mut groups: Vec<RowGroup> = Vec::with_capacity(n_triangles.div_ceil(group_size));
    let mut buf = [0u8; TRI_SIZE];
    for chunk in keys.chunks(group_size) {
        let byte_start = cur_offset;
        let mut g_bbox = Bbox::EMPTY;
        for &(_, idx) in chunk {
            let src_off = HEAD_LEN as u64 + (idx as u64) * TRI_SIZE as u64;
            in_file.seek(SeekFrom::Start(src_off))?;
            in_file.read_exact(&mut buf)?;
            Triangle::ref_from(&buf).fold_triangle(&mut g_bbox);
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

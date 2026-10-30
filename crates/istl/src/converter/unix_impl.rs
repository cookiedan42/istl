use super::{ConvertStats, HEAD_LEN, HILBERT_ORDER, HILBERT_SIDE};
use crate::format::{
    Bbox, Index, IstlFormat, IstlTrait, RowGroup, STL_HEADER_LEN, TRI_SIZE, Triangle, make_tail,
};
use crate::hilbert;
use rayon::prelude::*;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

pub(crate) fn convert_impl(
    input: &Path,
    output: &Path,
    n_triangles: usize,
    group_size: usize,
) -> io::Result<ConvertStats> {
    let in_file = File::open(input)?;
    // SAFETY: input is opened read-only and not mutated by us; concurrent
    // external truncation could produce torn reads but no UB beyond what any
    // mmap of a shared file implies.
    let in_map = unsafe { memmap2::Mmap::map(&in_file)? };
    #[cfg(target_os = "linux")]
    let _ = in_map.advise(memmap2::Advice::WillNeed);

    let triangles_end = HEAD_LEN + n_triangles * TRI_SIZE;
    if in_map.len() < triangles_end {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "input file too small: header claims {n_triangles} triangles ({triangles_end} B) but only {} B available",
                in_map.len()
            ),
        ));
    }
    let in_triangles: &[u8] = &in_map[HEAD_LEN..triangles_end];

    // Pass 1: parallel reduction over input triangles. Each chunk collects
    // its slice of first-vertex XY pairs (for later Hilbert quantisation) and
    // folds its own bbox; we tree-reduce the bboxes and concatenate the XY
    // slices in-order at the join.
    let (first_xy, dataset_bbox) = pass1_parallel(in_triangles, n_triangles);

    let group_count = n_triangles.div_ceil(group_size);
    let groups_bytes = (n_triangles as u64) * TRI_SIZE as u64;
    let footer_payload_size = 4 + (group_count as u64) * RowGroup::record_size() as u64;
    let total_size = HEAD_LEN as u64 + groups_bytes + footer_payload_size + 16;

    // Pre-allocate the output and mmap it. Pass 2 then writes each group's
    // slab into a disjoint slice of the mapping in parallel; the footer is
    // written serially at the end (it depends on the per-group bboxes).
    let out_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(output)?;
    out_file.set_len(total_size)?;
    // SAFETY: we own the file, hold the only mmap reference, and disjoint
    // writers index into non-overlapping byte ranges (one per output group).
    let mut out_map = unsafe { memmap2::MmapMut::map_mut(&out_file)? };

    // Build sort keys (parallel quantisation) and sort. We reuse the
    // `first_xy` allocation by mapping each (f32,f32) slot to a (u32,u32) key
    // in-place via par_iter_mut, avoiding the previous 432 MB double-alloc.
    debug_assert_eq!(std::mem::size_of::<(f32, f32)>(), 8);
    debug_assert_eq!(std::mem::size_of::<(u32, u32)>(), 8);
    let mut xy = first_xy;
    let bbox = dataset_bbox;
    let keys: Vec<(u32, u32)> = xy
        .par_iter_mut()
        .enumerate()
        .map(|(i, (x, y))| {
            let qx = hilbert::normalize(*x, bbox.xmin, bbox.xmax, HILBERT_SIDE);
            let qy = hilbert::normalize(*y, bbox.ymin, bbox.ymax, HILBERT_SIDE);
            (hilbert::xy2d(HILBERT_ORDER, qx, qy), i as u32)
        })
        .collect();
    drop(xy);
    let mut keys = keys;
    keys.par_sort_unstable_by_key(|p| p.0);

    // Write head: identifier + STL triangle count.
    out_map[..STL_HEADER_LEN].copy_from_slice(IstlFormat::HEADER);
    out_map[STL_HEADER_LEN..HEAD_LEN].copy_from_slice(&(n_triangles as u32).to_le_bytes());

    // Pass 2: parallel per-group. Each group's slab is a contiguous
    // group_size * 50 B region of out_map. Within a group we sequentially
    // memcpy(50) from the input mmap (already paged in) and fold the bbox.
    let groups_region = &mut out_map[HEAD_LEN..HEAD_LEN + groups_bytes as usize];
    let group_byte_size = group_size * TRI_SIZE;
    let in_bytes: &[u8] = in_triangles;
    let groups: Vec<RowGroup> = groups_region
        .par_chunks_mut(group_byte_size)
        .zip(keys.par_chunks(group_size))
        .enumerate()
        .map(|(gi, (out_slab, chunk))| {
            let byte_start = HEAD_LEN as u64 + (gi as u64) * group_byte_size as u64;
            let mut g_bbox = Bbox::EMPTY;
            for (i, (_, idx)) in chunk.iter().enumerate() {
                let src_off = (*idx as usize) * TRI_SIZE;
                let src_arr: &[u8; TRI_SIZE] =
                    (&in_bytes[src_off..src_off + TRI_SIZE]).try_into().unwrap();
                let dst_off = i * TRI_SIZE;
                out_slab[dst_off..dst_off + TRI_SIZE].copy_from_slice(src_arr);
                Triangle::ref_from(src_arr).fold_triangle(&mut g_bbox);
            }
            RowGroup {
                byte_start,
                byte_size: chunk.len() as u64 * TRI_SIZE as u64,
                bbox: g_bbox,
            }
        })
        .collect();

    let footer_offset = HEAD_LEN as u64 + groups_bytes;
    let index = Index { groups };
    let footer_size = index.encoded_size();
    debug_assert_eq!(footer_size, footer_payload_size);

    // Footer + tail go into the tail of the mapping.
    let footer_region =
        &mut out_map[footer_offset as usize..(footer_offset + footer_size) as usize];
    let mut cursor = io::Cursor::new(footer_region);
    index.encode(&mut cursor)?;
    out_map[(footer_offset + footer_size) as usize..].copy_from_slice(&make_tail(footer_size));

    out_map.flush()?;
    drop(out_map);

    Ok(ConvertStats {
        triangles: n_triangles as u64,
        groups: group_count as u64,
        dataset_bbox: bbox,
        footer_offset,
        footer_size,
    })
}

/// Pass 1 reduction over a slice of triangle records: collect first-vertex
/// XY pairs and fold the dataset bbox. Parallel across rayon worker chunks;
/// the per-worker XY vecs are concatenated in input order to preserve the
/// `src_index` correspondence used by the sort.
fn pass1_parallel(bytes: &[u8], n_triangles: usize) -> (Vec<(f32, f32)>, Bbox) {
    // Chunk size in TRIANGLES (~1M = ~50 MB scan per worker) — big enough to
    // amortise the rayon split, small enough to keep cores busy.
    const CHUNK_TRIS: usize = 1 << 20;
    const CHUNK_BYTES: usize = CHUNK_TRIS * TRI_SIZE;

    let parts: Vec<(Vec<(f32, f32)>, Bbox)> = bytes
        .par_chunks(CHUNK_BYTES)
        .map(|recs_bytes| {
            let n = recs_bytes.len() / TRI_SIZE;
            let mut xy: Vec<(f32, f32)> = Vec::with_capacity(n);
            let mut b = Bbox::EMPTY;
            for i in 0..n {
                let r: &[u8; TRI_SIZE] = (&recs_bytes[i * TRI_SIZE..(i + 1) * TRI_SIZE])
                    .try_into()
                    .unwrap();
                let t = Triangle::ref_from(r);
                let [x, y] = t.first_xy();
                xy.push((x, y));
                t.fold_triangle(&mut b);
            }
            (xy, b)
        })
        .collect();

    let mut xy_all: Vec<(f32, f32)> = Vec::with_capacity(n_triangles);
    let mut bbox = Bbox::EMPTY;
    for (mut xy, b) in parts {
        xy_all.append(&mut xy);
        // Fold the partial bbox's two corners into the global one.
        bbox.fold_point(b.xmin, b.ymin, b.zmin);
        bbox.fold_point(b.xmax, b.ymax, b.zmax);
    }
    (xy_all, bbox)
}

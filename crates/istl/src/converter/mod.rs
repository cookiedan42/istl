//! Convert a plain binary STL to an indexed binary STL.

use crate::format::{Bbox, STL_HEADER_LEN, STL_NUM_TRIANGLES_SIZE, parse_istl_header};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

#[cfg(unix)]
mod unix_impl;
#[cfg(unix)]
use unix_impl::convert_impl;

#[cfg(not(unix))]
mod non_unix_impl;
#[cfg(not(unix))]
use non_unix_impl::convert_impl;

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

const HEAD_LEN: usize = STL_HEADER_LEN + 4;

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
    drop(in_file);

    convert_impl(input.as_ref(), output.as_ref(), n_triangles, group_size)
}

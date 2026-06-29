//! ISTL file format.
//! an indexed binary-STL format inspired by GeoParquet.
//! Triangles are Hilbert-sorted by their first vertex's XY and grouped into row groups;
//! The footer at the end of the file records each group's byte range and 3D bounding box.
//!
//! A reader can then return all triangles in row groups whose bbox intersects a query bbox
//! without scanning the whole mesh.
//!
//! ## On-disk layout
//!
//! ```text
//! bytes  0..8    : IDENTIFIER "ISTL1\0\0\0"           (head identifier: lets file(1)/local sniff identify istl)
//! bytes  8..80   : reserved (zeroed, 72 B)
//! bytes 80..84   : triangle_count u32 LE          (valid plain-STL count)
//! bytes 84..     : triangles (50 B each, Hilbert-sorted)
//! ...
//! footer payload : group_count u32 LE + group_count × 40 B records
//!                   { byte_start u64, byte_size u64, bbox (6 × f32) }
//! last 16 bytes  : footer_size u64 LE + TRAILER "ISTLEND\0"
//! ```
//!
//! The footer is **self-locating from the tail** (Parquet-style): the last 16 bytes
//! of the file contain the trailer identifier and the size of the footer payload. A reader
//! never needs to consult the header to find the index.
//!
//! Invariants:
//! - `footer_size = group_count × 40 + 4` (payload only, excludes the 16-byte tail).
//! - `file_size = 84 + 50 × triangle_count + footer_size + 16`.
//!
//! Remote-reader access pattern (S3/Lambda) is a single round-trip in the common case:
//! 1. `GET Range: bytes=-N` for some upper-bound `N` (e.g. 64 KB) — covers the trailer
//!    identifier, `footer_size`, the entire footer, and possibly some triangles. Verify the
//!    last 8 bytes are `TRAILER` and read `footer_size` from the 8 bytes before that.
//! 2. If `N < footer_size + 16` (large index), issue one precise follow-up GET. Pick a
//!    big-enough `N` and this never happens in practice.

mod triangle;
pub use triangle::{TRI_SIZE, Triangle};

mod bbox;
pub use bbox::Bbox;

mod row_group;
pub use row_group::RowGroup;

mod header;
pub use header::parse_istl_header;

use header::IDENTIFIER;

mod stl;
pub use stl::{STL_HEADER_LEN, STL_NUM_TRIANGLES_SIZE};

mod footer;
pub use footer::{Index, TAIL_LEN, make_tail, parse_tail};

pub const MIN_ISTL_SIZE: usize = STL_HEADER_LEN + 4 + footer::TAIL_LEN;

pub trait IstlTrait {
    const HEADER_LEN: usize = STL_HEADER_LEN;
    const NUM_TRIANGLES_SIZE: usize = STL_NUM_TRIANGLES_SIZE;
    const TRI_SIZE: usize = TRI_SIZE;

    const HEADER: &'static [u8; STL_HEADER_LEN];
}
pub(crate) struct StlFormat;
impl IstlTrait for StlFormat {
    const HEADER: &'static [u8; STL_HEADER_LEN] = &[0u8; STL_HEADER_LEN];
}

pub(crate) struct IstlFormat;
impl IstlTrait for IstlFormat {
    const HEADER: &'static [u8; STL_HEADER_LEN] = &{
        let mut h = [0u8; STL_HEADER_LEN];
        let mut i = 0;
        while i < IDENTIFIER.len() {
            h[i] = IDENTIFIER[i];
            i += 1;
        }
        h
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_group_byte_round_trip() {
        let g = RowGroup {
            byte_start: 84,
            byte_size: 50 * 17,
            bbox: Bbox::from([-1.0, 2.5, 0.0, 100.25, -50.0, 50.0]),
        };
        let bytes: [u8; RowGroup::record_size()] = g.into();
        assert_eq!(RowGroup::from(bytes), g);
    }

    #[test]
    fn index_round_trip() {
        let idx = Index {
            groups: vec![
                RowGroup {
                    byte_start: 84,
                    byte_size: 50 * 3,
                    bbox: Bbox::from([0.0, 1.0, 0.0, 1.0, 0.0, 1.0]),
                },
                RowGroup {
                    byte_start: 84 + 150,
                    byte_size: 50 * 7,
                    bbox: Bbox::from([1.0, 2.0, 1.0, 2.0, -1.0, 0.0]),
                },
            ],
        };
        let mut buf = Vec::new();
        idx.encode(&mut buf).unwrap();
        assert_eq!(buf.len() as u64, idx.encoded_size());
        let decoded = Index::decode(&buf).unwrap();
        assert_eq!(decoded, idx);
    }

    #[test]
    fn header_is_magic_only() {
        let h = IstlFormat::HEADER;
        assert!(parse_istl_header(h));
        assert!(h[8..].iter().all(|&b| b == 0));
        let mut bad: [u8; STL_HEADER_LEN] = *h;
        bad[0] = b'X';
        assert!(!parse_istl_header(&bad));
    }

    #[test]
    fn tail_round_trip() {
        let t = make_tail(123_456_789);
        assert_eq!(parse_tail(&t), Some(123_456_789));
        let mut bad = t;
        bad[15] = b'X';
        assert_eq!(parse_tail(&bad), None);
    }

    #[test]
    fn intersects_basic() {
        let a = Bbox::from([0.0, 1.0, 0.0, 1.0, 0.0, 1.0]);
        let b = Bbox::from([0.5, 1.5, 0.5, 1.5, 0.5, 1.5]);
        let c = Bbox::from([2.0, 3.0, 0.0, 1.0, 0.0, 1.0]);
        assert!(a.intersects(&b));
        assert!(!a.intersects(&c));
        let nan = Bbox::from([f32::NAN, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert!(!a.intersects(&nan));
    }

    #[test]
    fn fold_point_grows_from_empty() {
        let mut b = Bbox::EMPTY;
        b.fold_point(1.0, 2.0, -3.0);
        b.fold_point(-5.0, 10.0, 4.0);
        assert_eq!(
            b,
            Bbox {
                xmin: -5.0,
                xmax: 1.0,
                ymin: 2.0,
                ymax: 10.0,
                zmin: -3.0,
                zmax: 4.0,
            }
        );
    }

    #[test]
    fn fold_point_single_point_from_empty_is_degenerate_zero_extent() {
        // Folding ONE point into EMPTY must set min == max == point on every
        // axis. This depends on both branches of fold_point firing on the
        // bootstrap (since xmin = +∞ and xmax = -∞ both compare strictly
        // against any finite value). An `else if` would break this.
        let mut b = Bbox::EMPTY;
        b.fold_point(7.5, -2.0, 0.0);
        assert_eq!(
            b,
            Bbox {
                xmin: 7.5,
                xmax: 7.5,
                ymin: -2.0,
                ymax: -2.0,
                zmin: 0.0,
                zmax: 0.0,
            }
        );
        // The resulting bbox satisfies the invariant xmin ≤ xmax, etc.,
        // so it intersects itself (a degenerate point bbox is still valid).
        assert!(b.intersects(&b));
    }

    #[test]
    fn fold_point_inside_does_not_change_bbox() {
        let initial = Bbox::from([0.0, 10.0, 0.0, 10.0, -1.0, 1.0]);
        let mut b = initial;
        // Interior point — both branches must be false; no writes.
        b.fold_point(5.0, 5.0, 0.0);
        assert_eq!(b, initial);
        // Boundary points — strict `<` / `>` means equality is a no-op too.
        b.fold_point(0.0, 10.0, -1.0);
        b.fold_point(10.0, 0.0, 1.0);
        assert_eq!(b, initial);
    }

    #[test]
    fn fold_point_ignores_nan() {
        // NaN comparisons are always false, so a NaN coord is silently
        // dropped on the affected axis (other axes still fold normally).
        let mut b = Bbox::EMPTY;
        b.fold_point(f32::NAN, 1.0, 2.0);
        assert!(b.xmin.is_infinite() && b.xmax.is_infinite()); // x untouched
        assert_eq!(b.ymin, 1.0);
        assert_eq!(b.ymax, 1.0);
        assert_eq!(b.zmin, 2.0);
        assert_eq!(b.zmax, 2.0);
    }

    #[test]
    fn array_round_trip() {
        let a: [f32; 6] = [1.0, 2.0, -3.0, 4.0, 5.0, 6.0];
        assert_eq!(<[f32; 6]>::from(Bbox::from(a)), a);
    }
}

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
//! bytes  0..8    : MAGIC "ISTL1\0\0\0"           (head magic: lets file(1)/local sniff identify istl)
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
//! of the file contain the trailer magic and the size of the footer payload. A reader
//! never needs to consult the header to find the index.
//!
//! Invariants:
//! - `footer_size = group_count × 40 + 4` (payload only, excludes the 16-byte tail).
//! - `file_size = 84 + 50 × triangle_count + footer_size + 16`.
//!
//! Remote-reader access pattern (S3/Lambda) is a single round-trip in the common case:
//! 1. `GET Range: bytes=-N` for some upper-bound `N` (e.g. 64 KB) — covers the trailer
//!    magic, `footer_size`, the entire footer, and possibly some triangles. Verify the
//!    last 8 bytes are `TRAILER` and read `footer_size` from the 8 bytes before that.
//! 2. If `N < footer_size + 16` (large index), issue one precise follow-up GET. Pick a
//!    big-enough `N` and this never happens in practice.

use std::io::{self, Read, Write};

pub const MAGIC: &[u8; 8] = b"ISTL1\0\0\0";
pub const TRAILER: &[u8; 8] = b"ISTLEND\0";
pub const STL_HEADER_LEN: usize = 80; // STL standard
pub const TRI_SIZE: usize = 50; // bytes per triangle, from STL standard
pub const ROW_GROUP_RECORD_SIZE: usize = 8 + 8 + 4 * 6; // 40 bytes — see encode
/// Size of the trailing footer-size + magic block at end of file.
pub const TAIL_LEN: usize = 8 + 8;

/// Axis-aligned 3D bounding box. Layout matches `[f32; 6]` in order
/// `xmin, xmax, ymin, ymax, zmin, zmax`, so on-disk encoding is just six
/// little-endian floats.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct Bbox {
    pub xmin: f32,
    pub xmax: f32,
    pub ymin: f32,
    pub ymax: f32,
    pub zmin: f32,
    pub zmax: f32,
}

impl Bbox {
    /// Identity element for folding points: every `fold_point` strictly shrinks
    /// max-bounds and grows min-bounds, so starting from ±inf is correct.
    pub const EMPTY: Bbox = Bbox {
        xmin: f32::INFINITY,
        xmax: f32::NEG_INFINITY,
        ymin: f32::INFINITY,
        ymax: f32::NEG_INFINITY,
        zmin: f32::INFINITY,
        zmax: f32::NEG_INFINITY,
    };

    /// Bbox that covers all finite space — useful as a sentinel "no filter" query.
    pub const UNIVERSE: Bbox = Bbox {
        xmin: f32::NEG_INFINITY,
        xmax: f32::INFINITY,
        ymin: f32::NEG_INFINITY,
        ymax: f32::INFINITY,
        zmin: f32::NEG_INFINITY,
        zmax: f32::INFINITY,
    };

    /// Grow this bbox to include `(x, y, z)`.
    pub fn fold_point(&mut self, x: f32, y: f32, z: f32) {
        // check both bounds so the we can fold into the empty state.
        if x < self.xmin {
            self.xmin = x;
        }
        if x > self.xmax {
            self.xmax = x;
        }

        if y < self.ymin {
            self.ymin = y;
        }
        if y > self.ymax {
            self.ymax = y;
        }

        if z < self.zmin {
            self.zmin = z;
        }
        if z > self.zmax {
            self.zmax = z;
        }
    }

    /// True if two 3D bboxes intersect (closed intervals).
    /// NaN on either side ⇒ no overlap (matches the loop predicate in earlier versions).
    pub fn intersects(&self, other: &Bbox) -> bool {
        self.xmin <= other.xmax
            && other.xmin <= self.xmax
            && self.ymin <= other.ymax
            && other.ymin <= self.ymax
            && self.zmin <= other.zmax
            && other.zmin <= self.zmax
    }
}

impl From<[f32; 6]> for Bbox {
    /// Decode from canonical array form `[xmin, xmax, ymin, ymax, zmin, zmax]`.
    fn from(a: [f32; 6]) -> Self {
        Bbox {
            xmin: a[0],
            xmax: a[1],
            ymin: a[2],
            ymax: a[3],
            zmin: a[4],
            zmax: a[5],
        }
    }
}

impl From<Bbox> for [f32; 6] {
    /// Encode to canonical array form `[xmin, xmax, ymin, ymax, zmin, zmax]`.
    fn from(b: Bbox) -> Self {
        [b.xmin, b.xmax, b.ymin, b.ymax, b.zmin, b.zmax]
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowGroup {
    pub byte_start: u64,
    pub byte_size: u64,
    pub bbox: Bbox,
}

impl RowGroup {
    pub fn triangle_count(&self) -> u64 {
        self.byte_size / TRI_SIZE as u64
    }
}

impl From<[u8; ROW_GROUP_RECORD_SIZE]> for RowGroup {
    /// Decode a row-group record from its 40-byte on-disk form
    /// (`byte_start u64 LE` + `byte_size u64 LE` + 6× `f32 LE`).
    fn from(buf: [u8; ROW_GROUP_RECORD_SIZE]) -> Self {
        let byte_start = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let byte_size = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let mut arr = [0f32; 6];
        for i in 0..6 {
            let s = 16 + i * 4;
            arr[i] = f32::from_le_bytes(buf[s..s + 4].try_into().unwrap());
        }
        RowGroup {
            byte_start,
            byte_size,
            bbox: Bbox::from(arr),
        }
    }
}

impl From<RowGroup> for [u8; ROW_GROUP_RECORD_SIZE] {
    /// Encode a row-group record into its 40-byte on-disk form. Inverse of
    /// `From<[u8; ROW_GROUP_RECORD_SIZE]>`.
    fn from(g: RowGroup) -> Self {
        let arr: [f32; 6] = g.bbox.into();
        let mut buf = [0u8; ROW_GROUP_RECORD_SIZE];
        buf[0..8].copy_from_slice(&g.byte_start.to_le_bytes());
        buf[8..16].copy_from_slice(&g.byte_size.to_le_bytes());
        for i in 0..6 {
            let s = 16 + i * 4;
            buf[s..s + 4].copy_from_slice(&arr[i].to_le_bytes());
        }
        buf
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Index {
    pub groups: Vec<RowGroup>,
}

impl Index {
    /// Encode the footer payload (count + records). Does NOT include the trailing
    /// `footer_size + TRAILER` block — that's written separately by the converter.
    pub fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        // group_count is bounded by STL's u32 triangle_count (≤ u32::MAX),
        // since group_size ≥ 1 guarantees groups ≤ triangles. No overflow path.
        let count = self.groups.len() as u32;
        w.write_all(&count.to_le_bytes())?;
        for g in &self.groups {
            let record: [u8; ROW_GROUP_RECORD_SIZE] = (*g).into();
            w.write_all(&record)?;
        }
        Ok(())
    }

    /// Decode the footer payload (count + records). Caller must already have
    /// positioned the reader at the start of the payload.
    pub fn decode<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut cbuf = [0u8; 4];
        r.read_exact(&mut cbuf)?;
        let count = u32::from_le_bytes(cbuf) as usize;

        let mut groups = Vec::with_capacity(count);
        for _ in 0..count {
            let mut buf = [0u8; ROW_GROUP_RECORD_SIZE];
            r.read_exact(&mut buf)?;
            groups.push(RowGroup::from(buf));
        }
        Ok(Index { groups })
    }

    /// Size of the encoded footer payload in bytes (count + records only).
    pub fn encoded_size(&self) -> u64 {
        4 + (self.groups.len() as u64) * ROW_GROUP_RECORD_SIZE as u64
    }
}

/// Build the 80-byte istl header: magic + zero pad.
pub fn make_istl_header() -> [u8; STL_HEADER_LEN] {
    let mut h = [0u8; STL_HEADER_LEN];
    h[0..8].copy_from_slice(MAGIC);
    h
}

/// True if the given 80-byte header starts with the istl magic.
pub fn parse_istl_header(header: &[u8]) -> bool {
    header.len() >= 8 && &header[0..8] == MAGIC
}

/// Build the 16-byte tail: footer_size u64 LE + TRAILER magic.
pub fn make_tail(footer_size: u64) -> [u8; TAIL_LEN] {
    let mut t = [0u8; TAIL_LEN];
    t[0..8].copy_from_slice(&footer_size.to_le_bytes());
    t[8..16].copy_from_slice(TRAILER);
    t
}

/// Parse the 16-byte tail. Returns `Some(footer_size)` if the trailer magic matches.
pub fn parse_tail(tail: &[u8]) -> Option<u64> {
    if tail.len() < TAIL_LEN {
        return None;
    }
    if &tail[8..16] != TRAILER {
        return None;
    }
    Some(u64::from_le_bytes(tail[0..8].try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn row_group_byte_round_trip() {
        let g = RowGroup {
            byte_start: 84,
            byte_size: 50 * 17,
            bbox: Bbox::from([-1.0, 2.5, 0.0, 100.25, -50.0, 50.0]),
        };
        let bytes: [u8; ROW_GROUP_RECORD_SIZE] = g.into();
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
        let decoded = Index::decode(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(decoded, idx);
    }

    #[test]
    fn header_is_magic_only() {
        let h = make_istl_header();
        assert!(parse_istl_header(&h));
        assert!(h[8..].iter().all(|&b| b == 0));
        let mut bad = h;
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

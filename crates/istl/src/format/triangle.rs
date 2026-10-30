use super::Bbox;

pub const TRI_SIZE: usize = 50; // bytes per triangle, from STL standard

// offsets into the triangle record
const _NORMAL: usize = 0; // 12 bytes
const V1: usize = 12; // 12 bytes
const V2: usize = 24; // 12 bytes
const V3: usize = 36; // 12 bytes
const _ATTR: usize = 48; // 2 bytes

fn read_f32(b: &[u8]) -> f32 {
    f32::from_le_bytes(b.try_into().expect("4-byte slice"))
}

/// A view into a triangle record.
pub struct Triangle<'a>(&'a [u8; TRI_SIZE]);

impl<'a> Triangle<'a> {
    pub fn ref_from(buf: &'a [u8; TRI_SIZE]) -> Self {
        Self(buf)
    }

    pub fn first_xy(&self) -> [f32; 2] {
        [
            read_f32(&self.0[V1..V1 + 4]),
            read_f32(&self.0[V1 + 8..V1 + 12]),
        ]
    }

    /// Fold all 3 vertices of a binary-STL triangle record into `bbox`.
    pub fn fold_triangle(&self, bbox: &mut Bbox) {
        for off in [V1, V2, V3] {
            let vx = read_f32(&self.0[off..off + 4]);
            let vy = read_f32(&self.0[off + 4..off + 8]);
            let vz = read_f32(&self.0[off + 8..off + 12]);

            bbox.fold_point(vx, vy, vz);
        }
    }
}

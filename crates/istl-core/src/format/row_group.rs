use super::{Bbox, TRI_SIZE};

const ROW_GROUP_RECORD_SIZE: usize = 8 + 8 + 4 * 6; // 40 bytes — see encode

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
    pub const fn record_size() -> usize {
        ROW_GROUP_RECORD_SIZE
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

use std::io::{self, Read, Write};

pub const MAGIC: &[u8; 8] = b"ISTL1\0\0\0";
pub const TRAILER: &[u8; 8] = b"ISTLEND\0";
pub const STL_HEADER_LEN: usize = 80;
pub const TRI_SIZE: usize = 50;
pub const ROW_GROUP_RECORD_SIZE: usize = 8 + 8 + 4 * 6; // 40 bytes — see encode

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowGroup {
    pub byte_start: u64,
    pub byte_size: u64,
    /// [xmin, xmax, ymin, ymax, zmin, zmax]
    pub bbox: [f32; 6],
}

impl RowGroup {
    pub fn triangle_count(&self) -> u64 {
        self.byte_size / TRI_SIZE as u64
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Index {
    pub groups: Vec<RowGroup>,
}

impl Index {
    pub fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let count: u32 = self
            .groups
            .len()
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many groups"))?;
        w.write_all(&count.to_le_bytes())?;
        for g in &self.groups {
            w.write_all(&g.byte_start.to_le_bytes())?;
            w.write_all(&g.byte_size.to_le_bytes())?;
            for v in g.bbox {
                w.write_all(&v.to_le_bytes())?;
            }
        }
        w.write_all(TRAILER)?;
        Ok(())
    }

    pub fn decode<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut cbuf = [0u8; 4];
        r.read_exact(&mut cbuf)?;
        let count = u32::from_le_bytes(cbuf) as usize;
        let mut groups = Vec::with_capacity(count);
        for _ in 0..count {
            let mut buf = [0u8; ROW_GROUP_RECORD_SIZE];
            r.read_exact(&mut buf)?;
            let byte_start = u64::from_le_bytes(buf[0..8].try_into().unwrap());
            let byte_size = u64::from_le_bytes(buf[8..16].try_into().unwrap());
            let mut bbox = [0f32; 6];
            for i in 0..6 {
                let s = 16 + i * 4;
                bbox[i] = f32::from_le_bytes(buf[s..s + 4].try_into().unwrap());
            }
            groups.push(RowGroup {
                byte_start,
                byte_size,
                bbox,
            });
        }
        let mut tr = [0u8; 8];
        r.read_exact(&mut tr)?;
        if &tr != TRAILER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing ISTL trailer",
            ));
        }
        Ok(Index { groups })
    }

    pub fn encoded_size(&self) -> u64 {
        4 + (self.groups.len() as u64) * ROW_GROUP_RECORD_SIZE as u64 + 8
    }
}

/// Build the 80-byte istl header: magic + footer_offset + footer_size + zero pad.
///
/// Layout:
/// - bytes  0..8  : MAGIC
/// - bytes  8..16 : footer_offset u64 LE   (absolute file offset of the index)
/// - bytes 16..24 : footer_size   u64 LE   (size of the encoded footer in bytes)
/// - bytes 24..80 : reserved (zeroed, 56 bytes free for future use)
///
/// The invariant `footer_offset + footer_size == file_size` always holds.
///
/// Remote-reader access pattern (S3/Lambda): two sequential range GETs are
/// needed to locate the footer — `bytes=0-23` to read the header pointer
/// fields, then `bytes=<footer_offset>-<footer_offset+footer_size-1>` to read
/// the footer itself. The header lives at the front of the file and the
/// footer at the back, so a single range request cannot contain both. A
/// client may issue the header GET in parallel with a speculative tail GET
/// (`bytes=-N`) and verify against `footer_size` once both land — that is a
/// client-side optimization, not a format property.
pub fn make_istl_header(footer_offset: u64, footer_size: u64) -> [u8; STL_HEADER_LEN] {
    let mut h = [0u8; STL_HEADER_LEN];
    h[0..8].copy_from_slice(MAGIC);
    h[8..16].copy_from_slice(&footer_offset.to_le_bytes());
    h[16..24].copy_from_slice(&footer_size.to_le_bytes());
    h
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeaderInfo {
    pub footer_offset: u64,
    pub footer_size: u64,
}

/// Returns Some(HeaderInfo) if the 80-byte header begins with the istl magic.
pub fn parse_istl_header(header: &[u8]) -> Option<HeaderInfo> {
    if header.len() < 24 {
        return None;
    }
    if &header[0..8] != MAGIC {
        return None;
    }
    Some(HeaderInfo {
        footer_offset: u64::from_le_bytes(header[8..16].try_into().unwrap()),
        footer_size: u64::from_le_bytes(header[16..24].try_into().unwrap()),
    })
}

/// True if two 3D bboxes intersect (closed intervals). NaN on either side ⇒ no overlap.
pub fn bbox_intersects(a: &[f32; 6], b: &[f32; 6]) -> bool {
    for axis in 0..3 {
        let (amin, amax) = (a[axis * 2], a[axis * 2 + 1]);
        let (bmin, bmax) = (b[axis * 2], b[axis * 2 + 1]);
        if !(amin <= bmax) || !(bmin <= amax) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn index_round_trip() {
        let idx = Index {
            groups: vec![
                RowGroup {
                    byte_start: 84,
                    byte_size: 50 * 3,
                    bbox: [0.0, 1.0, 0.0, 1.0, 0.0, 1.0],
                },
                RowGroup {
                    byte_start: 84 + 150,
                    byte_size: 50 * 7,
                    bbox: [1.0, 2.0, 1.0, 2.0, -1.0, 0.0],
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
    fn header_round_trip() {
        let h = make_istl_header(123_456_789, 4_096);
        let info = parse_istl_header(&h).unwrap();
        assert_eq!(info.footer_offset, 123_456_789);
        assert_eq!(info.footer_size, 4_096);
        // Bytes 24..80 must be zero (reserved).
        assert!(h[24..].iter().all(|&b| b == 0));
        let mut bad = h;
        bad[0] = b'X';
        assert_eq!(parse_istl_header(&bad), None);
    }

    #[test]
    fn intersects_basic() {
        let a = [0.0, 1.0, 0.0, 1.0, 0.0, 1.0];
        let b = [0.5, 1.5, 0.5, 1.5, 0.5, 1.5];
        let c = [2.0, 3.0, 0.0, 1.0, 0.0, 1.0];
        assert!(bbox_intersects(&a, &b));
        assert!(!bbox_intersects(&a, &c));
        let nan = [f32::NAN, 0.0, 0.0, 0.0, 0.0, 0.0];
        assert!(!bbox_intersects(&a, &nan));
    }
}

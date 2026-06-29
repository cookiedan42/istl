use super::RowGroup;
use std::io::{self, Write};

/// Size of the trailing footer-size + identifier block at end of file.
pub const TAIL_LEN: usize = 8 + 8;
const TRAILER: &[u8; 8] = b"ISTLEND\0";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Index {
    pub groups: Vec<RowGroup>,
}

impl Index {
    /// Encode the footer payload (count + records). Does NOT include the trailing
    /// `footer_size + TRAILER` block — that's written separately by the converter.
    pub fn encode<W: Write + ?Sized>(&self, w: &mut W) -> io::Result<()> {
        // group_count is bounded by STL's u32 triangle_count (≤ u32::MAX),
        // since group_size ≥ 1 guarantees groups ≤ triangles. No overflow path.
        let count = self.groups.len() as u32;
        w.write_all(&count.to_le_bytes())?;
        for g in &self.groups {
            let record: [u8; RowGroup::record_size()] = (*g).into();
            w.write_all(&record)?;
        }
        Ok(())
    }

    /// Decode the footer payload (count + records). Caller must already have
    /// positioned the reader at the start of the payload.
    pub fn decode(r: &[u8]) -> io::Result<Self> {
        let count = u32::from_le_bytes(r[0..4].try_into().unwrap()) as usize;
        let mut groups = Vec::with_capacity(count);

        for chunk in r[4..].chunks_exact(RowGroup::record_size()) {
            let buf: [u8; RowGroup::record_size()] = chunk.try_into().unwrap();
            groups.push(RowGroup::from(buf));
        }
        Ok(Index { groups })
    }

    /// Size of the encoded footer payload in bytes (count + records only).
    pub fn encoded_size(&self) -> u64 {
        4 + (self.groups.len() as u64) * RowGroup::record_size() as u64
    }

    /// Sum of triangle counts across every row group. Lets `Reader::open_source`
    /// skip reading the STL `triangle_count` at bytes 80..84.
    pub fn triangle_count(&self) -> u64 {
        self.groups.iter().map(|g| g.triangle_count()).sum()
    }

    /// `triangle_count()` narrowed to `u32`, the on-disk STL width. Errors if
    /// the sum doesn't fit — would only happen on a corrupt index.
    pub fn triangle_count_u32(&self) -> io::Result<u32> {
        u32::try_from(self.triangle_count()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "row-group triangle count exceeds u32",
            )
        })
    }
}

/// Parse the 16-byte tail. Returns `Some(footer_size)` if the trailer identifier matches.
pub fn parse_tail(tail: &[u8]) -> Option<u64> {
    if tail.len() < TAIL_LEN {
        return None;
    }
    if &tail[8..16] != TRAILER {
        return None;
    }
    Some(u64::from_le_bytes(tail[0..8].try_into().unwrap()))
}

/// Build the 16-byte tail: footer_size u64 LE + TRAILER identifier.
pub fn make_tail(footer_size: u64) -> [u8; TAIL_LEN] {
    let mut t = [0u8; TAIL_LEN];
    t[0..8].copy_from_slice(&footer_size.to_le_bytes());
    t[8..16].copy_from_slice(TRAILER);
    t
}

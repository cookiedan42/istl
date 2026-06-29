pub(super) const IDENTIFIER: &[u8; 8] = b"ISTL1\0\0\0";

/// True if the given 80-byte header starts with the istl identifier.
pub fn parse_istl_header(header: &[u8]) -> bool {
    header.len() >= 8 && &header[0..8] == IDENTIFIER
}

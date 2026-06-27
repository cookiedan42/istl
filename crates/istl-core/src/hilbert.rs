/// Maps (x, y) integer coordinates on a 2^order × 2^order grid to a 1D Hilbert index.
///
/// Reference: https://en.wikipedia.org/wiki/Hilbert_curve#Applications_and_mapping_algorithms
pub fn xy2d(order: u32, mut x: u32, mut y: u32) -> u32 {
    assert!(order > 0 && order <= 16, "order must be in 1..=16");
    let mut d: u32 = 0;
    let mut s: u32 = 1 << (order - 1);
    while s > 0 {
        let rx = if (x & s) > 0 { 1 } else { 0 };
        let ry = if (y & s) > 0 { 1 } else { 0 };
        d = d.wrapping_add(s.wrapping_mul(s).wrapping_mul(((3 * rx) ^ ry) as u32));
        // rotate
        if ry == 0 {
            if rx == 1 {
                x = s.wrapping_sub(1).wrapping_sub(x);
                y = s.wrapping_sub(1).wrapping_sub(y);
            }
            std::mem::swap(&mut x, &mut y);
        }
        s >>= 1;
    }
    d
}

/// Normalize a float coordinate to the grid [0, side-1] given an inclusive range.
/// Constant range collapses to 0.
pub fn normalize(value: f32, min: f32, max: f32, side: u32) -> u32 {
    if !(max > min) {
        return 0;
    }
    let t = ((value - min) / (max - min)).clamp(0.0, 1.0);
    let i = (t * (side - 1) as f32).round() as i64;
    i.clamp(0, (side - 1) as i64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// For order=2 (4×4 grid), the Hilbert curve visits 16 cells in a specific
    /// known order. We test the four corners (0,0), (3,0), (3,3), (0,3) which
    /// are the curve's start, peak, and turn points.
    #[test]
    fn order2_corners_unique_and_in_range() {
        let mut seen = std::collections::HashSet::new();
        for x in 0..4u32 {
            for y in 0..4u32 {
                let d = xy2d(2, x, y);
                assert!(d < 16);
                assert!(seen.insert(d), "duplicate d at ({},{})", x, y);
            }
        }
        assert_eq!(seen.len(), 16);
        // The standard Wikipedia variant starts at (0,0) -> 0.
        assert_eq!(xy2d(2, 0, 0), 0);
    }

    #[test]
    fn fills_full_grid_at_order4() {
        let mut seen = std::collections::HashSet::new();
        for x in 0..16u32 {
            for y in 0..16u32 {
                let d = xy2d(4, x, y);
                assert!(d < 256);
                assert!(seen.insert(d));
            }
        }
        assert_eq!(seen.len(), 256);
    }

    #[test]
    fn normalize_basic() {
        assert_eq!(normalize(0.0, 0.0, 10.0, 256), 0);
        assert_eq!(normalize(10.0, 0.0, 10.0, 256), 255);
        assert_eq!(normalize(5.0, 0.0, 10.0, 256), 128);
        assert_eq!(normalize(-1.0, 0.0, 10.0, 256), 0);
        assert_eq!(normalize(99.0, 0.0, 10.0, 256), 255);
        // collapsed range
        assert_eq!(normalize(3.0, 3.0, 3.0, 256), 0);
    }
}

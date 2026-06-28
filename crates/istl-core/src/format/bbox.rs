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

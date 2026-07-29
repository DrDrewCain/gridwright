//! The output format: a small pyramid of detail levels.
//!
//! **Levels, not tiles, and that is a decision about our use case rather than a
//! shortcut.** Tiles exist so a client can fetch the few square kilometres it is
//! looking at and skip the rest. Nothing here fetches: the map is bundled, so a
//! tile index buys nothing and costs a lookup, an index table, and shapes cut at
//! every tile boundary. What a bundled map actually needs is the opposite —
//! *fewer* points when zoomed out, which is a level pyramid.
//!
//! Three levels, chosen against what the reader is doing:
//!
//! | level | tolerance | for |
//! | --- | --- | --- |
//! | 0 | 0.40° (~45 km) | the whole world, orientation only |
//! | 1 | 0.10° (~11 km) | a continent or a large country |
//! | 2 | 0.025° (~3 km) | a region, where a coastline is a landmark |
//!
//! There is no level beyond that on purpose. Past a regional view the reader is
//! looking at one substation and a coastline three kilometres out of place is
//! not the thing limiting them — where the finest level *would* cost more than
//! everything above it combined.
//!
//! Coordinates are quantised per level to `i16`, spanning the whole globe. At
//! level 2 that is a rounding error near 600 m against a 3 km tolerance, so the
//! quantisation is never the limiting term.

/// One shape: a ring, and its triangulation if it is filled.
pub struct Shape {
    pub points: Vec<[f64; 2]>,
    /// Empty for a line layer.
    pub triangles: Vec<[u16; 3]>,
}

/// What the studio draws, in draw order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layer {
    Land,
    Lakes,
    Rivers,
    Urban,
    Borders,
}

impl Layer {
    /// The order layers are written and read in, which is also draw order.
    pub const ALL: [Layer; 5] = [
        Layer::Land,
        Layer::Lakes,
        Layer::Rivers,
        Layer::Urban,
        Layer::Borders,
    ];

    pub fn filled(self) -> bool {
        matches!(self, Layer::Land | Layer::Lakes | Layer::Urban)
    }

    pub fn name(self) -> &'static str {
        match self {
            Layer::Land => "land",
            Layer::Lakes => "lakes",
            Layer::Rivers => "rivers",
            Layer::Urban => "urban",
            Layer::Borders => "borders",
        }
    }
}

/// Simplification tolerance per level, in degrees.
pub const TOLERANCE: [f64; 3] = [0.40, 0.10, 0.025];

/// Layers worth carrying at each level.
///
/// Rivers and urban areas are absent from level 0 deliberately: at a world view
/// they are a grey haze over every continent and tell a reader nothing, while
/// costing more than the coastline they obscure.
pub fn layers_at(level: usize) -> &'static [Layer] {
    match level {
        0 => &[Layer::Land, Layer::Lakes, Layer::Borders],
        _ => &Layer::ALL,
    }
}

/// Encode one level.
///
/// Layout, all little-endian:
///
/// ```text
/// u8   layer count
/// per layer:
///   u8   layer tag
///   u32  shape count
///   per shape:
///     u16  point count
///     u16  triangle count
///     i16  x, i16 y  per point
///     u16  a, b, c    per triangle
/// ```
///
/// No offsets or checksums. The studio reads it start to finish in one pass and
/// bails on a short read, so an index would be a table nothing consults and a
/// checksum would guard against a corruption mode that cannot happen to a blob
/// compiled into the binary.
pub fn encode(level: &[(Layer, Vec<Shape>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(level.len() as u8);
    for (layer, shapes) in level {
        out.push(tag(*layer));
        out.extend((shapes.len() as u32).to_le_bytes());
        for s in shapes {
            out.extend((s.points.len() as u16).to_le_bytes());
            out.extend((s.triangles.len() as u16).to_le_bytes());
            for p in &s.points {
                out.extend(quantise(p[0], 180.0).to_le_bytes());
                out.extend(quantise(p[1], 90.0).to_le_bytes());
            }
            for t in &s.triangles {
                for i in t {
                    out.extend(i.to_le_bytes());
                }
            }
        }
    }
    out
}

pub fn tag(l: Layer) -> u8 {
    match l {
        Layer::Land => 1,
        Layer::Lakes => 2,
        Layer::Rivers => 3,
        Layer::Urban => 4,
        Layer::Borders => 5,
    }
}

/// Degrees to `i16`, saturating at the pole and the antimeridian.
///
/// Saturating rather than wrapping. A latitude of 90.0001 in a source file is a
/// rounding artefact, and wrapping would put it at the *south* pole — a point on
/// the other side of the planet, which is a far worse error than 600 m.
fn quantise(v: f64, full: f64) -> i16 {
    let scaled = (v / full * 32767.0).round();
    scaled.clamp(-32767.0, 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tolerances_get_finer_with_level() {
        // Otherwise zooming in would show *less* detail, which is the one thing
        // a pyramid must not do.
        for w in TOLERANCE.windows(2) {
            assert!(w[1] < w[0], "{:?} does not decrease", TOLERANCE);
        }
    }

    #[test]
    fn the_world_level_omits_the_busy_layers() {
        // Rivers and urban areas at a world view are a grey haze that costs more
        // than the coastline it obscures.
        let l0 = layers_at(0);
        assert!(!l0.contains(&Layer::Rivers));
        assert!(!l0.contains(&Layer::Urban));
        assert!(l0.contains(&Layer::Land));
    }

    #[test]
    fn finer_levels_carry_every_layer() {
        for level in 1..TOLERANCE.len() {
            assert_eq!(layers_at(level).len(), Layer::ALL.len(), "level {level}");
        }
    }

    #[test]
    fn every_layer_has_a_distinct_tag() {
        let mut seen = Vec::new();
        for l in Layer::ALL {
            assert!(!seen.contains(&tag(l)), "{l:?} reuses a tag");
            seen.push(tag(l));
        }
        // Zero is left unused so a truncated read cannot look like a valid layer.
        assert!(!seen.contains(&0));
    }

    #[test]
    fn quantisation_saturates_rather_than_wrapping() {
        // A latitude of 90.0001 is a rounding artefact in a source file. Wrapping
        // would move it to the south pole, which is a much worse error than the
        // 600 m the quantisation itself costs.
        assert_eq!(quantise(90.0, 90.0), 32767);
        assert_eq!(quantise(95.0, 90.0), 32767);
        assert_eq!(quantise(-95.0, 90.0), -32767);
        assert_eq!(quantise(0.0, 90.0), 0);
    }

    #[test]
    fn a_round_trip_preserves_position_to_the_quantisation() {
        // The finest tolerance is 0.025 degrees, about 3 km. Quantisation error
        // must stay well inside that or it becomes the limiting term.
        for v in [0.0, 13.405, -74.006, 179.9, -179.9] {
            let back = quantise(v, 180.0) as f64 / 32767.0 * 180.0;
            assert!(
                (back - v).abs() < 0.006,
                "{v} came back as {back}",
            );
        }
    }

    #[test]
    fn an_encoded_level_starts_with_its_layer_count() {
        let shapes = vec![Shape {
            points: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]],
            triangles: vec![[0, 1, 2]],
        }];
        let blob = encode(&[(Layer::Land, shapes)]);
        assert_eq!(blob[0], 1);
        assert_eq!(blob[1], tag(Layer::Land));
        // count, then one shape: 2 + 2 header, 3 points at 4 bytes, 1 triangle
        // at 6 bytes.
        assert_eq!(blob.len(), 1 + 1 + 4 + 2 + 2 + 3 * 4 + 6);
    }

    #[test]
    fn a_line_layer_encodes_no_triangles() {
        let blob = encode(&[(
            Layer::Borders,
            vec![Shape {
                points: vec![[0.0, 0.0], [1.0, 1.0]],
                triangles: Vec::new(),
            }],
        )]);
        assert_eq!(blob.len(), 1 + 1 + 4 + 2 + 2 + 2 * 4);
    }
}

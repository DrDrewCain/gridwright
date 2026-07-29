//! Coastlines and borders, under the network.
//!
//! **Bundled, not fetched.** The browser build has no server, and a tile layer
//! would give it one — a runtime dependency on somebody else's infrastructure,
//! their terms of service, and a network round trip, in a tool whose whole
//! claim is that it solves on your machine. Twenty kilobytes of vector outline
//! costs less than one tile and works on a plane.
//!
//! Natural Earth **1:50m** admin-0 boundaries, public domain, simplified with
//! Douglas–Peucker at 0.10° (about 11 km) and quantised to `i16`. 995 rings,
//! 18,000 points, 74 KB — roughly 1% of the wasm bundle.
//!
//! 1:110m was tried first at 0.28° and is 20 KB, and it is too coarse: at a
//! zoom that fits one country, an 11-point outline of Germany reads as a
//! polygon rather than as a coastline. The quantisation is the cheap part —
//! worst-case rounding is near 0.0055°, about 600 m, well under both the
//! simplification and one screen pixel at any zoom this draws at.
//!
//! **Three layers, and no more than three.** Land as a filled tone against the
//! sea, lakes punched back out of it, and national borders as a separate
//! hairline. That is what makes a picture read as a *map* rather than as a
//! wireframe: a tonal land/sea distinction does more for orientation than any
//! amount of outline detail, and it is what every published TSO map has.
//!
//! **And no more than three, deliberately.** Overbye (NAPS 2019) on geographic
//! grid displays: satellite and detailed backgrounds *"run the risk of
//! background camouflaging the electric grid information of interest."* So no
//! roads, no terrain, no labels, and the whole thing sits within a few percent
//! of the canvas tone. It answers "where is this?" and gets out of the way.
//!
//! Polygons are **triangulated ahead of time** and shipped as index triples.
//! Ear clipping a coastline is O(n^2) in the worst case and belongs in a build
//! step, not in a frame — and it means the runtime here is a decode and a
//! transform, with no geometry algorithm in the shipped path at all.

use eframe::egui::{Color32, Mesh, Painter, Pos2, Rect, Shape, Stroke, pos2};

/// Filled polygons: count, then per shape a point count, a triangle count,
/// `i16` coordinate pairs, and `u16` index triples.
const LAND: &[u8] = include_bytes!("land.bin");
const LAKES: &[u8] = include_bytes!("lakes.bin");
/// Open polylines: count, then per line a point count and `i16` pairs.
const BORDERS: &[u8] = include_bytes!("borders.bin");

/// A polygon with its triangulation, in Mercator.
struct Filled {
    pts: Vec<Pos2>,
    tris: Vec<[u16; 3]>,
    bounds: Rect,
}

/// An open line, in Mercator.
struct Line {
    pts: Vec<Pos2>,
    bounds: Rect,
}

/// Decoded once. Parsing 25,000 points per frame to draw a backdrop would cost
/// more than everything in front of it.
pub struct Basemap {
    land: Vec<Filled>,
    lakes: Vec<Filled>,
    borders: Vec<Line>,
}

impl Basemap {
    pub fn load() -> Self {
        Self {
            land: filled(LAND),
            lakes: filled(LAKES),
            borders: lines(BORDERS),
        }
    }

    /// `frame` maps Mercator into the layout's normalised space and `project`
    /// maps that onto the screen. Two steps rather than one because culling
    /// happens in between: `visible` is in layout space, and comparing raw
    /// Mercator bounds against it is a test between two coordinate systems that
    /// silently rejects everything.
    pub fn draw(
        &self,
        painter: &Painter,
        visible: Rect,
        frame: crate::layout::Frame,
        project: impl Fn(Pos2) -> Pos2,
        tone: Tone,
    ) {
        let at = |p: Pos2| project(frame.apply(p));
        let seen = |b: Rect| {
            Rect::from_min_max(frame.apply(b.min), frame.apply(b.max)).intersects(visible)
        };

        // Land first, then lakes over it in the sea tone, then borders on top.
        // Order is the whole of the algorithm: a lake is not a hole in a mesh,
        // it is the sea painted back over the land, which is how every map
        // renderer does it and avoids needing polygons with holes at all.
        for s in &self.land {
            if seen(s.bounds) {
                fill(painter, s, &at, tone.land);
            }
        }
        for s in &self.lakes {
            if seen(s.bounds) {
                fill(painter, s, &at, tone.sea);
            }
        }

        // Coastline last of the fills, from the land outlines, so the boundary
        // between land and sea is crisp rather than left to the mesh edge --
        // which is antialiased against whatever is behind it and reads soft.
        let coast = Stroke::new(0.9, tone.coast);
        for s in &self.land {
            if seen(s.bounds) {
                painter.add(Shape::line(
                    s.pts.iter().map(|p| at(*p)).collect::<Vec<_>>(),
                    coast,
                ));
            }
        }

        // Borders thinner and dimmer than the coast. A national boundary is a
        // fact about people and a coastline is a fact about the ground; the
        // reader is orienting by the second.
        let border = Stroke::new(0.7, tone.border);
        for l in &self.borders {
            if seen(l.bounds) {
                painter.add(Shape::line(
                    l.pts.iter().map(|p| at(*p)).collect::<Vec<_>>(),
                    border,
                ));
            }
        }
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.land.is_empty()
    }
}

/// The four tones a basemap needs, so the caller owns the palette.
#[derive(Clone, Copy)]
pub struct Tone {
    pub land: Color32,
    pub sea: Color32,
    pub coast: Color32,
    pub border: Color32,
}

fn fill(painter: &Painter, s: &Filled, at: &impl Fn(Pos2) -> Pos2, color: Color32) {
    let mut mesh = Mesh::default();
    for p in &s.pts {
        mesh.colored_vertex(at(*p), color);
    }
    for t in &s.tris {
        // Guarded rather than trusted. The indices come from a build step, and
        // an out-of-range one would panic inside egui's tessellator with a
        // message about a mesh rather than about this file.
        if t.iter().all(|i| (*i as usize) < s.pts.len()) {
            mesh.add_triangle(t[0] as u32, t[1] as u32, t[2] as u32);
        }
    }
    painter.add(Shape::mesh(mesh));
}

fn bounds_of(pts: &[Pos2]) -> Rect {
    let (mut lo, mut hi) = (pos2(f32::MAX, f32::MAX), pos2(f32::MIN, f32::MIN));
    for p in pts {
        lo.x = lo.x.min(p.x);
        lo.y = lo.y.min(p.y);
        hi.x = hi.x.max(p.x);
        hi.y = hi.y.max(p.y);
    }
    Rect::from_min_max(lo, hi)
}

fn filled(blob: &[u8]) -> Vec<Filled> {
    let mut out = Vec::new();
    let mut at = 0usize;
    let Some(n) = u32b(blob, &mut at) else {
        return out;
    };
    for _ in 0..n {
        let (Some(np), Some(nt)) = (u16b(blob, &mut at), u16b(blob, &mut at)) else {
            break;
        };
        let mut pts = Vec::with_capacity(np as usize);
        for _ in 0..np {
            let (Some(x), Some(y)) = (i16b(blob, &mut at), i16b(blob, &mut at)) else {
                break;
            };
            pts.push(mercator(x as f64 / 32767.0 * 180.0, y as f64 / 32767.0 * 90.0));
        }
        let mut tris = Vec::with_capacity(nt as usize);
        for _ in 0..nt {
            let (Some(a), Some(b), Some(c)) = (
                u16b(blob, &mut at),
                u16b(blob, &mut at),
                u16b(blob, &mut at),
            ) else {
                break;
            };
            tris.push([a, b, c]);
        }
        if pts.len() >= 3 {
            let bounds = bounds_of(&pts);
            out.push(Filled { pts, tris, bounds });
        }
    }
    out
}

fn lines(blob: &[u8]) -> Vec<Line> {
    let mut out = Vec::new();
    let mut at = 0usize;
    let Some(n) = u32b(blob, &mut at) else {
        return out;
    };
    for _ in 0..n {
        let Some(np) = u16b(blob, &mut at) else { break };
        let mut pts = Vec::with_capacity(np as usize);
        for _ in 0..np {
            let (Some(x), Some(y)) = (i16b(blob, &mut at), i16b(blob, &mut at)) else {
                break;
            };
            pts.push(mercator(x as f64 / 32767.0 * 180.0, y as f64 / 32767.0 * 90.0));
        }
        if pts.len() >= 2 {
            let bounds = bounds_of(&pts);
            out.push(Line { pts, bounds });
        }
    }
    out
}

/// The same projection `layout::project_one` uses, and it has to stay that way.
///
/// If these two ever disagree the map slides against the network, and both
/// halves look perfectly plausible on their own. Duplicated rather than shared
/// because the layout takes `f64` degrees from a `Coord` and this takes
/// quantised integers; a test pins them together.
fn mercator(lon: f64, lat: f64) -> Pos2 {
    let lat = lat.clamp(-85.051_129, 85.051_129).to_radians();
    let y = ((std::f64::consts::FRAC_PI_4 + lat / 2.0).tan()).ln();
    pos2(lon.to_radians() as f32, -y as f32)
}

fn u32b(b: &[u8], at: &mut usize) -> Option<u32> {
    let v = b.get(*at..*at + 4)?;
    *at += 4;
    Some(u32::from_le_bytes(v.try_into().ok()?))
}

fn u16b(b: &[u8], at: &mut usize) -> Option<u16> {
    let v = b.get(*at..*at + 2)?;
    *at += 2;
    Some(u16::from_le_bytes(v.try_into().ok()?))
}

fn i16b(b: &[u8], at: &mut usize) -> Option<i16> {
    let v = b.get(*at..*at + 2)?;
    *at += 2;
    Some(i16::from_le_bytes(v.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_layers_decode() {
        let m = Basemap::load();
        assert!(!m.is_empty(), "no land decoded");
        assert!(m.land.len() > 100, "only {} land shapes", m.land.len());
        assert!(!m.lakes.is_empty(), "no lakes decoded");
        assert!(!m.borders.is_empty(), "no borders decoded");
    }

    #[test]
    fn every_land_shape_has_a_triangulation() {
        // A shape with points and no triangles fills nothing and would show as
        // a hole in the map, which is the failure mode of a build step that
        // half worked.
        for s in Basemap::load().land {
            assert!(!s.tris.is_empty(), "a land shape has no triangles");
        }
    }

    #[test]
    fn every_index_is_in_range() {
        // Out of range would panic inside egui's tessellator with a message
        // about a mesh rather than about this file.
        let m = Basemap::load();
        for s in m.land.iter().chain(&m.lakes) {
            for t in &s.tris {
                for i in t {
                    assert!((*i as usize) < s.pts.len(), "index {i} of {}", s.pts.len());
                }
            }
        }
    }

    #[test]
    fn every_point_is_finite() {
        let m = Basemap::load();
        for s in m.land.iter().chain(&m.lakes) {
            for p in &s.pts {
                assert!(p.x.is_finite() && p.y.is_finite(), "{p:?}");
            }
        }
    }

    #[test]
    fn the_projection_matches_the_layouts() {
        for (lon, lat) in [
            (0.0, 0.0),
            (13.405, 52.52),
            (-74.006, 40.713),
            (151.209, -33.868),
            (0.0, 85.0),
        ] {
            let mine = mercator(lon, lat);
            let theirs = crate::layout::project_one(lon, lat);
            assert!(
                (mine.x - theirs.x).abs() < 1e-6 && (mine.y - theirs.y).abs() < 1e-6,
                "({lon}, {lat}): basemap {mine:?} against layout {theirs:?}",
            );
        }
    }

    #[test]
    fn a_truncated_blob_stops_rather_than_panicking() {
        assert!(filled(&[9, 0, 0, 0]).is_empty());
        assert!(lines(&[9, 0, 0, 0]).is_empty());
        assert!(filled(&[]).is_empty());
    }
}

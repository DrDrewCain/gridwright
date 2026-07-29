//! The map under the network: land, water, rivers, cities and borders.
//!
//! **Ours, from public-domain source, built by `gridwright-mapgen`.** Not a tile
//! service and not somebody's vector-tile archive. A tile layer would give a
//! serverless tool a server — a runtime dependency on another party's
//! infrastructure and terms, plus a network round trip — in a thing whose whole
//! claim is that it runs on your machine. This works on a plane.
//!
//! Satellite imagery is a different question and the honest answer is that it is
//! incompatible with a bundled map: street-level imagery is petabytes, and a
//! world raster small enough to compile in resolves at roughly 10 km per pixel,
//! which is mush across the few hundred kilometres a grid spans. There is also a
//! finding against it — Overbye (NAPS 2019) on geographic grid displays:
//! satellite and detailed backgrounds *"run the risk of background camouflaging
//! the electric grid information of interest."* The thing being overlaid here is
//! thin lines.
//!
//! # Levels
//!
//! Three, chosen by how much of the world is on screen. Only the level in use is
//! decoded, and it is decoded once and kept, so panning across a continent costs
//! nothing and crossing a boundary costs one parse.
//!
//! Detail *rises* with zoom, which is the point. The previous version drew one
//! coarse world outline at every magnification, so zooming in gave the same
//! eleven-point Germany scaled up.

use eframe::egui::{Color32, Mesh, Painter, Pos2, Rect, Shape, Stroke, pos2};

/// One blob per level, produced by `gridwright-mapgen`.
const LEVELS: [&[u8]; 3] = [
    include_bytes!("map/level0.bin"),
    include_bytes!("map/level1.bin"),
    include_bytes!("map/level2.bin"),
];

/// What the studio draws. Tags match the generator's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layer {
    Land,
    Lakes,
    Rivers,
    Urban,
    Borders,
}

impl Layer {
    fn from_tag(t: u8) -> Option<Self> {
        Some(match t {
            1 => Layer::Land,
            2 => Layer::Lakes,
            3 => Layer::Rivers,
            4 => Layer::Urban,
            5 => Layer::Borders,
            _ => return None,
        })
    }
}

/// A shape with its triangulation, in Mercator, plus bounds for culling.
struct Piece {
    pts: Vec<Pos2>,
    tris: Vec<[u16; 3]>,
    bounds: Rect,
}

/// Tones supplied by the caller, so the palette stays in the theme.
#[derive(Clone, Copy)]
pub struct Tone {
    pub land: Color32,
    pub sea: Color32,
    pub coast: Color32,
    pub border: Color32,
    pub river: Color32,
    pub urban: Color32,
}

/// Which layers to draw.
#[derive(Clone, Copy)]
pub struct Show {
    pub water: bool,
    pub rivers: bool,
    pub urban: bool,
    pub borders: bool,
}

impl Default for Show {
    fn default() -> Self {
        // Rivers and cities off by default: they are the two that compete with
        // the network for attention. A reader who wants them can ask; one who
        // does not should never have had to turn them off.
        Self {
            water: true,
            rivers: false,
            urban: false,
            borders: true,
        }
    }
}

#[derive(Default)]
pub struct Basemap {
    /// Decoded levels, `None` until first needed.
    cache: [Option<Vec<(Layer, Vec<Piece>)>>; 3],
}

impl Basemap {
    /// Which level suits a camera showing `span` of **Mercator** x.
    ///
    /// Mercator x runs 0 to 2π for the globe, so `span` is a fraction of the
    /// world. Thresholds are deliberately wide apart: crossing a boundary costs
    /// a parse, and one sitting where people habitually zoom would pay it over
    /// and over.
    fn level_for(span: f32) -> usize {
        let fraction = span / std::f32::consts::TAU;
        match fraction {
            f if f > 0.30 => 0,
            f if f > 0.02 => 1,
            _ => 2,
        }
    }

    /// Draw what is visible, at the level that suits the camera.
    ///
    /// `frame` maps Mercator into the layout's normalised space, `project` maps
    /// that onto the screen. Two steps because culling happens between them:
    /// `visible` is in layout space, and testing raw Mercator bounds against it
    /// compares two coordinate systems and rejects everything. That was a real
    /// bug — the first version of this drew nothing at all.
    pub fn draw(
        &mut self,
        painter: &Painter,
        visible: Rect,
        frame: crate::layout::Frame,
        project: impl Fn(Pos2) -> Pos2,
        tone: Tone,
        show: Show,
    ) {
        // Converted back into Mercator before asking which level fits. The
        // `visible` rect is in the layout's normalised space, where the whole
        // network is about one unit across whatever its true extent -- so
        // comparing its width against the globe's Mercator width would compare
        // two different units and pick a level at random. Dividing by the
        // frame's scale undoes the normalisation.
        let level = Self::level_for(visible.width() / frame.scale().max(1e-9));
        if self.cache[level].is_none() {
            self.cache[level] = Some(decode(LEVELS[level]));
        }
        let Some(layers) = self.cache[level].as_ref() else {
            return;
        };

        let at = |p: Pos2| project(frame.apply(p));
        let seen = |b: Rect| {
            Rect::from_min_max(frame.apply(b.min), frame.apply(b.max)).intersects(visible)
        };
        let of = |want: Layer| -> &[Piece] {
            layers
                .iter()
                .find(|(l, _)| *l == want)
                .map(|(_, p)| p.as_slice())
                .unwrap_or(&[])
        };

        // Order is the algorithm. Land, then water back over it in the sea tone,
        // then cities, then line work. A lake is not a hole in a mesh — it is the
        // sea painted over the land, which is how a renderer avoids ever needing
        // a polygon with a hole.
        for p in of(Layer::Land).iter().filter(|p| seen(p.bounds)) {
            fill(painter, p, &at, tone.land);
        }
        if show.water {
            for p in of(Layer::Lakes).iter().filter(|p| seen(p.bounds)) {
                fill(painter, p, &at, tone.sea);
            }
        }
        if show.urban {
            for p in of(Layer::Urban).iter().filter(|p| seen(p.bounds)) {
                fill(painter, p, &at, tone.urban);
            }
        }

        // The coastline after the fills, so the land/sea edge is crisp. A mesh
        // edge is antialiased against whatever is behind it and reads soft.
        let coast = Stroke::new(0.9, tone.coast);
        for p in of(Layer::Land).iter().filter(|p| seen(p.bounds)) {
            stroke(painter, p, &at, coast);
        }
        if show.rivers {
            let s = Stroke::new(0.6, tone.river);
            for p in of(Layer::Rivers).iter().filter(|p| seen(p.bounds)) {
                stroke(painter, p, &at, s);
            }
        }
        if show.borders {
            // Thinner and dimmer than the coast: a national boundary is a fact
            // about people, a coastline is a fact about the ground, and a reader
            // orienting themselves uses the second.
            let s = Stroke::new(0.7, tone.border);
            for p in of(Layer::Borders).iter().filter(|p| seen(p.bounds)) {
                stroke(painter, p, &at, s);
            }
        }
    }
}

fn stroke(painter: &Painter, s: &Piece, at: &impl Fn(Pos2) -> Pos2, st: Stroke) {
    painter.add(Shape::line(
        s.pts.iter().map(|q| at(*q)).collect::<Vec<_>>(),
        st,
    ));
}

fn fill(painter: &Painter, s: &Piece, at: &impl Fn(Pos2) -> Pos2, color: Color32) {
    let mut mesh = Mesh::default();
    for p in &s.pts {
        mesh.colored_vertex(at(*p), color);
    }
    for t in &s.tris {
        // Guarded rather than trusted. Indices come from a build step, and an
        // out-of-range one panics inside egui's tessellator with a message about
        // a mesh rather than about the data responsible.
        if t.iter().all(|i| (*i as usize) < s.pts.len()) {
            mesh.add_triangle(t[0] as u32, t[1] as u32, t[2] as u32);
        }
    }
    painter.add(Shape::mesh(mesh));
}

/// Read one level. A short read stops early rather than panicking.
fn decode(blob: &[u8]) -> Vec<(Layer, Vec<Piece>)> {
    let mut at = 0usize;
    let Some(n_layers) = u8b(blob, &mut at) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(n_layers as usize);

    for _ in 0..n_layers {
        let (Some(tag), Some(n_shapes)) = (u8b(blob, &mut at), u32b(blob, &mut at)) else {
            break;
        };
        let Some(layer) = Layer::from_tag(tag) else {
            break;
        };
        let mut pieces = Vec::with_capacity(n_shapes as usize);
        for _ in 0..n_shapes {
            let (Some(np), Some(nt)) = (u16b(blob, &mut at), u16b(blob, &mut at)) else {
                break;
            };
            let mut pts = Vec::with_capacity(np as usize);
            for _ in 0..np {
                let (Some(x), Some(y)) = (i16b(blob, &mut at), i16b(blob, &mut at)) else {
                    break;
                };
                pts.push(mercator(
                    x as f64 / 32767.0 * 180.0,
                    y as f64 / 32767.0 * 90.0,
                ));
            }
            let mut tris = Vec::with_capacity(nt as usize);
            for _ in 0..nt {
                let (Some(a), Some(b), Some(c)) =
                    (u16b(blob, &mut at), u16b(blob, &mut at), u16b(blob, &mut at))
                else {
                    break;
                };
                tris.push([a, b, c]);
            }
            if pts.len() >= 2 {
                let bounds = bounds_of(&pts);
                pieces.push(Piece { pts, tris, bounds });
            }
        }
        out.push((layer, pieces));
    }
    out
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

/// The same projection `layout::project_one` uses, and it must stay that way.
///
/// If the two disagree the map slides against the network and both halves look
/// perfectly plausible alone. Duplicated because the layout takes `f64` degrees
/// from a `Coord` and this takes quantised integers; a test pins them together.
fn mercator(lon: f64, lat: f64) -> Pos2 {
    let lat = lat.clamp(-85.051_129, 85.051_129).to_radians();
    let y = ((std::f64::consts::FRAC_PI_4 + lat / 2.0).tan()).ln();
    pos2(lon.to_radians() as f32, -y as f32)
}

fn u8b(b: &[u8], at: &mut usize) -> Option<u8> {
    let v = b.get(*at).copied();
    *at += 1;
    v
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
    fn every_level_decodes_and_has_land() {
        for (i, blob) in LEVELS.iter().enumerate() {
            let layers = decode(blob);
            assert!(!layers.is_empty(), "level {i} decoded no layers");
            let land = layers
                .iter()
                .find(|(l, _)| *l == Layer::Land)
                .map(|(_, p)| p.len())
                .unwrap_or(0);
            assert!(land > 20, "level {i} has only {land} land shapes");
        }
    }

    /// Whether the land layer covers a lon/lat, by the triangles it ships.
    ///
    /// The same question the renderer answers, asked of the data instead of the
    /// screen. A map can decode cleanly, index cleanly and still fill the wrong
    /// half of the world.
    fn land_covers(level: usize, lon: f64, lat: f64) -> bool {
        let q = mercator(lon, lat);
        let inside = |p: Pos2, a: Pos2, b: Pos2, c: Pos2| {
            let side = |u: Pos2, v: Pos2| (v.x - u.x) * (p.y - u.y) - (v.y - u.y) * (p.x - u.x);
            let (d1, d2, d3) = (side(a, b), side(b, c), side(c, a));
            !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
        };
        decode(LEVELS[level])
            .iter()
            .filter(|(l, _)| *l == Layer::Land)
            .flat_map(|(_, pieces)| pieces)
            .filter(|piece| piece.bounds.contains(q))
            .any(|piece| {
                piece.tris.iter().any(|t| {
                    inside(
                        q,
                        piece.pts[t[0] as usize],
                        piece.pts[t[1] as usize],
                        piece.pts[t[2] as usize],
                    )
                })
            })
    }

    #[test]
    fn land_covers_land_and_leaves_the_sea_alone() {
        // **The test the first bundled pyramid needed and did not have.** Every
        // structural check passed -- levels decoded, indices were in range, point
        // counts rose with detail -- while every continent was missing, because
        // simplification had made their rings self-crossing and the triangulator
        // dropped them. Only small islands survived, so the map drew coastlines
        // over open water.
        //
        // These are inland points far from any coast, so quantisation and a 3 km
        // tolerance cannot move the answer.
        for (name, lon, lat) in [
            ("Berlin", 13.4, 52.5),
            ("Kansas", -98.0, 38.5),
            ("Siberia", 90.0, 62.0),
            ("Sahara", 15.0, 23.0),
            ("Amazon", -60.0, -5.0),
            ("Australia", 133.0, -24.0),
        ] {
            for level in 0..LEVELS.len() {
                assert!(
                    land_covers(level, lon, lat),
                    "level {level} has no land at {name}",
                );
            }
        }

        // And the converse, or a map that filled everything would pass.
        for (name, lon, lat) in [
            ("mid-Atlantic", -30.0, 35.0),
            ("mid-Pacific", -140.0, 10.0),
            ("Indian Ocean", 80.0, -30.0),
        ] {
            for level in 0..LEVELS.len() {
                assert!(
                    !land_covers(level, lon, lat),
                    "level {level} fills the sea at {name}",
                );
            }
        }
    }

    #[test]
    fn detail_rises_with_level() {
        // The whole point of a pyramid. Inverted, zooming in would show *less*
        // detail than zooming out.
        let points = |i: usize| -> usize {
            decode(LEVELS[i])
                .iter()
                .flat_map(|(_, p)| p)
                .map(|p| p.pts.len())
                .sum()
        };
        let (a, b, c) = (points(0), points(1), points(2));
        assert!(a < b && b < c, "levels carry {a}, {b}, {c} points");
    }

    #[test]
    fn the_world_level_omits_the_busy_layers() {
        let l0 = decode(LEVELS[0]);
        assert!(!l0.iter().any(|(l, _)| *l == Layer::Rivers));
        assert!(!l0.iter().any(|(l, _)| *l == Layer::Urban));
    }

    #[test]
    fn finer_levels_carry_rivers_and_cities() {
        for i in 1..LEVELS.len() {
            let ls = decode(LEVELS[i]);
            assert!(ls.iter().any(|(l, _)| *l == Layer::Rivers), "level {i}");
            assert!(ls.iter().any(|(l, _)| *l == Layer::Urban), "level {i}");
        }
    }

    #[test]
    fn every_index_is_in_range() {
        for blob in LEVELS {
            for (_, pieces) in decode(blob) {
                for p in pieces {
                    for t in &p.tris {
                        for i in t {
                            assert!((*i as usize) < p.pts.len());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn every_point_is_finite() {
        for blob in LEVELS {
            for (_, pieces) in decode(blob) {
                for p in pieces {
                    for q in p.pts {
                        assert!(q.x.is_finite() && q.y.is_finite());
                    }
                }
            }
        }
    }

    #[test]
    fn the_level_never_falls_as_the_camera_narrows() {
        let world = std::f32::consts::TAU;
        assert_eq!(Basemap::level_for(world), 0);
        assert_eq!(Basemap::level_for(world * 0.1), 1);
        assert_eq!(Basemap::level_for(world * 0.001), 2);
        let mut last = 0;
        for k in 0..40 {
            let l = Basemap::level_for(world * 0.5_f32.powi(k));
            assert!(l >= last, "level fell from {last} to {l} at 2^-{k}");
            last = l;
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
        assert!(decode(&[]).is_empty());
        assert!(decode(&[3, 1, 9, 9]).iter().all(|(_, p)| p.is_empty()));
    }

    #[test]
    fn an_unknown_layer_tag_stops_the_read() {
        // Tag zero is never emitted, so meeting one means the stream is not what
        // it claims to be.
        assert!(decode(&[1, 0, 0, 0, 0, 0]).is_empty());
    }

    #[test]
    fn rivers_and_cities_are_off_by_default() {
        let s = Show::default();
        assert!(!s.rivers && !s.urban);
        assert!(s.water && s.borders);
    }
}


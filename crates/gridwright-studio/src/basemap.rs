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
//! **Deliberately faint, and that is a finding rather than taste.** Overbye
//! (NAPS 2019) on geographic grid displays: satellite and detailed backgrounds
//! *"run the risk of background camouflaging the electric grid information of
//! interest."* This is a hairline coastline and nothing else — no fill, no
//! labels, no roads, no terrain. It exists to answer "where is this?" and then
//! get out of the way.

use eframe::egui::{Color32, Painter, Pos2, Rect, Stroke, pos2};

/// Packed rings: ring count, then per ring a point count and `i16` pairs.
const OUTLINES: &[u8] = include_bytes!("coastline.bin");

/// Decoded once, because parsing 5,000 points per frame to draw a backdrop
/// would cost more than everything in front of it.
pub struct Basemap {
    /// Each ring in Web Mercator, in the same space `layout` produces — so the
    /// camera transform that draws the network draws this too, with no second
    /// projection to get out of step.
    rings: Vec<Vec<Pos2>>,
}

impl Basemap {
    pub fn load() -> Self {
        let mut rings = Vec::new();
        let mut at = 0usize;

        let Some(n) = read_u32(OUTLINES, &mut at) else {
            return Self { rings };
        };
        for _ in 0..n {
            let Some(count) = read_u16(OUTLINES, &mut at) else {
                break;
            };
            let mut ring = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let (Some(lon_q), Some(lat_q)) =
                    (read_i16(OUTLINES, &mut at), read_i16(OUTLINES, &mut at))
                else {
                    break;
                };
                let lon = lon_q as f64 / 32767.0 * 180.0;
                let lat = lat_q as f64 / 32767.0 * 90.0;
                ring.push(mercator(lon, lat));
            }
            if ring.len() >= 2 {
                rings.push(ring);
            }
        }
        Self { rings }
    }

    /// Draw every ring that intersects the view.
    ///
    /// `frame` maps Mercator into the layout's normalised space, and `project`
    /// maps that onto the screen. Two steps rather than one because the culling
    /// has to happen in between: `visible` is in layout space, and comparing
    /// raw Mercator bounds against it is a test between two different
    /// coordinate systems that silently rejects everything.
    pub fn draw(
        &self,
        painter: &Painter,
        visible: Rect,
        frame: crate::layout::Frame,
        project: impl Fn(Pos2) -> Pos2,
        color: Color32,
    ) {
        // A stroke thin enough to read as paper rather than as data. Below one
        // pixel egui's feathering does the rest, which is the right failure:
        // the coastline fades out as you zoom into a substation, exactly when
        // it has stopped being useful.
        let stroke = Stroke::new(0.8, color);

        for ring in &self.rings {
            // Culled by bounding box in layout space, not per segment. A world
            // outline is 282 rings and at a national zoom nearly all of them are
            // off screen, so one rejection per ring beats one per point.
            let framed: Vec<Pos2> = ring.iter().map(|p| frame.apply(*p)).collect();
            if !overlaps(&framed, visible) {
                continue;
            }
            painter.add(eframe::egui::Shape::line(
                framed.into_iter().map(&project).collect::<Vec<_>>(),
                stroke,
            ));
        }
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.rings.is_empty()
    }
}

/// Whether a ring's bounding box meets the visible rectangle.
fn overlaps(ring: &[Pos2], visible: Rect) -> bool {
    let (mut lo, mut hi) = (Pos2::new(f32::MAX, f32::MAX), Pos2::new(f32::MIN, f32::MIN));
    for p in ring {
        lo.x = lo.x.min(p.x);
        lo.y = lo.y.min(p.y);
        hi.x = hi.x.max(p.x);
        hi.y = hi.y.max(p.y);
    }
    Rect::from_min_max(lo, hi).intersects(visible)
}

/// The same projection `layout::project` uses, and it has to stay that way.
///
/// If these two ever disagree the coastline slides against the network, which
/// is the failure a shared projection exists to prevent. Duplicated rather than
/// shared because the layout works in `f64` degrees from a `Coord` and this
/// works from quantised integers; the formula is three lines and a test pins
/// them together.
fn mercator(lon: f64, lat: f64) -> Pos2 {
    let lat = lat.clamp(-85.051_129, 85.051_129).to_radians();
    let y = ((std::f64::consts::FRAC_PI_4 + lat / 2.0).tan()).ln();
    pos2(lon.to_radians() as f32, -y as f32)
}

fn read_u32(b: &[u8], at: &mut usize) -> Option<u32> {
    let v = b.get(*at..*at + 4)?;
    *at += 4;
    Some(u32::from_le_bytes(v.try_into().ok()?))
}

fn read_u16(b: &[u8], at: &mut usize) -> Option<u16> {
    let v = b.get(*at..*at + 2)?;
    *at += 2;
    Some(u16::from_le_bytes(v.try_into().ok()?))
}

fn read_i16(b: &[u8], at: &mut usize) -> Option<i16> {
    let v = b.get(*at..*at + 2)?;
    *at += 2;
    Some(i16::from_le_bytes(v.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_outlines_decode() {
        let m = Basemap::load();
        assert!(!m.is_empty(), "no rings decoded from the bundled blob");
        assert!(m.rings.len() > 100, "only {} rings", m.rings.len());
        assert!(
            m.rings.iter().all(|r| r.len() >= 2),
            "a ring too short to draw survived decoding",
        );
    }

    #[test]
    fn every_point_is_finite() {
        // A NaN here would propagate through the camera and take the whole
        // canvas with it, silently, because egui draws a NaN shape as nothing.
        for r in Basemap::load().rings {
            for p in r {
                assert!(p.x.is_finite() && p.y.is_finite(), "{p:?}");
            }
        }
    }

    #[test]
    fn the_projection_matches_the_layouts() {
        // The load-bearing invariant: if these drift, the coastline slides
        // against the network and both look plausible.
        for (lon, lat) in [
            (0.0, 0.0),
            (13.405, 52.52),   // Berlin
            (-74.006, 40.713), // New York
            (151.209, -33.868),// Sydney
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
    fn a_truncated_blob_does_not_panic() {
        // Decoding is all bounds-checked reads returning `None`, so a blob cut
        // short stops early rather than indexing past the end.
        let mut at = 0usize;
        assert!(read_u32(&[1, 2], &mut at).is_none());
        assert!(read_i16(&[], &mut at).is_none());
    }

    #[test]
    fn a_ring_off_screen_is_culled() {
        let ring = vec![pos2(10.0, 10.0), pos2(11.0, 11.0)];
        assert!(!overlaps(&ring, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))));
        assert!(overlaps(&ring, Rect::from_min_max(pos2(9.0, 9.0), pos2(12.0, 12.0))));
    }
}

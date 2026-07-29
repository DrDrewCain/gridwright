//! Where a bus goes on screen.
//!
//! Two ways, and which one is used depends on what the file said.
//!
//! **Geography, when the buses have any.** PyPSA carries longitude and latitude
//! and the readers now keep them, so for those networks there is a projection
//! to do rather than a picture to invent. Nothing beats the real map: an
//! engineer knows where their own network is, and a spring embedder's idea of a
//! good arrangement is not a thing anybody can recognise.
//!
//! **Fruchterman–Reingold, when they have none.** MATPOWER, PSS/E RAW, UCTE and
//! the IEEE cases have no coordinate field at all, and most of what people
//! actually load is one of those. A position then has to be invented from the
//! topology, and a spring embedder puts electrical closeness on the page, which
//! is the thing a person is looking for in a grid diagram when they cannot have
//! the map.
//!
//! **Both, when the file is half placed.** A real network with a handful of
//! synthetic buses added is the common case, not an exotic one. The located
//! buses are projected and then pinned, and the relaxation places the rest
//! around them.

use eframe::egui::{Pos2, Vec2, pos2};
use gridwright_net::Network;

/// Total pairwise-force evaluations one layout is allowed.
///
/// The relaxation is O(n²) per pass, so a fixed pass count means a 40-bus case
/// finishes instantly and a 1500-bus case stalls the frame that loaded it.
/// Fixing the *product* instead keeps the cost flat and lets the pass count fall
/// where it has to; small networks get many passes and settle properly, large
/// ones get few and stay closer to their seed ring.
const FORCE_BUDGET: usize = 8_000_000;

/// Above this the seed ring is kept as-is.
///
/// Not a rendering limit — the view draws far more than this — but the point
/// where even one relaxation pass is a visible hitch. A ring of a few thousand
/// buses is a poor diagram, and the honest fix is a layout that is not O(n²)
/// rather than a slower version of this one.
const MAX_RELAXED: usize = 2_000;

/// Positions for every bus, in a roughly unit-sized box centred on the origin.
///
/// Deterministic, which matters more than it sounds: this runs once per load,
/// and a randomised seed would mean the same file drawn differently every time
/// it was opened, so nobody could learn the shape of their own network.
pub fn layout(net: &Network) -> Placement {
    let n = net.buses.len();
    if n == 0 {
        return Placement {
            pos: Vec::new(),
            kind: Origin::Invented,
            frame: Frame::identity(),
        };
    }

    let placed = project(net);
    let located = placed.iter().filter(|p| p.is_some()).count();

    // Every bus located: there is nothing to invent, so the relaxation is
    // skipped entirely rather than run and then overwritten.
    if located == n {
        let mut pos: Vec<Pos2> = placed.into_iter().map(Option::unwrap).collect();
        let frame = normalise(&mut pos);
        // Spreading happens *after* the frame is taken, so the basemap follows
        // the projection rather than the nudges. That is the right way round: a
        // substation moved a few kilometres to stop it overlapping its
        // neighbour should not drag the coastline with it.
        spread(&mut pos, net);
        return Placement {
            pos,
            kind: Origin::Geographic,
            frame,
        };
    }
    let kind = if located == 0 {
        Origin::Invented
    } else {
        Origin::Mixed {
            located,
            total: n,
        }
    };

    let mut pos = seed_ring(n);
    // A located bus starts where it belongs and stays there. Seeding the
    // relaxation with real geography and then letting it move would produce a
    // map that is nearly right, which is worse than one that is obviously
    // schematic -- a reader trusts a map, and this one would be lying by a few
    // hundred kilometres.
    for (i, p) in placed.iter().enumerate() {
        if let Some(p) = p {
            pos[i] = *p;
        }
    }

    if !(2..=MAX_RELAXED).contains(&n) {
        return Placement {
            pos,
            kind,
            frame: Frame::identity(),
        };
    }

    // Links are drawn as edges too — an electrolyser tying a power bus to a
    // hydrogen bus is exactly as much a reason to place them near each other as
    // a transmission line is — so they pull here as well.
    let edges: Vec<(usize, usize)> = net
        .lines
        .iter()
        .map(|l| (l.bus0, l.bus1))
        .chain(net.links.iter().map(|l| (l.bus0, l.bus1)))
        .filter(|&(a, b)| a != b && a < n && b < n)
        .collect();

    // The ideal edge length for n nodes spread over a unit area.
    let k = (1.0 / n as f32).sqrt();
    let passes = (FORCE_BUDGET / (n * n)).clamp(30, 400);

    // Starts near the ring radius so the first pass can actually rearrange
    // things, and decays geometrically to roughly a hundredth of that, which is
    // below the point where further movement changes the picture.
    let mut temp = 0.25_f32;
    let cooling = 0.01_f32.powf(1.0 / passes as f32);

    let mut disp = vec![Vec2::ZERO; n];
    for _ in 0..passes {
        disp.fill(Vec2::ZERO);

        for i in 0..n {
            for j in (i + 1)..n {
                let d = pos[i] - pos[j];
                // Two buses at identical positions give a zero vector with no
                // direction to push along, so the floor is on the squared
                // length: it bounds the force and leaves the direction alone.
                let len_sq = d.length_sq().max(1e-9);
                let f = d * (k * k / len_sq);
                disp[i] += f;
                disp[j] -= f;
            }
        }

        for &(a, b) in &edges {
            let d = pos[a] - pos[b];
            let len = d.length().max(1e-6);
            let f = d * (len / k);
            disp[a] -= f;
            disp[b] += f;
        }

        for i in 0..n {
            if placed[i].is_some() {
                continue;
            }
            // Repulsion alone sends anything not held by an edge to infinity,
            // and disconnected components are common rather than exotic here:
            // asynchronous interconnections are joined only by HVDC, and a
            // carrier bus with no link yet is an island by construction.
            disp[i] -= pos[i].to_vec2() * 0.02;

            let len = disp[i].length();
            if len > 1e-9 {
                pos[i] += disp[i] * (temp.min(len) / len);
            }
        }

        temp *= cooling;
    }

    normalise(&mut pos);
    // A relaxed layout has no shared source space: the positions were invented,
    // so there is nothing for a basemap to line up with.
    Placement {
        pos,
        kind,
        frame: Frame::identity(),
    }
}

/// Where a bus goes, and whether that is a fact or an invention.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    pub pos: Vec<Pos2>,
    pub kind: Origin,
    /// How projected coordinates were mapped into the unit box. Only meaningful
    /// when `kind` is `Geographic`; anything else has no source space to share.
    pub frame: Frame,
}

/// What the positions on screen are actually derived from.
///
/// Carried out of this module rather than kept inside it because the reader has
/// to be told. A spring embedding looks exactly as authoritative as a map, and
/// a person who mistakes one for the other will draw conclusions about distance
/// and geography that the picture does not support. Nothing else in the diagram
/// can distinguish them, so the interface has to say it in words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Every bus came with a position, and this is a projection of the truth.
    Geographic,
    /// No bus came with a position; the arrangement is the topology relaxed.
    Invented,
    /// Some came placed and were pinned; the rest were arranged around them.
    Mixed { located: usize, total: usize },
}

impl Origin {
    /// A short phrase for the status strip.
    pub fn label(&self) -> String {
        match self {
            Origin::Geographic => "geographic".into(),
            Origin::Invented => "schematic".into(),
            Origin::Mixed { located, total } => {
                format!("schematic, {located} of {total} placed")
            }
        }
    }
}

/// Push apart substations that are too close together to draw.
///
/// A geographic layout has a problem a relaxation does not: real substations
/// cluster. Four of them within thirty kilometres are four legitimate points on
/// the map and one unreadable pile on the screen, with their symbols overlapping
/// and their labels colliding out of existence.
///
/// **Greedy, and deliberately not force-directed.** Birchfield and Overbye
/// measured both for exactly this problem: a greedy nudge left *"more than 90%
/// of substations not moved at all"* and never displaced one by more than 5 km,
/// where a force-directed pass moved some by 30 km and left *"almost no
/// substations untouched"*. On a map, being nearly right everywhere is worse
/// than being exactly right almost everywhere -- a reader trusts a map, and one
/// that has quietly relaxed is lying uniformly.
///
/// So: each bus is tested against those already placed, and only a bus that
/// actually collides moves, by the smallest push that separates it.
fn spread(pos: &mut [Pos2], net: &Network) {
    // In units of the normalised box, which is one across. This is roughly the
    // on-screen footprint of a busbar plus its label at a fit-to-window zoom --
    // below it the symbols themselves overlap, which is the thing being fixed.
    const MIN_GAP: f32 = 0.045;

    // Ordered by how much is attached, so the busiest substation keeps its true
    // position and the quiet ones move around it. A bus with six circuits and a
    // power station on it is the one a reader is orienting by.
    let mut weight = vec![0usize; pos.len()];
    for l in &net.lines {
        for b in [l.bus0, l.bus1] {
            if let Some(w) = weight.get_mut(b) {
                *w += 1;
            }
        }
    }
    let mut order: Vec<usize> = (0..pos.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(weight[i]));

    let mut settled: Vec<usize> = Vec::with_capacity(pos.len());
    for i in order {
        // A bounded search: eight directions at growing radius, taking the
        // first free spot. Bounded because an unbounded one on a dense national
        // model is how a "small nudge" becomes a relocation.
        'placed: for step in 0..6 {
            let r = MIN_GAP * (step as f32) * 0.55;
            for k in 0..8 {
                let a = std::f32::consts::TAU * k as f32 / 8.0;
                let at = if step == 0 {
                    pos[i]
                } else {
                    pos[i] + Vec2::new(r * a.cos(), r * a.sin())
                };
                if settled.iter().all(|&j| at.distance(pos[j]) >= MIN_GAP) {
                    pos[i] = at;
                    break 'placed;
                }
            }
        }
        settled.push(i);
    }
}

/// Project every located bus, and `None` for the rest.
///
/// **Web Mercator**, which is what every map tile in existence uses. That is
/// the whole argument: it is conformal, so a substation cluster keeps its
/// shape, and if a basemap is ever put underneath this it will line up without
/// a second projection to get wrong.
///
/// Its famous flaw is scale, not shape -- it inflates area with latitude, so a
/// Norwegian grid reads as larger than an Italian one of the same extent. For a
/// network diagram that is close to harmless: nobody measures distance off one,
/// and the alternative that fixes it (an equal-area projection) breaks the
/// shapes people recognise instead.
///
/// The poles go to infinity in this projection. Latitude is clamped to the
/// standard ±85.051129° before the transform, because the arctangent of an
/// infinity is a position that survives every later check and ruins the
/// bounding box for every other bus.
/// The projection, for one point.
///
/// Exposed so the basemap can be tested against it. If the two ever disagree
/// the coastline slides against the network, and both halves look plausible on
/// their own -- which is exactly the bug a shared formula is meant to prevent
/// and a shared *function* would prevent outright. They are separate because
/// this takes degrees from a `Coord` and the basemap takes quantised integers.
pub fn project_one(lon: f64, lat: f64) -> Pos2 {
    let lat = lat.clamp(-85.051_129, 85.051_129).to_radians();
    let y = ((std::f64::consts::FRAC_PI_4 + lat / 2.0).tan()).ln();
    pos2(lon.to_radians() as f32, -y as f32)
}

fn project(net: &Network) -> Vec<Option<Pos2>> {
    net.buses
        .iter()
        .map(|b| {
            // Negated inside `project_one`, because screen y grows downward
            // and latitude grows up. Without that every map is drawn upside
            // down, which is obvious on a recognisable coastline and completely
            // invisible on a synthetic case.
            b.position.map(|c| project_one(c.lon, c.lat))
        })
        .collect()
}

/// A circle, walked in index order.
///
/// Any deterministic seed would do, but a ring has no two nodes on top of each
/// other, which is the one starting condition the relaxation cannot recover
/// from cleanly.
fn seed_ring(n: usize) -> Vec<Pos2> {
    let r = 0.5;
    (0..n)
        .map(|i| {
            let a = std::f32::consts::TAU * i as f32 / n as f32;
            pos2(r * a.cos(), r * a.sin())
        })
        .collect()
}

/// Rescale into a unit box centred on the origin.
///
/// The view's fit-to-window works off the bounding box either way; this just
/// means the initial zoom is the same order of magnitude whatever the network,
/// so a file that happens to relax into a wide sparse shape does not open at a
/// zoom level three decades away from a compact one.
fn normalise(pos: &mut [Pos2]) -> Frame {
    let (mut lo, mut hi) = (Vec2::splat(f32::MAX), Vec2::splat(f32::MIN));
    for p in pos.iter() {
        lo = lo.min(p.to_vec2());
        hi = hi.max(p.to_vec2());
    }

    let span = hi - lo;
    let scale = 1.0 / span.x.max(span.y).max(1e-6);
    let centre = ((lo + hi) * 0.5).to_pos2();
    for p in pos.iter_mut() {
        *p = ((*p - centre) * scale).to_pos2();
    }
    Frame { centre, scale }
}

/// The similarity transform `normalise` applied, so anything drawn in the same
/// source space can follow the layout into it.
///
/// This exists for the basemap. Positions are normalised into a unit box after
/// projection, so a coastline in raw Mercator would be drawn at a wildly
/// different scale and offset from the network it is meant to sit under -- both
/// halves correct, the picture nonsense.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    centre: Pos2,
    scale: f32,
}

impl Frame {
    /// The identity, for a layout that never projected anything.
    pub fn identity() -> Self {
        Self {
            centre: pos2(0.0, 0.0),
            scale: 1.0,
        }
    }

    pub fn apply(&self, p: Pos2) -> Pos2 {
        ((p - self.centre) * self.scale).to_pos2()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gridwright_net::{Line, Snapshots};

    fn ring_network(n: usize) -> Network {
        let mut net = Network::new(Snapshots::hourly(1));
        for i in 0..n {
            net.add_bus(format!("b{i}"), "XX");
        }
        for i in 0..n {
            net.add_line(Line {
                name: format!("l{i}"),
                bus0: i,
                bus1: (i + 1) % n,
                s_nom: 100.0,
                susceptance: 10.0,
                ..Default::default()
            });
        }
        net
    }

    /// The property the view depends on: open the same file twice and it is the
    /// same picture. Anything seeded from the clock or from a hasher that varies
    /// per process would pass a single run and fail this.
    #[test]
    fn deterministic() {
        let net = ring_network(24);
        assert_eq!(layout(&net).pos, layout(&net).pos);
    }

    /// `NetworkView::fit` divides by the bounding-box span, so an unbounded or
    /// degenerate layout does not merely look wrong, it produces a camera with
    /// an infinite or NaN zoom.
    #[test]
    fn finite_and_bounded() {
        for n in [1, 2, 3, 40, 300] {
            let pos = layout(&ring_network(n)).pos;
            assert_eq!(pos.len(), n);
            for p in &pos {
                assert!(p.x.is_finite() && p.y.is_finite(), "n = {n}");
                assert!(p.x.abs() <= 1.0 && p.y.abs() <= 1.0, "n = {n}");
            }
        }
    }

    /// Buses with no line and no link between them are a normal case here —
    /// asynchronous interconnections, and carrier buses not yet coupled — and
    /// they are the case where repulsion has nothing pulling back against it.
    #[test]
    fn disconnected_buses_stay_bounded() {
        let mut net = Network::new(Snapshots::hourly(1));
        for i in 0..50 {
            net.add_bus(format!("island{i}"), "XX");
        }
        for p in layout(&net).pos {
            assert!(p.x.is_finite() && p.y.is_finite());
        }
    }

    #[test]
    fn empty_network_has_no_positions() {
        assert!(layout(&Network::new(Snapshots::hourly(1))).pos.is_empty());
    }
}

#[cfg(test)]
mod geographic_tests {
    use super::*;
    use gridwright_net::{Bus, Coord, Snapshots};

    fn placed(coords: &[Option<(f64, f64)>]) -> Network {
        let mut net = Network::new(Snapshots::hourly(1));
        for (i, c) in coords.iter().enumerate() {
            net.buses.push(Bus {
                name: format!("b{i}"),
                position: c.and_then(|(lon, lat)| Coord::new(lon, lat)),
                ..Default::default()
            });
        }
        net
    }

    #[test]
    fn north_is_up() {
        // Screen y grows downward and latitude grows up, so the projection has
        // to flip. Getting this wrong draws every map upside down, which is
        // glaring on a recognisable coastline and invisible on a synthetic case
        // -- so it is worth a test rather than an eye.
        let net = placed(&[Some((0.0, 60.0)), Some((0.0, 40.0))]);
        let pos = layout(&net).pos;
        assert!(pos[0].y < pos[1].y, "the northern bus was drawn below");
    }

    #[test]
    fn east_is_right() {
        let net = placed(&[Some((-5.0, 50.0)), Some((5.0, 50.0))]);
        let pos = layout(&net).pos;
        assert!(pos[0].x < pos[1].x, "the eastern bus was drawn to the left");
    }

    #[test]
    fn a_fully_placed_network_keeps_its_shape() {
        // Three buses in a right angle, projected over a small extent where
        // Mercator distortion is negligible. If the relaxation had been allowed
        // to run, this would not survive.
        let net = placed(&[
            Some((0.0, 50.0)),
            Some((1.0, 50.0)),
            Some((0.0, 50.0 + 0.6428)),
        ]);
        let pos = layout(&net).pos;
        let horizontal = (pos[1] - pos[0]).length();
        let vertical = (pos[2] - pos[0]).length();
        assert!(
            (horizontal / vertical - 1.0).abs() < 0.05,
            "aspect was distorted: {horizontal} across against {vertical} up",
        );
    }

    #[test]
    fn an_unplaced_network_still_gets_a_layout() {
        let net = placed(&[None, None, None]);
        let pos = layout(&net).pos;
        assert_eq!(pos.len(), 3);
        assert!(pos.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
    }

    #[test]
    fn placed_buses_do_not_move_when_others_are_relaxed() {
        // The mixed case: a real network with synthetic buses bolted on. The
        // located ones are the frame everything else is arranged around, and a
        // map that is nearly right is worse than one that is plainly schematic.
        let net = placed(&[Some((0.0, 50.0)), Some((2.0, 50.0)), None, None]);
        let pos = layout(&net).pos;

        // Both anchors kept the same latitude, so they must still share a row.
        assert!(
            (pos[0].y - pos[1].y).abs() < 1e-4,
            "anchors drifted apart vertically: {} against {}",
            pos[0].y,
            pos[1].y,
        );
        assert!(pos[0].x < pos[1].x, "anchors swapped sides");
    }

    #[test]
    fn a_bus_at_the_pole_does_not_take_the_layout_with_it() {
        // Mercator sends the poles to infinity. Without the clamp this produces
        // a non-finite position that survives into the bounding box and
        // collapses every other bus onto one point.
        let net = placed(&[Some((0.0, 90.0)), Some((0.0, 50.0)), Some((1.0, 50.0))]);
        let pos = layout(&net).pos;
        assert!(
            pos.iter().all(|p| p.x.is_finite() && p.y.is_finite()),
            "a pole produced {pos:?}",
        );
        assert!(pos[1].distance(pos[2]) > 1e-6, "the other buses collapsed");
    }
}

#[cfg(test)]
mod spread_tests {
    use super::*;
    use gridwright_net::{Bus, Coord, Line, Snapshots};

    /// Four substations within a few kilometres, as real ones cluster.
    fn cluster() -> Network {
        let mut net = Network::new(Snapshots::hourly(1));
        for (i, (lon, lat)) in [
            (9.10, 49.15),
            (9.20, 48.90),
            (9.37, 48.66),
            (9.28, 48.54),
            // One far away, so the bounding box is not the cluster itself.
            (10.0, 53.5),
        ]
        .into_iter()
        .enumerate()
        {
            net.buses.push(Bus {
                name: format!("s{i}"),
                position: Coord::new(lon, lat),
                ..Default::default()
            });
        }
        for (a, b) in [(0, 1), (1, 2), (2, 3), (0, 4)] {
            net.lines.push(Line {
                bus0: a,
                bus1: b,
                s_nom: 100.0,
                ..Default::default()
            });
        }
        net
    }

    #[test]
    fn clustered_substations_end_up_far_enough_apart_to_draw() {
        let pos = layout(&cluster()).pos;
        for i in 0..pos.len() {
            for j in (i + 1)..pos.len() {
                let d = pos[i].distance(pos[j]);
                assert!(d >= 0.04, "buses {i} and {j} are {d} apart, still a pile");
            }
        }
    }

    #[test]
    fn the_layout_stays_geographic_and_bounded() {
        let placed = layout(&cluster());
        assert_eq!(placed.kind, Origin::Geographic);
        for p in &placed.pos {
            assert!(p.x.is_finite() && p.y.is_finite());
            // Spreading happens after normalisation, so it can push slightly
            // outside the unit box; the camera fits to the bounding box either
            // way, but an unbounded push would mean the search never converged.
            assert!(p.x.abs() <= 2.0 && p.y.abs() <= 2.0, "{p:?} escaped");
        }
    }

    #[test]
    fn the_relative_arrangement_survives() {
        // The whole reason for a greedy nudge rather than a relaxation: the
        // northern bus must still be north of the southern ones afterwards.
        let pos = layout(&cluster()).pos;
        let north = pos[4];
        for s in &pos[..4] {
            assert!(north.y < s.y, "the northern bus moved south of a cluster bus");
        }
    }

    #[test]
    fn spreading_is_deterministic() {
        assert_eq!(layout(&cluster()).pos, layout(&cluster()).pos);
    }
}

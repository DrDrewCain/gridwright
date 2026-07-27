//! Where a bus goes on screen.
//!
//! `gridwright_net::Bus` carries a country and a synchronous area and a nominal
//! voltage, and no coordinates at all. That is a reasonable thing for an
//! optimisation model to omit — nothing in a linear program cares where a node
//! is — but it means there is no projection to do here. A position has to be
//! invented from the topology.
//!
//! Fruchterman–Reingold, because the thing a person is looking for in a grid
//! diagram is which nodes are electrically close to which, and a spring embedder
//! puts exactly that on the page. Geographic layout was the obvious
//! alternative and is not available: no reader in `gridwright-io` produces
//! coordinates, because none of the formats it reads are required to carry them
//! (MATPOWER and PSS/E have no coordinate field at all). If a reader ever does,
//! this module should be bypassed rather than tuned.

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
pub fn layout(net: &Network) -> Vec<Pos2> {
    let n = net.buses.len();
    if n == 0 {
        return Vec::new();
    }

    let mut pos = seed_ring(n);
    if !(2..=MAX_RELAXED).contains(&n) {
        return pos;
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
    pos
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
fn normalise(pos: &mut [Pos2]) {
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
        assert_eq!(layout(&net), layout(&net));
    }

    /// `NetworkView::fit` divides by the bounding-box span, so an unbounded or
    /// degenerate layout does not merely look wrong, it produces a camera with
    /// an infinite or NaN zoom.
    #[test]
    fn finite_and_bounded() {
        for n in [1, 2, 3, 40, 300] {
            let pos = layout(&ring_network(n));
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
        for p in layout(&net) {
            assert!(p.x.is_finite() && p.y.is_finite());
        }
    }

    #[test]
    fn empty_network_has_no_positions() {
        assert!(layout(&Network::new(Snapshots::hourly(1))).is_empty());
    }
}

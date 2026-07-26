//! Cycle constraints, which tighten the Jabr relaxation on meshed networks.
//!
//! # The problem they fix
//!
//! Jabr constrains each line independently. On a radial network that is enough,
//! because a tree fixes every angle difference uniquely once the root is
//! chosen. A meshed network has cycles, and around a cycle the relaxation is
//! free to pick angle differences that do not add up — it can route power along
//! two paths in ways no set of voltage angles could produce. That freedom is
//! exactly the looseness, and transmission planning lives in meshed networks.
//!
//! # The constraint
//!
//! Write `W_ij = R_ij + i·I_ij`, which is `V_i · conj(V_j)`. Around a closed
//! cycle the product telescopes:
//!
//! ```text
//!   W_ij · W_jk · W_ki  =  |V_i|² |V_j|² |V_k|²  =  u_i · u_j · u_k
//! ```
//!
//! The right hand side is real and non-negative, so the constraint splits into
//! a real part and, more usefully, the requirement that the **imaginary part
//! vanishes**. That is the algebraic form of "angle differences sum to zero
//! around a loop", without any arctangents in sight.
//!
//! For a triangle, expanding `Im(W₁W₂W₃)` gives
//!
//! ```text
//!   R₁R₂I₃ + R₁I₂R₃ + I₁R₂R₃ − I₁I₂I₃ = 0
//! ```
//!
//! # Why this is not simply added
//!
//! That expression is trilinear, so the constraint is nonconvex and cannot go
//! into a conic solver as written. What can go in is its **convex envelope**.
//! Each product is replaced by an auxiliary variable pinned between McCormick
//! bounds, which are the tightest linear over- and under-estimators of a
//! bilinear term given bounds on its factors. Trilinear terms are handled by
//! applying the construction twice.
//!
//! The result is still a relaxation, and still a lower bound, but a tighter one
//! than plain Jabr: it removes cycle-inconsistent points that Jabr admits. It
//! does not make the relaxation exact, and nothing short of spatial branch and
//! bound would.
//!
//! Following Riccardi, Bernardelli and Gualandi, *Theoretical Perspectives on
//! Jabr-Type Convex Relaxations for AC Optimal Power Flow*, arXiv:2604.00664,
//! which sets cycle constraints, convex envelopes and dual reformulations in
//! one frame of multilinear equalities.
//!
//! # Scope
//!
//! Triangles only. Longer fundamental cycles obey the same identity, but the
//! product has more factors and the envelope tower grows with it, so the
//! auxiliary variables multiply faster than the tightening repays. Triangles
//! are where the gain per variable is highest and are common in meshed
//! transmission. Longer cycles are left to the branch-and-bound work that would
//! be needed to close the gap properly anyway.

use gridwright_net::Network;

/// A triangle in the network graph, as three line indices with orientation.
///
/// `forward` records whether each line runs with the cycle or against it, since
/// `W_ji` is the conjugate of `W_ij` and traversing a line backwards flips the
/// sign of its imaginary part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Triangle {
    pub lines: [usize; 3],
    pub forward: [bool; 3],
    pub buses: [usize; 3],
}

/// A fundamental cycle of any length, as line indices with orientation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    pub lines: Vec<usize>,
    /// Whether each line runs with the cycle or against it. Traversing a line
    /// backwards conjugates its `W`, which flips the sign of its imaginary
    /// part.
    pub forward: Vec<bool>,
    pub buses: Vec<usize>,
}

impl Cycle {
    pub fn len(&self) -> usize {
        self.lines.len()
    }
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// Fundamental cycles, up to a given length.
///
/// A spanning forest is grown over the AC lines; every line it does not use
/// closes exactly one cycle, made of that line and the tree path between its
/// ends. Those cycles are a basis of the cycle space, so constraining them
/// constrains every cycle: any other is a combination of these, and the angle
/// identity is additive around combinations.
///
/// That basis property is what makes this worth doing rather than enumerating
/// cycles, of which a meshed network has exponentially many.
pub fn find_cycles(net: &Network, max_len: usize, limit: usize) -> Vec<Cycle> {
    let n = net.buses.len();
    let mut adj: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    for (l, line) in net.lines.iter().enumerate() {
        if line.is_transport() || line.bus0 == line.bus1 {
            continue;
        }
        if net.buses[line.bus0].synchronous_area != net.buses[line.bus1].synchronous_area {
            continue;
        }
        adj[line.bus0].push((line.bus1, l));
        adj[line.bus1].push((line.bus0, l));
    }

    // Breadth-first, so the tree paths are shortest and the cycles it finds are
    // the shortest available through each closing line. A depth-first tree
    // would produce a valid basis of much longer cycles, and length is what
    // costs variables here.
    let mut parent: Vec<Option<(usize, usize)>> = vec![None; n];
    let mut depth = vec![0usize; n];
    let mut seen = vec![false; n];
    let mut tree_line = vec![false; net.lines.len()];
    let mut order = Vec::with_capacity(n);
    for root in 0..n {
        if seen[root] || adj[root].is_empty() {
            continue;
        }
        seen[root] = true;
        let mut queue = std::collections::VecDeque::from([root]);
        while let Some(b) = queue.pop_front() {
            order.push(b);
            for &(next, l) in &adj[b] {
                if !seen[next] {
                    seen[next] = true;
                    parent[next] = Some((b, l));
                    depth[next] = depth[b] + 1;
                    tree_line[l] = true;
                    queue.push_back(next);
                }
            }
        }
    }

    let mut out = Vec::new();
    for (l, line) in net.lines.iter().enumerate() {
        if tree_line[l] || line.is_transport() || line.bus0 == line.bus1 {
            continue;
        }
        if net.buses[line.bus0].synchronous_area != net.buses[line.bus1].synchronous_area {
            continue;
        }
        if !seen[line.bus0] || !seen[line.bus1] {
            continue;
        }
        let Some(cycle) = close(net, line.bus0, line.bus1, l, &parent, &depth, max_len) else {
            continue;
        };
        out.push(cycle);
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Walk both ends up to their common ancestor and join them with the closing
/// line.
fn close(
    net: &Network,
    a: usize,
    b: usize,
    closing: usize,
    parent: &[Option<(usize, usize)>],
    depth: &[usize],
    max_len: usize,
) -> Option<Cycle> {
    let (mut x, mut y) = (a, b);
    let mut up: Vec<(usize, usize)> = Vec::new();
    let mut down: Vec<(usize, usize)> = Vec::new();
    while depth[x] > depth[y] {
        let (p, l) = parent[x]?;
        up.push((x, l));
        x = p;
    }
    while depth[y] > depth[x] {
        let (p, l) = parent[y]?;
        down.push((y, l));
        y = p;
    }
    while x != y {
        let (px, lx) = parent[x]?;
        let (py, ly) = parent[y]?;
        up.push((x, lx));
        down.push((y, ly));
        x = px;
        y = py;
    }
    // The path is a -> … -> meet -> … -> b, then the closing line back to a.
    let mut buses = vec![a];
    let mut lines = Vec::new();
    for &(node, l) in &up {
        let _ = node;
        lines.push(l);
        let (p, _) = parent[if buses.is_empty() { a } else { *buses.last().unwrap() }]?;
        buses.push(p);
    }
    for &(node, l) in down.iter().rev() {
        lines.push(l);
        buses.push(node);
    }
    lines.push(closing);
    if lines.len() < 3 || lines.len() > max_len {
        return None;
    }
    // Orientation: each line runs with the cycle when its `bus0` is the node
    // the cycle arrives from.
    let forward = lines
        .iter()
        .enumerate()
        .map(|(k, &l)| net.lines[l].bus0 == buses[k])
        .collect();
    Some(Cycle {
        lines,
        forward,
        buses,
    })
}

/// Find triangles among the AC lines.
///
/// Deliberately not every cycle: see the module note. Parallel lines between
/// the same pair are skipped, since a "triangle" using two of them is a
/// two-bus loop whose constraint the Jabr cone already implies.
pub fn find_triangles(net: &Network, limit: usize) -> Vec<Triangle> {
    let n = net.buses.len();
    // Adjacency as (neighbour, line index), AC lines only.
    let mut adj: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    for (l, line) in net.lines.iter().enumerate() {
        if line.is_transport() || line.bus0 == line.bus1 {
            continue;
        }
        if net.buses[line.bus0].synchronous_area != net.buses[line.bus1].synchronous_area {
            continue;
        }
        adj[line.bus0].push((line.bus1, l));
        adj[line.bus1].push((line.bus0, l));
    }

    let mut out = Vec::new();
    // Each triangle is found once by requiring i < j < k.
    for i in 0..n {
        for &(j, l_ij) in &adj[i] {
            if j <= i {
                continue;
            }
            for &(k, l_jk) in &adj[j] {
                if k <= j {
                    continue;
                }
                for &(back, l_ki) in &adj[k] {
                    if back != i {
                        continue;
                    }
                    // Orientation: does each line run i->j, j->k, k->i?
                    let f_ij = net.lines[l_ij].bus0 == i;
                    let f_jk = net.lines[l_jk].bus0 == j;
                    let f_ki = net.lines[l_ki].bus0 == k;
                    out.push(Triangle {
                        lines: [l_ij, l_jk, l_ki],
                        forward: [f_ij, f_jk, f_ki],
                        buses: [i, j, k],
                    });
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
    }
    out
}

/// The box a single branch's Jabr variables are confined to, and the voltage
/// ranges of the buses it joins.
///
/// The whole point of spatial branch and bound is that these are not fixed:
/// splitting one and solving both halves gives a bound at least as good as the
/// parent's, and usually better, because every envelope and every secant below
/// is drawn over the box it is given.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RiBox {
    pub r: (f64, f64),
    pub i: (f64, f64),
}

/// The full domain of a problem: one box per line, one voltage-squared range
/// per bus.
#[derive(Debug, Clone, PartialEq)]
pub struct Domain {
    pub lines: Vec<RiBox>,
    /// `u = |V|²` per bus.
    pub u: Vec<(f64, f64)>,
}

impl Domain {
    /// The root domain, from the network's own voltage limits.
    pub fn root(net: &Network) -> Self {
        Self {
            lines: (0..net.lines.len())
                .map(|l| {
                    let (lo, hi) = ri_bounds(net, l);
                    RiBox {
                        r: (lo, hi),
                        i: (lo, hi),
                    }
                })
                .collect(),
            u: net
                .buses
                .iter()
                .map(|b| (b.v_min * b.v_min, b.v_max * b.v_max))
                .collect(),
        }
    }

    /// Total width, as a crude measure of how much is left to explore.
    pub fn width(&self) -> f64 {
        self.lines
            .iter()
            .map(|b| (b.r.1 - b.r.0) + (b.i.1 - b.i.0))
            .sum::<f64>()
            + self.u.iter().map(|(lo, hi)| hi - lo).sum::<f64>()
    }
}

/// The secant of `x²` over `[lo, hi]`, as `(coefficient, constant)`.
///
/// Returns `(lo + hi, −lo·hi)` so that the secant is `(lo+hi)·x − lo·hi`. It
/// equals `x²` at both ends and lies above it in between, since `x²` is
/// convex. That makes it a valid **over**estimator, which is the direction
/// needed to relax `R² + I² ≥ u_i u_j`: replacing the left side by something
/// larger can only admit more points, never exclude a feasible one.
///
/// As the interval closes the secant collapses onto the parabola, so the
/// relaxation approaches the exact constraint. That convergence is what makes
/// splitting boxes worth doing.
pub fn secant(lo: f64, hi: f64) -> (f64, f64) {
    (lo + hi, -lo * hi)
}

/// Bounds on `R` and `I` for a line, from the voltage bands of its ends.
///
/// `|R|` and `|I|` are each at most `|V_i||V_j|`, since they are that product
/// times a cosine or a sine. Those bounds are what make the envelopes finite,
/// and a network without voltage limits would have none.
pub fn ri_bounds(net: &Network, line: usize) -> (f64, f64) {
    let l = &net.lines[line];
    let hi = net.buses[l.bus0].v_max * net.buses[l.bus1].v_max;
    (-hi, hi)
}

/// McCormick envelope for `w = x · y`, given bounds on `x` and `y`.
///
/// Returns the four inequalities as `(coefficient on x, on y, on w, rhs)` for
/// rows of the form `ax + by + cw ≤ rhs`. These are the convex hull of the
/// bilinear surface over the box, which is the tightest linear relaxation
/// available without splitting the box further.
pub fn mccormick(xl: f64, xu: f64, yl: f64, yu: f64) -> [(f64, f64, f64, f64); 4] {
    [
        // w >= xl*y + x*yl - xl*yl   ->   x*yl + xl*y - w <= xl*yl
        (yl, xl, -1.0, xl * yl),
        // w >= xu*y + x*yu - xu*yu   ->   x*yu + xu*y - w <= xu*yu
        (yu, xu, -1.0, xu * yu),
        // w <= xu*y + x*yl - xu*yl   ->   w - x*yl - xu*y <= -xu*yl
        (-yl, -xu, 1.0, -xu * yl),
        // w <= x*yu + xl*y - xl*yu   ->   w - x*yu - xl*y <= -xl*yu
        (-yu, -xl, 1.0, -xl * yu),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use gridwright_net::{Line, Snapshots};

    fn triangle_net() -> Network {
        let mut net = Network::new(Snapshots::hourly(1));
        let a = net.add_bus("A", "X");
        let b = net.add_bus("B", "X");
        let c = net.add_bus("C", "X");
        for (n0, n1) in [(a, b), (b, c), (c, a)] {
            net.add_line(Line {
                name: format!("{n0}{n1}"),
                bus0: n0,
                bus1: n1,
                s_nom: 100.0,
                susceptance: 1.0,
                resistance: 0.01,
                reactance: 0.1,
                ..Default::default()
            });
        }
        net
    }

    #[test]
    fn a_triangle_is_found_exactly_once() {
        let t = find_triangles(&triangle_net(), 100);
        assert_eq!(t.len(), 1, "found {t:?}");
        assert_eq!(t[0].buses, [0, 1, 2]);
    }

    #[test]
    fn a_radial_network_has_no_triangles() {
        let mut net = triangle_net();
        net.lines.pop();
        assert!(find_triangles(&net, 100).is_empty());
    }

    #[test]
    fn transport_corridors_are_not_part_of_a_cycle_constraint() {
        // An HVDC tie carries no angle relationship, so a loop through it is
        // not a loop the cycle identity applies to.
        let mut net = triangle_net();
        net.lines[2].susceptance = 0.0;
        net.lines[2].reactance = 0.0;
        assert!(find_triangles(&net, 100).is_empty());
    }

    #[test]
    fn orientation_is_recorded_for_each_edge() {
        let mut net = triangle_net();
        // Flip one line so it runs against the cycle direction.
        let l = &mut net.lines[0];
        std::mem::swap(&mut l.bus0, &mut l.bus1);
        let t = find_triangles(&net, 100);
        assert_eq!(t.len(), 1);
        assert!(!t[0].forward[0], "reversed line should be marked backwards");
    }

    #[test]
    fn the_search_respects_its_limit() {
        // A complete graph on five buses has ten triangles; asking for three
        // must stop at three rather than building them all.
        let mut net = Network::new(Snapshots::hourly(1));
        for i in 0..5 {
            net.add_bus(format!("b{i}"), "X");
        }
        for i in 0..5 {
            for j in (i + 1)..5 {
                net.add_line(Line {
                    name: format!("{i}{j}"),
                    bus0: i,
                    bus1: j,
                    s_nom: 100.0,
                    susceptance: 1.0,
                    resistance: 0.01,
                    reactance: 0.1,
                    ..Default::default()
                });
            }
        }
        assert_eq!(find_triangles(&net, 3).len(), 3);
        assert_eq!(find_triangles(&net, 100).len(), 10);
    }

    #[test]
    fn mccormick_is_exact_at_the_corners_of_the_box() {
        // The envelope must reproduce the product exactly where the factors sit
        // at their bounds; that is the property that makes it a hull rather
        // than merely a bound.
        let (xl, xu, yl, yu) = (-2.0, 3.0, -1.0, 4.0);
        let rows = mccormick(xl, xu, yl, yu);
        for (x, y) in [(xl, yl), (xl, yu), (xu, yl), (xu, yu)] {
            let w = x * y;
            for (a, b, c, rhs) in rows {
                let lhs = a * x + b * y + c * w;
                assert!(
                    lhs <= rhs + 1e-9,
                    "corner ({x}, {y}) violated an envelope row: {lhs} > {rhs}"
                );
            }
        }
    }

    #[test]
    fn mccormick_excludes_a_point_beyond_the_envelope() {
        // A product that no point in the box could produce must be cut off.
        let rows = mccormick(-1.0, 1.0, -1.0, 1.0);
        for (x, y, w) in [(0.0, 0.0, 1.5), (1.0, 1.0, 2.0), (0.5, 0.5, -1.5)] {
            let violated = rows
                .iter()
                .any(|&(a, b, c, rhs)| a * x + b * y + c * w > rhs + 1e-9);
            assert!(violated, "({x}, {y}, {w}) should have been cut off");
        }
    }

    #[test]
    fn mccormick_is_loose_in_the_interior_and_that_is_expected() {
        // Worth pinning down rather than discovering later. At the centre of a
        // symmetric box the envelope permits w up to xu*yu even though the true
        // product is zero. This is the known weakness of McCormick and the
        // reason tightening it needs the box split, which is what spatial
        // branch and bound does. A cycle constraint built on these envelopes
        // therefore removes some cycle-inconsistent points and not all of them.
        let rows = mccormick(-1.0, 1.0, -1.0, 1.0);
        let (x, y, w) = (0.0, 0.0, 0.9);
        let violated = rows
            .iter()
            .any(|&(a, b, c, rhs)| a * x + b * y + c * w > rhs + 1e-9);
        assert!(
            !violated,
            "the envelope is expected to admit this point; if it no longer does, \
             the construction has changed and the tightening claims need revisiting"
        );
    }

    #[test]
    fn ri_bounds_follow_the_voltage_band() {
        let mut net = triangle_net();
        net.buses[0].v_max = 1.1;
        net.buses[1].v_max = 1.2;
        let (lo, hi) = ri_bounds(&net, 0);
        assert!((hi - 1.32).abs() < 1e-9, "got {hi}");
        assert!((lo + 1.32).abs() < 1e-9, "got {lo}");
    }
}

#[cfg(test)]
mod cycle_tests {
    use super::*;
    use gridwright_net::{Line, Snapshots};

    /// A ring of `n` buses, which has exactly one fundamental cycle however
    /// long it is.
    fn ring(n: usize) -> Network {
        let mut net = Network::new(Snapshots::hourly(1));
        for i in 0..n {
            net.add_bus(format!("b{i}"), "X");
        }
        for i in 0..n {
            net.add_line(Line {
                name: format!("l{i}"),
                bus0: i,
                bus1: (i + 1) % n,
                s_nom: 100.0,
                susceptance: 10.0,
                reactance: 0.1,
                ..Default::default()
            });
        }
        net
    }

    #[test]
    fn a_ring_has_exactly_one_fundamental_cycle() {
        // Whatever its length. A cycle basis has one element per line outside
        // the spanning tree, and a ring has exactly one.
        for n in [3usize, 4, 5, 8] {
            let cycles = find_cycles(&ring(n), 32, 100);
            assert_eq!(cycles.len(), 1, "ring of {n}: {cycles:?}");
            assert_eq!(cycles[0].len(), n, "ring of {n} should give a cycle of {n}");
        }
    }

    #[test]
    fn a_tree_has_no_cycles() {
        let mut net = ring(6);
        net.lines.pop();
        assert!(find_cycles(&net, 32, 100).is_empty());
    }

    #[test]
    fn the_basis_has_one_cycle_per_line_outside_the_spanning_tree() {
        // The property that makes constraining a basis enough: `edges - nodes +
        // components` is the dimension of the cycle space, and every cycle in
        // the graph is a combination of the basis.
        let mut net = ring(6);
        // Two chords, so eight lines over six buses in one component.
        for (a, b) in [(0usize, 3usize), (1, 4)] {
            net.add_line(Line {
                name: format!("chord{a}{b}"),
                bus0: a,
                bus1: b,
                s_nom: 100.0,
                susceptance: 10.0,
                reactance: 0.1,
                ..Default::default()
            });
        }
        let cycles = find_cycles(&net, 32, 100);
        assert_eq!(cycles.len(), 8 - 6 + 1, "{cycles:?}");
    }

    #[test]
    fn a_length_limit_drops_the_cycles_beyond_it() {
        // Longer cycles cost more variables than the tightening repays, so the
        // limit is the knob that trades one against the other.
        assert!(find_cycles(&ring(8), 4, 100).is_empty());
        assert_eq!(find_cycles(&ring(8), 8, 100).len(), 1);
    }

    #[test]
    fn a_cycle_closes_on_itself() {
        // Each line has to join the bus before it to the bus after it, or the
        // identity being imposed is about some other loop entirely.
        for n in [3usize, 5, 7] {
            let net = ring(n);
            let c = &find_cycles(&net, 32, 100)[0];
            assert_eq!(c.buses.len(), c.lines.len());
            for k in 0..c.len() {
                let line = &net.lines[c.lines[k]];
                let from = c.buses[k];
                let to = c.buses[(k + 1) % c.len()];
                let (a, b) = (line.bus0, line.bus1);
                assert!(
                    (a == from && b == to) || (b == from && a == to),
                    "ring of {n}: line {k} joins {a}-{b}, not {from}-{to}"
                );
                assert_eq!(c.forward[k], line.bus0 == from, "orientation at {k}");
            }
        }
    }

    #[test]
    fn transport_corridors_and_separate_areas_are_left_out() {
        // A corridor carries no angle relationship, and two synchronous areas
        // have no comparable angles at all, so neither can close a cycle the
        // identity applies to.
        let mut net = ring(4);
        net.lines[0].susceptance = 0.0;
        net.lines[0].reactance = 0.0;
        assert!(find_cycles(&net, 32, 100).is_empty());

        let mut net = ring(4);
        net.buses[2].synchronous_area = "other".into();
        assert!(find_cycles(&net, 32, 100).is_empty());
    }

    #[test]
    fn a_real_meshed_network_gives_the_expected_number() {
        // IEEE 14 has 20 branches over 14 buses in one component, so its cycle
        // space has dimension seven.
        let net = gridwright_io::matpower::load_case(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../examples/pglib/case14_ieee.m"),
        )
        .unwrap()
        .network;
        let cycles = find_cycles(&net, 64, 1000);
        assert_eq!(cycles.len(), 20 - 14 + 1, "{} found", cycles.len());
    }
}

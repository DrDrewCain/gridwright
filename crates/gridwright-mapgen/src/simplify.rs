//! Line simplification, and the two cleanups that have to happen around it.
//!
//! Douglas–Peucker, iterative rather than recursive: a coastline part can carry
//! tens of thousands of points, and the recursive form is a stack overflow
//! waiting for the worst-case split.
//!
//! The tolerance is in **degrees**, which is not a distance. A degree of
//! longitude is 111 km at the equator and 30 km at 60°N, so a fixed tolerance
//! simplifies high latitudes more aggressively than low ones. That is deliberate
//! and worth stating: the output is drawn in Web Mercator, which stretches high
//! latitudes by close to the reciprocal factor, so the two distortions largely
//! cancel and the on-screen error stays roughly uniform. Simplifying in metres
//! would leave visibly over-detailed coastlines near the poles.
//!
//! **Douglas–Peucker is not topology preserving, and that is not a footnote.** It
//! judges each point against the chord its neighbours span, in isolation, and
//! never asks whether that chord crosses some other part of the same ring. On a
//! coastline with a narrow peninsula, flattening the neck can throw the chord
//! straight across the water on the far side. Measured on Natural Earth's
//! Afro-Eurasia part: the source ring has 81,512 points and **zero**
//! self-crossings, and simplified it has 775 points and 8 crossings at 0.40°,
//! 3,816 and 66 at 0.10°, 14,194 and 75 at 0.025°. A self-crossing ring has no
//! ear decomposition, so every continent was dropped by the triangulator while
//! small islands came through — a map with coastlines and no land. `repair` is
//! the fix, and it runs after simplification for exactly this reason.

/// Drop points that lie within `tolerance` of the line their neighbours span.
pub fn douglas_peucker(pts: &[[f64; 2]], tolerance: f64) -> Vec<[f64; 2]> {
    if pts.len() < 3 {
        return pts.to_vec();
    }

    let mut keep = vec![false; pts.len()];
    keep[0] = true;
    keep[pts.len() - 1] = true;

    // An explicit stack of spans. The recursive form is tidier and overflows on
    // a 40,000-point coastline that splits badly.
    let mut stack = vec![(0usize, pts.len() - 1)];
    while let Some((i, j)) = stack.pop() {
        if j <= i + 1 {
            continue;
        }
        let (mut worst, mut at) = (-1.0_f64, i);
        for k in (i + 1)..j {
            let d = perpendicular(pts[k], pts[i], pts[j]);
            if d > worst {
                worst = d;
                at = k;
            }
        }
        if worst > tolerance {
            keep[at] = true;
            stack.push((i, at));
            stack.push((at, j));
        }
    }

    pts.iter()
        .zip(&keep)
        .filter(|(_, k)| **k)
        .map(|(p, _)| *p)
        .collect()
}

/// Distance from `p` to the segment `a`–`b`.
///
/// To the *segment*, not the infinite line. A degenerate span where `a == b`
/// happens on real data — a ring that doubles back on itself — and projecting
/// onto an infinite line through two identical points divides by zero.
fn perpendicular(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    if len2 <= f64::EPSILON {
        return hypot(p, a);
    }
    let t = (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0);
    hypot(p, [a[0] + t * dx, a[1] + t * dy])
}

fn hypot(a: [f64; 2], b: [f64; 2]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

/// Remove consecutive duplicates, and a closing point equal to the first.
///
/// **This is what makes triangulation work at all.** Shapefile polygons close
/// explicitly, so the first and last points coincide; simplification can also
/// leave two points on top of each other. Either produces a zero-area ear that
/// ear clipping cannot cut, and the result is a *partial* triangulation that
/// renders as a fan of stray triangles across the shape. That was a real bug in
/// the first bundled map, visible as spikes across the North Atlantic, and this
/// function is the fix.
pub fn dedup_closed(pts: &[[f64; 2]]) -> Vec<[f64; 2]> {
    const EPS: f64 = 1e-12;
    let mut out: Vec<[f64; 2]> = Vec::with_capacity(pts.len());
    for p in pts {
        if out
            .last()
            .is_none_or(|q| (p[0] - q[0]).abs() > EPS || (p[1] - q[1]).abs() > EPS)
        {
            out.push(*p);
        }
    }
    while out.len() > 1 {
        let (first, last) = (out[0], out[out.len() - 1]);
        if (first[0] - last[0]).abs() < EPS && (first[1] - last[1]).abs() < EPS {
            out.pop();
        } else {
            break;
        }
    }
    out
}

/// A ring with the loops simplification introduced cut out of it.
pub struct Repaired {
    pub ring: Vec<[f64; 2]>,
    /// How many crossings were resolved by removing a loop.
    pub removed: usize,
    /// Crossings left alone because the smaller loop was too much of the shape
    /// to discard. Reported rather than forced: cutting a quarter of a continent
    /// to satisfy a triangulator is the wrong trade, and a caller that knows the
    /// count can decide.
    pub stubborn: usize,
}

/// The largest share of a ring this will discard to resolve one crossing.
///
/// A crossing that simplification introduced encloses a sliver — a flattened
/// inlet, a few vertices across. One that encloses a quarter of the ring is not
/// that, and removing it would be silently deleting half of a landmass.
const MAX_LOOP: f64 = 0.25;

/// Cut out the self-crossings that simplification introduced.
///
/// A crossing between segments `i` and `j` divides the ring into two loops. One
/// of them is the sliver the simplifier folded over; the other is the shape. The
/// smaller one goes, which resolves the crossing and costs an area a reader
/// cannot see at the tolerance that created it.
///
/// One removal per pass, shortest loop first. Passes are bounded by the number of
/// crossings, and a pass costs one grid build over the ring: on the worst real
/// input that is 75 passes over 14,194 points, which is nothing in a build step
/// and is why the simpler loop is preferred over batching removals that overlap.
pub fn repair(ring: &[[f64; 2]]) -> Repaired {
    let mut ring = ring.to_vec();
    let mut removed = 0usize;

    loop {
        let n = ring.len();
        if n < 4 {
            return Repaired { ring, removed, stubborn: 0 };
        }
        let mut found = crossings(&ring);
        if found.is_empty() {
            return Repaired { ring, removed, stubborn: 0 };
        }

        // Shortest loop first. It is the most likely to be a simplification
        // artefact and the least costly to be wrong about.
        found.sort_by_key(|(i, j)| (j - i).min(n - (j - i)));
        let limit = ((n as f64) * MAX_LOOP) as usize;
        let Some(&(i, j)) = found.iter().find(|(i, j)| (j - i).min(n - (j - i)) <= limit) else {
            return Repaired { ring, removed, stubborn: found.len() };
        };

        // Inner is i+1..=j; outer is everything else. Whichever is smaller goes.
        if j - i <= n - (j - i) {
            ring.drain(i + 1..=j);
        } else {
            ring = ring[i + 1..=j].to_vec();
        }
        ring = dedup_closed(&ring);
        removed += 1;
    }
}

/// Every pair of non-adjacent segments that cross, as ring indices `i < j`.
///
/// Bucketed by a uniform grid over segment bounding boxes rather than compared
/// pairwise: a 14,000-point ring is 100 million pairs, and this is called once
/// per pass.
fn crossings(ring: &[[f64; 2]]) -> Vec<(usize, usize)> {
    let n = ring.len();
    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    for p in ring {
        for a in 0..2 {
            lo[a] = lo[a].min(p[a]);
            hi[a] = hi[a].max(p[a]);
        }
    }
    let t = ((n as f64).sqrt().ceil() as usize).max(1);
    let cell = [
        (hi[0] - lo[0]).max(1e-12) / t as f64,
        (hi[1] - lo[1]).max(1e-12) / t as f64,
    ];
    let at = |p: [f64; 2]| -> (usize, usize) {
        (
            (((p[0] - lo[0]) / cell[0]) as usize).min(t - 1),
            (((p[1] - lo[1]) / cell[1]) as usize).min(t - 1),
        )
    };

    let mut buckets = vec![Vec::new(); t * t];
    for i in 0..n {
        let (a, b) = (ring[i], ring[(i + 1) % n]);
        let (x0, y0) = at([a[0].min(b[0]), a[1].min(b[1])]);
        let (x1, y1) = at([a[0].max(b[0]), a[1].max(b[1])]);
        for y in y0..=y1 {
            for x in x0..=x1 {
                buckets[y * t + x].push(i);
            }
        }
    }

    // A segment spanning several cells is tested against the same partner more
    // than once, so pairs are deduplicated by sorting rather than by a set.
    let mut out = Vec::new();
    for b in &buckets {
        for (k, i) in b.iter().enumerate() {
            for j in &b[k + 1..] {
                let (i, j) = (*i.min(j), *i.max(j));
                // Adjacent segments share an endpoint, and the first and last
                // share one too. Neither is a crossing.
                if j == i + 1 || (i == 0 && j == n - 1) {
                    continue;
                }
                if segments_cross(ring[i], ring[(i + 1) % n], ring[j], ring[(j + 1) % n]) {
                    out.push((i, j));
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Whether two segments properly cross.
///
/// Properly: touching at an endpoint or lying along each other does not count.
/// A shared endpoint is how a ring is built, and treating it as a crossing would
/// report every vertex.
fn segments_cross(a: [f64; 2], b: [f64; 2], c: [f64; 2], d: [f64; 2]) -> bool {
    let side = |p: [f64; 2], q: [f64; 2], r: [f64; 2]| -> i32 {
        let v = (q[0] - p[0]) * (r[1] - p[1]) - (q[1] - p[1]) * (r[0] - p[0]);
        if v > 1e-14 {
            1
        } else if v < -1e-14 {
            -1
        } else {
            0
        }
    };
    side(a, b, c) * side(a, b, d) < 0 && side(c, d, a) * side(c, d, b) < 0
}

/// Twice the signed area. Positive is counter-clockwise.
///
/// Used for winding rather than for area, which is why it is not halved: the
/// sign is all anyone asks of it, and the factor is a rounding opportunity for
/// nothing.
pub fn signed_area2(ring: &[[f64; 2]]) -> f64 {
    let n = ring.len();
    (0..n)
        .map(|i| {
            let a = ring[i];
            let b = ring[(i + 1) % n];
            a[0] * b[1] - b[0] * a[1]
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_straight_line_collapses_to_its_ends() {
        let line = [[0.0, 0.0], [1.0, 0.0], [2.0, 0.0], [3.0, 0.0]];
        assert_eq!(douglas_peucker(&line, 0.01), vec![[0.0, 0.0], [3.0, 0.0]]);
    }

    #[test]
    fn a_feature_larger_than_the_tolerance_survives() {
        let line = [[0.0, 0.0], [1.0, 5.0], [2.0, 0.0]];
        assert_eq!(douglas_peucker(&line, 1.0).len(), 3);
    }

    #[test]
    fn a_feature_smaller_than_the_tolerance_is_dropped() {
        let line = [[0.0, 0.0], [1.0, 0.001], [2.0, 0.0]];
        assert_eq!(douglas_peucker(&line, 0.1).len(), 2);
    }

    #[test]
    fn the_ends_are_never_dropped() {
        // Whatever the tolerance. A simplifier that can drop an endpoint opens
        // gaps between adjacent parts of the same coastline.
        for tol in [0.0, 1.0, 1e6] {
            let out = douglas_peucker(&[[0.0, 0.0], [1.0, 1.0], [2.0, 0.0]], tol);
            assert_eq!(out[0], [0.0, 0.0]);
            assert_eq!(*out.last().unwrap(), [2.0, 0.0]);
        }
    }

    #[test]
    fn a_degenerate_span_does_not_divide_by_zero() {
        // A ring that doubles back on itself gives `a == b`, and projecting onto
        // an infinite line through two identical points is a division by zero.
        assert!(perpendicular([1.0, 1.0], [0.0, 0.0], [0.0, 0.0]).is_finite());
        let out = douglas_peucker(&[[0.0, 0.0], [1.0, 1.0], [0.0, 0.0]], 0.1);
        assert!(out.iter().all(|p| p[0].is_finite() && p[1].is_finite()));
    }

    #[test]
    fn a_short_input_passes_through() {
        assert_eq!(douglas_peucker(&[], 1.0), Vec::<[f64; 2]>::new());
        assert_eq!(douglas_peucker(&[[1.0, 2.0]], 1.0), vec![[1.0, 2.0]]);
    }

    #[test]
    fn a_long_input_does_not_overflow_the_stack() {
        // A smooth curve rather than a zigzag. Both force deep recursion, which
        // is what this checks, but a zigzag is also the algorithm's worst case
        // for *work* -- every span keeps its midpoint, so nothing is ever
        // discarded and the scan is quadratic. At 40,000 points that test alone
        // took eighteen seconds; a curve of the same length runs in
        // milliseconds and exercises the same recursion depth.
        let curve: Vec<[f64; 2]> = (0..40_000)
            .map(|i| {
                let t = i as f64 / 40_000.0 * std::f64::consts::TAU;
                [t, t.sin()]
            })
            .collect();
        assert!(douglas_peucker(&curve, 0.001).len() >= 2);
    }

    #[test]
    fn the_pathological_case_still_terminates() {
        // A zigzag keeps every point, so this is the quadratic case. Kept small
        // on purpose -- the property being checked is that it finishes and keeps
        // everything, not how fast.
        let zig: Vec<[f64; 2]> = (0..400)
            .map(|i| [i as f64, if i % 2 == 0 { 0.0 } else { 1.0 }])
            .collect();
        assert_eq!(douglas_peucker(&zig, 0.1).len(), zig.len());
    }

    #[test]
    fn a_closing_point_is_removed() {
        let ring = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]];
        assert_eq!(dedup_closed(&ring).len(), 3);
    }

    #[test]
    fn consecutive_duplicates_are_removed() {
        // These are what leave a zero-area ear that ear clipping cannot cut,
        // which is how a shape ends up partially triangulated and drawn as a
        // fan of spikes.
        let ring = [[0.0, 0.0], [0.0, 0.0], [1.0, 0.0], [1.0, 0.0], [1.0, 1.0]];
        assert_eq!(dedup_closed(&ring).len(), 3);
    }

    #[test]
    fn a_ring_that_is_entirely_one_point_survives_as_one_point() {
        assert_eq!(dedup_closed(&[[3.0, 4.0], [3.0, 4.0], [3.0, 4.0]]).len(), 1);
    }

    /// Crossings, for tests that assert there are none left.
    fn n_crossings(ring: &[[f64; 2]]) -> usize {
        crossings(ring).len()
    }

    #[test]
    fn a_clean_ring_is_left_exactly_as_it_was() {
        // Repair must be a no-op on the overwhelming majority of shapes. If it
        // nibbles at clean rings it is silently reshaping the map.
        let square = [[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0]];
        let out = repair(&square);
        assert_eq!(out.ring, square.to_vec());
        assert_eq!(out.removed, 0);
        assert_eq!(out.stubborn, 0);
    }

    /// A convex ring with adjacent vertices swapped, which is what a folded
    /// sliver looks like: the outline is intact and a few edges cross their
    /// neighbours. `swaps` controls how many, because one crossing can sometimes
    /// be muddled through and a handful cannot.
    fn folded(n: usize, swaps: usize) -> Vec<[f64; 2]> {
        let mut ring: Vec<[f64; 2]> = (0..n)
            .map(|i| {
                let a = std::f64::consts::TAU * i as f64 / n as f64;
                [10.0 * a.cos(), 10.0 * a.sin()]
            })
            .collect();
        for k in 0..swaps {
            let i = 1 + k * (n / swaps.max(1));
            if i + 1 < n {
                ring.swap(i, i + 1);
            }
        }
        ring
    }

    #[test]
    fn a_ring_with_a_folded_sliver_comes_back_without_the_crossing() {
        let ring = folded(24, 1);
        assert!(n_crossings(&ring) > 0, "fixture is not self-crossing");
        let out = repair(&ring);
        assert_eq!(n_crossings(&out.ring), 0, "{:?}", out.ring);
        assert!(out.removed > 0);
        assert_eq!(out.stubborn, 0);
    }

    #[test]
    fn a_repaired_ring_can_be_triangulated() {
        // The whole reason repair exists. Before it, this returned `Partial` and
        // the caller dropped the shape -- which at map scale meant every
        // continent was absent while small islands came through.
        //
        // Several crossings rather than one: ear clipping can sometimes muddle
        // through a single fold, and Natural Earth's Afro-Eurasia part comes out
        // of simplification with 75.
        let ring = folded(48, 6);
        assert!(
            matches!(
                crate::triangulate::ear_clip(&ring),
                crate::triangulate::Outcome::Partial { .. }
            ),
            "fixture triangulates without repair, so it proves nothing"
        );
        let fixed = repair(&ring).ring;
        assert!(matches!(
            crate::triangulate::ear_clip(&fixed),
            crate::triangulate::Outcome::Complete(_)
        ));
    }

    #[test]
    fn repair_keeps_the_larger_side_of_the_loop() {
        // A crossing splits the ring in two. The sliver goes and the shape
        // stays, so the area must barely move -- taking the wrong side would
        // delete most of a landmass and still satisfy every other assertion here.
        let ring = folded(48, 6);
        let before = signed_area2(&ring).abs() / 2.0;
        let after = signed_area2(&repair(&ring).ring).abs() / 2.0;
        assert!(
            after > before * 0.8,
            "area fell from {before} to {after}",
        );
    }

    #[test]
    fn a_crossing_that_would_cost_a_quarter_of_the_shape_is_left_alone() {
        // A bowtie's two loops are half the ring each, so there is no sliver to
        // discard -- the crossing is the shape. Reported rather than resolved.
        let bowtie = [[0.0, 0.0], [4.0, 4.0], [4.0, 0.0], [0.0, 4.0]];
        let out = repair(&bowtie);
        assert_eq!(out.removed, 0);
        assert!(out.stubborn > 0);
        assert_eq!(out.ring, bowtie.to_vec());
    }

    #[test]
    fn repair_terminates_on_a_ring_that_crosses_itself_repeatedly() {
        // Every pass must either resolve a crossing or stop. A pass that removes
        // nothing and loops again would hang the build.
        let zig: Vec<[f64; 2]> = (0..60)
            .map(|i| {
                let t = i as f64;
                [t, if i % 3 == 0 { 0.0 } else { (t * 2.7).sin() * 8.0 }]
            })
            .collect();
        let out = repair(&zig);
        assert!(out.removed + out.stubborn > 0 || n_crossings(&zig) == 0);
        // Whatever it decided, it decided it: nothing left that it claimed to fix.
        if out.stubborn == 0 {
            assert_eq!(n_crossings(&out.ring), 0);
        }
    }

    #[test]
    fn crossings_ignores_the_shared_endpoints_a_ring_is_built_from() {
        // Adjacent segments always touch, and so do the first and last. Counting
        // those would report a crossing at every vertex.
        let square = [[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0]];
        assert_eq!(n_crossings(&square), 0);
        let tri = [[0.0, 0.0], [4.0, 0.0], [2.0, 4.0]];
        assert_eq!(n_crossings(&tri), 0);
    }

    #[test]
    fn touching_segments_are_not_a_crossing() {
        // Sharing an endpoint, and lying along one another, are both how real
        // ring data looks. Neither defeats a triangulator.
        assert!(!segments_cross(
            [0.0, 0.0],
            [1.0, 0.0],
            [1.0, 0.0],
            [2.0, 0.0]
        ));
        assert!(!segments_cross(
            [0.0, 0.0],
            [2.0, 0.0],
            [1.0, 0.0],
            [3.0, 0.0]
        ));
        assert!(segments_cross(
            [0.0, 0.0],
            [2.0, 2.0],
            [0.0, 2.0],
            [2.0, 0.0]
        ));
    }

    #[test]
    fn winding_has_the_sign_it_says() {
        let ccw = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]];
        let cw = [[0.0, 0.0], [1.0, 1.0], [1.0, 0.0]];
        assert!(signed_area2(&ccw) > 0.0);
        assert!(signed_area2(&cw) < 0.0);
    }
}

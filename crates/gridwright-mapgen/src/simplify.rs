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

    #[test]
    fn winding_has_the_sign_it_says() {
        let ccw = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]];
        let cw = [[0.0, 0.0], [1.0, 1.0], [1.0, 0.0]];
        assert!(signed_area2(&ccw) > 0.0);
        assert!(signed_area2(&cw) < 0.0);
    }
}

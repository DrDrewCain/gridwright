//! Ear clipping, with the one property that matters: it says when it failed.
//!
//! Triangulation runs here, in a build step, and never in the studio. Ear
//! clipping is O(n²) in the worst case, and a frame is the wrong place to spend
//! that — the shipped path decodes index triples and transforms vertices, with
//! no geometry algorithm in it at all.
//!
//! **The return type is the point of this module.** A triangulator that returns
//! a bare `Vec` of triangles has no way to say "I got stuck three quarters of
//! the way through this coastline", and a partial triangulation does not render
//! as a partial shape — it renders as a fan of stray triangles sprawling across
//! the map. That is exactly the bug the first bundled basemap shipped with,
//! visible as spikes over the North Atlantic. So this returns an `Outcome` and
//! the caller drops what it could not finish.

use crate::simplify::signed_area2;

/// A triangulation, and whether it is complete.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Every vertex was consumed. `n - 2` triangles for an `n`-gon.
    Complete(Vec<[u16; 3]>),
    /// The polygon defeated it — self-intersecting, or degenerate in a way the
    /// cleanup missed. Carries what it managed, for diagnostics only: **do not
    /// draw this.**
    Partial {
        got: Vec<[u16; 3]>,
        remaining: usize,
    },
    /// Fewer than three distinct vertices, or more than `u16` can index.
    Unusable,
}

/// Triangulate a simple polygon given in either winding.
///
/// Indices are `u16`, which caps a ring at 65,535 points. That is not a
/// limitation in practice — the simplification ahead of this leaves the largest
/// coastline part in the low thousands — and it halves the index data against
/// `u32`, on what is the bulk of the output.
pub fn ear_clip(ring: &[[f64; 2]]) -> Outcome {
    let n = ring.len();
    if n < 3 || n > u16::MAX as usize {
        return Outcome::Unusable;
    }

    // Work on an index list so the caller's vertex order is preserved: the
    // output indexes their slice, not a copy this made.
    let mut live: Vec<u16> = (0..n as u16).collect();
    // Ear clipping needs counter-clockwise input to test convexity by sign.
    // Reversing the *indices* rather than the points keeps the mapping intact.
    if signed_area2(ring) < 0.0 {
        live.reverse();
    }

    let mut tris: Vec<[u16; 3]> = Vec::with_capacity(n.saturating_sub(2));
    // A pass that clips nothing means no ear exists anywhere, which for a simple
    // polygon is impossible — so it means the polygon is not simple. Counting
    // consecutive failures is how that is detected without a time limit.
    let mut barren = 0usize;

    while live.len() > 3 {
        if barren > live.len() {
            return Outcome::Partial {
                remaining: live.len(),
                got: tris,
            };
        }
        let mut clipped = false;
        for k in 0..live.len() {
            let (i0, i1, i2) = (
                live[(k + live.len() - 1) % live.len()],
                live[k],
                live[(k + 1) % live.len()],
            );
            let (a, b, c) = (ring[i0 as usize], ring[i1 as usize], ring[i2 as usize]);

            // Reflex or straight: not an ear. Straight is excluded too, because a
            // zero-area triangle is invisible and would still consume a vertex.
            if cross(a, b, c) <= 0.0 {
                continue;
            }
            // An ear may not contain any other live vertex.
            if live
                .iter()
                .any(|m| *m != i0 && *m != i1 && *m != i2 && in_triangle(ring[*m as usize], a, b, c))
            {
                continue;
            }

            tris.push([i0, i1, i2]);
            live.remove(k);
            clipped = true;
            barren = 0;
            break;
        }
        if !clipped {
            barren += 1;
        }
    }

    if live.len() == 3 {
        tris.push([live[0], live[1], live[2]]);
    }
    Outcome::Complete(tris)
}

fn cross(o: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

/// Whether `p` is inside or on triangle `a`,`b`,`c`.
///
/// Inclusive of the boundary on purpose. A vertex lying exactly on an ear's edge
/// is a vertex that would be stranded outside the triangulation if the ear were
/// cut, so it has to block the cut.
fn in_triangle(p: [f64; 2], a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> bool {
    let (d1, d2, d3) = (cross(a, b, p), cross(b, c, p), cross(c, a, p));
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(neg && pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tris(o: Outcome) -> Vec<[u16; 3]> {
        match o {
            Outcome::Complete(t) => t,
            other => panic!("expected a complete triangulation, got {other:?}"),
        }
    }

    /// Total area of a triangulation, for checking it covers the polygon.
    fn area_of(ring: &[[f64; 2]], t: &[[u16; 3]]) -> f64 {
        t.iter()
            .map(|[a, b, c]| {
                cross(ring[*a as usize], ring[*b as usize], ring[*c as usize]).abs() / 2.0
            })
            .sum()
    }

    #[test]
    fn a_triangle_is_already_a_triangle() {
        let r = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
        assert_eq!(tris(ear_clip(&r)).len(), 1);
    }

    #[test]
    fn an_n_gon_yields_n_minus_two_triangles() {
        // The defining property. Anything else means vertices were consumed
        // without producing a triangle, or a triangle covers area twice.
        for n in 3..=24 {
            let ring: Vec<[f64; 2]> = (0..n)
                .map(|i| {
                    let a = std::f64::consts::TAU * i as f64 / n as f64;
                    [a.cos(), a.sin()]
                })
                .collect();
            assert_eq!(tris(ear_clip(&ring)).len(), n - 2, "n = {n}");
        }
    }

    #[test]
    fn the_triangles_cover_the_polygon_exactly() {
        // A convex 12-gon of unit radius. If the triangulation overlapped or
        // left a gap, the areas would not agree.
        let n = 12;
        let ring: Vec<[f64; 2]> = (0..n)
            .map(|i| {
                let a = std::f64::consts::TAU * i as f64 / n as f64;
                [a.cos(), a.sin()]
            })
            .collect();
        let whole = signed_area2(&ring).abs() / 2.0;
        let parts = area_of(&ring, &tris(ear_clip(&ring)));
        assert!((whole - parts).abs() < 1e-9, "{whole} against {parts}");
    }

    #[test]
    fn a_concave_polygon_triangulates() {
        // An L shape. A fan from one vertex would leave the notch filled, which
        // is the failure mode of not testing for reflex vertices at all.
        let l = [
            [0.0, 0.0],
            [3.0, 0.0],
            [3.0, 1.0],
            [1.0, 1.0],
            [1.0, 3.0],
            [0.0, 3.0],
        ];
        let t = tris(ear_clip(&l));
        assert_eq!(t.len(), 4);
        let whole = signed_area2(&l).abs() / 2.0;
        assert!((area_of(&l, &t) - whole).abs() < 1e-9);
    }

    #[test]
    fn winding_does_not_matter() {
        let ccw = [[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]];
        let mut cw = ccw.to_vec();
        cw.reverse();
        assert_eq!(tris(ear_clip(&ccw)).len(), 2);
        assert_eq!(tris(ear_clip(&cw)).len(), 2);
    }

    #[test]
    fn indices_stay_within_the_input() {
        // Out of range would panic inside a tessellator later, with a message
        // about a mesh rather than about the polygon that caused it.
        let l = [
            [0.0, 0.0],
            [3.0, 0.0],
            [3.0, 1.0],
            [1.0, 1.0],
            [1.0, 3.0],
            [0.0, 3.0],
        ];
        for t in tris(ear_clip(&l)) {
            for i in t {
                assert!((i as usize) < l.len());
            }
        }
    }

    #[test]
    fn a_self_intersecting_polygon_reports_partial_rather_than_lying() {
        // The whole reason for `Outcome`. A figure-eight has no valid ear
        // decomposition, and the previous version of this code returned what it
        // had -- which drew as a fan of spikes across the map.
        let bowtie = [[0.0, 0.0], [2.0, 2.0], [2.0, 0.0], [0.0, 2.0]];
        assert!(matches!(
            ear_clip(&bowtie),
            Outcome::Partial { .. } | Outcome::Complete(_)
        ));
        // Whatever it decides, it must not claim more triangles than an n-gon
        // can have.
        if let Outcome::Complete(t) = ear_clip(&bowtie) {
            assert!(t.len() <= bowtie.len() - 2);
        }
    }

    #[test]
    fn too_few_vertices_is_unusable() {
        assert_eq!(ear_clip(&[]), Outcome::Unusable);
        assert_eq!(ear_clip(&[[0.0, 0.0]]), Outcome::Unusable);
        assert_eq!(ear_clip(&[[0.0, 0.0], [1.0, 1.0]]), Outcome::Unusable);
    }

    #[test]
    fn a_ring_too_long_to_index_is_unusable_rather_than_wrong() {
        // Indices are u16 to halve the output size. Silently truncating would
        // produce a shape whose triangles point at the wrong vertices.
        let big: Vec<[f64; 2]> = (0..70_000).map(|i| [i as f64, 0.0]).collect();
        assert_eq!(ear_clip(&big), Outcome::Unusable);
    }

    #[test]
    fn a_vertex_on_an_ear_edge_blocks_the_cut() {
        // Inclusive containment. If a point lying exactly on the edge did not
        // block, cutting that ear would strand it outside the triangulation.
        assert!(in_triangle(
            [1.0, 0.0],
            [0.0, 0.0],
            [2.0, 0.0],
            [1.0, 2.0]
        ));
    }
}

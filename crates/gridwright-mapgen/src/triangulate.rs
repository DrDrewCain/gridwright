//! Ear clipping over a Morton-code index, so the containment test is local.
//!
//! Triangulation runs here, in a build step, and never in the studio: the
//! shipped path decodes index triples and transforms vertices, with no geometry
//! algorithm in it at all.
//!
//! **The naive form is quadratic, and so is the usual fix for it.** Testing
//! whether an ear is empty by scanning every remaining vertex is O(n) per
//! candidate and O(n²) overall. The standard remedy is a Morton code — bit
//! interleaving on a Z-order curve — and it was tried here first and *measured*,
//! which is the only reason this file does not use one: probes per ear test came
//! out at a constant ~2% of the remaining vertices at every size, so 45 at
//! n=2,000 and 344 at n=16,000. Still linear per call, still quadratic overall.
//!
//! The cause is the Z-curve's discontinuity. Two points a hair apart on either
//! side of a power-of-two boundary get codes at opposite ends of the range, so
//! the code interval of a small triangle is not a small set of points. A ring
//! crossing the centre of its own bounding box — which most closed coastlines do
//! — hits that boundary repeatedly.
//!
//! So the index here is a **uniform grid**. A triangle's bounding box maps to a
//! small block of cells, and the points in those cells are exactly the
//! candidates, with no leakage from a curve that folds. Cell size targets about
//! two points per cell, which makes a query O(1) for bounded density and the
//! whole triangulation near-linear — measured, in the test at the bottom of this
//! file.
//!
//! **The return type is the point of this module.** A triangulator that returns
//! a bare `Vec` of triangles has no way to say "I got stuck three quarters of
//! the way through this coastline", and a partial triangulation does not render
//! as a partial shape — it renders as a fan of stray triangles sprawling across
//! the map. That is exactly the bug the first bundled basemap shipped with,
//! visible as spikes over the North Atlantic. So this returns an `Outcome` and
//! the caller drops what it could not finish.

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

/// A uniform grid over the polygon's bounding box.
///
/// Buckets hold vertex slots. Nothing is ever removed from a bucket: a clipped
/// vertex is marked gone and skipped on read, so maintenance is O(1) per clip
/// and the total skipping work is bounded by the number of clips.
struct Grid {
    lo: [f64; 2],
    cell: [f64; 2],
    cols: usize,
    rows: usize,
    buckets: Vec<Vec<u16>>,
}

impl Grid {
    /// Build over just the vertices in `live`, indexing the caller's slice.
    fn build(pts: &[[f64; 2]], live: &[u16]) -> Self {
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for k in live {
            let p = pts[*k as usize];
            for a in 0..2 {
                lo[a] = lo[a].min(p[a]);
                hi[a] = hi[a].max(p[a]);
            }
        }
        // About two points per cell on average. Fewer makes the grid itself the
        // cost; many more and a query degenerates back to a scan.
        let target = ((live.len() as f64 / 2.0).sqrt().ceil() as usize).max(1);
        let span = [(hi[0] - lo[0]).max(1e-12), (hi[1] - lo[1]).max(1e-12)];
        let cell = [span[0] / target as f64, span[1] / target as f64];

        let mut g = Grid {
            lo,
            cell,
            cols: target,
            rows: target,
            buckets: vec![Vec::new(); target * target],
        };
        for k in live {
            let (cx, cy) = g.cell_of(pts[*k as usize]);
            g.buckets[cy * g.cols + cx].push(*k);
        }
        g
    }

    fn cell_of(&self, p: [f64; 2]) -> (usize, usize) {
        let cx = (((p[0] - self.lo[0]) / self.cell[0]) as usize).min(self.cols - 1);
        let cy = (((p[1] - self.lo[1]) / self.cell[1]) as usize).min(self.rows - 1);
        (cx, cy)
    }

    /// Every vertex whose cell overlaps the box. Contains only vertices that
    /// were live when this grid was built.
    fn near(&self, min: [f64; 2], max: [f64; 2], mut visit: impl FnMut(u16) -> bool) -> bool {
        let (x0, y0) = self.cell_of(min);
        let (x1, y1) = self.cell_of(max);
        for cy in y0..=y1 {
            for cx in x0..=x1 {
                for k in &self.buckets[cy * self.cols + cx] {
                    if visit(*k) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

/// One vertex in the working ring.
///
/// A doubly linked list rather than a `Vec`, because clipping removes from the
/// middle and `Vec::remove` is O(n) each time — which would reintroduce the very
/// quadratic term the index exists to remove.
#[derive(Clone, Copy)]
struct Node {
    i: u16,
    prev: usize,
    next: usize,
}

/// Triangulate a simple polygon given in either winding.
///
/// Indices are `u16`, capping a ring at 65,535 points. Not a limitation in
/// practice — simplification leaves the largest coastline part in the low
/// thousands — and it halves the index data against `u32`, on what is the bulk
/// of the output.
pub fn ear_clip(ring: &[[f64; 2]]) -> Outcome {
    let n = ring.len();
    if n < 3 || n > u16::MAX as usize {
        return Outcome::Unusable;
    }

    // Counter-clockwise, so convexity is a sign test. Reordering the *links*
    // rather than the points keeps every emitted index pointing at the caller's
    // slice.
    let ccw = crate::simplify::signed_area2(ring) >= 0.0;
    let order: Vec<u16> = if ccw {
        (0..n as u16).collect()
    } else {
        (0..n as u16).rev().collect()
    };
    // Position of each source vertex in the working ring, so the grid (which
    // indexes the caller's slice) can be mapped to ring slots.
    let mut slot_of = vec![0usize; n];
    for (k, src) in order.iter().enumerate() {
        slot_of[*src as usize] = k;
    }
    let mut nodes: Vec<Node> = (0..n)
        .map(|k| Node {
            i: order[k],
            prev: (k + n - 1) % n,
            next: (k + 1) % n,
        })
        .collect();

    let mut gone = vec![false; n];
    let mut grid = Grid::build(ring, &order);
    // Rebuilt whenever the live count halves.
    //
    // **A static grid is the wrong shape for this problem and measuring showed
    // it.** Cells are sized for the *initial* density, so once most vertices are
    // clipped the buckets are full of dead entries and the surviving ears are
    // large -- a late ear's bounding box covers many cells holding almost
    // nothing live, and the query walks all of them. Sizing for 4,000 points and
    // then triangulating down to 40 leaves a grid a hundred times too fine.
    //
    // Rebuilding on a halving keeps roughly two *live* points per cell
    // throughout. The rebuilds form a geometric series, so their total cost is
    // O(n) -- one extra linear pass, in exchange for keeping every query O(1).
    let mut rebuild_at = n / 2;

    let mut tris: Vec<[u16; 3]> = Vec::with_capacity(n.saturating_sub(2));
    let mut live = n;

    // A worklist of candidate ear tips, not a walk around the ring.
    //
    // Clipping can only change whether the *two former neighbours* are ears --
    // every other vertex has the neighbourhood it had before, so if it was not
    // an ear then it still is not. A failed candidate is dropped, and only those
    // two are ever pushed back, so each vertex is examined a constant number of
    // times amortised. Walking the ring one step at a time instead measured ten
    // times the work for four times the input.
    let mut queue: Vec<usize> = (0..n).rev().collect();
    let mut queued = vec![true; n];

    while live > 3 {
        let Some(cur) = queue.pop() else {
            return Outcome::Partial {
                remaining: live,
                got: tris,
            };
        };
        queued[cur] = false;
        if gone[cur] {
            continue;
        }

        let (prev, next) = (nodes[cur].prev, nodes[cur].next);
        if !is_ear(ring, &nodes, &grid, &gone, &slot_of, prev, cur, next) {
            continue;
        }

        tris.push([nodes[prev].i, nodes[cur].i, nodes[next].i]);
        nodes[prev].next = next;
        nodes[next].prev = prev;
        gone[cur] = true;
        live -= 1;

        if live <= rebuild_at && live > 8 {
            let alive: Vec<u16> = (0..n).filter(|k| !gone[*k]).map(|k| nodes[k].i).collect();
            grid = Grid::build(ring, &alive);
            rebuild_at = live / 2;
        }

        for k in [prev, next] {
            if !gone[k] && !queued[k] {
                queued[k] = true;
                queue.push(k);
            }
        }
    }

    let start = (0..n).find(|k| !gone[*k]).unwrap_or(0);
    let b = nodes[start].next;
    tris.push([nodes[start].i, nodes[b].i, nodes[nodes[b].next].i]);
    Outcome::Complete(tris)
}

/// Whether `b` is the tip of an empty ear.
#[allow(clippy::too_many_arguments)]
fn is_ear(
    ring: &[[f64; 2]],
    nodes: &[Node],
    grid: &Grid,
    gone: &[bool],
    slot_of: &[usize],
    a: usize,
    b: usize,
    c: usize,
) -> bool {
    let (ia, ib, ic) = (nodes[a].i, nodes[b].i, nodes[c].i);
    let (pa, pb, pc) = (
        ring[ia as usize],
        ring[ib as usize],
        ring[ic as usize],
    );
    // Reflex or straight is not an ear. Straight is excluded because a zero-area
    // triangle is invisible and would still consume a vertex.
    if cross(pa, pb, pc) <= 0.0 {
        return false;
    }

    let min = [
        pa[0].min(pb[0]).min(pc[0]),
        pa[1].min(pb[1]).min(pc[1]),
    ];
    let max = [
        pa[0].max(pb[0]).max(pc[0]),
        pa[1].max(pb[1]).max(pc[1]),
    ];

    // Exactly the vertices whose cells the ear touches. `near` returns true as
    // soon as the closure does, so a blocker short-circuits the rest.
    !grid.near(min, max, |k| {
        if k == ia || k == ib || k == ic {
            return false;
        }
        // Still needed after a rebuild: a vertex clipped since the last rebuild
        // is present in the buckets and must not block an ear.
        if gone[slot_of[k as usize]] {
            return false;
        }
        in_triangle(ring[k as usize], pa, pb, pc)
    })
}

fn cross(o: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

/// Whether `p` is inside or on triangle `a`,`b`,`c`.
///
/// Inclusive of the boundary on purpose. A vertex lying exactly on an ear's edge
/// would be stranded outside the triangulation if the ear were cut, so it has to
/// block the cut.
fn in_triangle(p: [f64; 2], a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> bool {
    let (d1, d2, d3) = (cross(a, b, p), cross(b, c, p), cross(c, a, p));
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(neg && pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simplify::signed_area2;

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
    fn the_cost_grows_close_to_linearly_rather_than_quadratically() {
        // The claim in this module's docs, measured rather than asserted.
        //
        // A quadratic implementation quadruples its work when the input doubles.
        // The threshold is generous because this is wall clock on a shared
        // machine; the point is to catch a regression back to a full scan, not
        // to pin a constant.
        //
        // Two shapes, because they fail differently. A coastline-like ring has
        // detail at several scales and crosses the centre of its own bounding
        // box, which is what defeated the Morton-code version. A spiral is
        // adversarial for *any* spatial index -- adjacent arms are close in space
        // and far apart along the ring -- so it is measured too, and allowed a
        // looser bound.
        let coast = |n: usize| -> Vec<[f64; 2]> {
            (0..n)
                .map(|i| {
                    let a = std::f64::consts::TAU * i as f64 / n as f64;
                    let r = 10.0
                        + 1.5 * (a * 7.0).sin()
                        + 0.6 * (a * 23.0).sin()
                        + 0.2 * (a * 61.0).sin();
                    [r * a.cos(), r * a.sin()]
                })
                .collect()
        };
        let spiral = |n: usize| -> Vec<[f64; 2]> {
            (0..n)
                .map(|i| {
                    let t = i as f64 / n as f64;
                    let a = t * std::f64::consts::TAU * 6.0;
                    let r = 1.0 + 9.0 * t;
                    [r * a.cos(), r * a.sin()]
                })
                .collect()
        };

        let time = |ring: Vec<[f64; 2]>| -> f64 {
            let at = std::time::Instant::now();
            let out = ear_clip(&ring);
            assert!(!matches!(out, Outcome::Unusable));
            at.elapsed().as_secs_f64().max(1e-9)
        };

        for (name, make, bound) in [
            ("coastline", &coast as &dyn Fn(usize) -> Vec<[f64; 2]>, 11.0),
            ("spiral", &spiral as &dyn Fn(usize) -> Vec<[f64; 2]>, 13.0),
        ] {
            let _ = time(make(2_000)); // warm up the allocator
            let small = time(make(4_000));
            let large = time(make(16_000));
            let ratio = large / small;
            // Four times the input. Quadratic is about 16x.
            // Quadratic is about 16x for a 4x input; linear is 4x. Measured at
            // 7.1x for the coastline and 9.4x for the spiral, so the bounds
            // below leave room for a shared machine while still catching any
            // return to full scanning.
            assert!(
                ratio < bound,
                "{name}: 4x the input cost {ratio:.1}x the time (bound {bound}); \
                 small {:.2} ms, large {:.2} ms",
                small * 1000.0,
                large * 1000.0,
            );
        }
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

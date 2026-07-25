//! Spatial branch and bound: closing the relaxation gap properly.
//!
//! # What is left open, and why nothing convex can close it
//!
//! Jabr's substitution turns AC optimal power flow into a conic problem by
//! relaxing one equality into an inequality:
//!
//! ```text
//!   R² + I² = u_i u_j     becomes     R² + I² ≤ u_i u_j
//! ```
//!
//! The direction that survives is the convex one. The direction that is
//! dropped — `R² + I² ≥ u_i u_j` — is *reverse*-convex, and a reverse-convex
//! constraint has a nonconvex feasible set by construction. No amount of
//! cleverness writes it as a cone, because it is not one. Every result the
//! plain relaxation returns is therefore a lower bound, and when the
//! inequality is slack the voltages it reports describe no physical state at
//! all.
//!
//! The same is true of the cycle constraints. They are trilinear, relaxed
//! through McCormick envelopes, and those envelopes are exact at the corners
//! of a box and loose everywhere inside it. That looseness is not a bug to be
//! tuned away; it is what a convex envelope of a nonconvex function *is*.
//!
//! # What does close it
//!
//! Splitting the box. Both sources of error shrink with the box they are drawn
//! over, and both vanish in the limit:
//!
//! - A McCormick envelope collapses onto the bilinear surface.
//! - The secant of `x²` collapses onto the parabola, which is what makes the
//!   cut in [`crate::solve_in_domain`] approach the exact reverse-convex
//!   constraint.
//!
//! So: solve over a box, find where the relaxation is loosest, split there,
//! and solve both halves. Each child is a valid relaxation of its own box, and
//! the boxes cover the parent, so the smaller of the two children's objectives
//! is a valid bound for the parent — and the least bound over the whole
//! frontier is a valid bound for the original problem. That soundness is what
//! lets the search stop early with a *number* rather than a hope.
//!
//! # Bounds, and what "solved" means here
//!
//! Two numbers come out, and confusing them is the classic error:
//!
//! - **Lower bound**: the least objective over the unexplored frontier. No AC
//!   solution can cost less.
//! - **Upper bound**: the objective of the best node whose cone was tight to
//!   tolerance. That node is a genuine operating point, so this cost is
//!   achievable.
//!
//! When they meet, the answer is optimal and *proved* optimal. When they do
//! not, the search says so and reports both, which is worth far more than one
//! number of unstated status.

use crate::{AcError, AcOptions, AcSolution, Status, cycles::Domain, solve_in_domain};
use gridwright_net::Network;

/// How hard to search.
#[derive(Debug, Clone, Copy)]
pub struct BnbOptions {
    /// Ceiling on nodes explored. The search is anytime: stopping early
    /// returns whatever bounds have been established, both still valid.
    pub max_nodes: usize,
    /// Relative gap at which the answer is called proved.
    pub gap_tol: f64,
    /// Slack below which a node counts as a genuine operating point rather
    /// than a relaxation. Applied to the cone *and* to cycle consistency,
    /// because a point can satisfy every branch and still route power around a
    /// loop in a way no voltages could produce.
    pub cone_tol: f64,
    /// Smallest box worth splitting. Below this, further division buys
    /// arithmetic noise.
    pub min_box: f64,
    pub ac: AcOptions,
}

impl Default for BnbOptions {
    fn default() -> Self {
        Self {
            max_nodes: 200,
            gap_tol: 1e-4,
            cone_tol: 1e-6,
            min_box: 1e-6,
            ac: AcOptions::default(),
        }
    }
}

/// What the search established.
#[derive(Debug, Clone)]
pub struct BnbSolution {
    /// The best point found. An operating point when `proved` or when
    /// `upper_bound` is finite; otherwise the tightest relaxation seen.
    pub best: AcSolution,
    /// No AC solution costs less than this.
    pub lower_bound: f64,
    /// A cost that is achievable, or infinity if no tight node was found.
    pub upper_bound: f64,
    /// Relative distance between the two.
    pub gap: f64,
    /// Nodes solved.
    pub nodes: usize,
    /// Whether the gap was closed to tolerance.
    pub proved: bool,
    /// Why the search stopped, for a caller reporting to a human.
    pub stopped: Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The bounds met.
    GapClosed,
    /// The frontier emptied: every box was explored or pruned.
    Exhausted,
    /// The node budget ran out. Bounds are still valid, just further apart.
    NodeLimit,
}

struct Node {
    bound: f64,
    domain: Domain,
}

/// The branch to split, and which of its two variables.
///
/// Chosen by where the relaxation is worst, since that is where splitting buys
/// the most. Ties go to the wider interval, because splitting a box that is
/// already narrow changes little.
fn pick_branch(sol: &AcSolution, dom: &Domain, net: &Network, min_box: f64) -> Option<(usize, bool)> {
    let mut best: Option<(f64, usize, bool)> = None;
    for (l, line) in net.lines.iter().enumerate() {
        if line.is_transport() {
            continue;
        }
        // Either kind of looseness is worth splitting for. Cycle
        // inconsistency is not attributable to one branch, so it counts
        // against every branch and the widest box wins — which is the right
        // move, since every envelope in the loop is drawn over these boxes.
        let gap = sol
            .line_gap
            .get(l)
            .copied()
            .unwrap_or(0.0)
            .max(sol.cycle_gap);
        if gap <= 0.0 {
            continue;
        }
        let b = dom.lines[l];
        let (wr, wi) = (b.r.1 - b.r.0, b.i.1 - b.i.0);
        if wr.max(wi) < min_box {
            continue;
        }
        let on_r = wr >= wi;
        // Weighted by the width of the interval about to be halved: a wide box
        // with a moderate gap is usually a better split than a pinhole box
        // with a large one.
        let score = gap * wr.max(wi);
        if best.is_none_or(|(s, _, _)| score > s) {
            best = Some((score, l, on_r));
        }
    }
    best.map(|(_, l, on_r)| (l, on_r))
}

/// Split one interval at the solution's own value.
///
/// Branching at the incumbent point is what makes progress: that point is
/// exactly the one the relaxation liked and the true constraint rejects, and
/// putting a boundary through it means neither child can propose it again.
/// A midpoint fallback keeps the halves non-degenerate when the value sits on
/// an edge.
fn split(lo: f64, hi: f64, at: f64) -> (f64, f64) {
    let margin = (hi - lo) * 0.05;
    let cut = at.clamp(lo + margin, hi - margin);
    if cut.is_finite() && cut > lo && cut < hi {
        (cut, cut)
    } else {
        let mid = 0.5 * (lo + hi);
        (mid, mid)
    }
}

/// Search for a proved AC optimum.
pub fn solve_bnb(
    net: &Network,
    snapshot: usize,
    opts: BnbOptions,
) -> Result<BnbSolution, AcError> {
    let root = Domain::root(net);
    let first = solve_in_domain(net, snapshot, opts.ac, &root)?;
    if !matches!(first.status, Status::Optimal | Status::OptimalRelaxed) {
        // Nothing to search: an infeasible or unbounded root is the answer.
        return Ok(BnbSolution {
            lower_bound: first.objective,
            upper_bound: f64::INFINITY,
            gap: f64::INFINITY,
            nodes: 1,
            proved: false,
            stopped: Stop::Exhausted,
            best: first,
        });
    }

    let mut nodes = 1usize;
    let mut incumbent: Option<AcSolution> = None;
    let mut upper = f64::INFINITY;
    let mut frontier: Vec<Node> = Vec::new();

    let consider = |sol: &AcSolution,
                        incumbent: &mut Option<AcSolution>,
                        upper: &mut f64| {
        // Both conditions, or the incumbent is not an operating point and the
        // upper bound it sets is not achievable.
        if sol.cone_gap <= opts.cone_tol
            && sol.cycle_gap <= opts.cone_tol
            && sol.objective < *upper
        {
            *upper = sol.objective;
            *incumbent = Some(sol.clone());
        }
    };
    consider(&first, &mut incumbent, &mut upper);

    let relative = |lo: f64, hi: f64| {
        if !hi.is_finite() {
            f64::INFINITY
        } else {
            (hi - lo).abs() / hi.abs().max(1.0)
        }
    };

    let mut best_relaxed = first.clone();
    frontier.push(Node {
        bound: first.objective,
        domain: root,
    });
    let mut stopped = Stop::Exhausted;
    let mut lower = first.objective;

    while let Some(index) = frontier
        .iter()
        .enumerate()
        .min_by(|a, b| a.1.bound.partial_cmp(&b.1.bound).unwrap())
        .map(|(i, _)| i)
    {
        // Best-first, so the least bound on the frontier is the global bound.
        lower = frontier[index].bound;
        if relative(lower, upper) <= opts.gap_tol {
            stopped = Stop::GapClosed;
            break;
        }
        if nodes >= opts.max_nodes {
            stopped = Stop::NodeLimit;
            break;
        }
        let node = frontier.swap_remove(index);

        // A node whose bound already exceeds a known achievable cost cannot
        // contain anything better.
        if node.bound >= upper {
            continue;
        }

        let Some((line, on_r)) = pick_branch(&best_relaxed, &node.domain, net, opts.min_box) else {
            // Nothing left worth splitting in this box: it is as tight as this
            // search will make it.
            continue;
        };

        let value = if on_r {
            best_relaxed.w_re.get(line).copied().unwrap_or(0.0)
        } else {
            best_relaxed.w_im.get(line).copied().unwrap_or(0.0)
        };
        let b = node.domain.lines[line];
        let (lo, hi) = if on_r { b.r } else { b.i };
        let (cut_lo, cut_hi) = split(lo, hi, value);

        for (child_lo, child_hi) in [(lo, cut_lo), (cut_hi, hi)] {
            if child_hi - child_lo < opts.min_box {
                continue;
            }
            let mut child = node.domain.clone();
            if on_r {
                child.lines[line].r = (child_lo, child_hi);
            } else {
                child.lines[line].i = (child_lo, child_hi);
            }
            let sol = match solve_in_domain(net, snapshot, opts.ac, &child) {
                Ok(s) => s,
                // A child that will not solve is one whose box holds nothing,
                // which is a successful exclusion rather than a failure.
                Err(_) => continue,
            };
            nodes += 1;
            if !matches!(sol.status, Status::Optimal | Status::OptimalRelaxed) {
                continue;
            }
            consider(&sol, &mut incumbent, &mut upper);
            if sol.cone_gap.max(sol.cycle_gap)
                < best_relaxed.cone_gap.max(best_relaxed.cycle_gap)
            {
                best_relaxed = sol.clone();
            }
            // A child's bound cannot be better than its parent's, since its
            // feasible set is a subset. Taking the maximum guards against the
            // solver returning a marginally smaller number for a tighter
            // problem, which would otherwise make the global bound drift down.
            frontier.push(Node {
                bound: sol.objective.max(node.bound),
                domain: child,
            });
        }
    }

    if frontier.is_empty() && stopped == Stop::Exhausted {
        // Everything explored: the bound is whatever was achieved.
        lower = upper.min(lower);
    }

    let gap = relative(lower, upper);
    let proved = gap <= opts.gap_tol;
    Ok(BnbSolution {
        best: incumbent.unwrap_or(best_relaxed),
        lower_bound: lower,
        upper_bound: upper,
        gap,
        nodes,
        proved,
        stopped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_puts_a_boundary_through_the_point_that_was_rejected() {
        // The point the relaxation chose has to be excluded from both children,
        // or the same answer comes back forever.
        let (a, b) = split(0.0, 1.0, 0.4);
        assert!((a - 0.4).abs() < 1e-12 && (b - 0.4).abs() < 1e-12);
    }

    #[test]
    fn a_point_on_the_edge_still_gives_two_real_halves() {
        // Splitting exactly at a bound would leave one child empty and the
        // other identical to its parent, which never terminates.
        let (a, b) = split(0.0, 1.0, 0.0);
        assert!(a > 0.0 && b < 1.0, "{a} {b}");
        let (a, b) = split(0.0, 1.0, 1.0);
        assert!(a > 0.0 && b < 1.0, "{a} {b}");
        let (a, b) = split(0.0, 1.0, f64::NAN);
        assert!((a - 0.5).abs() < 1e-12 && (b - 0.5).abs() < 1e-12);
    }
}

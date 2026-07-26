//! Branch and bound over the integer variables.
//!
//! Without this the pure-Rust backend refuses any model with an integer in it,
//! which means it refuses unit commitment and head bands. That refusal is
//! honest — a commitment answer with a unit running at 43% of its on state is
//! not an answer — but it leaves the browser build unable to run two of the
//! things people most want to run, since a page cannot call HiGHS.
//!
//! # How it works
//!
//! The relaxation is the same linear program with the integrality dropped, and
//! it is a bound: no integer solution costs less. If its answer happens to be
//! integral, it is optimal and there is nothing to search. Otherwise pick a
//! variable sitting between two integers and split the problem in two, one side
//! forced down and the other up. Neither side can contain the fractional point,
//! so the split makes progress, and together they contain every integer
//! solution the parent did.
//!
//! Branching is by bounds alone. The simplex takes its bounds as slices, so a
//! node is a pair of vectors and a re-solve, with no change to the solver
//! itself.
//!
//! # Two numbers, again
//!
//! As with the AC search, what comes out is a pair:
//!
//! - the **incumbent**, the best integer solution found, which is achievable;
//! - the **bound**, the least relaxation value among nodes not yet closed,
//!   which nothing can beat.
//!
//! When they meet the answer is proved. When the node budget runs out first
//! they do not, and saying so is worth more than a number of unstated status —
//! particularly in a browser, where the budget will run out.
//!
//! # Depth first
//!
//! Nodes are explored depth first, taking the branch nearer the relaxation's
//! own choice before its sibling. Best-first proves optimality with fewer nodes
//! and holds the whole frontier in memory; depth first finds a usable answer
//! early and keeps only the current path. For an interactive setting the early
//! answer is worth more, and the bound is tracked across the open nodes either
//! way.

use crate::{Options, Problem, SolveError, Solution, Status, solve};

/// How hard to search.
#[derive(Debug, Clone, Copy)]
pub struct MipOptions {
    /// Ceiling on nodes explored. The search is anytime: stopping early
    /// returns the best solution found and an honest bound.
    pub max_nodes: usize,
    /// How far from an integer a value may sit and still count as one.
    pub integrality_tolerance: f64,
    /// Relative gap at which the answer is called proved.
    pub gap_tolerance: f64,
    /// Options for the relaxation at each node.
    pub lp: Options,
}

impl Default for MipOptions {
    fn default() -> Self {
        Self {
            max_nodes: 5_000,
            integrality_tolerance: 1e-6,
            gap_tolerance: 1e-6,
            lp: Options::default(),
        }
    }
}

/// The result of the search.
#[derive(Debug, Clone)]
pub struct MipSolution {
    pub status: Status,
    /// Objective of the best integer solution found.
    pub objective: f64,
    pub col_value: Vec<f64>,
    /// Duals from the relaxation at the incumbent's node.
    ///
    /// A mixed-integer program has no duals in the sense a linear one does: the
    /// value function is not convex and there is no shadow price. These are the
    /// duals of the relaxation with the branching bounds in place, which is
    /// what every solver reports and is useful with that caveat attached.
    pub row_dual: Vec<f64>,
    /// No integer solution costs less than this.
    pub lower_bound: f64,
    /// Relative distance between the bound and the incumbent.
    pub gap: f64,
    /// Whether the gap closed to tolerance.
    pub proved: bool,
    pub nodes: usize,
}

/// A node: the bounds that define it, and the bound it inherited.
struct Node {
    lower: Vec<f64>,
    upper: Vec<f64>,
    parent_bound: f64,
}

/// Whether a value is close enough to a whole number.
#[inline]
fn integral(v: f64, tol: f64) -> bool {
    (v - v.round()).abs() <= tol
}

/// The integer variable furthest from a whole number.
///
/// Most-fractional is the simplest useful rule. Pseudo-cost branching is
/// better and needs history this does not keep; the difference matters most on
/// problems far larger than a page will run.
fn pick(values: &[f64], integer: &[bool], tol: f64) -> Option<usize> {
    let mut best: Option<(f64, usize)> = None;
    for (j, &is_int) in integer.iter().enumerate() {
        if !is_int {
            continue;
        }
        let v = values[j];
        if integral(v, tol) {
            continue;
        }
        let distance = (v - v.floor() - 0.5).abs();
        if best.is_none_or(|(d, _)| distance < d) {
            best = Some((distance, j));
        }
    }
    best.map(|(_, j)| j)
}

/// Solve a mixed-integer program by branch and bound.
///
/// `integer[j]` marks column `j` as needing a whole value. Columns not marked
/// are left continuous, so this handles mixed problems rather than pure ones.
pub fn solve_mip(
    p: Problem<'_>,
    integer: &[bool],
    o: MipOptions,
) -> Result<MipSolution, SolveError> {
    let relative = |lo: f64, hi: f64| {
        if !hi.is_finite() {
            f64::INFINITY
        } else {
            (hi - lo).abs() / hi.abs().max(1.0)
        }
    };

    // The root relaxation, which is both the first bound and the answer when it
    // happens to come out integral.
    let root = solve(p, o.lp)?;
    if root.status != Status::Optimal {
        return Ok(MipSolution {
            status: root.status,
            objective: root.objective,
            col_value: root.col_value,
            row_dual: root.row_dual,
            lower_bound: f64::NEG_INFINITY,
            gap: f64::INFINITY,
            proved: false,
            nodes: 1,
        });
    }

    let mut nodes = 1usize;
    let mut lower = root.objective;
    let mut incumbent: Option<Solution> = None;
    let mut upper = f64::INFINITY;

    if pick(&root.col_value, integer, o.integrality_tolerance).is_none() {
        // Integral already: the relaxation was tight and nothing needs
        // searching.
        return Ok(MipSolution {
            status: Status::Optimal,
            objective: root.objective,
            lower_bound: root.objective,
            gap: 0.0,
            proved: true,
            nodes,
            col_value: root.col_value,
            row_dual: root.row_dual,
        });
    }

    // Whether the search ran out of budget rather than out of nodes. Without
    // this the two are indistinguishable at the end: popping the last node and
    // then hitting the limit leaves an empty stack, which would otherwise read
    // as "searched everything and found nothing" and report a feasible problem
    // infeasible.
    let mut hit_limit = false;
    let mut stack: Vec<Node> = vec![Node {
        lower: p.col_lower.to_vec(),
        upper: p.col_upper.to_vec(),
        parent_bound: root.objective,
    }];

    while let Some(node) = stack.pop() {
        // A node whose inherited bound is already worse than a known
        // achievable cost cannot contain anything better.
        if node.parent_bound >= upper - o.gap_tolerance * upper.abs().max(1.0) {
            continue;
        }
        if nodes >= o.max_nodes {
            hit_limit = true;
            // The node just popped was never explored, so its bound still
            // stands over whatever it contained.
            lower = lower.min(node.parent_bound);
            break;
        }

        let sub = Problem {
            col_lower: &node.lower,
            col_upper: &node.upper,
            ..p
        };
        let relaxed = match solve(sub, o.lp) {
            Ok(s) => s,
            // A node that will not solve is one whose bounds contradict, which
            // is a successful exclusion rather than a failure.
            Err(_) => continue,
        };
        nodes += 1;
        if relaxed.status != Status::Optimal {
            continue;
        }
        if relaxed.objective >= upper {
            continue;
        }

        match pick(&relaxed.col_value, integer, o.integrality_tolerance) {
            None => {
                // Integral, and better than anything found so far.
                upper = relaxed.objective;
                incumbent = Some(relaxed);
            }
            Some(j) => {
                let v = relaxed.col_value[j];
                let (down, up) = (v.floor(), v.ceil());

                // The branch nearer the relaxation's own choice goes on top of
                // the stack, so depth-first descends the side more likely to
                // hold a good solution and finds an incumbent sooner.
                let prefer_down = v - down <= up - v;
                let mut push = |lo: f64, hi: f64| {
                    let mut l = node.lower.clone();
                    let mut u = node.upper.clone();
                    l[j] = l[j].max(lo);
                    u[j] = u[j].min(hi);
                    if l[j] <= u[j] + 1e-9 {
                        stack.push(Node {
                            lower: l,
                            upper: u,
                            parent_bound: relaxed.objective,
                        });
                    }
                };
                if prefer_down {
                    push(up, f64::INFINITY);
                    push(f64::NEG_INFINITY, down);
                } else {
                    push(f64::NEG_INFINITY, down);
                    push(up, f64::INFINITY);
                }
            }
        }

        // The bound is the least relaxation value still open. With nothing
        // open, the incumbent is optimal.
        lower = stack
            .iter()
            .map(|n| n.parent_bound)
            .fold(f64::INFINITY, f64::min)
            .min(upper);
    }

    let Some(best) = incumbent else {
        // Nothing integral was found. That means infeasible only if the search
        // actually finished; if it ran out of budget it means undetermined,
        // and the two must not be confused.
        return Ok(MipSolution {
            status: if hit_limit {
                Status::IterationLimit
            } else {
                Status::Infeasible
            },
            objective: f64::INFINITY,
            col_value: vec![0.0; p.n_cols],
            row_dual: vec![0.0; p.n_rows],
            lower_bound: lower,
            gap: f64::INFINITY,
            proved: false,
            nodes,
        });
    };

    let gap = relative(lower, upper);
    Ok(MipSolution {
        status: Status::Optimal,
        objective: best.objective,
        col_value: best.col_value,
        row_dual: best.row_dual,
        lower_bound: lower.min(upper),
        gap,
        proved: !hit_limit && (gap <= o.gap_tolerance || stack.is_empty()),
        nodes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_on_a_whole_number_is_integral() {
        assert!(integral(3.0, 1e-6));
        assert!(integral(3.0000001, 1e-6));
        assert!(!integral(3.4, 1e-6));
        assert!(integral(-2.0, 1e-6));
    }

    #[test]
    fn branching_picks_the_variable_furthest_from_a_whole_number() {
        let values = [1.0, 2.9, 3.5, 4.1];
        let integer = [true, true, true, true];
        // 3.5 is exactly halfway, which is the least decided and so the most
        // worth splitting.
        assert_eq!(pick(&values, &integer, 1e-6), Some(2));
    }

    #[test]
    fn a_continuous_variable_is_never_branched_on() {
        let values = [0.5, 2.9];
        let integer = [false, true];
        assert_eq!(pick(&values, &integer, 1e-6), Some(1));
    }

    #[test]
    fn an_all_integral_point_needs_no_branch() {
        let values = [1.0, 2.0];
        assert_eq!(pick(&values, &[true, true], 1e-6), None);
    }
}

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
//!
//! # Which variable to split on
//!
//! Most-fractional is the obvious rule and is barely better than choosing at
//! random. Pseudo-cost branching keeps, per variable and per direction, what
//! branching on it has actually cost, and on the commitment ladder that is two
//! to four times fewer nodes once the models are large enough for the choice to
//! matter at all. It is the default; [`Branching`] carries the table and the
//! argument.

use crate::{Options, Problem, Solution, SolveError, Status, solve};

/// How the search chooses which fractional variable to split on.
///
/// # The two rules
///
/// **Most-fractional** takes the variable furthest from a whole number, on the
/// reasoning that it is the least decided and so the most worth deciding. It is
/// the simplest rule that is not arbitrary, and the literature is unanimous
/// that it is barely better than picking at random: how undecided a variable
/// looks says nothing about how much forcing it would cost.
///
/// **Pseudo-cost** asks the better question. Every branch already taken on a
/// variable produced an observation — the bound moved by so much for so much of
/// fractionality removed — and the average of those observations, kept per
/// variable and per direction, estimates what the next branch on it would cost.
/// The two directions are combined as a product, since a variable worth
/// branching on is one that hurts *both* ways: a variable that is expensive
/// upwards and free downwards gives one useful child and one that changes
/// nothing, and the product says so where a sum does not.
///
/// # What the measurement says here
///
/// Measured on a ladder of unit commitment problems built so that every
/// relaxation is provably fractional, on three unrelated demand profiles each,
/// best of five runs; `tests/branching.rs` builds them and prints the table.
/// Nodes explored, and wall-clock:
///
/// | Units × periods | Profile | Most-fractional | Pseudo-cost | |
/// | --- | --- | --- | --- | --- |
/// | 4 × 4 | 0 | 12, 1.3 ms | 12, 1.2 ms | 1.08× |
/// | 4 × 4 | 1 | 78, 8.2 ms | 51, 5.3 ms | 1.53× |
/// | 4 × 4 | 2 | 10, 1.1 ms | 10, 1.1 ms | 1.01× |
/// | 6 × 6 | 0 | 61, 21 ms | 48, 16 ms | 1.30× |
/// | 6 × 6 | 1 | 27, 9.6 ms | 28, 9.9 ms | 0.97× |
/// | 6 × 6 | 2 | 116, 42 ms | 82, 30 ms | 1.42× |
/// | 8 × 8 | 0 | 344, 0.32 s | 286, 0.26 s | 1.22× |
/// | 8 × 8 | 1 | 132, 0.12 s | 146, 0.14 s | 0.91× |
/// | 8 × 8 | 2 | 468, 0.43 s | 320, 0.29 s | 1.49× |
/// | 10 × 10 | 0 | 1,038, 2.09 s | 439, 0.88 s | 2.37× |
/// | 10 × 10 | 1 | 1,215, 2.44 s | 558, 1.12 s | 2.17× |
/// | 10 × 10 | 2 | 814, 1.69 s | 356, 0.74 s | 2.28× |
/// | 12 × 12 | 0 | 7,044, 27.2 s | 1,644, 6.3 s | 4.31× |
/// | 12 × 12 | 1 | 2,970, 11.8 s | 1,500, 6.0 s | 1.97× |
/// | 12 × 12 | 2 | 4,498, 18.0 s | 1,641, 6.8 s | 2.63× |
///
/// So [`Branching::PseudoCost`] is the default, on that evidence.
///
/// Two things in the table matter more than the headline. The first is that the
/// win **grows with size**: within noise below two hundred columns, two to four
/// times at four hundred and forty. That is what the rule is for and it is what
/// makes it worth its complexity — a fixed 10% would not be. The second is that
/// the time column tracks the node column almost exactly, which says the
/// scoring itself is free: a node costs a full simplex solve from scratch here,
/// and averaging two numbers per branch does not register against that. There
/// is therefore no size at which the rule is a liability, only sizes at which
/// it is not yet an advantage.
///
/// The default node budget is five thousand, which makes the top row of that
/// table the case the change was worth making for: most-fractional wants 7,044
/// nodes there and would have run out, returning an unproved incumbent and an
/// open gap, while pseudo-cost proves the same answer in 1,644. A rule that
/// turns "here is something, I could not tell you if it is optimal" into "here
/// is the optimum" is worth more than the ratio suggests.
///
/// Where it loses it loses by a little and by luck — 0.91× on one profile at
/// 8 × 8 — which is the expected behaviour of a rule that is better on average
/// rather than dominant. Nothing about the search's guarantees changes with it:
/// the same optimum, proved, on every rung, which is asserted rather than
/// assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Branching {
    /// The variable furthest from a whole number.
    MostFractional,
    /// The variable whose past branches moved the objective most, up and down.
    PseudoCost,
}

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
    /// Which fractional variable to split on.
    ///
    /// Pseudo-cost by measurement rather than by taste: on the commitment
    /// ladder in `tests/branching.rs` it explores two to four times fewer nodes
    /// at the large end and is within noise at the small end, and it costs
    /// nothing per node against a full relaxation solve. [`Branching`] carries
    /// the table.
    pub branching: Branching,
    /// Options for the relaxation at each node.
    pub lp: Options,
}

impl Default for MipOptions {
    fn default() -> Self {
        Self {
            max_nodes: 5_000,
            integrality_tolerance: 1e-6,
            gap_tolerance: 1e-6,
            branching: Branching::PseudoCost,
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

/// The branch that produced a node.
///
/// Kept so that solving the node can be turned into an observation about the
/// variable that was split: the bound moved by so much, for so much
/// fractionality removed. Without this the pseudo-costs would have nothing to
/// average, since by the time a child is solved its parent is long gone from
/// the stack.
#[derive(Debug, Clone, Copy)]
struct Branch {
    col: usize,
    /// Whether this is the child whose lower bound was raised.
    up: bool,
    /// How far the branch moved the variable from the parent's fractional
    /// value: `v − ⌊v⌋` downwards, `⌈v⌉ − v` upwards.
    moved: f64,
}

/// The best objective still reachable by anything left unexplored.
///
/// This is the search's lower bound, and it has to be derived from what is
/// *currently* open rather than carried along and patched, because there are
/// three ways a node leaves the stack — explored, pruned, or abandoned when the
/// budget runs out — and a bound updated on only some of them goes stale
/// without any signal that it has.
///
/// A node dropped by the pruning test is deliberately excluded: it was dropped
/// precisely because its inherited bound was no better than the incumbent, so
/// it cannot contain anything worth reporting a gap about.
///
/// Capped at the incumbent, since a bound above the best known achievable cost
/// is not information about the problem.
fn open_bound(stack: &[Node], upper: f64) -> f64 {
    stack
        .iter()
        .map(|n| n.parent_bound)
        .fold(f64::INFINITY, f64::min)
        .min(upper)
}

/// A node: the bounds that define it, the bound it inherited, and the branch it
/// came from.
struct Node {
    lower: Vec<f64>,
    upper: Vec<f64>,
    parent_bound: f64,
    /// `None` for the root, which nothing branched into existence.
    branch: Option<Branch>,
}

/// Whether a value is close enough to a whole number.
#[inline]
fn integral(v: f64, tol: f64) -> bool {
    (v - v.round()).abs() <= tol
}

/// The integer variable furthest from a whole number.
///
/// The simplest rule that is not arbitrary, and no better than that: how
/// undecided a variable looks says nothing about how much forcing it would
/// cost. Kept because it is what [`PseudoCosts`] is measured against, and
/// because it needs no history and so cannot be led astray by one.
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

/// A branch's score may not fall below this, whatever the estimates say.
///
/// A variable observed to cost nothing in one direction would otherwise score
/// zero under a product, and every such variable would then tie at zero — which
/// hands the choice to the index tiebreak and throws away what the *other*
/// direction knew. The floor keeps a one-sided variable ranked by its useful
/// side while still ranking it below anything that hurts both ways.
const SCORE_FLOOR: f64 = 1e-6;

/// What a variable's branches are estimated to cost, learned as the search
/// goes.
///
/// One average per variable per direction. The unit is objective per unit of
/// fractionality: a branch that moved the bound by `d` while removing `f` of
/// fractionality contributes `d / f`, so that a branch from 0.9 up to 1 and one
/// from 0.1 up to 1 are comparable observations about the same variable rather
/// than two numbers of different size.
struct PseudoCosts {
    down_sum: Vec<f64>,
    down_count: Vec<u32>,
    up_sum: Vec<f64>,
    up_count: Vec<u32>,
    /// What to assume before anything has been observed.
    prior: Vec<f64>,
}

impl PseudoCosts {
    /// Start from the objective coefficients.
    ///
    /// The cold-start problem is that the first branch on a variable has no
    /// history, and the two usual answers are strong branching — solve both
    /// child relaxations for a shortlist of candidates and use the real
    /// degradation — or a prior taken from the objective.
    ///
    /// The prior is the right choice *here*, and the reason is a property of
    /// this solver rather than a general preference. Strong branching is
    /// affordable in solvers that can re-solve a child from its parent's basis
    /// with a handful of dual simplex iterations. This solver has no warm start
    /// at all: [`crate::solve`] builds its tableau, crashes a basis, runs phase
    /// one and then phase two, every time. A strong branching probe therefore
    /// costs exactly what exploring the node costs, so shortlisting ten
    /// candidates spends twenty node-solves to choose one branch — and the
    /// whole point of the exercise is to spend fewer node-solves. It would be
    /// paying the bill it came to reduce.
    ///
    /// So the prior it is. A variable's own objective coefficient is a genuine
    /// lower estimate of what forcing it costs: driving a variable with cost
    /// `c` one unit against the objective's preference costs at least `c`,
    /// before anything the constraints then force elsewhere. A variable with no
    /// cost of its own gets the mean of those that have one, which keeps the
    /// priors on one scale rather than mixing an absolute floor into a model
    /// whose costs might be thousands or thousandths.
    ///
    /// When *no* integer variable carries a cost — which is exactly the unit
    /// commitment case, where the binaries are statuses and the money is on the
    /// dispatch they gate — every prior is one, the product rule reduces to
    /// `f⁻ · f⁺`, and the first branches are chosen most-fractionally. That is
    /// the right degeneracy: with nothing observed there is nothing better to
    /// go on, and the rule should fall back to the rule it is replacing rather
    /// than to the index order.
    fn new(cost: &[f64], integer: &[bool]) -> Self {
        let mut total = 0.0;
        let mut counted = 0usize;
        // Indexed through `get`, because `integer` is the caller's slice and
        // nothing has promised it is no longer than the objective. A caller who
        // passes a longer one gets priors for the columns that exist rather
        // than a panic from the branching rule.
        for (j, &is_int) in integer.iter().enumerate() {
            if is_int && cost.get(j).is_some_and(|c| *c != 0.0) {
                total += cost[j].abs();
                counted += 1;
            }
        }
        let typical = if counted == 0 {
            1.0
        } else {
            total / counted as f64
        };
        let prior = cost
            .iter()
            .map(|c| if *c == 0.0 { typical } else { c.abs() })
            .collect();
        let n = cost.len();
        Self {
            down_sum: vec![0.0; n],
            down_count: vec![0; n],
            up_sum: vec![0.0; n],
            up_count: vec![0; n],
            prior,
        }
    }

    /// Record what a branch actually cost.
    ///
    /// `degradation` is how much worse the child's relaxation is than its
    /// parent's, which for a minimisation is non-negative in exact arithmetic
    /// and can come out very slightly negative in floating point; clamping is
    /// cheaper than reasoning about what a negative average would mean.
    fn observe(&mut self, b: Branch, degradation: f64) {
        // A branch that moved nothing carries no information and would divide
        // by zero saying so.
        if b.moved <= 1e-12 || b.col >= self.prior.len() {
            return;
        }
        let unit = degradation.max(0.0) / b.moved;
        if b.up {
            self.up_sum[b.col] += unit;
            self.up_count[b.col] += 1;
        } else {
            self.down_sum[b.col] += unit;
            self.down_count[b.col] += 1;
        }
    }

    /// What one unit of fractionality in this direction is estimated to cost.
    fn estimate(&self, col: usize, up: bool) -> f64 {
        let (sum, count) = if up {
            (self.up_sum.get(col), self.up_count.get(col))
        } else {
            (self.down_sum.get(col), self.down_count.get(col))
        };
        match (sum, count) {
            (Some(s), Some(&n)) if n > 0 => s / f64::from(n),
            _ => self.prior.get(col).copied().unwrap_or(1.0),
        }
    }

    /// The integer variable whose two branches are estimated to hurt most.
    ///
    /// Scored as a product of the two estimated degradations, each floored, so
    /// that the variable chosen is one whose children are *both* worse than
    /// their parent. A sum would happily choose a variable that is ruinous one
    /// way and free the other, which produces one child that prunes and one
    /// identical to the node it replaced — the search would then have spent a
    /// level of depth to make no progress at all on half the tree.
    ///
    /// The scan runs in index order and takes a new best only on a strict
    /// improvement, so equal scores go to the lowest index. That is what makes
    /// the search reproducible: the estimates are floating point averages and
    /// exact ties between symmetric variables are the normal case in a
    /// commitment model, so the tiebreak decides real branches rather than rare
    /// ones. Iterating a map instead would give a different tree per run and a
    /// different node count per run.
    fn pick(&self, values: &[f64], integer: &[bool], tol: f64) -> Option<usize> {
        let mut best: Option<(f64, usize)> = None;
        for (j, &is_int) in integer.iter().enumerate() {
            if !is_int {
                continue;
            }
            let v = values[j];
            if integral(v, tol) {
                continue;
            }
            let down = (self.estimate(j, false) * (v - v.floor())).max(SCORE_FLOOR);
            let up = (self.estimate(j, true) * (v.ceil() - v)).max(SCORE_FLOOR);
            let score = down * up;
            if best.is_none_or(|(s, _)| score > s) {
                best = Some((score, j));
            }
        }
        best.map(|(_, j)| j)
    }
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

    let mut costs = PseudoCosts::new(p.col_cost, integer);
    // Which rule chooses the branch. Both rules see the same relaxation and
    // both return a variable that is genuinely fractional, so this changes the
    // shape of the tree and nothing about which points are feasible: the
    // incumbent, the bound and whether the two met are the same either way.
    let choose = |values: &[f64], costs: &PseudoCosts| match o.branching {
        Branching::MostFractional => pick(values, integer, o.integrality_tolerance),
        Branching::PseudoCost => costs.pick(values, integer, o.integrality_tolerance),
    };

    if choose(&root.col_value, &costs).is_none() {
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
        branch: None,
    }];

    // The per-node work sits in a labelled block, and the bound is recomputed
    // once after it, so that every way a node can leave the search goes through
    // the same update. There are five: pruned by the incumbent before solving,
    // bounds that contradict, an infeasible relaxation, a relaxation no better
    // than the incumbent, and ordinary exploration.
    //
    // They used to be four `continue`s and a fall-through, and only the
    // fall-through updated the bound. When the last open nodes all left by one
    // of the other four, `lower` kept the value it held while those nodes were
    // still on the stack, and the search finished with an empty stack and a
    // bound it had already outgrown — a closed search reporting an open gap.
    // Writing it this way means a sixth exit cannot reintroduce that.
    'search: while let Some(node) = stack.pop() {
        'node: {
            // A node whose inherited bound is already worse than a known
            // achievable cost cannot contain anything better.
            if node.parent_bound >= upper - o.gap_tolerance * upper.abs().max(1.0) {
                break 'node;
            }
            if nodes >= o.max_nodes {
                hit_limit = true;
                // The node just popped was never explored, so its bound still
                // stands over whatever it contained, alongside everything left
                // on the stack behind it.
                lower = open_bound(&stack, upper).min(node.parent_bound);
                break 'search;
            }

            let sub = Problem {
                col_lower: &node.lower,
                col_upper: &node.upper,
                ..p
            };
            let relaxed = match solve(sub, o.lp) {
                Ok(s) => s,
                // A node that will not solve is one whose bounds contradict,
                // which is a successful exclusion rather than a failure.
                Err(_) => break 'node,
            };
            nodes += 1;
            if relaxed.status != Status::Optimal {
                // An infeasible child says something too — the strongest thing
                // a branch can say — but recording it as an unbounded
                // degradation would let one such branch dominate every average
                // that variable ever accumulates. Solvers that use the
                // information count it separately; here it is simply not
                // recorded.
                break 'node;
            }
            // Whatever happens to this node, solving it has priced the branch
            // that created it, and that is worth keeping even if the node is
            // about to be pruned: a node pruned by the incumbent is a node whose
            // bound moved a long way, which is exactly the observation worth
            // having.
            if let Some(b) = node.branch {
                costs.observe(b, relaxed.objective - node.parent_bound);
            }
            if relaxed.objective >= upper {
                break 'node;
            }

            match choose(&relaxed.col_value, &costs) {
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
                    let mut push = |upward: bool| {
                        let (lo, hi) = if upward {
                            (up, f64::INFINITY)
                        } else {
                            (f64::NEG_INFINITY, down)
                        };
                        let mut l = node.lower.clone();
                        let mut u = node.upper.clone();
                        l[j] = l[j].max(lo);
                        u[j] = u[j].min(hi);
                        if l[j] <= u[j] + 1e-9 {
                            stack.push(Node {
                                lower: l,
                                upper: u,
                                parent_bound: relaxed.objective,
                                branch: Some(Branch {
                                    col: j,
                                    up: upward,
                                    moved: if upward { up - v } else { v - down },
                                }),
                            });
                        }
                    };
                    if prefer_down {
                        push(true);
                        push(false);
                    } else {
                        push(false);
                        push(true);
                    }
                }
            }
        }

        // The bound is the least relaxation value still open. With nothing
        // open, the incumbent is optimal.
        lower = open_bound(&stack, upper);
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
        // `proved` means the gap closed, and now says only that. It used to
        // also accept an empty stack, which papered over the stale bound above:
        // with the bound maintained correctly an exhausted search closes the gap
        // by itself, so the extra clause would only ever hide a recurrence.
        proved: !hit_limit && gap <= o.gap_tolerance,
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

    #[test]
    fn a_costless_set_of_binaries_makes_pseudo_cost_fall_back_to_most_fractional() {
        // The unit commitment case: the binaries are statuses and the money is
        // on the dispatch they gate, so no integer column carries a cost. With
        // every prior equal the product rule is f- times f+, which peaks at a
        // half, and the first branches are the ones most-fractional would have
        // made. Falling back to the rule being replaced is the right degeneracy
        // — falling back to index order would not be.
        let integer = [true, true, true];
        let costs = PseudoCosts::new(&[0.0, 0.0, 0.0], &integer);
        let values = [1.9, 2.5, 3.1];
        assert_eq!(costs.pick(&values, &integer, 1e-6), Some(1));
        assert_eq!(
            costs.pick(&values, &integer, 1e-6),
            pick(&values, &integer, 1e-6)
        );
    }

    #[test]
    fn a_variable_with_a_dearer_coefficient_is_preferred_before_any_history() {
        // Same fractionality on both, so only the prior can separate them.
        let integer = [true, true];
        let costs = PseudoCosts::new(&[1.0, 50.0], &integer);
        assert_eq!(costs.pick(&[0.5, 0.5], &integer, 1e-6), Some(1));
    }

    #[test]
    fn a_costless_variable_inherits_the_scale_of_the_costed_ones() {
        // Mixing an absolute floor into the priors would mean something quite
        // different in a model priced in thousands and one priced in
        // thousandths. The mean of the coefficients that exist is scale-free.
        let integer = [true, true];
        let costs = PseudoCosts::new(&[40.0, 0.0], &integer);
        assert!((costs.estimate(1, true) - 40.0).abs() < 1e-12);
    }

    #[test]
    fn an_observation_is_recorded_per_unit_of_fractionality_moved() {
        let integer = [true];
        let mut costs = PseudoCosts::new(&[0.0], &integer);
        // Moving a quarter cost two, so a whole unit is estimated at eight.
        costs.observe(
            Branch {
                col: 0,
                up: true,
                moved: 0.25,
            },
            2.0,
        );
        assert!((costs.estimate(0, true) - 8.0).abs() < 1e-12);
        // The other direction has learned nothing and still holds its prior.
        assert!((costs.estimate(0, false) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_branch_that_moved_nothing_teaches_nothing() {
        // Guarding the division rather than the caller: a zero move would give
        // an infinite estimate that no later observation could average away.
        let integer = [true];
        let mut costs = PseudoCosts::new(&[3.0], &integer);
        costs.observe(
            Branch {
                col: 0,
                up: false,
                moved: 0.0,
            },
            5.0,
        );
        assert!((costs.estimate(0, false) - 3.0).abs() < 1e-12);
    }

    #[test]
    fn a_variable_that_hurts_both_ways_beats_one_that_hurts_once() {
        // What the product buys over a sum. Column 0 is ruinous upwards and
        // free downwards, so branching on it gives one child that prunes and
        // one indistinguishable from its parent. Column 1 is moderate both
        // ways and splits the problem properly. A sum would choose column 0.
        let integer = [true, true];
        let mut costs = PseudoCosts::new(&[0.0, 0.0], &integer);
        let up = |col| Branch {
            col,
            up: true,
            moved: 0.5,
        };
        let down = |col| Branch {
            col,
            up: false,
            moved: 0.5,
        };
        costs.observe(up(0), 100.0);
        costs.observe(down(0), 0.0);
        costs.observe(up(1), 5.0);
        costs.observe(down(1), 5.0);
        assert_eq!(costs.pick(&[0.5, 0.5], &integer, 1e-6), Some(1));
    }

    #[test]
    fn equal_scores_break_on_the_lowest_index() {
        // Determinism, which in a commitment model is not an edge case: the
        // status variables of interchangeable units accumulate identical
        // histories, so exact ties are the normal situation and the tiebreak
        // decides real branches.
        let integer = [true, true, true];
        let costs = PseudoCosts::new(&[7.0, 7.0, 7.0], &integer);
        assert_eq!(costs.pick(&[0.5, 0.5, 0.5], &integer, 1e-6), Some(0));
    }

    #[test]
    fn pseudo_cost_never_picks_a_variable_that_is_already_integral() {
        let integer = [true, true];
        let mut costs = PseudoCosts::new(&[100.0, 1.0], &integer);
        costs.observe(
            Branch {
                col: 0,
                up: true,
                moved: 0.5,
            },
            1e6,
        );
        // Column 0 scores far higher on every count and is whole, so it is not
        // a candidate at all; branching on it would produce a child identical
        // to its parent and a search that never terminates.
        assert_eq!(costs.pick(&[2.0, 0.3], &integer, 1e-6), Some(1));
    }
}

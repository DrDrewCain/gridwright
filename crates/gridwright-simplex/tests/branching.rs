//! Which variable to branch on, and what choosing better is actually worth.
//!
//! Two things are tested here and they are not the same thing. The first is
//! that changing the branching rule changes only how fast the search runs and
//! never what it finds: the same optimum, proved, from both rules, on every
//! rung of the ladder. The second is the measurement itself, which is the point
//! of the exercise — a branching rule that is not measured is a preference.
//!
//! # The trap this file exists to avoid
//!
//! Most unit commitment relaxations come out integral on their own. A
//! commitment model whose demand happens to be met by units running flat out is
//! solved by its own relaxation, the search never branches, and a comparison of
//! branching rules built on such a model compares nothing at all — both rules
//! report one node and identical times, and the table looks like a result.
//!
//! So the models here are built so the relaxation is *provably* fractional, and
//! there is a test that says so rather than a comment claiming it. Dropping the
//! integrality leaves a unit free to set its status to exactly the fraction of
//! its capacity it is producing, and the demand profile is chosen so that in
//! every period some unit is loaded strictly between its minimum and its
//! maximum. That fraction is the fractional variable the search has to resolve.
//!
//! # Running the measurement
//!
//! ```text
//! cargo test -p gridwright-simplex --test branching --release \
//!     -- --ignored --nocapture
//! ```
//!
//! Ignored by default because it takes minutes and its output is a table for a
//! human rather than an assertion. Debug builds are perhaps twenty times slower
//! here and the ratio between the two rules is not the same one, so the release
//! flag is not optional.

use std::time::Instant;

use gridwright_simplex::{Branching, Cuts, MipOptions, Problem, Status, solve, solve_mip};

// ---------------------------------------------------------------------------
// A unit commitment problem, as a bare linear program
// ---------------------------------------------------------------------------

/// A commitment model in the shape the solver takes.
///
/// Written out here rather than built through `gridwright-net`, because this
/// crate sits below that one and because a measurement wants the problem it is
/// measuring to be visible in the file that reports the numbers.
///
/// Per thermal unit `i` and period `t` there are three columns:
///
/// - `p[i][t]`, what it produces, priced at its marginal cost;
/// - `u[i][t]`, whether it is running, integer and priced at nothing;
/// - `s[i][t]`, whether this period is a start, priced at its start-up cost.
///
/// and four families of rows:
///
/// ```text
///   p − Pmax·u ≤ 0        a unit that is off produces nothing
///   p − Pmin·u ≥ 0        a unit that is on produces at least its minimum
///   Σ p + gas = D         demand is met
///   s − u[t] + u[t−1] ≥ 0 turning on is a start
/// ```
///
/// The last one is why `s` need not be integer: it is pushed down by its own
/// cost and up by the difference of two binaries, so at any integral `u` it
/// lands on an integer by itself.
///
/// A continuous, expensive gas unit stands behind the fleet in every period, so
/// every rung is feasible whatever the thermal units do. Without it a demand
/// that falls between two commitments would make the model infeasible rather
/// than expensive, and the ladder would be measuring how fast the search proves
/// there is nothing to find.
struct Commitment {
    n_cols: usize,
    n_rows: usize,
    starts: Vec<u32>,
    rows: Vec<u32>,
    vals: Vec<f64>,
    lower: Vec<f64>,
    upper: Vec<f64>,
    cost: Vec<f64>,
    row_lower: Vec<f64>,
    row_upper: Vec<f64>,
    integer: Vec<bool>,
}

impl Commitment {
    fn problem(&self) -> Problem<'_> {
        Problem {
            n_cols: self.n_cols,
            n_rows: self.n_rows,
            col_starts: &self.starts,
            row_indices: &self.rows,
            values: &self.vals,
            col_lower: &self.lower,
            col_upper: &self.upper,
            col_cost: &self.cost,
            row_lower: &self.row_lower,
            row_upper: &self.row_upper,
        }
    }
}

/// The capacity, stable minimum, marginal cost and start-up cost of unit `i`.
///
/// Spread rather than identical, because a fleet of clones is a symmetric
/// problem and symmetric problems flatter any branching rule that breaks ties
/// by index. These differ enough that the units are genuinely orderable and
/// little enough that the ordering is not obvious.
fn unit(i: usize) -> (f64, f64, f64, f64) {
    let p_max = 100.0 + 20.0 * i as f64;
    (
        p_max,
        0.5 * p_max,
        10.0 + 3.0 * i as f64,
        400.0 + 50.0 * i as f64,
    )
}

/// Demand in period `t`, as a fraction of the fleet, under demand profile
/// `variant`.
///
/// The `+ 13` is what keeps the relaxation fractional. Without it a demand that
/// happened to equal the sum of some units' capacities would be met by running
/// those units flat out, every status would come out at one, and the search
/// would have nothing to do — the exact failure this file is written against.
///
/// The variant exists because a branching rule measured on one demand profile
/// has been measured on one problem. Stepping by seven modulo eleven walks the
/// whole range of shapes without repeating inside any horizon here, so the
/// variants are genuinely different problems rather than the same problem
/// shifted.
fn demand(t: usize, fleet: f64, variant: usize) -> f64 {
    let step = (t * 7 + variant * 3) % 11;
    let shape = 0.38 + 0.34 * (step as f64) / 10.0;
    fleet * shape + 13.0 + variant as f64
}

/// Build the rung with `units` thermal units over `periods` periods, on demand
/// profile `variant`.
fn commitment(units: usize, periods: usize, variant: usize) -> Commitment {
    let u_t = units * periods;
    // p, then u, then s, then the gas unit's output per period.
    let n_cols = 3 * u_t + periods;
    // Capacity, minimum, demand, start.
    let n_rows = 2 * u_t + periods + u_t;

    let cap_row = |i: usize, t: usize| (i * periods + t) as u32;
    let min_row = |i: usize, t: usize| (u_t + i * periods + t) as u32;
    let demand_row = |t: usize| (2 * u_t + t) as u32;
    let start_row = |i: usize, t: usize| (2 * u_t + periods + i * periods + t) as u32;

    let fleet: f64 = (0..units).map(|i| unit(i).0).sum();

    let mut starts = vec![0u32];
    let mut rows = Vec::new();
    let mut vals = Vec::new();
    let mut lower = Vec::new();
    let mut upper = Vec::new();
    let mut cost = Vec::new();
    let mut integer = Vec::new();

    // Entries go in increasing row order within each column, which is what a
    // reader expects of compressed sparse column even where the solver does not
    // insist on it.
    let mut push = |entries: &[(u32, f64)],
                    lo: f64,
                    hi: f64,
                    c: f64,
                    is_int: bool,
                    rows: &mut Vec<u32>,
                    vals: &mut Vec<f64>,
                    starts: &mut Vec<u32>| {
        for &(r, v) in entries {
            rows.push(r);
            vals.push(v);
        }
        starts.push(rows.len() as u32);
        lower.push(lo);
        upper.push(hi);
        cost.push(c);
        integer.push(is_int);
    };

    for i in 0..units {
        for t in 0..periods {
            let (p_max, _, marginal, _) = unit(i);
            push(
                &[
                    (cap_row(i, t), 1.0),
                    (min_row(i, t), 1.0),
                    (demand_row(t), 1.0),
                ],
                0.0,
                p_max,
                marginal,
                false,
                &mut rows,
                &mut vals,
                &mut starts,
            );
        }
    }
    for i in 0..units {
        for t in 0..periods {
            let (p_max, p_min, _, _) = unit(i);
            // The status appears in its own start row negatively and in the
            // next period's positively: starting is `u[t] − u[t−1]`.
            let mut entries = vec![
                (cap_row(i, t), -p_max),
                (min_row(i, t), -p_min),
                (start_row(i, t), -1.0),
            ];
            if t + 1 < periods {
                entries.push((start_row(i, t + 1), 1.0));
            }
            push(
                &entries,
                0.0,
                1.0,
                0.0,
                true,
                &mut rows,
                &mut vals,
                &mut starts,
            );
        }
    }
    for i in 0..units {
        for t in 0..periods {
            let (_, _, _, start_cost) = unit(i);
            push(
                &[(start_row(i, t), 1.0)],
                0.0,
                1.0,
                start_cost,
                false,
                &mut rows,
                &mut vals,
                &mut starts,
            );
        }
    }
    for t in 0..periods {
        push(
            &[(demand_row(t), 1.0)],
            0.0,
            fleet,
            80.0,
            false,
            &mut rows,
            &mut vals,
            &mut starts,
        );
    }

    let mut row_lower = vec![0.0; n_rows];
    let mut row_upper = vec![0.0; n_rows];
    for i in 0..units {
        for t in 0..periods {
            // p − Pmax·u ≤ 0.
            row_lower[cap_row(i, t) as usize] = f64::NEG_INFINITY;
            row_upper[cap_row(i, t) as usize] = 0.0;
            // p − Pmin·u ≥ 0.
            row_lower[min_row(i, t) as usize] = 0.0;
            row_upper[min_row(i, t) as usize] = f64::INFINITY;
            // s − u[t] + u[t−1] ≥ 0, with everything off before the horizon.
            row_lower[start_row(i, t) as usize] = 0.0;
            row_upper[start_row(i, t) as usize] = f64::INFINITY;
        }
    }
    for t in 0..periods {
        let d = demand(t, fleet, variant);
        row_lower[demand_row(t) as usize] = d;
        row_upper[demand_row(t) as usize] = d;
    }

    Commitment {
        n_cols,
        n_rows,
        starts,
        rows,
        vals,
        lower,
        upper,
        cost,
        row_lower,
        row_upper,
        integer,
    }
}

/// The rungs the measurement walks, smallest first.
///
/// It reaches twelve units over twelve periods because that is where the answer
/// is: the two rules are within noise of each other below about two hundred
/// columns, and the gap between them opens with size. Stopping at the small end
/// would have concluded that branching does not matter.
const LADDER: [(usize, usize); 5] = [(4, 4), (6, 6), (8, 8), (10, 10), (12, 12)];

/// The rungs the correctness tests use.
///
/// A shorter ladder than the measurement's, because these run on every
/// `cargo test` and in a debug build, where a node costs perhaps twenty times
/// what it costs optimised. The top two rungs take half an hour that way and
/// prove nothing the smaller ones do not: what they measure is speed, and
/// speed is what the ignored test is for.
const CHECKED: [(usize, usize); 3] = [(4, 4), (5, 5), (6, 6)];

/// The demand profiles each rung is built on.
///
/// Three rather than one, because a rule that wins on a single profile has won
/// once. The correctness tests use the first; the measurement uses all of them,
/// and reports each separately rather than averaging, since an average of three
/// hides the case where a rule wins twice and loses catastrophically.
const VARIANTS: [usize; 3] = [0, 1, 2];

fn options(rule: Branching) -> MipOptions {
    MipOptions {
        // Generous enough that every rung closes its gap, so the ladder
        // compares two searches that both finished rather than two that both
        // gave up at the same ceiling.
        max_nodes: 200_000,
        branching: rule,
        // Cutting is off here, and deliberately, though it is on by default.
        // Root cuts remove three quarters of this ladder's tree before the
        // branching rule ever chooses anything, so leaving them on would make
        // every number in this file a measurement of the cuts. The two changes
        // are worth knowing separately, and `tests/cuts.rs` measures the other
        // one against this same generator.
        cuts: Cuts::Off,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// That the ladder exercises what it claims to
// ---------------------------------------------------------------------------

#[test]
fn every_rung_has_a_relaxation_that_is_genuinely_fractional() {
    // The trap in the module comment, as an assertion. A rung whose relaxation
    // is integral is solved before the search starts and measures nothing, and
    // that failure is silent: the numbers come out, they are just numbers about
    // a search that did not happen.
    for (units, periods) in CHECKED {
        let c = commitment(units, periods, VARIANTS[0]);
        let relaxed = solve(c.problem(), Default::default()).unwrap();
        assert_eq!(relaxed.status, Status::Optimal, "{units}x{periods}");
        let fractional = c
            .integer
            .iter()
            .enumerate()
            .filter(|&(j, &is_int)| {
                is_int && (relaxed.col_value[j] - relaxed.col_value[j].round()).abs() > 1e-6
            })
            .count();
        assert!(
            fractional > 0,
            "{units}x{periods}: the relaxation came out integral, so this rung \
             exercises the branching not at all"
        );
    }
}

#[test]
fn every_rung_needs_more_than_one_node() {
    // The same point from the other side: a fractional relaxation that the
    // first branch resolves is barely a search either.
    for (units, periods) in CHECKED {
        let c = commitment(units, periods, VARIANTS[0]);
        let r = solve_mip(c.problem(), &c.integer, options(Branching::MostFractional)).unwrap();
        assert!(r.nodes > 1, "{units}x{periods}: {} nodes", r.nodes);
    }
}

// ---------------------------------------------------------------------------
// That the rule changes the search and not the answer
// ---------------------------------------------------------------------------

#[test]
fn both_rules_find_the_same_optimum_and_prove_it() {
    // The whole safety argument for touching the branching at all. Both rules
    // pick a variable that is genuinely fractional and split its range at an
    // integer, so both trees contain every integer point the root contained.
    // Which is explored first decides how long it takes and nothing else.
    for (units, periods) in CHECKED {
        let c = commitment(units, periods, VARIANTS[0]);
        let a = solve_mip(c.problem(), &c.integer, options(Branching::MostFractional)).unwrap();
        let b = solve_mip(c.problem(), &c.integer, options(Branching::PseudoCost)).unwrap();

        assert_eq!(a.status, Status::Optimal, "{units}x{periods}");
        assert_eq!(b.status, Status::Optimal, "{units}x{periods}");
        assert!(a.proved && b.proved, "{units}x{periods}: neither may guess");
        assert!(
            (a.objective - b.objective).abs() < 1e-6 * a.objective.abs().max(1.0),
            "{units}x{periods}: {} against {}",
            a.objective,
            b.objective
        );
    }
}

#[test]
fn the_bound_and_the_incumbent_are_reported_separately() {
    // Two numbers, and the search must not conflate them under either rule.
    // The incumbent is achievable and the bound is not, and a proved answer is
    // the case where they have met rather than the case where one was copied
    // over the other.
    for rule in [Branching::MostFractional, Branching::PseudoCost] {
        for (units, periods) in CHECKED {
            let c = commitment(units, periods, VARIANTS[0]);
            let r = solve_mip(c.problem(), &c.integer, options(rule)).unwrap();
            assert!(
                r.lower_bound <= r.objective + 1e-6 * r.objective.abs().max(1.0),
                "{rule:?} {units}x{periods}: bound {} above the answer {}",
                r.lower_bound,
                r.objective
            );
            assert!(r.proved, "{rule:?} {units}x{periods}");

            let relaxed = solve(c.problem(), Default::default()).unwrap();
            assert!(
                r.objective >= relaxed.objective - 1e-6,
                "{rule:?} {units}x{periods}: the integer answer beat its own relaxation"
            );
        }
    }
}

#[test]
fn pseudo_cost_branching_returns_an_integral_point() {
    // A different tree must not mean a different notion of what counts as an
    // answer. Every variable marked integer comes back whole.
    for (units, periods) in CHECKED {
        let c = commitment(units, periods, VARIANTS[0]);
        let r = solve_mip(c.problem(), &c.integer, options(Branching::PseudoCost)).unwrap();
        for (j, &is_int) in c.integer.iter().enumerate() {
            if is_int {
                let v = r.col_value[j];
                assert!(
                    (v - v.round()).abs() < 1e-6,
                    "{units}x{periods}: column {j} came back at {v}"
                );
            }
        }
    }
}

#[test]
fn the_same_model_gives_the_same_node_count_every_run() {
    // Determinism is not a nicety here. Pseudo-costs are floating point
    // averages over variables that a commitment model makes near-identical, so
    // exact ties are the normal case rather than a rare one, and a tiebreak
    // that followed the iteration order of a map would give a different tree —
    // and a different node count, and a different time — on every run. Ranking
    // by index makes the search reproducible.
    for rule in [Branching::MostFractional, Branching::PseudoCost] {
        for (units, periods) in CHECKED {
            let c = commitment(units, periods, VARIANTS[0]);
            let first = solve_mip(c.problem(), &c.integer, options(rule)).unwrap();
            for _ in 0..2 {
                let again = solve_mip(c.problem(), &c.integer, options(rule)).unwrap();
                assert_eq!(
                    first.nodes, again.nodes,
                    "{rule:?} {units}x{periods}: node count moved between runs"
                );
                assert_eq!(
                    first.col_value, again.col_value,
                    "{rule:?} {units}x{periods}"
                );
            }
        }
    }
}

#[test]
fn the_default_is_the_rule_that_measured_better() {
    // Pinning the decision, so that changing the default is a deliberate act
    // with a measurement behind it rather than an edit that nothing notices.
    // The measurement is in `Branching`'s own documentation: two to four times
    // fewer nodes at the top of the ladder, within noise at the bottom, and no
    // measurable cost per node either way.
    assert_eq!(MipOptions::default().branching, Branching::PseudoCost);
}

#[test]
fn a_node_budget_still_stops_the_search_under_either_rule() {
    // The anytime property survives the change of rule: a starved search says
    // it did not prove anything, and whatever it found is still achievable.
    let c = commitment(6, 6, VARIANTS[0]);
    for rule in [Branching::MostFractional, Branching::PseudoCost] {
        let full = solve_mip(c.problem(), &c.integer, options(rule)).unwrap();
        let starved = solve_mip(
            c.problem(),
            &c.integer,
            MipOptions {
                max_nodes: 4,
                branching: rule,
                cuts: Cuts::Off,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(starved.nodes <= 5, "{rule:?}: {} nodes", starved.nodes);
        assert!(!starved.proved, "{rule:?}");
        if starved.status == Status::Optimal {
            assert!(
                starved.objective >= full.objective - 1e-6,
                "{rule:?}: a partial search beat the proved optimum"
            );
        }
    }
}

#[test]
#[ignore = "half a minute, and it is about the ladder's top rung"]
fn at_the_default_budget_the_rule_decides_whether_the_answer_is_proved_at_all() {
    // The claim in `Branching`'s documentation that the ratio understates the
    // change, checked rather than asserted in prose. The default ceiling is
    // five thousand nodes; the top rung takes seven thousand under
    // most-fractional and sixteen hundred under pseudo-cost, so on the same
    // budget one of them comes back saying it could not tell and the other
    // comes back with a proof.
    let c = commitment(12, 12, VARIANTS[0]);
    let budgeted = |rule| {
        solve_mip(
            c.problem(),
            &c.integer,
            MipOptions {
                branching: rule,
                // Off for the same reason `options` turns it off: this is a
                // claim about how far each branching rule gets on the default
                // node budget, and root cuts would decide it instead.
                cuts: Cuts::Off,
                ..Default::default()
            },
        )
        .unwrap()
    };
    let mf = budgeted(Branching::MostFractional);
    let pc = budgeted(Branching::PseudoCost);

    assert!(
        !mf.proved,
        "most-fractional finished inside the budget after all"
    );
    assert!(pc.proved, "pseudo-cost failed to prove the top rung");
    // And what the unproved run did find is still achievable, so it cannot be
    // better than the proved optimum. A search that ran out of budget must
    // report an honest incumbent, not an optimistic one.
    if mf.status == Status::Optimal {
        assert!(
            mf.objective >= pc.objective - 1e-6,
            "the unproved incumbent beat the proved optimum"
        );
    }
}

// ---------------------------------------------------------------------------
// The measurement
// ---------------------------------------------------------------------------

/// Best of this many runs per cell.
///
/// Wall-clock on this machine moves a great deal with whatever else is running,
/// and an average carries that noise into the number while a minimum does not:
/// background load can only ever make a run slower, so the fastest of several
/// is the closest estimate available of what the work actually costs. Five is
/// enough to make the estimate stable to a few percent and cheap enough that
/// the whole table finishes in a couple of minutes.
const REPEATS: usize = 5;

#[test]
#[ignore = "minutes, and its output is a table rather than an assertion"]
fn the_two_branching_rules_are_measured_against_each_other() {
    println!();
    println!(
        "{:>10} {:>4}  {:>5} {:>5}  {:>9} {:>10}  {:>9} {:>10}  {:>7}",
        "units×per", "prof", "cols", "rows", "mf nodes", "mf time", "pc nodes", "pc time", "ratio"
    );

    for (units, periods) in LADDER {
        for variant in VARIANTS {
            let c = commitment(units, periods, variant);
            let mut cell = Vec::new();
            for rule in [Branching::MostFractional, Branching::PseudoCost] {
                let mut best = f64::INFINITY;
                let mut nodes = 0usize;
                // One untimed run first. Whichever rule is measured first
                // otherwise pays for faulting in the pages this model touches,
                // which on the small rungs is larger than the difference being
                // measured — the 4×4 cells came out two to one on time at
                // identical node counts until this was here.
                let _ = solve_mip(c.problem(), &c.integer, options(rule)).unwrap();
                for _ in 0..REPEATS {
                    let t0 = Instant::now();
                    let r = solve_mip(c.problem(), &c.integer, options(rule)).unwrap();
                    let secs = t0.elapsed().as_secs_f64();
                    assert_eq!(r.status, Status::Optimal);
                    assert!(r.proved);
                    nodes = r.nodes;
                    best = best.min(secs);
                }
                cell.push((nodes, best));
            }
            let (mf_nodes, mf_time) = cell[0];
            let (pc_nodes, pc_time) = cell[1];
            println!(
                "{:>10} {:>4}  {:>5} {:>5}  {:>9} {:>9.1}ms  {:>9} {:>9.1}ms  {:>6.2}×",
                format!("{units}×{periods}"),
                variant,
                c.n_cols,
                c.n_rows,
                mf_nodes,
                mf_time * 1e3,
                pc_nodes,
                pc_time * 1e3,
                mf_time / pc_time,
            );
        }
    }
    println!();
    println!("best of {REPEATS} runs; ratio above one means pseudo-cost won");
}


#[test]
fn a_proved_answer_has_actually_closed_its_gap() {
    // `proved` is a claim about the bound, not about the search having stopped.
    //
    // It used to be granted whenever the stack emptied, whatever the bound
    // said, and the bound was recomputed on only one of the five ways a node
    // can leave the search — the four early exits skipped it. When the last
    // open nodes all left by one of those four, the search finished holding a
    // bound it had already outgrown, and reported a closed search alongside an
    // open gap.
    //
    // On this generator that was not a rounding wrinkle: every rung below
    // reported `proved` while still carrying a gap, the worst of them 7.2%. A
    // caller reading `proved` to mean "this is the optimum" was being told so
    // by a search that could not yet know it.
    //
    // Commitment problems are where it shows, because they prune late: the
    // incumbent arrives early and the tail of the tree dies to the bound test
    // rather than to exploration. Knapsacks do not reproduce it at all, which
    // is why this test lives beside the generator that does.
    for units in 2..7usize {
        for periods in 2..7usize {
            for variant in VARIANTS {
                for rule in [Branching::MostFractional, Branching::PseudoCost] {
                    let c = commitment(units, periods, variant);
                    let o = options(rule);
                    let r = solve_mip(c.problem(), &c.integer, o).unwrap();
                    let what = format!("{units}x{periods} v{variant} {rule:?}");

                    assert_eq!(r.status, Status::Optimal, "{what}");
                    assert!(r.proved, "{what}: a rung this size should finish");
                    assert!(
                        r.gap <= o.gap_tolerance,
                        "{what}: proved with an open gap of {}",
                        r.gap
                    );
                    // The two numbers it proved it from have to have met, since
                    // the gap is exactly what separates them.
                    assert!(
                        (r.lower_bound - r.objective).abs()
                            <= 1e-6 * r.objective.abs().max(1.0),
                        "{what}: bound {} against answer {}",
                        r.lower_bound,
                        r.objective
                    );
                }
            }
        }
    }
}

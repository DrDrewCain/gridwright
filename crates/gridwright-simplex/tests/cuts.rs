//! What cutting planes are worth, on two families of model that answer
//! differently.
//!
//! Three things are tested here and they are not the same thing. The first is
//! that adding cuts changes only how fast the search runs and never what it
//! finds: the same optimum, proved, with every cut setting, on every rung. The
//! second is that the cuts are actually generated — a cut setting that silently
//! separates nothing measures nothing, and would look like a clean negative
//! result. The third is the measurement itself.
//!
//! # Why two families
//!
//! Cuts help very unevenly by structure, and a single family would have
//! concluded whatever that family happened to say. So there are two:
//!
//! - **Unit commitment**, the same generator the branching measurement uses, so
//!   the numbers are comparable with the table already recorded there. Its rows
//!   mix a continuous dispatch variable with a binary status, which means it has
//!   no knapsack rows at all and cover cuts have nothing to work on. Gomory cuts
//!   do.
//! - **Multidimensional knapsack**, where every row is a knapsack over binaries
//!   and both families apply. This is the shape cover cuts were invented for,
//!   and if they do not pay here they do not pay anywhere.
//!
//! # Running the measurement
//!
//! ```text
//! cargo test -p gridwright-simplex --test cuts --release \
//!     -- --ignored --nocapture
//! ```
//!
//! Ignored by default because it takes minutes and its output is a table for a
//! human rather than an assertion.

use std::time::Instant;

use gridwright_simplex::{Cuts, MipOptions, Problem, Status, solve, solve_mip};

// ---------------------------------------------------------------------------
// A unit commitment problem, as a bare linear program
// ---------------------------------------------------------------------------

/// A commitment model in the shape the solver takes.
///
/// The same generator as `tests/branching.rs`, repeated rather than shared
/// because each test binary is its own crate and because a measurement wants
/// the problem it is measuring to be visible in the file that reports the
/// numbers. Any change to one has to be made to the other, and the node counts
/// being comparable between the two files is the reason to bother.
///
/// Per thermal unit `i` and period `t` there are three columns:
///
/// - `p[i][t]`, what it produces, priced at its marginal cost;
/// - `u[i][t]`, whether it is running, integer and priced at nothing;
/// - `s[i][t]`, whether this period is a start, priced at its start-up cost;
///
/// and four families of rows:
///
/// ```text
///   p − Pmax·u ≤ 0        a unit that is off produces nothing
///   p − Pmin·u ≥ 0        a unit that is on produces at least its minimum
///   Σ p + gas = D         demand is met
///   s − u[t] + u[t−1] ≥ 0 turning on is a start
/// ```
struct Model {
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

impl Model {
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
fn unit(i: usize) -> (f64, f64, f64, f64) {
    let p_max = 100.0 + 20.0 * i as f64;
    (
        p_max,
        0.5 * p_max,
        10.0 + 3.0 * i as f64,
        400.0 + 50.0 * i as f64,
    )
}

/// Demand in period `t`, as a fraction of the fleet, under profile `variant`.
///
/// The `+ 13` is what keeps the relaxation fractional; without it a demand that
/// happened to equal the sum of some units' capacities would be met by running
/// those units flat out and the search would have nothing to do.
fn demand(t: usize, fleet: f64, variant: usize) -> f64 {
    let step = (t * 7 + variant * 3) % 11;
    let shape = 0.38 + 0.34 * (step as f64) / 10.0;
    fleet * shape + 13.0 + variant as f64
}

/// Build the rung with `units` thermal units over `periods` periods.
fn commitment(units: usize, periods: usize, variant: usize) -> Model {
    let u_t = units * periods;
    let n_cols = 3 * u_t + periods;
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
            row_lower[cap_row(i, t) as usize] = f64::NEG_INFINITY;
            row_upper[cap_row(i, t) as usize] = 0.0;
            row_lower[min_row(i, t) as usize] = 0.0;
            row_upper[min_row(i, t) as usize] = f64::INFINITY;
            row_lower[start_row(i, t) as usize] = 0.0;
            row_upper[start_row(i, t) as usize] = f64::INFINITY;
        }
    }
    for t in 0..periods {
        let d = demand(t, fleet, variant);
        row_lower[demand_row(t) as usize] = d;
        row_upper[demand_row(t) as usize] = d;
    }

    Model {
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

// ---------------------------------------------------------------------------
// A multidimensional knapsack
// ---------------------------------------------------------------------------

/// A deterministic stream, so a rung is the same problem on every run and on
/// every machine.
fn stream(seed: u64) -> impl FnMut(u64) -> u64 {
    let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    move |modulus| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % modulus
    }
}

/// Choose `items` of the `n` on offer, subject to `constraints` capacities.
///
/// ```text
///   maximise  Σ vⱼ xⱼ
///   subject to  Σ wᵢⱼ xⱼ ≤ cᵢ ,  x binary
/// ```
///
/// The half-of-total capacity is the standard hard setting for this family: a
/// much tighter capacity is solved by taking almost nothing and a much looser
/// one by taking almost everything, and it is in the middle that the relaxation
/// is furthest from integral. Every row is a knapsack over binaries by
/// construction, which is what makes this the family cover cuts should win on
/// if they win anywhere.
fn knapsack(n: usize, constraints: usize, variant: usize) -> Model {
    let mut next = stream(1_000 + variant as u64);
    let mut weight = vec![vec![0.0f64; n]; constraints];
    for row in weight.iter_mut() {
        for w in row.iter_mut() {
            *w = (5 + next(45)) as f64;
        }
    }
    // Value correlated with weight, which is what makes the family hard: with
    // values independent of weights the greedy order is nearly optimal and the
    // search finds the answer at once.
    let value: Vec<f64> = (0..n)
        .map(|j| {
            let total: f64 = weight.iter().map(|row| row[j]).sum();
            total / constraints as f64 + (1 + next(10)) as f64
        })
        .collect();

    let mut starts = vec![0u32];
    let mut rows = Vec::new();
    let mut vals = Vec::new();
    for j in 0..n {
        for (i, row) in weight.iter().enumerate() {
            rows.push(i as u32);
            vals.push(row[j]);
        }
        starts.push(rows.len() as u32);
    }

    let row_lower = vec![f64::NEG_INFINITY; constraints];
    let row_upper: Vec<f64> = weight
        .iter()
        .map(|row| (row.iter().sum::<f64>() * 0.5).floor())
        .collect();

    Model {
        n_cols: n,
        n_rows: constraints,
        starts,
        rows,
        vals,
        lower: vec![0.0; n],
        upper: vec![1.0; n],
        // Minimisation of the negated value, since the solver minimises.
        cost: value.iter().map(|v| -v).collect(),
        row_lower,
        row_upper,
        integer: vec![true; n],
    }
}

// ---------------------------------------------------------------------------
// The ladders
// ---------------------------------------------------------------------------

/// The commitment rungs the measurement walks.
const COMMITMENT_LADDER: [(usize, usize); 4] = [(6, 6), (8, 8), (10, 10), (12, 12)];

/// The commitment rungs the correctness tests use, which run in a debug build
/// on every `cargo test` and so have to stay small.
const COMMITMENT_CHECKED: [(usize, usize); 3] = [(4, 4), (5, 5), (6, 6)];

/// The knapsack rungs: items, and how many capacities constrain them.
const KNAPSACK_LADDER: [(usize, usize); 4] = [(24, 5), (30, 5), (36, 5), (40, 8)];

/// The knapsack rungs the correctness tests use.
const KNAPSACK_CHECKED: [(usize, usize); 3] = [(14, 3), (18, 4), (22, 5)];

const VARIANTS: [usize; 3] = [0, 1, 2];

/// Every setting the measurement compares, in the order the table prints them.
const SETTINGS: [Cuts; 4] = [Cuts::Off, Cuts::Gomory, Cuts::Cover, Cuts::Both];

fn options(cuts: Cuts) -> MipOptions {
    MipOptions {
        // Generous enough that every rung closes its gap, so the ladder
        // compares searches that all finished rather than several that gave up
        // at the same ceiling.
        max_nodes: 200_000,
        cuts,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// That the ladders exercise what they claim to
// ---------------------------------------------------------------------------

#[test]
fn every_rung_has_a_relaxation_that_is_genuinely_fractional() {
    // A rung whose relaxation is integral is solved before the search starts
    // and measures nothing, and that failure is silent: the numbers come out,
    // they are just numbers about a search that did not happen.
    let mut models: Vec<(String, Model)> = Vec::new();
    for (units, periods) in COMMITMENT_CHECKED {
        models.push((format!("{units}x{periods}"), commitment(units, periods, 0)));
    }
    for (n, c) in KNAPSACK_CHECKED {
        models.push((format!("knapsack {n}x{c}"), knapsack(n, c, 0)));
    }
    for (what, model) in models {
        let relaxed = solve(model.problem(), Default::default()).unwrap();
        assert_eq!(relaxed.status, Status::Optimal, "{what}");
        let fractional = model
            .integer
            .iter()
            .enumerate()
            .filter(|&(j, &is_int)| {
                is_int && (relaxed.col_value[j] - relaxed.col_value[j].round()).abs() > 1e-6
            })
            .count();
        assert!(fractional > 0, "{what}: the relaxation came out integral");
    }
}

#[test]
fn the_families_actually_separate_something() {
    // The failure this test exists to catch is a cut setting that quietly
    // generates nothing: every table would then show cuts costing nothing and
    // buying nothing, which reads as a clean negative result and is in fact a
    // measurement of an unwired switch.
    let commitment = commitment(6, 6, 0);
    let gomory = solve_mip(
        commitment.problem(),
        &commitment.integer,
        options(Cuts::Gomory),
    )
    .unwrap();
    assert!(
        gomory.cuts_kept > 0,
        "no Gomory cuts on a commitment model: {} generated",
        gomory.cuts_generated
    );

    // Commitment rows pair a continuous dispatch variable with a binary status,
    // so none of them is a knapsack over binaries and cover cuts must find
    // nothing at all. That is a fact about the model rather than a failure, and
    // it is worth pinning: a cover separator that claimed a cut here would be
    // inventing a bound on the dispatch.
    let cover = solve_mip(
        commitment.problem(),
        &commitment.integer,
        options(Cuts::Cover),
    )
    .unwrap();
    assert_eq!(
        cover.cuts_kept, 0,
        "a commitment model has no knapsack rows for a cover cut to come from"
    );

    let sack = knapsack(22, 5, 0);
    for family in [Cuts::Gomory, Cuts::Cover] {
        let r = solve_mip(sack.problem(), &sack.integer, options(family)).unwrap();
        assert!(
            r.cuts_kept > 0,
            "{family:?} separated nothing on a knapsack"
        );
    }
}

// ---------------------------------------------------------------------------
// That cuts change the search and not the answer
// ---------------------------------------------------------------------------

#[test]
fn every_cut_setting_finds_the_same_optimum_and_proves_it() {
    // The whole safety argument, end to end. A cut is only allowed to remove
    // points that no integer solution occupies, so the optimum has to survive
    // every setting — and a cut that removed it would not announce itself, it
    // would simply return a different number and call it proved.
    let mut models: Vec<(String, Model)> = Vec::new();
    for (units, periods) in COMMITMENT_CHECKED {
        for variant in VARIANTS {
            models.push((
                format!("{units}x{periods} v{variant}"),
                commitment(units, periods, variant),
            ));
        }
    }
    for (n, c) in KNAPSACK_CHECKED {
        for variant in VARIANTS {
            models.push((
                format!("knapsack {n}x{c} v{variant}"),
                knapsack(n, c, variant),
            ));
        }
    }

    for (what, model) in models {
        let plain = solve_mip(model.problem(), &model.integer, options(Cuts::Off)).unwrap();
        assert_eq!(plain.status, Status::Optimal, "{what}");
        assert!(plain.proved, "{what}");
        for cuts in SETTINGS {
            let r = solve_mip(model.problem(), &model.integer, options(cuts)).unwrap();
            assert_eq!(r.status, Status::Optimal, "{what} {cuts:?}");
            assert!(r.proved, "{what} {cuts:?}: the search must not guess");
            assert!(
                (r.objective - plain.objective).abs() <= 1e-6 * plain.objective.abs().max(1.0),
                "{what} {cuts:?}: {} against {} without cuts",
                r.objective,
                plain.objective
            );
            for (j, &is_int) in model.integer.iter().enumerate() {
                if is_int {
                    let v = r.col_value[j];
                    assert!(
                        (v - v.round()).abs() < 1e-6,
                        "{what} {cuts:?}: column {j} came back at {v}"
                    );
                }
            }
        }
    }
}

#[test]
fn cutting_leaves_the_caller_exactly_the_duals_it_asked_for() {
    // Cuts are rows, so the relaxation the search actually solves has more rows
    // than the caller's problem. Everything past the caller's row count belongs
    // to a cut and is not theirs to read, and handing back a longer vector
    // would silently shift the meaning of every index past it in whatever the
    // caller indexes by row.
    let model = knapsack(18, 4, 0);
    for cuts in SETTINGS {
        let r = solve_mip(model.problem(), &model.integer, options(cuts)).unwrap();
        assert_eq!(r.row_dual.len(), model.n_rows, "{cuts:?}");
        assert_eq!(r.col_value.len(), model.n_cols, "{cuts:?}");
    }
}

#[test]
fn cutting_never_loosens_the_root_bound() {
    // Cuts only remove points, so the relaxation they leave cannot be cheaper
    // than the one they started from, and it cannot pass the integer optimum
    // either. Those two inequalities bracket every honest cut, and a violation
    // of the second is the signature of a cut that removed a feasible point.
    for (n, c) in KNAPSACK_CHECKED {
        let model = knapsack(n, c, 0);
        let plain = solve_mip(model.problem(), &model.integer, options(Cuts::Off)).unwrap();
        for cuts in SETTINGS {
            let r = solve_mip(model.problem(), &model.integer, options(cuts)).unwrap();
            assert!(
                r.root_bound >= plain.root_bound - 1e-6,
                "knapsack {n}x{c} {cuts:?}: the root bound fell from {} to {}",
                plain.root_bound,
                r.root_bound
            );
            assert!(
                r.root_bound <= r.objective + 1e-6 * r.objective.abs().max(1.0),
                "knapsack {n}x{c} {cuts:?}: the root bound {} passed the optimum {}",
                r.root_bound,
                r.objective
            );
        }
    }
}

#[test]
fn the_same_model_gives_the_same_node_count_every_run() {
    // Determinism, which cutting could break in a way branching cannot: the
    // candidates are ranked by a floating point efficacy and truncated to a
    // budget, so a ranking without a deterministic tiebreak would keep a
    // different set of cuts, and then a different tree, on every run.
    let models = [knapsack(20, 4, 0), commitment(5, 5, 0)];
    for model in models {
        for cuts in SETTINGS {
            let first = solve_mip(model.problem(), &model.integer, options(cuts)).unwrap();
            for _ in 0..2 {
                let again = solve_mip(model.problem(), &model.integer, options(cuts)).unwrap();
                assert_eq!(first.nodes, again.nodes, "{cuts:?}: node count moved");
                assert_eq!(
                    first.cuts_kept, again.cuts_kept,
                    "{cuts:?}: cut count moved"
                );
                assert_eq!(first.col_value, again.col_value, "{cuts:?}");
            }
        }
    }
}

#[test]
fn a_node_budget_still_stops_the_search_with_cuts_on() {
    // The anytime property survives cutting. A cut round is a relaxation solve
    // and is counted as a node, so a starved search spends its budget on
    // cutting before it spends it on the tree — and it still has to say it
    // proved nothing, and whatever it found still has to be achievable.
    let model = knapsack(30, 5, 0);
    let full = solve_mip(model.problem(), &model.integer, options(Cuts::Off)).unwrap();
    for cuts in SETTINGS {
        let starved = solve_mip(
            model.problem(),
            &model.integer,
            MipOptions {
                max_nodes: 4,
                cuts,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!starved.proved, "{cuts:?}");
        if starved.status == Status::Optimal {
            assert!(
                starved.objective >= full.objective - 1e-6,
                "{cuts:?}: a partial search beat the proved optimum"
            );
        }
    }
}

#[test]
fn the_default_is_the_setting_that_measured_better() {
    // Pinning the decision, so that changing the default is a deliberate act
    // with a measurement behind it rather than an edit nothing notices. The
    // measurement is in `MipOptions::cuts`: three to twenty-one times faster on
    // the commitment ladder, and about half again slower on a knapsack, and the
    // models this engine builds are commitment models.
    assert_eq!(MipOptions::default().cuts, Cuts::Gomory);
}

#[test]
fn a_proved_answer_has_actually_closed_its_gap_with_cuts_on() {
    // The regression that `tests/branching.rs` pins for the uncut search, run
    // again over the configuration that is now the default. Cutting adds a
    // sixth way for the root to be disposed of — the cuts can make it integral
    // outright — and that exit reports `proved` like any other, so it has to
    // have earned it.
    for units in 2..6usize {
        for periods in 2..6usize {
            for variant in VARIANTS {
                let c = commitment(units, periods, variant);
                let o = MipOptions {
                    max_nodes: 200_000,
                    ..Default::default()
                };
                let r = solve_mip(c.problem(), &c.integer, o).unwrap();
                let what = format!("{units}x{periods} v{variant}");
                assert_eq!(r.status, Status::Optimal, "{what}");
                assert!(r.proved, "{what}: a rung this size should finish");
                assert!(
                    r.gap <= o.gap_tolerance,
                    "{what}: proved with an open gap of {}",
                    r.gap
                );
                assert!(
                    (r.lower_bound - r.objective).abs() <= 1e-6 * r.objective.abs().max(1.0),
                    "{what}: bound {} against answer {}",
                    r.lower_bound,
                    r.objective
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The measurement
// ---------------------------------------------------------------------------

/// Best of this many runs per cell.
///
/// Wall-clock on this machine moves a great deal with whatever else is running,
/// and an average carries that noise into the number while a minimum does not:
/// background load can only make a run slower, so the fastest of several is the
/// closest estimate available of what the work actually costs.
const REPEATS: usize = 5;

/// One cell of the table.
struct Cell {
    nodes: usize,
    seconds: f64,
    generated: usize,
    survived: usize,
    kept: usize,
    root_bound: f64,
}

fn measure(model: &Model, cuts: Cuts) -> Cell {
    // One untimed run first. Whichever setting is measured first otherwise pays
    // for faulting in the pages this model touches, which on the small rungs is
    // larger than the difference being measured.
    let _ = solve_mip(model.problem(), &model.integer, options(cuts)).unwrap();
    let mut best = f64::INFINITY;
    let mut last = None;
    for _ in 0..REPEATS {
        let t0 = Instant::now();
        let r = solve_mip(model.problem(), &model.integer, options(cuts)).unwrap();
        best = best.min(t0.elapsed().as_secs_f64());
        assert_eq!(r.status, Status::Optimal);
        assert!(r.proved);
        last = Some(r);
    }
    let r = last.expect("at least one run");
    Cell {
        nodes: r.nodes,
        seconds: best,
        generated: r.cuts_generated,
        survived: r.cuts_survived,
        kept: r.cuts_kept,
        root_bound: r.root_bound,
    }
}

/// Print one family's table.
fn table(name: &str, models: Vec<(String, Model)>) {
    println!();
    println!("{name}");
    println!(
        "{:>14} {:>6} {:>6}  {:>8} {:>10} {:>8} {:>10} {:>8} {:>10} {:>8} {:>10}  {:>12} {:>7}",
        "rung",
        "cols",
        "rows",
        "off n",
        "off time",
        "gom n",
        "gom time",
        "cov n",
        "cov time",
        "both n",
        "both time",
        "cuts g/s/k",
        "closed",
    );
    for (label, model) in models {
        let cells: Vec<Cell> = SETTINGS.iter().map(|&c| measure(&model, c)).collect();
        let integer_optimum = solve_mip(model.problem(), &model.integer, options(Cuts::Off))
            .unwrap()
            .objective;
        // How much of the distance from the plain relaxation to the integer
        // optimum the cuts closed before the search started. That is what a cut
        // is for; the node count is what it bought.
        let plain = cells[0].root_bound;
        let both = cells[3].root_bound;
        let closed = if (integer_optimum - plain).abs() > 1e-9 {
            (both - plain) / (integer_optimum - plain) * 100.0
        } else {
            0.0
        };
        println!(
            "{:>14} {:>6} {:>6}  {:>8} {:>9.1}ms {:>8} {:>9.1}ms {:>8} {:>9.1}ms {:>8} \
             {:>9.1}ms  {:>4}/{:>3}/{:>3} {:>6.1}%",
            label,
            model.n_cols,
            model.n_rows,
            cells[0].nodes,
            cells[0].seconds * 1e3,
            cells[1].nodes,
            cells[1].seconds * 1e3,
            cells[2].nodes,
            cells[2].seconds * 1e3,
            cells[3].nodes,
            cells[3].seconds * 1e3,
            cells[3].generated,
            cells[3].survived,
            cells[3].kept,
            closed,
        );
    }
}

#[test]
#[ignore = "minutes, and its output is a table rather than an assertion"]
fn cuts_are_measured_against_no_cuts_on_both_families() {
    let mut commitments = Vec::new();
    for (units, periods) in COMMITMENT_LADDER {
        for variant in VARIANTS {
            commitments.push((
                format!("{units}×{periods} v{variant}"),
                commitment(units, periods, variant),
            ));
        }
    }
    table("unit commitment", commitments);

    let mut sacks = Vec::new();
    for (n, c) in KNAPSACK_LADDER {
        for variant in VARIANTS {
            sacks.push((format!("{n}×{c} v{variant}"), knapsack(n, c, variant)));
        }
    }
    table("multidimensional knapsack", sacks);

    println!();
    println!(
        "best of {REPEATS} runs, one untimed warm-up per cell; node counts include \
         the relaxation solves spent cutting; `cuts g/s/k` is candidates \
         attempted, survivors of the guards, and cuts actually added, \
         under Cuts::Both; `closed` is how much of the root gap Cuts::Both closed \
         before the search started"
    );
}

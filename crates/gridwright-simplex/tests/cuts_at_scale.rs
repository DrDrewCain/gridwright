//! Whether cuts matter at a size anybody would run.
//!
//! `cuts.rs` measures a ladder topping out at twelve units over twelve periods.
//! That is 144 binaries and 444 columns, and the honest objection to every
//! number in it is that a problem which takes six seconds without cuts is not a
//! problem anyone needed to make faster. A nineteen-fold speed-up on a toy is a
//! fact about the toy.
//!
//! A commitment model somebody actually runs is tens of thermal units over a
//! week or a year: fifty units over 168 hours is 8,400 binaries, sixty times
//! the largest rung in that file. So the question this asks is not "how much
//! faster" but **"where does the search stop being usable, and do cuts move
//! that boundary"**. Those are different questions and only the second one
//! decides whether the feature was worth building.
//!
//! Answering it needs the node budget to be the thing that runs out, so every
//! rung here is given a generous one and the interesting column is whether the
//! answer came back *proved*. An unproved incumbent is not a failure of the
//! solver, it is the honest report that the budget ran out first, and it is
//! exactly the outcome a user would hit.
//!
//! Run it explicitly, since the larger rungs take minutes:
//!
//! ```text
//! cargo test -p gridwright-simplex --test cuts_at_scale --release -- --ignored --nocapture
//! ```

use gridwright_simplex::{Branching, Cuts, MipOptions, Problem, Status, solve_mip};
use std::time::Instant;

/// A unit commitment problem in compressed sparse column form.
///
/// Deliberately the same generator as `cuts.rs` and `branching.rs`, so that the
/// numbers here extend those ladders rather than describing a different family
/// that happens to share a name.
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

/// `units` thermal plants over `periods` snapshots, plus a gas unit per period
/// that will serve anything the committed plant does not.
///
/// Columns are dispatch, then status, then start-up, then the gas unit. Rows
/// are the capacity ceiling and stable minimum per unit per period, the demand
/// balance per period, and the start-up linkage.
///
/// The demand offset of 13 keeps demand off any sum of capacities, which is
/// what stops a rung's relaxation coming out integral and testing nothing.
fn commitment(units: usize, periods: usize, variant: usize) -> Commitment {
    commitment_with_reserve(units, periods, variant, None)
}

/// The same model with an optional reserve requirement: in every period the
/// committed capacity must cover demand by a stated margin.
///
/// This row is the reason the option exists. It sums `Pmax * status` over the
/// units against a right hand side, which is a 0-1 knapsack over the binaries,
/// and it is the shape a cover cut separates. A commitment model without it has
/// no knapsack row anywhere, which is why cover cuts found nothing on the
/// ladder in `cuts.rs` and why concluding from that that they are useless here
/// would have been wrong: real commitment models carry a reserve requirement,
/// and capacity expansion budgets are knapsacks outright.
fn commitment_with_reserve(
    units: usize,
    periods: usize,
    variant: usize,
    reserve_margin: Option<f64>,
) -> Commitment {
    let u_t = units * periods;
    let n_cols = 3 * u_t + periods;
    let reserve_rows = if reserve_margin.is_some() { periods } else { 0 };
    let n_rows = 2 * u_t + periods + u_t + reserve_rows;

    let p_max = |u: usize| 40.0 + 12.0 * ((u % 5) as f64);
    let p_min = |u: usize| 0.35 * p_max(u);
    let run_cost = |u: usize| 8.0 + 3.0 * ((u % 7) as f64);
    let start_cost = |u: usize| 220.0 + 40.0 * ((u % 4) as f64);

    let total: f64 = (0..units).map(p_max).sum();
    let demand = |t: usize| {
        let shape = match variant {
            0 => 0.55 + 0.30 * ((t as f64) * 0.7).sin(),
            1 => 0.50 + 0.35 * ((t as f64) * 0.3).cos(),
            _ => 0.60 + 0.25 * (((t * 7) % 11) as f64 / 11.0),
        };
        total * shape + 13.0
    };

    // Built by column, since that is the form the solver takes.
    let mut starts = vec![0u32];
    let mut rows: Vec<u32> = Vec::new();
    let mut vals: Vec<f64> = Vec::new();
    let mut lower = Vec::new();
    let mut upper = Vec::new();
    let mut cost = Vec::new();
    let mut integer = Vec::new();

    let cap_row = |u: usize, t: usize| (u * periods + t) as u32;
    let min_row = |u: usize, t: usize| (u_t + u * periods + t) as u32;
    let bal_row = |t: usize| (2 * u_t + t) as u32;
    let start_row = |u: usize, t: usize| (2 * u_t + periods + u * periods + t) as u32;
    let reserve_row = |t: usize| (3 * u_t + periods + t) as u32;

    // Dispatch.
    for u in 0..units {
        for t in 0..periods {
            rows.push(cap_row(u, t));
            vals.push(1.0);
            rows.push(min_row(u, t));
            vals.push(1.0);
            rows.push(bal_row(t));
            vals.push(1.0);
            starts.push(rows.len() as u32);
            lower.push(0.0);
            upper.push(p_max(u));
            cost.push(run_cost(u));
            integer.push(false);
        }
    }
    // Status.
    for u in 0..units {
        for t in 0..periods {
            rows.push(cap_row(u, t));
            vals.push(-p_max(u));
            rows.push(min_row(u, t));
            vals.push(-p_min(u));
            rows.push(start_row(u, t));
            vals.push(-1.0);
            if t + 1 < periods {
                rows.push(start_row(u, t + 1));
                vals.push(1.0);
            }
            if reserve_margin.is_some() {
                rows.push(reserve_row(t));
                vals.push(p_max(u));
            }
            starts.push(rows.len() as u32);
            lower.push(0.0);
            upper.push(1.0);
            cost.push(0.0);
            integer.push(true);
        }
    }
    // Start-up.
    for u in 0..units {
        for t in 0..periods {
            rows.push(start_row(u, t));
            vals.push(1.0);
            starts.push(rows.len() as u32);
            lower.push(0.0);
            upper.push(1.0);
            cost.push(start_cost(u));
            integer.push(false);
        }
    }
    // The gas unit, which makes every rung feasible whatever the binaries do.
    for t in 0..periods {
        rows.push(bal_row(t));
        vals.push(1.0);
        starts.push(rows.len() as u32);
        lower.push(0.0);
        upper.push(f64::INFINITY);
        cost.push(180.0);
        integer.push(false);
    }

    let mut row_lower = vec![f64::NEG_INFINITY; n_rows];
    let mut row_upper = vec![f64::INFINITY; n_rows];
    for u in 0..units {
        for t in 0..periods {
            // dispatch - Pmax * status <= 0
            row_upper[cap_row(u, t) as usize] = 0.0;
            // dispatch - Pmin * status >= 0
            row_lower[min_row(u, t) as usize] = 0.0;
            // start >= status[t] - status[t-1]
            row_lower[start_row(u, t) as usize] = 0.0;
        }
    }
    for t in 0..periods {
        row_lower[bal_row(t) as usize] = demand(t);
        row_upper[bal_row(t) as usize] = demand(t);
        if let Some(margin) = reserve_margin {
            row_lower[reserve_row(t) as usize] = demand(t) * (1.0 + margin);
        }
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

fn options(cuts: Cuts, max_nodes: usize) -> MipOptions {
    MipOptions {
        cuts,
        branching: Branching::PseudoCost,
        max_nodes,
        ..Default::default()
    }
}

/// Sizes going from the largest rung `cuts.rs` measures up towards something a
/// user would recognise. Fifty units over a week is the realistic end of this;
/// whether it is reachable at all is the question.
const SCALE: [(usize, usize); 6] = [
    (12, 12),  // where cuts.rs stops: 144 binaries
    (16, 24),  // 384
    (24, 24),  // 576
    (20, 48),  // 960
    (30, 48),  // 1,440
    (40, 96),  // 3,840
];

#[test]
#[ignore = "minutes; run explicitly for numbers"]
fn where_the_search_stops_being_usable_and_whether_cuts_move_it() {
    // A budget large enough that reaching it means something, rather than the
    // default 5,000 which the twelve-by-twelve rung already brushes.
    const BUDGET: usize = 200_000;

    println!(
        "\n  units x periods   binaries    cols    rows  |  {:>22}  |  {:>22}",
        "cuts off", "gomory"
    );
    println!(
        "  {:->17} {:->10} {:->7} {:->7}  |  {:->22}  |  {:->22}",
        "", "", "", "", "", ""
    );

    for (units, periods) in SCALE {
        let c = commitment(units, periods, 0);
        let binaries = units * periods;
        print!(
            "  {units:3} x {periods:-3}        {binaries:8}  {:6}  {:6}  |",
            c.n_cols, c.n_rows
        );

        for cuts in [Cuts::Off, Cuts::Gomory] {
            let t = Instant::now();
            let r = solve_mip(c.problem(), &c.integer, options(cuts, BUDGET));
            let dt = t.elapsed();
            match r {
                Ok(s) if s.status == Status::Optimal => {
                    print!(
                        "  {:>7} nodes {:>7.1?} {:>5}  |",
                        s.nodes,
                        dt,
                        if s.proved { "" } else { "OPEN" }
                    );
                }
                Ok(s) => print!("  {:>13?} {:>7.1?}  |", s.status, dt),
                Err(e) => print!("  {e:>22}  |"),
            }
        }
        println!();
    }
    println!(
        "\n  OPEN means the node budget of {BUDGET} ran out and the answer is an \
         incumbent rather than a proved optimum."
    );
}

#[test]
fn the_scale_generator_produces_genuinely_fractional_relaxations() {
    // The trap this family is prone to, asserted rather than assumed. A
    // commitment relaxation that comes out integral exercises neither the
    // branching nor the cuts, and a ladder built on those would measure
    // nothing while looking like it measured something.
    for (units, periods) in [(4, 4), (6, 8), (8, 6)] {
        let c = commitment(units, periods, 0);
        let relaxed = gridwright_simplex::solve(c.problem(), Default::default()).unwrap();
        assert_eq!(relaxed.status, Status::Optimal, "{units}x{periods}");
        let fractional = c
            .integer
            .iter()
            .enumerate()
            .filter(|&(_, &is_int)| is_int)
            .filter(|(j, _)| {
                let v = relaxed.col_value[*j];
                (v - v.round()).abs() > 1e-6
            })
            .count();
        assert!(
            fractional > 0,
            "{units}x{periods}: the relaxation is already integral, so this rung \
             exercises neither branching nor cuts"
        );
    }
}

#[test]
fn cuts_do_not_change_the_answer_at_the_sizes_that_finish_quickly() {
    // The safety property, checked at every size small enough to prove both
    // ways. Cuts may only make the search shorter, never let it reach a
    // different optimum, and a cut that removed the true optimum would produce
    // an answer that still looks like an answer.
    for (units, periods) in [(4, 4), (5, 6), (6, 6)] {
        let c = commitment(units, periods, 0);
        let off = solve_mip(c.problem(), &c.integer, options(Cuts::Off, 100_000)).unwrap();
        let on = solve_mip(c.problem(), &c.integer, options(Cuts::Gomory, 100_000)).unwrap();

        assert_eq!(off.status, Status::Optimal, "{units}x{periods}");
        assert_eq!(on.status, Status::Optimal, "{units}x{periods}");
        assert!(off.proved && on.proved, "{units}x{periods}: both should finish");
        assert!(
            (off.objective - on.objective).abs() <= 1e-6 * off.objective.abs().max(1.0),
            "{units}x{periods}: cuts changed the optimum from {} to {}",
            off.objective,
            on.objective
        );
    }
}

#[test]
fn cover_cuts_fire_on_commitment_once_it_carries_a_reserve_requirement() {
    // `cuts.rs` records that cover cuts find nothing on unit commitment, and
    // attributes it to there being no knapsack row: every row in the model it
    // measures pairs a continuous dispatch variable with a binary status, which
    // is not a knapsack. The explanation is right. The conclusion that cover
    // cuts are therefore useless for commitment is not.
    //
    // A commitment model somebody runs carries knapsack rows. A reserve
    // requirement sums committed capacity over the units against a right hand
    // side, and a capacity expansion budget is a 0-1 knapsack outright. This
    // engine builds both, in `build_reserve` and `build_budget`, so the model
    // that found nothing was missing the row the separator exists for. The
    // literature says the same from the other direction: a published commitment
    // instance applied fifty-five cover cuts against ten Gomory.
    //
    // Add the reserve row and they fire, at every margin and size tried.
    let cover = |c: &Commitment| {
        solve_mip(c.problem(), &c.integer, options(Cuts::Cover, 20_000)).unwrap()
    };

    let plain = commitment(8, 8, 0);
    assert_eq!(
        cover(&plain).cuts_generated,
        0,
        "the minimal model has no knapsack row at all, which is the whole of why \
         cover cuts found nothing on it"
    );

    // Whether a cover is actually *violated* depends on the answer the
    // relaxation happens to give, so this is a sweep rather than a single case
    // and the assertion is the honest weak one: on a family that carries the
    // row, some of it separates. Printing the pattern matters more than the
    // assertion does.
    let mut fired = 0;
    let mut cells = 0;
    for margin in [0.05, 0.15, 0.30, 0.50, 0.80] {
        for &(u, t) in &[(6usize, 6usize), (8, 8), (10, 10)] {
            let c = commitment_with_reserve(u, t, 0, Some(margin));
            let r = cover(&c);
            cells += 1;
            if r.cuts_generated > 0 {
                fired += 1;
            }
            println!(
                "  reserve margin {margin:>4}  {u:2}x{t:-2}  generated {:3}  kept {:3}",
                r.cuts_generated, r.cuts_kept
            );
        }
    }
    assert!(
        fired > 0,
        "a reserve requirement is a knapsack over the binaries and nothing \
         separated one at any of the {cells} settings tried"
    );
    println!("  cover cuts separated something in {fired} of {cells} settings");

    // The control, and it earned its place. An earlier version of this test
    // gave the solver a node budget of one, on the reasoning that whether a cut
    // is separated is decided at the root before any node is explored. Cut
    // rounds are charged against `max_nodes`, so a budget of one suppresses cut
    // generation outright, and every cell of the sweep came back zero. Read
    // alone that is a clean negative result with a plausible mechanism behind
    // it, and it was wrong. The control failed at the same time, which is the
    // only reason it was caught.
    let knap = pure_knapsack();
    let r = solve_mip(knap.problem(), &knap.integer, options(Cuts::Cover, 20_000)).unwrap();
    assert!(
        r.cuts_generated > 0,
        "the separator found nothing on a plain knapsack either, so the counts \
         above say nothing about commitment"
    );

    // And the cuts have not changed the answer, on a rung small enough to prove
    // both ways. A reserve requirement makes commitment markedly harder, so
    // this needs a far larger budget than the same size without one, which is
    // itself worth knowing: the row that gives cover cuts something to separate
    // is also the row that makes the search need them.
    let small = commitment_with_reserve(4, 4, 0, Some(0.05));
    let exact = solve_mip(small.problem(), &small.integer, options(Cuts::Off, 500_000)).unwrap();
    let cut = solve_mip(small.problem(), &small.integer, options(Cuts::Cover, 500_000)).unwrap();
    assert!(
        exact.proved && cut.proved,
        "neither finished within the budget: off proved {}, cover proved {}",
        exact.proved,
        cut.proved
    );
    assert!(
        (exact.objective - cut.objective).abs() <= 1e-6 * exact.objective.abs().max(1.0),
        "cover cuts changed the optimum from {} to {}",
        exact.objective,
        cut.objective
    );
}

/// A plain 0-1 knapsack with a fractional relaxation, as the control for the
/// test above: it establishes that the separator works, so that finding nothing
/// on commitment is a statement about commitment.
fn pure_knapsack() -> Commitment {
    let n = 14;
    let weight = |j: usize| 7.0 + ((j * 13) % 11) as f64;
    let value = |j: usize| 5.0 + ((j * 7) % 9) as f64;
    let capacity: f64 = (0..n).map(weight).sum::<f64>() * 0.45;

    let mut starts = vec![0u32];
    let (mut rows, mut vals) = (Vec::new(), Vec::new());
    let (mut lower, mut upper, mut cost, mut integer) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for j in 0..n {
        rows.push(0);
        vals.push(weight(j));
        starts.push(rows.len() as u32);
        lower.push(0.0);
        upper.push(1.0);
        cost.push(-value(j));
        integer.push(true);
    }
    Commitment {
        n_cols: n,
        n_rows: 1,
        starts,
        rows,
        vals,
        lower,
        upper,
        cost,
        row_lower: vec![f64::NEG_INFINITY],
        row_upper: vec![capacity],
        integer,
    }
}

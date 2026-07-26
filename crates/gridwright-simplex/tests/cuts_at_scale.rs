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

/// Stable minimum as a share of rating, shared by the generator and by the
/// reserve clamp that has to respect it.
const P_MIN_SHARE: f64 = 0.35;

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
/// committed capacity must reach a stated fraction of everything installed.
///
/// Stated against installed capacity rather than as a margin over demand, and
/// that is not cosmetic. A margin over demand is only feasible while
/// `(1 + margin) * peak <= total`, and peak demand here is 0.85 of total, so
/// margins above about 0.10 ask for more capacity than exists and the model is
/// infeasible. An earlier sweep did exactly that and reported that cover cuts
/// fired in 4 of 15 settings; 11 of those 15 were infeasible, and the true
/// figure was 4 of 4. A fraction of installed capacity cannot be infeasible,
/// so the sweep measures what it says it measures.
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
    reserve_fraction: Option<f64>,
) -> Commitment {
    let u_t = units * periods;
    let n_cols = 3 * u_t + periods;
    let reserve_rows = if reserve_fraction.is_some() { periods } else { 0 };
    let n_rows = 2 * u_t + periods + u_t + reserve_rows;

    let p_max = |u: usize| 40.0 + 12.0 * ((u % 5) as f64);
    let p_min = |u: usize| P_MIN_SHARE * p_max(u);
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

    // The lightest period, which is what limits how much plant may be committed
    // at once: everything committed must run at its stable minimum.
    let trough = (0..periods).map(demand).fold(f64::INFINITY, f64::min);

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
            if reserve_fraction.is_some() {
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
        if let Some(fraction) = reserve_fraction {
            // At least this share of installed capacity committed, clamped to
            // what can actually be committed.
            //
            // Two ceilings, and the second is the one that is easy to miss.
            // Committed capacity obviously cannot exceed what is installed. But
            // every committed unit must also generate at least its stable
            // minimum, so committing capacity C forces at least 0.35*C onto the
            // system, and in the trough period there may be nowhere for it to
            // go. Ask for 85% of capacity committed and the model is infeasible
            // at the trough, which looks exactly like a hard instance and is
            // not one.
            let headroom = trough / P_MIN_SHARE;
            row_lower[reserve_row(t) as usize] =
                (total * fraction.clamp(0.0, 1.0)).min(headroom).min(total);
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
/// user would recognise.
///
/// A 40 x 96 rung, 3,840 binaries, was here and has been removed on arithmetic
/// rather than on taste. [`MipOptions`] carries measured node costs for this
/// exact model family, 0.33 ms at 114 rows rising to 3.92 ms at 444, which fit
/// `rows^1.8`. A rung of `u` units over `t` periods has `3*u*t + t` rows, so
/// 40 x 96 is 11,616 rows and one node there costs about 1.5 s. Against any
/// budget worth setting that rung alone is hours: it was left running for three
/// of them, never finished, and projected to well over a day.
///
/// It also had nothing to add. The question is where the search stops being
/// usable, and 30 x 48 already answers it by exhausting its budget.
const SCALE: [(usize, usize); 5] = [
    (12, 12),  // where cuts.rs stops: 144 binaries
    (16, 24),  // 384
    (24, 24),  // 576
    (20, 48),  // 960
    (30, 48),  // 1,440
];

#[test]
#[ignore = "about an hour; run explicitly for numbers"]
fn where_the_search_stops_being_usable_and_whether_cuts_move_it() {
    // Every cell of this ladder is an independent search, so they are run
    // concurrently: one branch and bound tree cannot use a second core, but ten
    // of them can use ten.
    //
    // It changes what the timing column means and not what the node column
    // means. Node counts and whether a rung proved are deterministic and do not
    // care what else is running; the times are taken under contention and are
    // upper bounds. The findings this test exists for are in the columns that
    // are safe.
    //
    // Concurrency buys much less here than the cell count suggests, which is
    // worth stating so nobody budgets on it. The cells differ in cost by orders
    // of magnitude, so the cheap ones retire in the first seconds and from then
    // on the wall clock is simply whatever the single slowest cell takes. Ten
    // cores do not make that cell faster; they only stop it waiting behind the
    // others. Expect the machine to sit mostly idle for the second half of a
    // run.
    //
    // On the budget, which is the number that decides whether this finishes at
    // all. A node is one LP solve, and this solver has no warm start, so a node
    // costs a *whole cold solve*: build the tableau, crash a basis, run phase
    // one, run phase two. `MipOptions` carries measured costs on this model
    // family, 0.33 ms at 114 rows rising to 3.92 ms at 444, which fit
    // `rows^1.8`. A rung of `u` units over `t` periods has `3*u*t + t` rows:
    //
    //   | rung    | rows  | predicted ms/node | measured | ratio |
    //   | ---     | ---   | ---               | ---      | ---   |
    //   | 12 x 12 | 444   | 3.9               | 4.97     | 1.27x |
    //   | 16 x 24 | 1,176 | 23                | 32.4     | 1.41x |
    //   | 20 x 48 | 2,928 | 120               | 178      | 1.48x |
    //   | 30 x 48 | 4,368 | 248               | 412      | 1.66x |
    //
    // The measured column is from the run this comment was written against, and
    // the prediction held: same shape, and a ratio that is not noise but the
    // cost of running ten cells at once, rising with model size because larger
    // cells contend harder for memory bandwidth. Budget on the measured column.
    // The whole ladder took 55 minutes, of which the last cell alone was 21
    // with the machine otherwise idle.
    //
    // An earlier version set this to 20,000 on the estimate that a node at
    // 1,440 binaries "costs tens of milliseconds". It costs about 250, so the
    // estimate was low by an order of magnitude, and the ladder was stopped
    // after three hours with cells still running and a projected finish of well
    // over a day. The solver was behaving exactly as designed; the cost model
    // in the comment was the fault. That is why the arithmetic is written out
    // above rather than asserted.
    //
    // 5,000 answers the same question, because the question is answered by
    // *whether a rung reaches the budget* rather than by how long it took, and
    // it still leaves headroom over the largest proving search `cuts.rs`
    // records, which is 1,644 nodes.
    const BUDGET: usize = 5_000;

    let cells: Vec<(usize, usize, Cuts)> = SCALE
        .iter()
        .flat_map(|&(u, t)| [Cuts::Off, Cuts::Gomory].map(move |c| (u, t, c)))
        .collect();

    // Each cell reports as it lands, and the assembled table comes afterwards.
    //
    // The previous version collected every cell and printed once at the end,
    // so a run interrupted at any point returned nothing whatsoever: three
    // hours of solving produced not a single number. `real_scale.rs` already
    // states the rule this restores, that a ladder which runs for a long time
    // should publish the rungs it has reached. Going concurrent is what lost
    // it, because a thread that returns its result on join cannot report
    // before every other thread has also finished.
    println!("\n  {} cells, each reported as it finishes:", cells.len());

    let (tx, rx) = std::sync::mpsc::channel();
    let mut results = Vec::with_capacity(cells.len());
    std::thread::scope(|scope| {
        for &(units, periods, cuts) in &cells {
            let tx = tx.clone();
            scope.spawn(move || {
                let c = commitment(units, periods, 0);
                let cols = c.n_cols;
                let rows = c.n_rows;
                let t = Instant::now();
                let r = solve_mip(c.problem(), &c.integer, options(cuts, BUDGET));
                let _ = tx.send((units, periods, cuts, cols, rows, r, t.elapsed()));
            });
        }
        // The original sender has to go, or the loop below waits on a sender
        // that will never send and the test hangs after the last cell.
        drop(tx);

        for cell in rx {
            let tag = match cell.2 {
                Cuts::Off => "off",
                Cuts::Gomory => "gomory",
                _ => "other",
            };
            match &cell.5 {
                Ok(s) if s.status == Status::Optimal => println!(
                    "    {:3} x {:3}  {:>6}  {:>7} nodes  {:>9.1?}  {}",
                    cell.0,
                    cell.1,
                    tag,
                    s.nodes,
                    cell.6,
                    if s.proved { "proved" } else { "OPEN" }
                ),
                Ok(s) => println!(
                    "    {:3} x {:3}  {:>6}  {:>13?}  {:>9.1?}",
                    cell.0, cell.1, tag, s.status, cell.6
                ),
                Err(e) => println!("    {:3} x {:3}  {:>6}  {e}", cell.0, cell.1, tag),
            }
            results.push(cell);
        }
    });

    println!(
        "\n  units x periods   binaries    cols    rows  |  {:>22}  |  {:>22}",
        "cuts off", "gomory"
    );
    println!(
        "  {:->17} {:->10} {:->7} {:->7}  |  {:->22}  |  {:->22}",
        "", "", "", "", "", ""
    );

    for &(units, periods) in &SCALE {
        let binaries = units * periods;
        let mine: Vec<_> = results
            .iter()
            .filter(|r| r.0 == units && r.1 == periods)
            .collect();
        let (_, _, _, cols, rows, _, _) = mine[0];
        print!("  {units:3} x {periods:-3}        {binaries:8}  {cols:6}  {rows:6}  |");
        for cuts in [Cuts::Off, Cuts::Gomory] {
            let cell = mine.iter().find(|r| r.2 == cuts).unwrap();
            match &cell.5 {
                Ok(s) if s.status == Status::Optimal => print!(
                    "  {:>7} nodes {:>7.1?} {:>5}  |",
                    s.nodes,
                    cell.6,
                    if s.proved { "" } else { "OPEN" }
                ),
                Ok(s) => print!("  {:>13?} {:>7.1?}  |", s.status, cell.6),
                Err(e) => print!("  {e:>22}  |"),
            }
        }
        println!();
    }

    println!(
        "\n  OPEN means the node budget of {BUDGET} ran out and the answer is an \
         incumbent rather than a proved optimum. Times are taken with every \
         cell running at once and are upper bounds; node counts are not affected."
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
    // A modest budget. Cuts are separated at the root, so what has to be true
    // for a cut count to mean anything is that the root solved, not that the
    // whole search finished. A tight reserve makes these genuinely hard and
    // several rungs below stop on the node limit with a good incumbent, which
    // is a fact about the models rather than a fault.
    let cover = |c: &Commitment| {
        solve_mip(c.problem(), &c.integer, options(Cuts::Cover, 1_500)).unwrap()
    };

    let plain = commitment(8, 8, 0);
    assert_eq!(
        cover(&plain).cuts_generated,
        0,
        "the minimal model has no knapsack row at all, which is the whole of why \
         cover cuts found nothing on it"
    );

    // Every cell here is feasible by construction, and asserted to have solved,
    // because the first version of this sweep was mostly measuring
    // infeasibility. Eleven of its fifteen settings asked for more committed
    // capacity than the system had installed, the root relaxation never solved,
    // and zero cuts on an infeasible model was read as a fact about cuts.
    let mut fired = 0;
    let mut cells = 0;
    // A small sweep, because this runs on every `cargo test`. The full one,
    // five fractions by three sizes, separates cuts in 12 of its 15 settings
    // and takes half a minute; the three it misses are the tightest reserves,
    // where the clamp above has pinned the requirement and left nothing
    // fractional to cut.
    for fraction in [0.55, 0.75] {
        for &(u, t) in &[(6usize, 6usize), (7, 6)] {
            let c = commitment_with_reserve(u, t, 0, Some(fraction));
            let r = cover(&c);
            // The root having solved is what makes a cut count meaningful, and
            // a finite bound is how that shows. Requiring the *search* to
            // finish would throw away the hardest and most interesting rungs;
            // requiring nothing at all is how the first version of this sweep
            // came to report cut counts for infeasible models.
            assert!(
                r.lower_bound.is_finite(),
                "fraction {fraction} at {u}x{t} came back {:?} with no root bound, \
                 so its cut count says nothing",
                r.status
            );
            cells += 1;
            if r.cuts_generated > 0 {
                fired += 1;
            }
            println!(
                "  committed at least {fraction:>4} of installed  {u:2}x{t:-2}  \
                 generated {:3}  kept {:3}  {}",
                r.cuts_generated,
                r.cuts_kept,
                if r.proved { "proved" } else { "node limit" }
            );
        }
    }
    println!("  cover cuts separated something in {fired} of {cells} feasible settings");
    assert!(
        fired > 0,
        "a reserve requirement is a knapsack over the binaries and nothing \
         separated one at any of the {cells} feasible settings tried"
    );

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
    let small = commitment_with_reserve(4, 4, 0, Some(0.65));
    let exact = solve_mip(small.problem(), &small.integer, options(Cuts::Off, 100_000)).unwrap();
    let cut = solve_mip(small.problem(), &small.integer, options(Cuts::Cover, 100_000)).unwrap();
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


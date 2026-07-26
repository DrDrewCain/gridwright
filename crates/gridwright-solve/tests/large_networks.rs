//! The same validation as `real_networks.rs`, on the largest real networks
//! PGLib publishes.
//!
//! `real_networks.rs` stops at PEGASE 1354. That case established that the
//! formulation survives a real European topology, and this one asks the next
//! question: whether it survives ten times as much of one. The cases here are
//! PEGASE 2869, 9241 and 13659, models of the European transmission system at
//! increasing coverage, and RTE 6470, a model of the French one. All come from
//! PGLib-OPF v23.07 under CC-BY 4.0, unmodified, in the MATPOWER format the
//! reader already takes.
//!
//! The structure is deliberately inverted relative to `real_networks.rs`, and
//! the reason is arithmetic. There, one property per test over every case is
//! right, because a re-solve of the largest case costs 50 ms and a failure
//! names itself. Here a re-solve of PEGASE 13659 costs seven seconds, so seven
//! property-per-test loops would spend a minute proving the same solution over
//! and over. Each network is therefore solved once and every property checked
//! against that one solution, with each assertion carrying a message specific
//! enough that a failure still names itself.
//!
//! What is asserted is what must be true of any correct solution of any
//! network: the case loads and validates, the program solves to optimality,
//! power balances at every individual bus rather than merely in total, every
//! flow obeys its rating and every unit its limits, the DC flow equation holds
//! on every branch against the solved angles, and each synchronous area has
//! exactly one pinned reference. Construction determinism and agreement with
//! HiGHS get their own tests below, because neither is a property of a single
//! solution.
//!
//! Two of the four cases hold all of that in the default suite, and the reasons
//! the other two do not are worth stating up front rather than discovering:
//!
//! - PEGASE 13659 holds every property, but its solve costs seven seconds, so
//!   it is behind `#[ignore]`.
//! - PEGASE 9241 holds every property **under our own simplex**, because HiGHS
//!   1.15.0 returns an error rather than a status on it. That is unexpected
//!   enough to have its own three tests below.
//!
//! Not asserted, here as there: agreement with published AC-OPF objectives.
//! This is a DC model and the costs are the linear term of a quadratic.

#![cfg(feature = "highs")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gridwright_build::build_lopf;
use gridwright_io::matpower::load_case;
use gridwright_solve::{HighsSolver, Solver, Status};

fn case_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/pglib")
        .join(format!("{name}.m"))
}

/// name, buses, branches, generators.
///
/// The counts are what the reader produces, not what the file's header claims:
/// out-of-service rows are skipped, so the two can differ and pinning the
/// former is what catches a reader that starts admitting them.
const PEGASE_2869: (&str, usize, usize, usize) = ("case2869_pegase", 2869, 4582, 510);
const RTE_6470: (&str, usize, usize, usize) = ("case6470_rte", 6470, 9005, 761);
const PEGASE_9241: (&str, usize, usize, usize) = ("case9241_pegase", 9241, 16049, 1445);
const PEGASE_13659: (&str, usize, usize, usize) = ("case13659_pegase", 13659, 20467, 4092);
// Fewer branches and generators than the file lists, and both differences are
// the reader correctly leaving out equipment the file marks out of service:
// 3,633 branches of 3,639, six carrying status 0, and 238 generators of 384,
// with 146 out. Texas carries far more idle plant than the European cases,
// which is a property of the model rather than of the reader. A network built
// as designed rather than as operated is a different and usually more capable
// one, so the counts are pinned here to keep that distinction checkable.
const GOC_2000: (&str, usize, usize, usize) = ("case2000_goc", 2000, 3633, 238);

/// What one pass over a network cost, so the scaling table below is produced by
/// the same code that checks the properties rather than by a separate run that
/// might not be doing the same work.
struct Measured {
    buses: usize,
    rows: usize,
    cols: usize,
    nnz: usize,
    build: Duration,
    solve: Duration,
}

/// Everything that must be true of a solved network, whoever solved it.
///
/// Taken as a function of the solution rather than written inline, because one
/// of these networks is solved by the from-scratch simplex alone and it would
/// be a poor kind of validation if the case nobody else can solve were also the
/// case held to a weaker standard.
///
/// Panics with a message naming the network and the specific failure, so this
/// being one test per network rather than one test per property costs nothing
/// in diagnosis.
fn the_solution_is_physical(
    name: &str,
    net: &gridwright_net::Network,
    lopf: &gridwright_build::Lopf,
    sol: &gridwright_solve::Solution,
) {
    assert_eq!(sol.status, Status::Optimal, "{name} did not reach an optimum");
    assert!(sol.objective.is_finite(), "{name} objective is not finite");

    // Balance at every individual bus, which is strictly stronger than the
    // aggregate check `real_networks.rs` makes. A formulation can generate
    // exactly as much as it consumes in total while injecting power at a bus
    // where nothing is connected, and the aggregate check cannot see it. At
    // this size that is not a hypothetical: 13,659 balance rows is 13,659
    // chances for an indexing mistake that a total would average away.
    let mut injection = vec![0.0f64; net.buses.len()];
    for (b, bus) in net.buses.iter().enumerate() {
        // Shunt conductance draws real power, and unserved energy is a supply
        // of last resort, so both belong in the balance rather than beside it.
        injection[b] += sol.shed(&lopf.vars, b)[0] - bus.g_shunt * net.base_mva;
    }
    for (g, unit) in net.generators.iter().enumerate() {
        injection[unit.bus] += sol.dispatch(&lopf.vars, g)[0];
    }
    for load in &net.loads {
        injection[load.bus] -= load.p_set;
    }
    for (l, line) in net.lines.iter().enumerate() {
        // Flow is signed from `bus0` toward `bus1`, so it leaves the first bus
        // and arrives at the second. Getting this pair of signs the same way
        // round would balance every bus perfectly and mean nothing.
        let f = sol.flow(&lopf.vars, l)[0];
        injection[line.bus0] -= f;
        injection[line.bus1] += f;
    }
    for (b, residual) in injection.iter().enumerate() {
        assert!(
            residual.abs() < 1e-4,
            "{name}: bus {b} is out of balance by {residual:.6} MW"
        );
    }

    // And nothing was shed, which says the network is genuinely able to serve
    // its demand. Without this the balance check above would pass on a solution
    // that gave up everywhere at once.
    let shed = sol.total_shed(&lopf.vars);
    assert!(shed < 1e-4, "{name}: {shed:.4} MW unserved on a feasible case");

    for (l, line) in net.lines.iter().enumerate() {
        let f = sol.flow(&lopf.vars, l)[0];
        assert!(
            f.abs() <= line.s_nom + 1e-4,
            "{name}: branch {l} carries {f:.4} against a rating of {:.4}",
            line.s_nom
        );
    }

    for (g, unit) in net.generators.iter().enumerate() {
        let p = sol.dispatch(&lopf.vars, g)[0];
        assert!(
            p >= -1e-6 && p <= unit.p_nom + 1e-4,
            "{name}: generator {g} produced {p:.4}, limit {:.4}",
            unit.p_nom
        );
        let floor = unit.p_nom * unit.p_min_pu;
        assert!(
            p >= floor - 1e-4,
            "{name}: generator {g} produced {p:.4}, below its {floor:.4} minimum"
        );
    }

    // The defining equation, against the solved angles rather than assumed.
    // These networks carry reactances spanning five orders of magnitude, which
    // is the regime where an ill-conditioned formulation stops agreeing with
    // itself, so this is the check most likely to fail here and nowhere else.
    let mut checked = 0;
    for (l, line) in net.lines.iter().enumerate() {
        if line.is_transport() {
            continue;
        }
        let f = sol.flow(&lopf.vars, l)[0];
        let a0 = sol.trajectory(lopf.vars.angle[line.bus0])[0];
        let a1 = sol.trajectory(lopf.vars.angle[line.bus1])[0];
        let expected = line.susceptance * (a0 - a1 - line.phase_shift);
        // The tolerance is relative to the susceptance, which it has to be: a
        // branch of susceptance 5,800 carrying an angle difference known to a
        // part in 10^12 still has an absolute flow error a thousand times
        // larger than one of susceptance 0.01. A fixed absolute tolerance here
        // would either pass everything or fail the stiff branches for being
        // stiff.
        let tol = 1e-6 * line.susceptance.abs().max(1.0);
        assert!(
            (f - expected).abs() < tol,
            "{name}: branch {l} carries {f:.6} but B*(dtheta - shift) = {expected:.6}"
        );
        checked += 1;
    }
    assert!(checked > 0, "{name}: no DC branches were checked");

    // One reference per synchronous area. All four of these networks are a
    // single synchronous grid, so the count is one, and asserting it against
    // `synchronous_areas()` rather than against the literal 1 keeps the check
    // honest if a multi-area case is ever added here.
    let cols = lopf.model.columns();
    let pinned = (0..net.buses.len())
        .filter(|&b| {
            let i = lopf.vars.angle[b].start() as usize;
            cols.lower[i] == 0.0 && cols.upper[i] == 0.0
        })
        .count();
    assert_eq!(
        pinned,
        net.synchronous_areas().len(),
        "{name}: {pinned} pinned angles for {} areas",
        net.synchronous_areas().len()
    );
}

/// Load a case, check its shape, and build it, timing the build.
///
/// Loading is where validation runs, so a case that violates an invariant of
/// the network model never reaches the builder. Reporting the error rather than
/// swallowing it matters more here than on a small case: these files are large
/// enough that "it failed" without a reason is not a starting point.
fn load_and_build(
    case: (&str, usize, usize, usize),
) -> (gridwright_io::Case, gridwright_build::Lopf, Duration) {
    let (name, buses, branches, gens) = case;
    let loaded = load_case(case_path(name)).unwrap_or_else(|e| panic!("{name} did not load: {e}"));
    assert_eq!(loaded.network.buses.len(), buses, "{name} bus count");
    assert_eq!(loaded.network.lines.len(), branches, "{name} branch count");
    assert_eq!(loaded.network.generators.len(), gens, "{name} generator count");

    let t = Instant::now();
    let lopf = build_lopf(&loaded.network).unwrap_or_else(|e| panic!("{name} did not build: {e}"));
    let build = t.elapsed();
    (loaded, lopf, build)
}

/// Load, build, solve once with HiGHS, and assert everything above.
fn every_property_holds(case: (&str, usize, usize, usize)) -> Measured {
    let name = case.0;
    let (loaded, lopf, build) = load_and_build(case);
    let net = &loaded.network;

    let t = Instant::now();
    let sol = HighsSolver::default()
        .solve(&lopf)
        .unwrap_or_else(|e| panic!("{name} did not solve: {e}"));
    let solve = t.elapsed();

    the_solution_is_physical(name, net, &lopf, &sol);

    Measured {
        buses: net.buses.len(),
        rows: lopf.model.num_rows(),
        cols: lopf.model.num_cols(),
        nnz: lopf.model.nnz(),
        build,
        solve,
    }
}

#[test]
fn every_property_holds_on_a_two_thousand_eight_hundred_bus_network() {
    every_property_holds(PEGASE_2869);
}

#[test]
fn every_property_holds_on_a_six_thousand_bus_network() {
    // RTE rather than PEGASE, so the suite is not validating one data vendor's
    // conventions six times over. The French model is built by the transmission
    // operator that runs the network, and it carries branch ratings on very
    // nearly everything, where the PEGASE cases leave many branches unrated.
    every_property_holds(RTE_6470);
}

#[test]
#[ignore = "seven seconds of solve; run with --ignored"]
fn every_property_holds_on_a_thirteen_thousand_bus_network() {
    // The largest case PGLib publishes short of the 24,464-bus GOC model, and
    // the only property check here that is ignored for its cost alone. The line
    // is drawn at the solve time rather than the bus count: 2,869 and 6,470
    // buses cost 0.25 s and 0.83 s, which a suite can absorb, and 13,659 costs
    // 7.2 s, which is most of the crate's remaining test time spent re-proving
    // what the two smaller ones establish. Run with
    // `cargo test -p gridwright-solve --test large_networks --release -- --ignored`.
    every_property_holds(PEGASE_13659);
}

#[test]
fn building_the_largest_networks_twice_gives_an_identical_matrix() {
    // Assembly is parallel, and the largest case is the one that uses the most
    // threads, so it is the one where a scheduling-dependent ordering would
    // show. This does not solve anything: determinism is a property of
    // construction, and a build at 13,659 buses costs seven milliseconds, so
    // there is no reason to leave the largest network out of the check that it
    // is most likely to fail.
    for (name, ..) in [PEGASE_2869, RTE_6470, PEGASE_13659] {
        let case = load_case(case_path(name)).unwrap();
        let a = build_lopf(&case.network).unwrap();
        let b = build_lopf(&case.network).unwrap();
        assert_eq!(a.model.matrix(), b.model.matrix(), "{name}: matrices differ");
        assert_eq!(
            a.model.row_bounds(),
            b.model.row_bounds(),
            "{name}: row bounds differ"
        );
    }
}

#[test]
fn a_nine_thousand_bus_network_loads_and_builds_even_though_highs_will_not_solve_it() {
    // PEGASE 9241 is the one case here that HiGHS 1.15.0 cannot solve — see the
    // pair of tests below for what actually happens and what does solve it. It
    // still belongs in the default suite for the parts that do work, because
    // "the LP backend fails on it" and "the reader or the builder fails on it"
    // are very different diagnoses and this is what separates them: the file
    // parses, validates, builds, and builds the same way twice.
    let (_, a, _) = load_and_build(PEGASE_9241);
    let (_, b, _) = load_and_build(PEGASE_9241);
    assert_eq!(a.model.matrix(), b.model.matrix(), "9241: matrices differ");
    assert_eq!(a.model.num_rows(), 25_274, "9241 row count");
    assert_eq!(a.model.num_cols(), 35_976, "9241 column count");
    // Every coefficient finite, which is the specific thing worth ruling out
    // when a solver returns an error rather than a status. A susceptance of
    // `1/x` with a zero reactance would put an infinity in the matrix and would
    // look exactly like this from the outside.
    assert!(
        a.model.matrix().vals.iter().all(|v| v.is_finite()),
        "9241: the constraint matrix contains a non-finite coefficient"
    );
    let (lower, upper) = a.model.row_bounds();
    assert!(
        lower.iter().chain(upper).all(|v| !v.is_nan()),
        "9241: a row bound is NaN"
    );
}

#[test]
#[ignore = "nine seconds spent proving a third-party solver still fails; run with --ignored"]
fn highs_still_declines_the_nine_thousand_bus_network() {
    // Pinned deliberately, and it is the least comfortable test in the file,
    // because what it asserts is that something does not work.
    //
    // HiGHS 1.15.0 presolves this model down to 12,450 rows without complaint,
    // starts the dual simplex, and after three to nine seconds returns
    // `kHighsStatusError` from `Highs_run` with the model status left Not Set.
    // It is deterministic, unaffected by the thread count, and specific to this
    // case: PEGASE 13659, which is a larger model of the same system from the
    // same publisher with the same susceptance range, solves in seven seconds.
    // Everything checkable on our side is clean — no infinite or NaN
    // coefficient, no NaN bound — and the test above pins that separately.
    //
    // The reason to assert it rather than to leave a note is that this is a
    // capability the project reports to its callers. If a HiGHS upgrade fixes
    // it, this test failing is how anyone finds out, and at that point it
    // should be deleted and `every_property_holds(PEGASE_9241)` written in its
    // place.
    let (_, lopf, _) = load_and_build(PEGASE_9241);
    let outcome = HighsSolver::default().solve(&lopf);
    assert!(
        outcome.is_err(),
        "HiGHS now solves PEGASE 9241: {:?}. Replace this test with a call to \
         every_property_holds(PEGASE_9241).",
        outcome.map(|s| (s.status, s.objective))
    );
}

#[test]
#[ignore = "two minutes of pure-Rust simplex; run with --ignored"]
#[cfg(feature = "simplex")]
fn the_from_scratch_simplex_solves_the_network_highs_declines() {
    // The best argument this project has for having written its own solver, and
    // it arrived by accident: PEGASE 9241 was added as another rung on a
    // scaling ladder and turned out to be a case where the reference
    // implementation gives up and ours does not. 25,274 rows, optimal in one to
    // two minutes depending on what else the machine is doing.
    //
    // What is asserted is every physical property the other networks are held
    // to — balance at each of the 9,241 buses, ratings, unit limits, the DC
    // flow equation on all 16,049 branches, one angle reference. What is not
    // asserted, and cannot be, is agreement with an independent solver on the
    // objective, since the absence of one is the whole point. So optimality
    // here rests on our own dual feasibility test, which is a weaker claim than
    // anywhere else in this file, and saying so is better than implying the
    // check happened.
    use gridwright_solve::SimplexSolver;

    let (loaded, lopf, _) = load_and_build(PEGASE_9241);
    let t = Instant::now();
    let sol = SimplexSolver::default()
        .solve(&lopf)
        .unwrap_or_else(|e| panic!("PEGASE 9241 defeated our simplex too: {e}"));
    println!(
        "  case9241_pegase: {} rows, ours {:.1?}, HiGHS declines",
        lopf.model.num_rows(),
        t.elapsed()
    );
    the_solution_is_physical("case9241_pegase", &loaded.network, &lopf, &sol);
}

#[test]
fn a_case_whose_declared_areas_are_control_areas_still_loads() {
    // The ACTIVSg Texas model: one synchronous interconnection divided into
    // three *control* areas, joined by sixty-one ordinary AC branches.
    //
    // This test previously pinned the opposite. The reader mapped MATPOWER's
    // `area` column onto a synchronous area, so an AC branch between two of
    // them looked like an AC branch between two asynchronous grids, which the
    // network model refuses and should refuse. The refusal was correct given
    // the premise and the premise was wrong: a control area is a market or
    // operator zone and AC branches cross it freely. It rejected most of the
    // remaining PGLib library on the same grounds, including the Polish and
    // SDET cases.
    //
    // Synchronous areas are now derived from which branches carry susceptance,
    // which is the definition of the term rather than a reading of a column, so
    // the file cannot contradict them. Texas comes out as one area, correctly,
    // and every physical property is checkable on it.
    let m = every_property_holds(GOC_2000);
    let loaded = load_case(case_path("case2000_goc")).expect("Texas should load");
    assert_eq!(
        loaded.network.synchronous_areas().len(),
        1,
        "ERCOT is one synchronous grid however many control areas it is divided into"
    );
    // The column is not discarded, only demoted to what it actually is.
    let zones: std::collections::BTreeSet<&str> =
        loaded.network.buses.iter().map(|b| b.country.as_str()).collect();
    assert!(
        zones.len() > 1,
        "the three control areas should survive as zones, got {zones:?}"
    );
    println!("  case2000_goc: {} rows, {:.1?}", m.rows, m.solve);
}

/// Relative agreement, so the tolerance scales with the size of the number.
///
/// Deliberately the same shape as the helper in `differential.rs`, and
/// deliberately comparing objectives rather than variable values. A linear
/// program frequently has many optima that cost the same, and two solvers
/// landing on different vertices of one optimal face is correct behaviour
/// rather than a discrepancy. At 13,659 buses that is not a remote possibility:
/// thousands of these branches are unrated and thousands of generators sit at
/// identical costs, so the optimal face is enormous and the vertices really do
/// differ. The objective is unique even when the solution is not.
#[cfg(feature = "simplex")]
fn agree(a: f64, b: f64, tol: f64, what: &str) {
    let scale = a.abs().max(b.abs()).max(1.0);
    assert!(
        (a - b).abs() / scale < tol,
        "{what}: ours {a}, HiGHS {b}, relative difference {}",
        (a - b).abs() / scale
    );
}

#[cfg(feature = "simplex")]
fn both_solvers_agree_on(case: (&str, usize, usize, usize)) {
    use gridwright_solve::SimplexSolver;

    let name = case.0;
    let (_, lopf, _) = load_and_build(case);

    let t = Instant::now();
    let ours = SimplexSolver::default().solve(&lopf).unwrap();
    let mine = t.elapsed();
    let t = Instant::now();
    let theirs = HighsSolver::default().solve(&lopf).unwrap();
    let hi = t.elapsed();

    assert_eq!(ours.status, Status::Optimal, "{name}: ours did not solve");
    assert_eq!(theirs.status, Status::Optimal, "{name}: HiGHS did not solve");
    agree(ours.objective, theirs.objective, 1e-6, &format!("{name} objective"));
    println!(
        "  {name}: {} rows, ours {:.1?}, HiGHS {:.1?}",
        lopf.model.num_rows(),
        mine,
        hi
    );
}

// Where the from-scratch simplex is left out of the default suite, and why.
//
// `differential.rs` runs it on PEGASE 1354 by default, at about 0.6 s in
// release, and that is the right place for the line. The rungs above it, one
// pass each, release build:
//
//   | Case             | Rows   | Ours   | HiGHS  |
//   | ---              | ---    | ---    | ---    |
//   | PEGASE 1354      | 3,345  | 0.56 s | 0.05 s |
//   | PEGASE 2869      | 7,451  | 2.89 s | 0.26 s |
//   | RTE 6470         | 15,395 | 23.5 s | 0.87 s |
//   | PEGASE 13659     | 34,110 | 197 s  | 7.2 s  |
//
// Read those as upper bounds rather than as figures: the machine was not idle,
// and a second pass gave 2.9 s, 42 s and 230 s for the same three rows. The
// short one repeated exactly and the long ones did not, which is what load
// looks like. What survives the noise is the shape, below, and the fact that
// every rung agreed to at least eleven significant figures.
//
// A default suite can absorb the first row and nothing below it, so all three
// larger cases are ignored and run deliberately:
//
//   cargo test -p gridwright-solve --test large_networks --release -- --ignored
//
// Ignoring them costs less than it looks. What a differential test can find is
// a formulation or a solver that is wrong, and wrongness of that kind does not
// wait for 6,470 buses to appear: every disagreement this project has found so
// far showed up on the smallest network that contained the structure at fault.
// What the larger cases add is evidence about conditioning, which is a reason
// to run them deliberately rather than constantly.
//
// The exponent is the interesting number and the one to take seriously, since
// it survives the machine being busier on one run than another in a way the
// absolute seconds do not. Over that ladder the from-scratch solver runs as
// about `rows^2.5`, against the `rows^1.9` the scaling section measured on
// synthetic rings, and no single rung is doing the work: the three intervals
// give 2.1, 2.9 and 2.7. A ring has degree two at every bus and one reactance,
// so its bases stay nearly banded and the factors barely fill in. A
// transmission network has hubs of degree twenty, radial spurs, parallel
// circuits and reactances spanning five orders of magnitude, and its bases do
// fill in. HiGHS on the same ladder runs at about `rows^2.1`, so the difficulty
// is partly the problem and partly us, and how that divides is not something
// four points can settle.

#[test]
#[ignore = "three seconds of pure-Rust simplex; run with --ignored"]
#[cfg(feature = "simplex")]
fn the_from_scratch_simplex_agrees_with_highs_on_a_two_thousand_eight_hundred_bus_network() {
    both_solvers_agree_on(PEGASE_2869);
}

#[test]
#[ignore = "half a minute of pure-Rust simplex; run with --ignored"]
#[cfg(feature = "simplex")]
fn the_from_scratch_simplex_agrees_with_highs_on_a_six_thousand_bus_network() {
    both_solvers_agree_on(RTE_6470);
}

#[test]
#[ignore = "three to four minutes of pure-Rust simplex; run with --ignored"]
#[cfg(feature = "simplex")]
fn the_from_scratch_simplex_agrees_with_highs_on_a_thirteen_thousand_bus_network() {
    // The largest program the pure-Rust backend has been asked for: 34,110 rows
    // of real topology, against the 20,736 rows of synthetic ring the scaling
    // section is otherwise written from. Agreement here is the strongest single
    // piece of evidence that the solver is right, because a shared bug is not
    // what makes two independent implementations agree to fourteen figures on a
    // program this size.
    both_solvers_agree_on(PEGASE_13659);
}

#[test]
#[ignore = "a measurement, not a guard"]
fn what_the_largest_real_topologies_cost() {
    // Real-topology numbers for the scaling section, which otherwise has only
    // synthetic rings. A ring has degree two everywhere and one reactance; a
    // transmission network has hubs, radial spurs, parallel circuits and
    // reactances spanning five orders of magnitude, and those are what decide
    // how a sparse factorisation actually behaves.
    //
    // Every row here is produced by the same function the property tests call,
    // so a number in this table is a number from a run that was also checked
    // rather than from a faster path that skipped the checking.
    println!(
        "\n  {:<18} {:>7} {:>8} {:>8} {:>9} {:>9} {:>10}",
        "case", "buses", "rows", "cols", "nonzeros", "build", "solve"
    );
    for case in [PEGASE_2869, RTE_6470, PEGASE_13659] {
        let m = every_property_holds(case);
        println!(
            "  {:<18} {:>7} {:>8} {:>8} {:>9} {:>9.1?} {:>10.1?}",
            case.0, m.buses, m.rows, m.cols, m.nnz, m.build, m.solve
        );
    }
    // PEGASE 9241 has no HiGHS solve time to report, so it is shown for its
    // construction figures alone rather than left out of a table it belongs in
    // by size. Omitting it would make the sequence look like 6,470 buses to
    // 13,659 with nothing between.
    let (_, lopf, build) = load_and_build(PEGASE_9241);
    println!(
        "  {:<18} {:>7} {:>8} {:>8} {:>9} {:>9.1?} {:>10}",
        PEGASE_9241.0,
        PEGASE_9241.1,
        lopf.model.num_rows(),
        lopf.model.num_cols(),
        lopf.model.nnz(),
        build,
        "declined"
    );
}

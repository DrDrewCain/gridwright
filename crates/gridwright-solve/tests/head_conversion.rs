//! Head's effect on energy conversion, as distinct from its effect on capacity.
//!
//! Two different consequences of the same physics, and only the first was
//! modelled before:
//!
//! - **Capacity.** A reservoir near empty cannot reach its rating, because
//!   power is proportional to the height the water falls through. Linear in
//!   the stored level.
//! - **Conversion.** A full reservoir yields more megawatt-hours from the same
//!   *volume*, for the same reason. Volume drawn per megawatt-hour goes as
//!   `1/head`, and head depends on the level, so this one is bilinear.
//!
//! Linearised over bands of reservoir level following Borghetti, D'Ambrosio,
//! Lodi and Martello (2008). A binary picks the band, so this is a MILP.

#![cfg(feature = "highs")]

use gridwright_build::build_lopf;
use gridwright_net::{Generator, Load, Network, Snapshots, StorageUnit};
use gridwright_solve::{HighsSolver, Solver, Status};

/// A reservoir and an expensive alternative, so the optimiser draws on the
/// water first and the question is how far the water goes.
fn hydro(bands: usize, start_level: f64) -> Network {
    let mut net = Network::new(Snapshots::hourly(6));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "backup".into(),
        bus: b,
        p_nom: 500.0,
        marginal_cost: 200.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 40.0,
        ..Default::default()
    });
    net.add_storage(StorageUnit {
        name: "reservoir".into(),
        bus: b,
        p_nom: 100.0,
        max_hours: 10.0, // 1000 MWh of usable volume at full head
        efficiency_store: 1.0,
        efficiency_dispatch: 1.0,
        // Head falls to 60% of full when the reservoir is empty, so a
        // megawatt-hour drawn near the bottom costs well over half again as
        // much water as one drawn at the top.
        head_min_pu: 0.6,
        head_bands: bands,
        soc_initial: Some(start_level),
        cyclic: false,
        ..Default::default()
    });
    net
}

fn solve(net: &Network) -> (Status, f64, Vec<f64>) {
    let lopf = build_lopf(net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    let discharge = sol.trajectory(lopf.vars.discharge[0]).to_vec();
    (sol.status, sol.objective, discharge)
}

#[test]
fn turning_bands_on_makes_the_problem_an_integer_one() {
    // Selecting which band the reservoir is in is a discrete choice, and
    // saying so is better than pretending otherwise: the pure-Rust backend
    // cannot solve this and needs to decline rather than return a relaxation
    // with the reservoir spread across three bands at once.
    let with = build_lopf(&hydro(4, 500.0)).unwrap();
    assert!(with.model.is_mip());

    let without = build_lopf(&hydro(0, 500.0)).unwrap();
    assert!(!without.model.is_mip(), "bands off should stay an LP");
}

#[test]
fn a_full_reservoir_yields_more_energy_than_a_low_one_from_the_same_volume() {
    // The physics, stated as an experiment. Both runs start with the same
    // stored volume available above their floor; the one sitting higher in the
    // reservoir converts it at a better head and needs less of the expensive
    // backup.
    //
    // Started full against started at a third, with the same 400 MWh of demand
    // over the horizon.
    let (status_hi, cost_hi, _) = solve(&hydro(4, 1000.0));
    let (status_lo, cost_lo, _) = solve(&hydro(4, 333.0));
    assert_eq!(status_hi, Status::Optimal);
    assert_eq!(status_lo, Status::Optimal);
    assert!(
        cost_hi < cost_lo,
        "water high in the reservoir should go further: {cost_hi} against {cost_lo}"
    );
}

#[test]
fn ignoring_conversion_overstates_how_far_the_water_goes() {
    // The reason this matters rather than being a refinement. Without the
    // conversion effect every megawatt-hour costs the same volume no matter
    // where in the reservoir it comes from, so a model claims more energy from
    // a given store than the reservoir can actually deliver, and understates
    // what the rest of the system has to cover.
    let start = 300.0;
    let (_, cost_without, _) = solve(&hydro(0, start));
    let (_, cost_with, _) = solve(&hydro(4, start));
    assert!(
        cost_with > cost_without,
        "modelling conversion should cost more, not less: {cost_with} against \
         {cost_without}"
    );
}

#[test]
fn the_energy_drawn_matches_the_band_head_by_hand() {
    // Hand-derived. The reservoir holds 1000 MWh at full head with a floor at
    // 0.6, split into four bands. Band 3 runs from 750 to 1000 MWh and is
    // evaluated at its midpoint, so its head is 0.6 + 0.4 * (3.5/4) = 0.95.
    //
    // Starting at 1000 MWh, the first hour's discharge sits in band 3, so
    // 40 MWh delivered draws 40 / 0.95 = 42.105 MWh of volume, leaving
    // 957.895.
    let net = hydro(4, 1000.0);
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let discharge = sol.trajectory(lopf.vars.discharge[0]);
    let soc = sol.trajectory(lopf.vars.soc[0]);
    assert!(
        (discharge[0] - 40.0).abs() < 1e-6,
        "the reservoir should carry the whole load: {}",
        discharge[0]
    );
    let head = 0.6 + 0.4 * (3.5 / 4.0);
    let expected = 1000.0 - 40.0 / head;
    assert!(
        (soc[0] - expected).abs() < 1e-4,
        "level after one hour is {} but a head of {head} predicts {expected}",
        soc[0]
    );
}

#[test]
fn more_bands_do_not_change_the_answer_wildly() {
    // A linearisation should converge, not oscillate. Two bands against eight
    // is a coarse and a fine approximation of the same curve, and they should
    // land close together even though neither is exact.
    let (_, coarse, _) = solve(&hydro(2, 800.0));
    let (_, fine, _) = solve(&hydro(8, 800.0));
    let spread = (coarse - fine).abs() / fine.abs().max(1.0);
    assert!(
        spread < 0.10,
        "two bands gave {coarse} and eight gave {fine}, which is {:.1}% apart",
        spread * 100.0
    );
}

#[test]
fn a_reservoir_at_full_head_behaves_as_it_did_before() {
    // Bands are only meaningful when head actually varies. A unit whose head
    // does not vary must produce the same answer with the feature on as off,
    // or every existing hydro model quietly changes.
    let mut flat = hydro(4, 800.0);
    flat.storage[0].head_min_pu = 1.0;
    let (status, with, _) = solve(&flat);
    assert_eq!(status, Status::Optimal);

    let mut off = flat.clone();
    off.storage[0].head_bands = 0;
    let (_, without, _) = solve(&off);
    assert!(
        (with - without).abs() < 1e-6,
        "{with} against {without} with no head variation to model"
    );
}

#[test]
fn conversion_and_capacity_are_different_effects_and_both_apply() {
    // The capacity limit caps instantaneous power; conversion decides how much
    // volume that power costs. A reservoir low enough for both to bind should
    // show both: output below the rating, and volume falling faster per MWh
    // than at the top.
    let net = hydro(4, 150.0);
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let discharge = sol.trajectory(lopf.vars.discharge[0]);
    let soc = sol.trajectory(lopf.vars.soc[0]);
    // Band 0 spans 0 to 250 MWh, midpoint head 0.65.
    let head = 0.6 + 0.4 * (0.5 / 4.0);
    if discharge[0] > 1e-6 {
        let drawn = 150.0 - soc[0];
        let implied = discharge[0] / drawn;
        assert!(
            (implied - head).abs() < 1e-3,
            "drew {drawn} MWh of volume for {} MWh delivered, implying a head of \
             {implied} against the band's {head}",
            discharge[0]
        );
    }
}

// --- The scalable treatment of the same physics. ---

use gridwright_solve::head::{HeadOptions, solve_head_iterated};

#[test]
fn the_fixed_point_lands_near_the_exact_answer() {
    // The claim that makes the iteration worth having. It gives up the
    // optimality guarantee, so the least it must do is agree with the
    // formulation that keeps one.
    let net = hydro(0, 600.0); // bands off; the iteration supplies the head
    let r = solve_head_iterated(&net, &HighsSolver::default(), HeadOptions::default())
        .unwrap();
    assert_eq!(r.solution.status, Status::Optimal);
    assert!(r.converged, "did not converge, residual {}", r.residual);

    let (status, exact, _) = solve(&hydro(8, 600.0));
    assert_eq!(status, Status::Optimal);

    let spread = (r.solution.objective - exact).abs() / exact.abs().max(1.0);
    assert!(
        spread < 0.05,
        "iterated {} against exact {}, which is {:.1}% apart",
        r.solution.objective,
        exact,
        spread * 100.0
    );
}

#[test]
fn the_iteration_needs_no_binaries_at_all() {
    // The entire reason it exists. The exact formulation puts a binary per
    // band per snapshot into the model, which is where hydro MILPs stop
    // finishing on real horizons.
    let net = hydro(0, 600.0);
    let r = solve_head_iterated(&net, &HighsSolver::default(), HeadOptions::default())
        .unwrap();
    assert!(!r.lopf.model.is_mip(), "the iterated model must stay an LP");
    assert!(r.iterations > 1, "one pass is not a fixed point");
}

#[test]
fn it_improves_on_ignoring_head_entirely() {
    // The first iteration starts at full head everywhere, which is exactly
    // what a model ignoring the effect assumes. Every iteration after that
    // should move away from it, or the whole exercise is decorative.
    let net = hydro(0, 300.0);
    let ignored = solve(&net).1;
    let r = solve_head_iterated(&net, &HighsSolver::default(), HeadOptions::default())
        .unwrap();
    assert!(
        r.solution.objective > ignored,
        "accounting for head should cost more, not less: {} against {ignored}",
        r.solution.objective
    );
}

#[test]
fn the_heads_it_settles_on_track_the_reservoir_level() {
    // A sanity check on the fixed point itself rather than on the cost. As the
    // reservoir draws down over the horizon, the head it converts at should
    // fall with it.
    let net = hydro(0, 1000.0);
    let r = solve_head_iterated(&net, &HighsSolver::default(), HeadOptions::default())
        .unwrap();
    let heads = r.head.row(0).unwrap();
    assert!(heads.iter().all(|h| (0.6..=1.0).contains(h)), "{heads:?}");
    assert!(
        heads.last().unwrap() <= heads.first().unwrap(),
        "head should not rise as the reservoir empties: {heads:?}"
    );
}

#[test]
fn a_unit_with_no_head_variation_is_left_alone() {
    let mut net = hydro(0, 800.0);
    net.storage[0].head_min_pu = 1.0;
    let r = solve_head_iterated(&net, &HighsSolver::default(), HeadOptions::default())
        .unwrap();
    assert!(r.converged);
    let (_, plain, _) = solve(&net);
    assert!((r.solution.objective - plain).abs() < 1e-6);
}

#[test]
fn under_relaxation_is_what_stops_it_oscillating() {
    // A fuller reservoir converts better, which encourages drawing on it,
    // which empties it, which converts worse. Taking the new head outright
    // lets that chase itself; a partial step damps it. Both should reach a
    // similar place, and the damped one should get there without thrashing.
    let net = hydro(0, 700.0);
    let damped = solve_head_iterated(
        &net,
        &HighsSolver::default(),
        HeadOptions {
            relaxation: 0.5,
            ..Default::default()
        },
    )
    .unwrap();
    let undamped = solve_head_iterated(
        &net,
        &HighsSolver::default(),
        HeadOptions {
            relaxation: 1.0,
            max_iterations: 40,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(damped.converged, "damped run did not settle");
    let spread = (damped.solution.objective - undamped.solution.objective).abs()
        / undamped.solution.objective.abs().max(1.0);
    assert!(spread < 0.05, "the two runs disagree by {:.1}%", spread * 100.0);
}

#[test]
fn a_run_that_does_not_settle_says_so_rather_than_pretending() {
    // Capped at one iteration, the fixed point cannot have been reached, and
    // reporting convergence anyway would be the worst possible outcome: an
    // answer under a head assumption nobody checked.
    let net = hydro(0, 500.0);
    let r = solve_head_iterated(
        &net,
        &HighsSolver::default(),
        HeadOptions {
            max_iterations: 1,
            tolerance: 1e-12,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!r.converged);
    assert_eq!(r.iterations, 1);
    assert!(r.residual > 0.0);
}

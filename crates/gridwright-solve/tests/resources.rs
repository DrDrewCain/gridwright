//! Water and land, on the same machinery as carbon.
//!
//! Carbon is not the only thing a system is asked to stay under. Thermal plant
//! is cooled with water, and in much of the world that rather than emissions
//! decides whether a station can run through a dry summer — a constraint that
//! binds in exactly the weeks demand peaks. Land is the mirror image, binding
//! against renewables rather than for them, since a wind farm's footprint is
//! what limits how much of it a region will accept.
//!
//! All three are one row with different coefficients, which is why they share a
//! builder.

#![cfg(feature = "highs")]

use gridwright_build::build_lopf;
use gridwright_net::{Generator, Load, Network, Snapshots};
use gridwright_solve::{HighsSolver, Solver, Status};

/// A thirsty cheap unit and a dry expensive one, so a water ceiling has
/// something to bite on.
fn thirsty() -> Network {
    let mut net = Network::new(Snapshots::hourly(2));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "coal".into(),
        bus: b,
        p_nom: 200.0,
        marginal_cost: 10.0,
        // Two cubic metres per megawatt-hour, which is the right order for
        // once-through cooling.
        water_use: 2.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "gas".into(),
        bus: b,
        p_nom: 200.0,
        marginal_cost: 60.0,
        water_use: 0.5,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 100.0,
        ..Default::default()
    });
    net
}

fn run(net: &Network) -> (Status, f64, Vec<f64>) {
    let lopf = build_lopf(net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    let dispatch = (0..net.generators.len())
        .map(|g| sol.dispatch(&lopf.vars, g).iter().sum::<f64>())
        .collect();
    (sol.status, sol.objective, dispatch)
}

#[test]
fn without_a_ceiling_the_thirsty_unit_runs_because_it_is_cheap() {
    let (status, _, dispatch) = run(&thirsty());
    assert_eq!(status, Status::Optimal);
    assert!((dispatch[0] - 200.0).abs() < 1e-6, "{dispatch:?}");
    assert!(dispatch[1].abs() < 1e-6, "{dispatch:?}");
}

#[test]
fn a_water_ceiling_moves_generation_to_the_drier_unit() {
    // 200 MWh from coal would draw 400 cubic metres. Cap at 250 and some of it
    // has to come from the gas unit instead.
    //
    // Hand-derived: with coal at 2 and gas at 0.5, meeting 200 MWh under a
    // 250 cap means 2c + 0.5(200 - c) <= 250, so c <= 100.
    let mut net = thirsty();
    net.water_limit = Some(250.0);
    let (status, _, dispatch) = run(&net);
    assert_eq!(status, Status::Optimal);
    assert!(
        (dispatch[0] - 100.0).abs() < 1e-6,
        "coal should be held to 100 MWh, got {dispatch:?}"
    );
    assert!((dispatch[1] - 100.0).abs() < 1e-6, "{dispatch:?}");

    let used = dispatch[0] * 2.0 + dispatch[1] * 0.5;
    assert!(used <= 250.0 + 1e-6, "drew {used} against a cap of 250");
}

#[test]
fn a_water_ceiling_costs_money_and_the_dual_says_how_much() {
    // The reason it is a constraint rather than a report: it changes the
    // dispatch, and the shadow price on the row is what another cubic metre
    // would be worth.
    let free = run(&thirsty()).1;
    let mut capped = thirsty();
    capped.water_limit = Some(250.0);
    let constrained = run(&capped).1;
    assert!(
        constrained > free,
        "a binding water cap should cost something: {constrained} against {free}"
    );
    // 100 MWh moves from a unit at 10 to one at 60.
    assert!((constrained - free - 100.0 * 50.0).abs() < 1e-6);
}

#[test]
fn a_ceiling_nobody_reaches_changes_nothing() {
    let mut net = thirsty();
    net.water_limit = Some(10_000.0);
    let (status, cost, dispatch) = run(&net);
    assert_eq!(status, Status::Optimal);
    assert!((cost - run(&thirsty()).1).abs() < 1e-6);
    assert!((dispatch[0] - 200.0).abs() < 1e-6);
}

#[test]
fn land_is_charged_on_what_gets_built_not_on_what_already_stands() {
    // The existing fleet's land is already taken, so charging for it would
    // forbid using plant that is standing there.
    let mut net = Network::new(Snapshots::hourly(1));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "existing_wind".into(),
        bus: b,
        p_nom: 100.0,
        marginal_cost: 0.0,
        land_use: 0.2,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "new_wind".into(),
        bus: b,
        p_nom: 0.0,
        p_nom_extendable: true,
        p_nom_max: 500.0,
        capital_cost: 1.0,
        marginal_cost: 0.0,
        land_use: 0.2,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "gas".into(),
        bus: b,
        p_nom: 500.0,
        marginal_cost: 80.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 300.0,
        ..Default::default()
    });
    // Room for 100 MW of new build and no more.
    net.land_limit = Some(20.0);

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let built = sol.capacity_built(lopf.vars.gen_capacity[1].unwrap());
    assert!(
        (built - 100.0).abs() < 1e-6,
        "20 km² at 0.2 per MW allows 100 MW, got {built}"
    );
    // The existing 100 MW still runs, so demand is met without land for it.
    assert!((sol.dispatch(&lopf.vars, 0)[0] - 100.0).abs() < 1e-6);
}

#[test]
fn the_three_budgets_are_independent() {
    // One row each, and a carbon cap should not silently constrain water.
    let mut net = thirsty();
    net.generators[0].co2_emissions = 1.0;
    net.co2_limit = Some(10_000.0);
    net.water_limit = Some(250.0);
    let (status, _, dispatch) = run(&net);
    assert_eq!(status, Status::Optimal);
    // Water binds, carbon does not, so the answer is the water-only one.
    assert!((dispatch[0] - 100.0).abs() < 1e-6, "{dispatch:?}");

    // And tightening carbon on top binds further.
    net.co2_limit = Some(50.0);
    let (status, _, tighter) = run(&net);
    assert_eq!(status, Status::Optimal);
    assert!(
        tighter[0] <= 50.0 + 1e-6,
        "carbon should now be the binding one: {tighter:?}"
    );
}

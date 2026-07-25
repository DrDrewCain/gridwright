//! Capacity expansion and emissions constraints, checked against arithmetic.
//!
//! Expansion is a different question from dispatch: not "how should today be
//! run" but "what should we build". Every expected number below was worked out
//! by hand first, because a build decision that is plausible and wrong is the
//! most expensive kind of wrong an energy model can produce.

use gridwright_build::{Lopf, build_lopf};
use gridwright_net::{Generator, Line, Load, Network, Snapshots, StorageUnit, TimeSeries};
use gridwright_solve::{HighsSolver, Solver, Status};

const EPS: f64 = 1e-6;

fn assert_close(got: f64, want: f64, what: &str) {
    assert!(
        (got - want).abs() < 1e-5,
        "{what}: got {got}, expected {want} (difference {})",
        (got - want).abs()
    );
}

/// One bus. Expensive existing plant, and the option to build something
/// cheaper to run but with a capital cost attached.
fn build_or_buy(hours: usize, demand: f64, capital: f64) -> Network {
    let mut net = Network::new(Snapshots::hourly(hours));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "existing".into(),
        bus: b,
        p_nom: 1_000.0,
        marginal_cost: 200.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "new".into(),
        bus: b,
        p_nom: 0.0,
        marginal_cost: 10.0,
        p_nom_extendable: true,
        p_nom_max: 1_000.0,
        capital_cost: capital,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: demand,
        ..Default::default()
    });
    net
}

#[test]
fn cheap_capital_gets_built_and_displaces_expensive_fuel() {
    // 80 MW for 10 h. Running existing costs 80*10*200 = 160,000.
    // Building 80 MW at 50/MW costs 4,000 capital + 8,000 fuel = 12,000.
    let net = build_or_buy(10, 80.0, 50.0);
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();

    assert_eq!(sol.status, Status::Optimal);
    let built = sol.capacity_built(lopf.vars.gen_capacity[1].unwrap());
    assert_close(built, 80.0, "capacity built");
    assert_close(sol.objective, 12_000.0, "total system cost");
    // The existing plant should now be idle.
    for &p in sol.dispatch(&lopf.vars, 0) {
        assert!(p < EPS, "existing plant should be displaced, ran {p}");
    }
}

#[test]
fn capital_above_break_even_is_not_built() {
    // Break-even is hours * (200 - 10) = 1900 per MW. Above that, building
    // cannot pay for itself and the optimiser must decline.
    let net = build_or_buy(10, 80.0, 2_500.0);
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();

    let built = sol.capacity_built(lopf.vars.gen_capacity[1].unwrap());
    assert_close(built, 0.0, "nothing should be built above break-even");
    assert_close(sol.objective, 80.0 * 10.0 * 200.0, "cost of running existing");
}

#[test]
fn the_break_even_capital_cost_is_where_the_decision_flips() {
    // Straddling the analytic break-even of 1900 must flip the answer, and the
    // two objectives must meet there. This is a sharper test than either side
    // alone: it checks the *location* of the threshold, not just its existence.
    let solve = |capital: f64| {
        let net = build_or_buy(10, 80.0, capital);
        let lopf = build_lopf(&net).unwrap();
        let sol = HighsSolver::default().solve(&lopf).unwrap();
        (
            sol.capacity_built(lopf.vars.gen_capacity[1].unwrap()),
            sol.objective,
        )
    };
    let (below, cost_below) = solve(1_899.0);
    let (above, _) = solve(1_901.0);
    assert_close(below, 80.0, "just below break-even, build");
    assert_close(above, 0.0, "just above break-even, do not build");
    // At 1899 the two options are nearly identical in cost.
    assert!(
        (cost_below - 160_000.0).abs() < 100.0,
        "cost at break-even should approach the do-nothing cost, got {cost_below}"
    );
}

#[test]
fn existing_capacity_is_a_floor_that_cannot_be_un_built() {
    // An extendable unit with 60 MW already installed cannot go below 60 even
    // when it is uneconomic, because plant does not un-build itself.
    let mut net = build_or_buy(4, 10.0, 5_000.0);
    net.generators[1].p_nom = 60.0;
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    // The variable is additional build, so an incumbent that should not grow
    // reports zero new capacity and sixty installed.
    assert_close(
        sol.capacity_built(lopf.vars.gen_capacity[1].unwrap()),
        0.0,
        "nothing new should be built",
    );
    assert_close(
        sol.total_capacity(lopf.vars.gen_capacity[1], net.generators[1].p_nom),
        60.0,
        "installed capacity is the existing fleet",
    );
}

#[test]
fn expansion_respects_the_capacity_ceiling() {
    // Demand exceeds what may be built, so the ceiling binds and the rest is
    // served by the expensive incumbent rather than by more cheap plant.
    let mut net = build_or_buy(5, 500.0, 10.0);
    net.generators[1].p_nom_max = 200.0;
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();

    let built = sol.capacity_built(lopf.vars.gen_capacity[1].unwrap());
    assert_close(built, 200.0, "build up to the ceiling");
    assert_close(sol.dispatch(&lopf.vars, 1)[0], 200.0, "new plant at full output");
    assert_close(sol.dispatch(&lopf.vars, 0)[0], 300.0, "incumbent covers the rest");
}

#[test]
fn an_availability_profile_scales_what_built_capacity_can_deliver() {
    // Wind is only half available, so 100 MW of demand needs 200 MW built.
    // This is the constraint p[g,t] <= availability[g,t] * P_g doing its job;
    // with fixed capacity it would be a bound instead.
    let mut net = Network::new(Snapshots::hourly(1));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "wind".into(),
        bus: b,
        p_nom: 0.0,
        marginal_cost: 0.0,
        p_nom_extendable: true,
        p_nom_max: 10_000.0,
        capital_cost: 1.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 100.0,
        ..Default::default()
    });
    net.gen_availability = TimeSeries::from_rows(&[vec![0.5]], 1).unwrap();

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    let built = sol.capacity_built(lopf.vars.gen_capacity[0].unwrap());
    assert_close(built, 200.0, "must build double to deliver through 50% availability");
    assert_close(sol.dispatch(&lopf.vars, 0)[0], 100.0, "delivered energy");
}

#[test]
fn transmission_expansion_relieves_a_binding_interconnector() {
    // Cheap generation stranded behind a 10 MW link. Reinforcing costs 5 per MW
    // against a 90/MWh saving over 4 hours, so it is obviously worth it and the
    // link should be widened to carry the whole 100 MW.
    let mut net = Network::new(Snapshots::hourly(4));
    let a = net.add_bus("A", "AA");
    let b = net.add_bus("B", "BB");
    net.add_generator(Generator {
        name: "cheap".into(),
        bus: a,
        p_nom: 1_000.0,
        marginal_cost: 5.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "dear".into(),
        bus: b,
        p_nom: 1_000.0,
        marginal_cost: 95.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 100.0,
        ..Default::default()
    });
    net.add_line(Line {
        name: "A-B".into(),
        bus0: a,
        bus1: b,
        s_nom: 10.0,
        susceptance: 0.0, // transport, since expanding an AC line is nonlinear
        s_nom_extendable: true,
        s_nom_max: 500.0,
        capital_cost: 5.0,
        ..Default::default()
    });

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    // 10 MW already exists, so 90 MW is added to reach the 100 MW transfer.
    assert_close(
        sol.capacity_built(lopf.vars.line_capacity[0].unwrap()),
        90.0,
        "reinforcement added",
    );
    assert_close(
        sol.total_capacity(lopf.vars.line_capacity[0], net.lines[0].s_nom),
        100.0,
        "total transfer capability",
    );
    // 100 MW * 4 h at 5/MWh, plus capital on the 90 MW *added*. The 10 MW that
    // already existed is not paid for again, which an earlier version of this
    // model got wrong by charging capital on total capacity rather than on the
    // build.
    assert_close(sol.objective, 100.0 * 4.0 * 5.0 + 90.0 * 5.0, "total cost");
}

#[test]
fn expanding_an_ac_line_is_refused_rather_than_linearised() {
    // Widening an AC line changes its impedance, which makes the DC flow
    // equation bilinear. Silently solving that as though susceptance were
    // fixed would produce a confident wrong answer, so it is rejected.
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("A", "AA");
    let b = net.add_bus("B", "BB");
    net.add_line(Line {
        name: "ac".into(),
        bus0: a,
        bus1: b,
        s_nom: 10.0,
        susceptance: 5.0,
        s_nom_extendable: true,
        ..Default::default()
    });
    assert!(build_lopf(&net).is_err(), "extendable AC line must be refused");
}

// ---------------------------------------------------------------------------
// Emissions
// ---------------------------------------------------------------------------

/// Dirty plant that is cheap to run, clean plant that is not.
fn carbon_pair(demand: f64, cap: Option<f64>) -> Network {
    let mut net = Network::new(Snapshots::hourly(1));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "coal".into(),
        bus: b,
        p_nom: 1_000.0,
        marginal_cost: 10.0,
        co2_emissions: 0.8,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "clean".into(),
        bus: b,
        p_nom: 1_000.0,
        marginal_cost: 50.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: demand,
        ..Default::default()
    });
    net.co2_limit = cap;
    net
}

#[test]
fn without_a_cap_the_dirty_plant_runs() {
    let net = carbon_pair(100.0, None);
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_close(sol.dispatch(&lopf.vars, 0)[0], 100.0, "coal runs unconstrained");
    assert_close(sol.objective, 1_000.0, "cost");
}

#[test]
fn a_carbon_cap_substitutes_clean_for_dirty_by_exactly_the_binding_amount() {
    // 0.8 t/MWh. A 40 t budget permits 50 MWh of coal; the other 50 must be
    // clean. Cost becomes 50*10 + 50*50 = 3,000.
    for (cap, coal, cost) in [(80.0, 100.0, 1_000.0), (40.0, 50.0, 3_000.0), (0.0, 0.0, 5_000.0)] {
        let net = carbon_pair(100.0, Some(cap));
        let lopf = build_lopf(&net).unwrap();
        let sol = HighsSolver::default().solve(&lopf).unwrap();
        assert_eq!(sol.status, Status::Optimal, "cap {cap}");
        assert_close(
            sol.dispatch(&lopf.vars, 0)[0],
            coal,
            &format!("coal output under a {cap} t cap"),
        );
        assert_close(sol.objective, cost, &format!("cost under a {cap} t cap"));
    }
}

#[test]
fn the_emissions_row_exists_only_when_a_cap_is_set() {
    let with = Lopf::row_counts(&carbon_pair(100.0, Some(10.0)));
    let without = Lopf::row_counts(&carbon_pair(100.0, None));
    assert_eq!(with.co2, 1);
    assert_eq!(without.co2, 0);
    assert_eq!(with.total(), without.total() + 1);
}

#[test]
fn a_carbon_cap_and_expansion_together_build_clean_capacity() {
    // The combination is the question decarbonisation policy actually asks:
    // given a budget, what should be built? Coal is cheap to run but capped;
    // clean plant must be built to serve the remainder.
    let mut net = Network::new(Snapshots::hourly(10));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "coal".into(),
        bus: b,
        p_nom: 1_000.0,
        marginal_cost: 10.0,
        co2_emissions: 1.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "solar".into(),
        bus: b,
        p_nom: 0.0,
        marginal_cost: 0.0,
        p_nom_extendable: true,
        p_nom_max: 1_000.0,
        // Above 100/MW, since X MW of solar yields 10X MWh over the horizon and
        // coal would supply that for 100X. Cheaper solar than that gets built
        // on economics alone and the cap never binds, which would make this
        // test pass for the wrong reason.
        capital_cost: 200.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 100.0,
        ..Default::default()
    });
    // 1000 MWh of demand over the horizon, 400 t of budget at 1 t/MWh means at
    // most 400 MWh from coal, so 600 MWh must come from built solar. Running
    // flat out that needs 60 MW, for 4,000 of fuel plus 12,000 of capital.
    net.co2_limit = Some(400.0);

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let coal_total: f64 = sol.dispatch(&lopf.vars, 0).iter().sum();
    assert_close(coal_total, 400.0, "coal limited by the carbon budget");
    let built = sol.capacity_built(lopf.vars.gen_capacity[1].unwrap());
    assert_close(built, 60.0, "solar built to cover the remainder");
    assert_close(sol.objective, 16_000.0, "fuel plus capital");
    assert!(sol.total_shed(&lopf.vars) < 1e-4, "demand must still be met");
}

#[test]
fn a_cap_that_does_not_bind_changes_nothing() {
    // The companion to the test above, and the reason its capital cost is what
    // it is: when clean capacity is cheap enough it gets built on economics
    // alone, and the carbon budget is slack. A cap test that cannot distinguish
    // these two cases is not testing the cap.
    let mut net = Network::new(Snapshots::hourly(10));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "coal".into(),
        bus: b,
        p_nom: 1_000.0,
        marginal_cost: 10.0,
        co2_emissions: 1.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "solar".into(),
        bus: b,
        p_nom: 0.0,
        marginal_cost: 0.0,
        p_nom_extendable: true,
        p_nom_max: 1_000.0,
        capital_cost: 20.0, // well below the 100/MW break-even
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 100.0,
        ..Default::default()
    });
    net.co2_limit = Some(400.0);

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    let coal_total: f64 = sol.dispatch(&lopf.vars, 0).iter().sum();
    assert_close(coal_total, 0.0, "cheap solar displaces coal without any cap");
    assert_close(
        sol.capacity_built(lopf.vars.gen_capacity[1].unwrap()),
        100.0,
        "build enough solar to serve everything",
    );
    assert_close(sol.objective, 2_000.0, "capital only, no fuel");
}

#[test]
fn storage_expansion_is_driven_by_the_value_of_shifting_energy() {
    // Generation only in hour 0, demand only in hour 1, so the only way to
    // serve it is to build a store. 50 MW of demand needs 50 MW of rating.
    let mut net = Network::new(Snapshots::hourly(2));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "g".into(),
        bus: b,
        p_nom: 1_000.0,
        marginal_cost: 1.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 0.0,
        ..Default::default()
    });
    net.load_profile = TimeSeries::from_rows(&[vec![0.0, 50.0]], 2).unwrap();
    net.gen_availability = TimeSeries::from_rows(&[vec![1.0, 0.0]], 2).unwrap();
    net.add_storage(StorageUnit {
        name: "batt".into(),
        bus: b,
        p_nom: 0.0,
        max_hours: 4.0,
        efficiency_store: 1.0,
        efficiency_dispatch: 1.0,
        cyclic: false,
        p_nom_extendable: true,
        p_nom_max: 500.0,
        capital_cost: 2.0,
        ..Default::default()
    });

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    let built = sol.capacity_built(lopf.vars.storage_capacity[0].unwrap());
    assert_close(built, 50.0, "storage rating built to meet the peak");
    assert!(sol.total_shed(&lopf.vars) < 1e-4, "demand must be met");
}

#[test]
fn a_built_store_may_not_exceed_its_energy_ceiling() {
    // max_hours ties energy to power, so a 4 hour store built at 50 MW can hold
    // at most 200 MWh no matter how much the optimiser would like it to.
    let mut net = Network::new(Snapshots::hourly(6));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "g".into(),
        bus: b,
        p_nom: 1_000.0,
        marginal_cost: 1.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 20.0,
        ..Default::default()
    });
    net.add_storage(StorageUnit {
        name: "batt".into(),
        bus: b,
        p_nom: 0.0,
        max_hours: 4.0,
        p_nom_extendable: true,
        p_nom_max: 50.0,
        capital_cost: 0.01,
        ..Default::default()
    });

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    let built = sol.capacity_built(lopf.vars.storage_capacity[0].unwrap());
    for &e in sol.trajectory(lopf.vars.soc[0]) {
        assert!(
            e <= built * 4.0 + 1e-4,
            "stored {e} exceeds the {} MWh implied by {built} MW over 4 h",
            built * 4.0
        );
    }
}

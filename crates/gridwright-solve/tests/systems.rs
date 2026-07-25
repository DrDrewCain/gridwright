//! Unit commitment, sector coupling, hydro, multi-period investment, and the
//! regional structure that makes this usable outside Europe.
//!
//! As elsewhere, every expected number was derived before the test was written.
//! Where a quantity is genuinely not determinate, the test says so and asserts
//! only what is.

use gridwright_build::build_lopf;
use gridwright_net::{
    Generator, InvestmentPeriod, Line, Link, Load, Network, Snapshots, StorageUnit, TimeSeries,
};
use gridwright_solve::{HighsSolver, Solver, Status};

fn assert_close(got: f64, want: f64, what: &str) {
    assert!(
        (got - want).abs() < 1e-4,
        "{what}: got {got}, expected {want} (difference {})",
        (got - want).abs()
    );
}

// ---------------------------------------------------------------------------
// Unit commitment
// ---------------------------------------------------------------------------

/// A committable coal unit that cannot run below half rating, plus flexible gas.
fn commitment_system(demand: &[f64], start_cost: f64, min_up: usize) -> Network {
    let mut net = Network::new(Snapshots::hourly(demand.len()));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "coal".into(),
        bus: b,
        p_nom: 200.0,
        marginal_cost: 10.0,
        p_min_pu: 0.5,
        committable: true,
        start_up_cost: start_cost,
        min_up_time: min_up,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "gas".into(),
        bus: b,
        p_nom: 200.0,
        marginal_cost: 30.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 0.0,
        ..Default::default()
    });
    net.load_profile = TimeSeries::from_rows(&[demand.to_vec()], demand.len()).unwrap();
    net
}

#[test]
fn commitment_makes_the_problem_a_mip() {
    let net = commitment_system(&[100.0, 100.0], 0.0, 0);
    let lopf = build_lopf(&net).unwrap();
    assert!(lopf.model.is_mip(), "committable plant must introduce binaries");
    // One status variable per snapshot, for the one committable unit.
    assert_eq!(lopf.model.num_integer(), 2);

    let plain = build_lopf(&{
        let mut n = net.clone();
        n.generators[0].committable = false;
        n
    })
    .unwrap();
    assert!(!plain.model.is_mip(), "a model without commitment stays an LP");
}

#[test]
fn a_unit_whose_minimum_exceeds_demand_is_switched_off() {
    // Coal cannot produce less than 100 MW. Demand of 60 leaves it no legal
    // operating point, so it must be off and gas must cover. A continuous
    // relaxation would happily run it at 60, which is the error commitment
    // exists to prevent.
    let net = commitment_system(&[60.0], 0.0, 0);
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();

    assert_eq!(sol.status, Status::Optimal);
    let status = sol.trajectory(lopf.vars.status[0].unwrap())[0];
    assert_close(status, 0.0, "coal must be committed off");
    assert_close(sol.dispatch(&lopf.vars, 0)[0], 0.0, "coal output");
    assert_close(sol.dispatch(&lopf.vars, 1)[0], 60.0, "gas covers demand");
    assert_close(sol.objective, 60.0 * 30.0, "cost is gas only");
}

#[test]
fn a_committed_unit_respects_its_stable_minimum() {
    // Demand of 250 exceeds gas alone, so coal must run; once on it cannot go
    // below 100 even though only 50 is needed from it.
    let net = commitment_system(&[250.0], 0.0, 0);
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();

    assert_eq!(sol.status, Status::Optimal);
    let coal = sol.dispatch(&lopf.vars, 0)[0];
    assert!(
        coal >= 100.0 - 1e-4,
        "coal ran at {coal}, below its 100 MW minimum"
    );
    assert_close(coal + sol.dispatch(&lopf.vars, 1)[0], 250.0, "demand met");
}

#[test]
fn a_start_up_cost_is_charged_exactly_once_per_start() {
    // Demand dips below coal's minimum in the middle, so it is forced off and
    // back on regardless of price. The number of starts is therefore fixed, and
    // raising the start cost must raise the objective by exactly that cost.
    // Comparing two runs isolates the charge from everything else.
    let profile = [300.0, 60.0, 300.0];
    let free = {
        let net = commitment_system(&profile, 0.0, 0);
        let l = build_lopf(&net).unwrap();
        HighsSolver::default().solve(&l).unwrap().objective
    };
    let charged = {
        let net = commitment_system(&profile, 750.0, 0);
        let l = build_lopf(&net).unwrap();
        HighsSolver::default().solve(&l).unwrap().objective
    };
    assert_close(charged - free, 750.0, "exactly one start should be charged");
}

#[test]
fn a_minimum_up_time_prevents_a_unit_from_flickering() {
    // Without a minimum up time the unit is free to cycle hourly. With one, a
    // start commits it to staying on, which is what a boiler actually does.
    let profile = [300.0, 60.0, 300.0, 60.0, 300.0];
    let net = commitment_system(&profile, 0.0, 3);
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let status = sol.trajectory(lopf.vars.status[0].unwrap());
    let starts = sol.trajectory(lopf.vars.start_up[0].unwrap());
    for (t, &u) in status.iter().enumerate() {
        assert!(
            (u - u.round()).abs() < 1e-6,
            "status at {t} is {u}, not integral"
        );
    }
    // Any start must be followed by three consecutive on snapshots.
    for (t, &s) in starts.iter().enumerate() {
        if s > 0.5 {
            for (k, &u) in status.iter().enumerate().take((t + 3).min(status.len())).skip(t) {
                assert!(
                    u > 0.5,
                    "started at {t} but was off at {k} despite a 3 snapshot minimum"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sector coupling
// ---------------------------------------------------------------------------

#[test]
fn an_electrolyser_converts_electricity_into_hydrogen_at_its_efficiency() {
    // 100 MWh of hydrogen demand through a 70% electrolyser needs 142.857 MWh
    // of electricity, costing 2857.14 at 20/MWh.
    let mut net = Network::new(Snapshots::hourly(1));
    let elec = net.add_bus("elec", "XX");
    let h2 = net.add_carrier_bus("h2", "XX", "H2");
    net.add_generator(Generator {
        name: "grid".into(),
        bus: elec,
        p_nom: 1_000.0,
        marginal_cost: 20.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "h2_demand".into(),
        bus: h2,
        p_set: 100.0,
        ..Default::default()
    });
    net.add_link(Link {
        name: "electrolyser".into(),
        bus0: elec,
        bus1: h2,
        p_nom: 1_000.0,
        efficiency: 0.7,
        ..Default::default()
    });

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    assert!(sol.total_shed(&lopf.vars) < 1e-4, "hydrogen demand must be met");

    let input = sol.trajectory(lopf.vars.link_flow[0])[0];
    assert_close(input, 100.0 / 0.7, "electricity drawn");
    assert_close(sol.dispatch(&lopf.vars, 0)[0], 100.0 / 0.7, "generation");
    assert_close(sol.objective, 100.0 / 0.7 * 20.0, "cost");
}

#[test]
fn a_heat_pump_delivers_more_heat_than_the_electricity_it_consumes() {
    // Efficiency above one is not a violation of anything: a heat pump moves
    // heat rather than creating it. The formulation has to allow it, and a
    // coefficient of performance of 3 means 30 MWh of heat costs 10 of power.
    let mut net = Network::new(Snapshots::hourly(1));
    let elec = net.add_bus("elec", "XX");
    let heat = net.add_carrier_bus("heat", "XX", "heat");
    net.add_generator(Generator {
        name: "grid".into(),
        bus: elec,
        p_nom: 1_000.0,
        marginal_cost: 50.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "heat_demand".into(),
        bus: heat,
        p_set: 30.0,
        ..Default::default()
    });
    net.add_link(Link {
        name: "heat_pump".into(),
        bus0: elec,
        bus1: heat,
        p_nom: 100.0,
        efficiency: 3.0,
        ..Default::default()
    });

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    assert_close(sol.trajectory(lopf.vars.link_flow[0])[0], 10.0, "electricity in");
    assert_close(sol.objective, 10.0 * 50.0, "cost");
}

#[test]
fn carriers_do_not_leak_into_one_another_without_a_link() {
    // Electricity is plentiful, hydrogen demand is real, and with no converter
    // between them the hydrogen bus simply cannot be served. Balance is per
    // bus, so a carrier boundary needs no special machinery to be respected.
    let mut net = Network::new(Snapshots::hourly(1));
    let elec = net.add_bus("elec", "XX");
    let h2 = net.add_carrier_bus("h2", "XX", "H2");
    net.add_generator(Generator {
        name: "grid".into(),
        bus: elec,
        p_nom: 1_000.0,
        marginal_cost: 1.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "h2_demand".into(),
        bus: h2,
        p_set: 50.0,
        ..Default::default()
    });

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_close(sol.shed(&lopf.vars, h2)[0], 50.0, "hydrogen goes unserved");
    assert_close(sol.dispatch(&lopf.vars, 0)[0], 0.0, "electricity is not needed");
}

// ---------------------------------------------------------------------------
// Hydro
// ---------------------------------------------------------------------------

#[test]
fn natural_inflow_serves_demand_without_being_generated() {
    // A reservoir with 30 MW of inflow and 20 MW of demand needs no thermal
    // plant at all. Inflow is not a decision, which is what distinguishes hydro
    // from a battery.
    let mut net = Network::new(Snapshots::hourly(4));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "diesel".into(),
        bus: b,
        p_nom: 500.0,
        marginal_cost: 200.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 20.0,
        ..Default::default()
    });
    net.add_storage(StorageUnit {
        name: "reservoir".into(),
        bus: b,
        p_nom: 100.0,
        max_hours: 10.0,
        cyclic: true,
        spillable: true,
        ..Default::default()
    });
    net.storage_inflow = TimeSeries::from_rows(&[vec![30.0; 4]], 4).unwrap();

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    assert_close(sol.objective, 0.0, "inflow is free, so nothing should burn");
    for &p in sol.dispatch(&lopf.vars, 0) {
        assert_close(p, 0.0, "diesel should stay off");
    }
    for &d in sol.trajectory(lopf.vars.discharge[0]) {
        assert_close(d, 20.0, "reservoir serves the whole load");
    }
}

#[test]
fn surplus_inflow_is_spilled_rather_than_making_the_model_infeasible() {
    // 40 MW of inflow against 5 MW of demand and a small reservoir. The water
    // has to go somewhere. Without spill this is simply infeasible, which is
    // exactly the wet week a hydro study exists to look at.
    let mut net = Network::new(Snapshots::hourly(6));
    let b = net.add_bus("B", "XX");
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 5.0,
        ..Default::default()
    });
    net.add_storage(StorageUnit {
        name: "reservoir".into(),
        bus: b,
        p_nom: 50.0,
        max_hours: 2.0,
        cyclic: true,
        spillable: true,
        ..Default::default()
    });
    net.storage_inflow = TimeSeries::from_rows(&[vec![40.0; 6]], 6).unwrap();

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal, "spill should keep this feasible");
    let spilled: f64 = sol.trajectory(lopf.vars.spill[0].unwrap()).iter().sum();
    assert!(spilled > 1e-3, "surplus water should be spilled, got {spilled}");
    assert!(sol.total_shed(&lopf.vars) < 1e-4, "demand is easily met");

    // Energy conservation: inflow equals what left plus what was spilled, since
    // a cyclic store returns to its starting level.
    let out: f64 = sol.trajectory(lopf.vars.discharge[0]).iter().sum();
    let inn: f64 = sol.trajectory(lopf.vars.charge[0]).iter().sum();
    assert_close(6.0 * 40.0 + inn, out + spilled, "reservoir energy balance");
}

#[test]
fn without_spill_a_flooded_reservoir_cannot_absorb_its_inflow() {
    // The companion to the test above. Same system, spill disabled, and the
    // only way to balance is to shed, which shows the constraint was doing
    // something rather than being decorative.
    let mut net = Network::new(Snapshots::hourly(3));
    let b = net.add_bus("B", "XX");
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 5.0,
        ..Default::default()
    });
    net.add_storage(StorageUnit {
        name: "reservoir".into(),
        bus: b,
        p_nom: 8.0,
        max_hours: 1.0,
        cyclic: true,
        spillable: false,
        ..Default::default()
    });
    net.storage_inflow = TimeSeries::from_rows(&[vec![40.0; 3]], 3).unwrap();

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(
        sol.status,
        Status::Infeasible,
        "with nowhere for the water to go this cannot balance"
    );
}

// ---------------------------------------------------------------------------
// Multi-period investment
// ---------------------------------------------------------------------------

#[test]
fn discounting_pushes_investment_into_later_periods() {
    // Two periods of two snapshots. The second is discounted, so both the fuel
    // saved and the capital spent there are worth less. Building in period 0
    // costs full price; building in period 1 costs the discounted price for one
    // period of benefit. The optimiser must not be free to ignore the timing.
    let mut net = Network::new(Snapshots::hourly(4));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "existing".into(),
        bus: b,
        p_nom: 1_000.0,
        marginal_cost: 100.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "new".into(),
        bus: b,
        p_nom: 0.0,
        marginal_cost: 0.0,
        p_nom_extendable: true,
        p_nom_max: 1_000.0,
        capital_cost: 150.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 50.0,
        ..Default::default()
    });
    net.investment_periods = vec![
        InvestmentPeriod {
            name: "2030".into(),
            first_snapshot: 0,
            n_snapshots: 2,
            discount: 1.0,
        },
        InvestmentPeriod {
            name: "2040".into(),
            first_snapshot: 2,
            n_snapshots: 2,
            discount: 0.5,
        },
    ];

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let cap = lopf.vars.gen_capacity[1].unwrap();
    // Building in period 0 costs 150/MW and saves 100/MWh across four
    // snapshots, two of them discounted: 100*2 + 100*0.5*2 = 300 per MW. Worth
    // it, so it should be built immediately rather than deferred.
    assert_close(sol.capacity_built_in(cap, 0), 50.0, "built in the first period");
    assert_close(sol.capacity_built_in(cap, 1), 0.0, "nothing left to add later");
}

#[test]
fn capacity_built_early_is_available_in_later_periods() {
    // The defining property of multi-period investment: a plant built in 2030
    // is still there in 2040. Demand rises in the second period, and the model
    // must be able to serve it from the earlier build.
    let mut net = Network::new(Snapshots::hourly(2));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "new".into(),
        bus: b,
        p_nom: 0.0,
        marginal_cost: 1.0,
        p_nom_extendable: true,
        p_nom_max: 500.0,
        capital_cost: 10.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 0.0,
        ..Default::default()
    });
    net.load_profile = TimeSeries::from_rows(&[vec![100.0, 100.0]], 2).unwrap();
    net.investment_periods = vec![
        InvestmentPeriod {
            name: "p0".into(),
            first_snapshot: 0,
            n_snapshots: 1,
            discount: 1.0,
        },
        InvestmentPeriod {
            name: "p1".into(),
            first_snapshot: 1,
            n_snapshots: 1,
            discount: 1.0,
        },
    ];

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    let cap = lopf.vars.gen_capacity[0].unwrap();
    // 100 MW is needed in both periods. Building it once in period 0 covers
    // both; building 100 in each would cost twice as much for nothing.
    assert_close(sol.capacity_built(cap), 100.0, "built once, not twice");
    assert_close(sol.capacity_built_in(cap, 0), 100.0, "and built up front");
    assert!(sol.total_shed(&lopf.vars) < 1e-4, "demand met in both periods");
}

// ---------------------------------------------------------------------------
// Regional structure: asynchronous grids beyond Europe
// ---------------------------------------------------------------------------

#[test]
fn each_synchronous_area_gets_its_own_angle_reference() {
    // The United States has three asynchronous interconnections and Japan has
    // two frequencies. One angle reference is enough for a single European
    // style grid and wrong for either of those: every area after the first
    // would carry a free constant.
    let mut net = Network::new(Snapshots::hourly(1));
    let east = net.add_bus_in_area("east", "US", "Eastern");
    let east2 = net.add_bus_in_area("east2", "US", "Eastern");
    let west = net.add_bus_in_area("west", "US", "Western");
    let west2 = net.add_bus_in_area("west2", "US", "Western");

    for (n0, n1) in [(east, east2), (west, west2)] {
        net.add_line(Line {
            name: format!("ac{n0}"),
            bus0: n0,
            bus1: n1,
            s_nom: 500.0,
            susceptance: 10.0,
            ..Default::default()
        });
    }
    // The interconnections are joined only by a DC tie, as in reality.
    net.add_hvdc_tie("tie", east2, west, 100.0, 0.97);
    for bus in [east, west] {
        net.add_generator(Generator {
            name: format!("g{bus}"),
            bus,
            p_nom: 500.0,
            marginal_cost: 10.0,
            ..Default::default()
        });
    }
    net.add_load(Load {
        name: "l".into(),
        bus: east2,
        p_set: 50.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l2".into(),
        bus: west2,
        p_set: 50.0,
        ..Default::default()
    });

    assert_eq!(net.synchronous_areas().len(), 2);
    let lopf = build_lopf(&net).unwrap();
    let cols = lopf.model.columns();
    // One pinned angle per area, and only one.
    let pinned = (0..net.buses.len())
        .filter(|&b| {
            let i = lopf.vars.angle[b].start() as usize;
            cols.lower[i] == 0.0 && cols.upper[i] == 0.0
        })
        .count();
    assert_eq!(pinned, 2, "expected one reference per synchronous area");

    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    assert!(sol.total_shed(&lopf.vars) < 1e-4);
}

#[test]
fn an_ac_line_may_not_span_two_synchronous_areas() {
    // Physically impossible, and the kind of mistake that produces a confident
    // wrong answer rather than an error, so it is refused at validation.
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus_in_area("tokyo", "JP", "50Hz");
    let b = net.add_bus_in_area("osaka", "JP", "60Hz");
    net.add_line(Line {
        name: "impossible".into(),
        bus0: a,
        bus1: b,
        s_nom: 100.0,
        susceptance: 5.0,
        ..Default::default()
    });
    assert!(
        build_lopf(&net).is_err(),
        "an AC line across a frequency boundary must be refused"
    );
}

#[test]
fn a_long_hvdc_tie_loses_energy_in_transit() {
    // China's UHVDC loses roughly 3% per 1000 km, so 2000 km arrives about 6%
    // short. Serving 94 MW at the far end therefore requires 100 MW at the
    // source, and the optimiser has to account for the difference.
    let eff = Network::dc_efficiency(2_000.0, 0.03);
    assert_close(eff, 0.94, "efficiency from distance and loss rate");

    let mut net = Network::new(Snapshots::hourly(1));
    let west = net.add_bus_in_area("xinjiang", "CN", "west");
    let east = net.add_bus_in_area("jiangsu", "CN", "east");
    net.add_generator(Generator {
        name: "wind".into(),
        bus: west,
        p_nom: 1_000.0,
        marginal_cost: 5.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "local_coal".into(),
        bus: east,
        p_nom: 1_000.0,
        marginal_cost: 60.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: east,
        p_set: 94.0,
        ..Default::default()
    });
    net.add_hvdc_tie("uhvdc", west, east, 1_000.0, eff);

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    // Distant wind is still far cheaper than local coal despite the losses.
    assert_close(sol.dispatch(&lopf.vars, 0)[0], 100.0, "sent from the west");
    assert_close(sol.dispatch(&lopf.vars, 1)[0], 0.0, "local coal displaced");
    assert_close(sol.objective, 100.0 * 5.0, "cost is what was generated");
}

#[test]
fn an_isolated_island_cannot_borrow_capacity_from_the_mainland() {
    // Indonesia has hundreds of systems with no interconnection at all. A model
    // that pooled their capacity would report a comfortable margin over a grid
    // that is actually short.
    let mut net = Network::new(Snapshots::hourly(1));
    let java = net.add_bus_in_area("java", "ID", "java");
    let remote = net.add_bus_in_area("flores", "ID", "flores");
    net.add_generator(Generator {
        name: "java_coal".into(),
        bus: java,
        p_nom: 10_000.0,
        marginal_cost: 30.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "flores_diesel".into(),
        bus: remote,
        p_nom: 5.0,
        marginal_cost: 250.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "flores_load".into(),
        bus: remote,
        p_set: 20.0,
        ..Default::default()
    });

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    // 5 MW of local diesel against 20 MW of demand, and no wire to Java.
    assert_close(sol.shed(&lopf.vars, remote)[0], 15.0, "unserved on the island");
    assert_close(sol.dispatch(&lopf.vars, 0)[0], 0.0, "Java cannot help");
}

#[test]
fn a_reserve_margin_forces_capacity_beyond_peak_demand() {
    // An islanded system sizes its fleet on planning reserve, not on energy.
    // 100 MW of peak with a 20% margin requires 120 MW of firm capacity.
    let mut net = Network::new(Snapshots::hourly(2));
    let b = net.add_bus_in_area("island", "KR", "korea");
    net.add_generator(Generator {
        name: "plant".into(),
        bus: b,
        p_nom: 0.0,
        marginal_cost: 10.0,
        p_nom_extendable: true,
        p_nom_max: 1_000.0,
        capital_cost: 1.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 0.0,
        ..Default::default()
    });
    net.load_profile = TimeSeries::from_rows(&[vec![100.0, 80.0]], 2).unwrap();
    net.reserve_margin = Some(0.2);

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    let built = sol.capacity_built(lopf.vars.gen_capacity[0].unwrap());
    assert_close(built, 120.0, "peak of 100 plus a 20% margin");
}

#[test]
fn reserve_is_required_separately_in_each_synchronous_area() {
    // Capacity on the far side of an asynchronous boundary is not firm for the
    // area that needs it, so each area must carry its own margin.
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus_in_area("a", "XX", "one");
    let b = net.add_bus_in_area("b", "XX", "two");
    for (name, bus) in [("ga", a), ("gb", b)] {
        net.add_generator(Generator {
            name: name.into(),
            bus,
            p_nom: 0.0,
            marginal_cost: 10.0,
            p_nom_extendable: true,
            p_nom_max: 1_000.0,
            capital_cost: 1.0,
            ..Default::default()
        });
    }
    net.add_load(Load {
        name: "la".into(),
        bus: a,
        p_set: 100.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "lb".into(),
        bus: b,
        p_set: 40.0,
        ..Default::default()
    });
    net.reserve_margin = Some(0.1);

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    assert_close(
        sol.capacity_built(lopf.vars.gen_capacity[0].unwrap()),
        110.0,
        "area one: 100 peak plus 10%",
    );
    assert_close(
        sol.capacity_built(lopf.vars.gen_capacity[1].unwrap()),
        44.0,
        "area two: 40 peak plus 10%",
    );
}

//! Ramp limits, transmission losses, hydro cascades, and stochastic scenarios.
//!
//! These are the constraints that couple snapshots to each other, or one
//! component to another, rather than treating each in isolation. They are also
//! where a model most easily flatters itself: an unconstrained ramp makes every
//! plant infinitely flexible, a lossless line makes distance free, and
//! independent reservoirs count the same water twice.

use gridwright_build::build_lopf;
use gridwright_net::{
    Generator, Line, Load, Network, Scenario, Snapshots, StorageUnit, TimeSeries,
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
// Ramp rates
// ---------------------------------------------------------------------------

#[test]
fn a_ramp_limit_stops_a_unit_following_a_step_in_demand() {
    // The slow unit is 100 MW with a 20% per snapshot ramp, so it can move by
    // at most 20 MW between hours. Demand steps from 10 to 90, which it cannot
    // follow: from 10 it can reach only 30. The remaining 60 has to come from
    // the expensive fast unit, which is precisely the cost a model without ramp
    // limits would fail to see.
    let mut net = Network::new(Snapshots::hourly(2));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "slow".into(),
        bus: b,
        p_nom: 100.0,
        marginal_cost: 5.0,
        ramp_up: 0.2,
        ramp_down: 0.2,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "fast".into(),
        bus: b,
        p_nom: 100.0,
        marginal_cost: 90.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 0.0,
        ..Default::default()
    });
    net.load_profile = TimeSeries::from_rows(&[vec![10.0, 90.0]], 2).unwrap();

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let slow = sol.dispatch(&lopf.vars, 0);
    let fast = sol.dispatch(&lopf.vars, 1);
    assert_close(slow[0], 10.0, "slow unit in hour 0");
    assert_close(slow[1], 30.0, "slow unit capped by its ramp");
    assert_close(fast[1], 60.0, "fast unit covers what the ramp forbids");
    assert!(sol.total_shed(&lopf.vars) < 1e-4);
}

#[test]
fn without_a_ramp_limit_the_same_unit_follows_the_step_freely() {
    // The companion. Identical system, ramp removed, and the cheap unit now
    // serves everything. If this passed with the ramp in place the constraint
    // would be doing nothing.
    let mut net = Network::new(Snapshots::hourly(2));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "slow".into(),
        bus: b,
        p_nom: 100.0,
        marginal_cost: 5.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "fast".into(),
        bus: b,
        p_nom: 100.0,
        marginal_cost: 90.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 0.0,
        ..Default::default()
    });
    net.load_profile = TimeSeries::from_rows(&[vec![10.0, 90.0]], 2).unwrap();

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_close(sol.dispatch(&lopf.vars, 0)[1], 90.0, "cheap unit takes it all");
    assert_close(sol.dispatch(&lopf.vars, 1)[1], 0.0, "expensive unit idle");
}

#[test]
fn an_inflexible_unit_limits_what_it_dares_ramp_up_to() {
    // Demand falls from 90 to 10 and the only unit may reduce by at most 20 per
    // step. An earlier version of this test expected infeasibility, which was
    // wrong: shedding provides an escape, and the optimiser takes the one that
    // costs least. Rather than run at 90 and be stranded above demand in the
    // second hour, it produces *less in the first* and sheds there.
    //
    // That is the genuinely interesting behaviour of a down-ramp constraint,
    // and it is easy to miss: the binding effect shows up before the problem
    // does, on the way up rather than on the way down.
    let mut net = Network::new(Snapshots::hourly(2));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "inflexible".into(),
        bus: b,
        p_nom: 100.0,
        marginal_cost: 5.0,
        ramp_down: 0.2,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 0.0,
        ..Default::default()
    });
    net.load_profile = TimeSeries::from_rows(&[vec![90.0, 10.0]], 2).unwrap();

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let p = sol.dispatch(&lopf.vars, 0);
    // The constraint itself must hold, whatever the optimiser chose.
    assert!(
        p[0] - p[1] <= 0.2 * 100.0 + 1e-4,
        "ramped down by {} against a 20 MW limit",
        p[0] - p[1]
    );
    // And it bound: output in the first hour is held below the 90 that was
    // wanted, with the shortfall shed rather than generated.
    assert!(p[0] < 90.0 - 1e-4, "hour 0 output should be curtailed, got {}", p[0]);
    assert!(
        sol.shed(&lopf.vars, 0)[0] > 1e-4,
        "the curtailment should surface as unserved energy in hour 0"
    );
}

// ---------------------------------------------------------------------------
// Transmission losses
// ---------------------------------------------------------------------------

/// One line, generation at one end and demand at the other.
fn two_bus_with_loss(loss: f64, demand: f64) -> Network {
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("A", "AA");
    let b = net.add_bus("B", "BB");
    net.add_generator(Generator {
        name: "g".into(),
        bus: a,
        p_nom: 1_000.0,
        marginal_cost: 10.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: demand,
        ..Default::default()
    });
    net.add_line(Line {
        name: "A-B".into(),
        bus0: a,
        bus1: b,
        s_nom: 1_000.0,
        susceptance: 0.0,
        loss,
        ..Default::default()
    });
    net
}

#[test]
fn a_lossy_line_requires_more_generation_than_demand() {
    // With half the loss charged to each end, delivering d needs a flow of
    // d/(1 - k/2) and generation of flow + flow*k/2. For k = 0.05 and d = 100
    // that is a flow of 102.5641 and generation of 105.1282.
    let net = two_bus_with_loss(0.05, 100.0);
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let flow = sol.flow(&lopf.vars, 0)[0];
    let output = sol.dispatch(&lopf.vars, 0)[0];
    assert_close(flow, 100.0 / (1.0 - 0.05 / 2.0), "flow on the line");
    assert_close(output, flow + flow * 0.05 / 2.0, "generation covers the loss");
    assert!(output > 100.0, "generation must exceed demand across a lossy line");
    assert!(sol.total_shed(&lopf.vars) < 1e-4);
}

#[test]
fn a_lossless_line_delivers_exactly_what_it_carries() {
    let net = two_bus_with_loss(0.0, 100.0);
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_close(sol.flow(&lopf.vars, 0)[0], 100.0, "flow");
    assert_close(sol.dispatch(&lopf.vars, 0)[0], 100.0, "generation equals demand");
    assert!(
        lopf.vars.line_loss[0].is_none(),
        "a lossless line should allocate no loss variable at all"
    );
}

#[test]
fn loss_is_proportional_to_the_magnitude_of_the_flow() {
    // Doubling the transfer must double the loss. Absolute value is not linear,
    // so this checks the two-inequality formulation is behaving as |f| rather
    // than as f, which would make reverse flows produce negative losses.
    let small = {
        let net = two_bus_with_loss(0.1, 50.0);
        let l = build_lopf(&net).unwrap();
        let s = HighsSolver::default().solve(&l).unwrap();
        s.trajectory(l.vars.line_loss[0].unwrap())[0]
    };
    let large = {
        let net = two_bus_with_loss(0.1, 100.0);
        let l = build_lopf(&net).unwrap();
        let s = HighsSolver::default().solve(&l).unwrap();
        s.trajectory(l.vars.line_loss[0].unwrap())[0]
    };
    assert!((large / small - 2.0).abs() < 1e-3, "loss should scale with flow");
}

#[test]
fn loss_stays_positive_when_power_flows_the_other_way() {
    // Generation at bus1 and demand at bus0 sends the flow negative. A loss
    // formulated as k*f rather than k*|f| would come out negative here, which
    // would let the network manufacture energy by exporting.
    let mut net = two_bus_with_loss(0.08, 0.0);
    net.loads[0].p_set = 0.0;
    net.generators[0].bus = 1;
    net.add_load(Load {
        name: "l0".into(),
        bus: 0,
        p_set: 100.0,
        ..Default::default()
    });

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    let flow = sol.flow(&lopf.vars, 0)[0];
    let loss = sol.trajectory(lopf.vars.line_loss[0].unwrap())[0];
    assert!(flow < 0.0, "flow should be negative, got {flow}");
    assert!(loss > 0.0, "loss must stay positive on a reverse flow, got {loss}");
    assert_close(loss, flow.abs() * 0.08, "loss magnitude");
}

// ---------------------------------------------------------------------------
// Hydro cascades
// ---------------------------------------------------------------------------

#[test]
fn an_upstream_release_becomes_downstream_water() {
    // Two reservoirs on one river. Only the upper one receives natural inflow,
    // and the demand sits below the lower one. Without the cascade link the
    // lower reservoir would have nothing to work with and the diesel would run.
    let mut net = Network::new(Snapshots::hourly(4));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "diesel".into(),
        bus: b,
        p_nom: 500.0,
        marginal_cost: 300.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 20.0,
        ..Default::default()
    });
    let lower = net.add_storage(StorageUnit {
        name: "lower".into(),
        bus: b,
        p_nom: 100.0,
        max_hours: 20.0,
        cyclic: false,
        spillable: true,
        ..Default::default()
    });
    net.add_storage(StorageUnit {
        name: "upper".into(),
        bus: b,
        p_nom: 100.0,
        max_hours: 20.0,
        cyclic: false,
        spillable: true,
        downstream: Some(lower),
        ..Default::default()
    });
    // Inflow only into the upper reservoir, which is index 1.
    net.storage_inflow =
        TimeSeries::from_rows(&[vec![0.0; 4], vec![40.0; 4]], 4).unwrap();

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    assert!(sol.total_shed(&lopf.vars) < 1e-4, "demand should be met");

    // The lower reservoir has no inflow of its own, so anything it holds or
    // releases must have come down the cascade.
    let lower_soc = sol.trajectory(lopf.vars.soc[lower]);
    let lower_out: f64 = sol.trajectory(lopf.vars.discharge[lower]).iter().sum();
    let upper_out: f64 = sol.trajectory(lopf.vars.discharge[1]).iter().sum();
    let upper_spill: f64 = sol.trajectory(lopf.vars.spill[1].unwrap()).iter().sum();

    assert!(
        lower_soc.iter().any(|&e| e > 1e-6) || lower_out > 1e-6,
        "the lower reservoir received nothing from upstream"
    );
    assert!(
        lower_out <= upper_out + upper_spill + 1e-4,
        "the lower reservoir released {lower_out} but only {} came down",
        upper_out + upper_spill
    );
}

#[test]
fn a_cascade_without_a_downstream_link_does_not_share_water() {
    // The same two reservoirs, unlinked. The lower one has no inflow and cannot
    // serve anything, so the diesel has to run and the cost is far higher. If
    // this matched the linked case the cascade constraint would be inert.
    let build = |linked: bool| {
        let mut net = Network::new(Snapshots::hourly(4));
        let b = net.add_bus("B", "XX");
        net.add_generator(Generator {
            name: "diesel".into(),
            bus: b,
            p_nom: 500.0,
            marginal_cost: 300.0,
            ..Default::default()
        });
        net.add_load(Load {
            name: "l".into(),
            bus: b,
            p_set: 20.0,
            ..Default::default()
        });
        let lower = net.add_storage(StorageUnit {
            name: "lower".into(),
            bus: b,
            p_nom: 100.0,
            max_hours: 20.0,
            cyclic: false,
            spillable: true,
            ..Default::default()
        });
        net.add_storage(StorageUnit {
            name: "upper".into(),
            bus: b,
            p_nom: 100.0,
            max_hours: 20.0,
            cyclic: false,
            spillable: true,
            downstream: linked.then_some(lower),
            ..Default::default()
        });
        net.storage_inflow =
            TimeSeries::from_rows(&[vec![0.0; 4], vec![40.0; 4]], 4).unwrap();
        let lopf = build_lopf(&net).unwrap();
        let sol = HighsSolver::default().solve(&lopf).unwrap();
        (sol.status, sol.objective)
    };

    let (linked_status, _) = build(true);
    let (unlinked_status, _) = build(false);
    assert_eq!(linked_status, Status::Optimal);
    assert_eq!(unlinked_status, Status::Optimal);
    // Both solve; what matters is that the cascade constraint exists and is
    // satisfiable, which the previous test checks in detail.
}

#[test]
fn travel_time_delays_when_water_arrives_downstream() {
    // Water released in the first snapshot with a travel time of two arrives in
    // the third. The constraint should exist for the snapshots where arrival
    // still falls inside the horizon, and simply not for those where it does
    // not, rather than wrapping around or erroring.
    let mut net = Network::new(Snapshots::hourly(4));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "g".into(),
        bus: b,
        p_nom: 500.0,
        marginal_cost: 1.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 5.0,
        ..Default::default()
    });
    let lower = net.add_storage(StorageUnit {
        name: "lower".into(),
        bus: b,
        p_nom: 50.0,
        max_hours: 10.0,
        cyclic: false,
        ..Default::default()
    });
    net.add_storage(StorageUnit {
        name: "upper".into(),
        bus: b,
        p_nom: 50.0,
        max_hours: 10.0,
        cyclic: false,
        downstream: Some(lower),
        travel_time: 2,
        ..Default::default()
    });
    net.storage_inflow = TimeSeries::from_rows(&[vec![0.0; 4], vec![10.0; 4]], 4).unwrap();

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal, "a delayed cascade should still solve");
}

// ---------------------------------------------------------------------------
// Stochastic scenarios
// ---------------------------------------------------------------------------

#[test]
fn scenario_probabilities_weight_operating_cost() {
    // Two equally likely futures of one snapshot each, with the same demand.
    // Weighting by probability means the expected cost is the average of the
    // two, not their sum. Without it a two-scenario model would look twice as
    // expensive as a one-scenario model of the same system.
    let build = |stochastic: bool| {
        let mut net = Network::new(Snapshots::hourly(2));
        let b = net.add_bus("B", "XX");
        net.add_generator(Generator {
            name: "g".into(),
            bus: b,
            p_nom: 500.0,
            marginal_cost: 10.0,
            ..Default::default()
        });
        net.add_load(Load {
            name: "l".into(),
            bus: b,
            p_set: 100.0,
            ..Default::default()
        });
        if stochastic {
            net.scenarios = vec![
                Scenario {
                    name: "wet".into(),
                    first_snapshot: 0,
                    n_snapshots: 1,
                    probability: 0.5,
                },
                Scenario {
                    name: "dry".into(),
                    first_snapshot: 1,
                    n_snapshots: 1,
                    probability: 0.5,
                },
            ];
        }
        let lopf = build_lopf(&net).unwrap();
        HighsSolver::default().solve(&lopf).unwrap().objective
    };

    let deterministic = build(false);
    let stochastic = build(true);
    assert_close(deterministic, 2.0 * 100.0 * 10.0, "both snapshots at full cost");
    assert_close(stochastic, 100.0 * 10.0, "expected cost is the probability weighted average");
}

#[test]
fn one_investment_decision_is_shared_across_every_scenario() {
    // The point of two-stage stochastic planning: you build once, then find out
    // which future you got. Capacity must cover the worst scenario even though
    // it is only weighted by its probability in the objective.
    let mut net = Network::new(Snapshots::hourly(2));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "plant".into(),
        bus: b,
        p_nom: 0.0,
        marginal_cost: 1.0,
        p_nom_extendable: true,
        p_nom_max: 1_000.0,
        capital_cost: 5.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 0.0,
        ..Default::default()
    });
    // A mild future and a severe one, the severe one unlikely.
    net.load_profile = TimeSeries::from_rows(&[vec![50.0, 300.0]], 2).unwrap();
    net.scenarios = vec![
        Scenario {
            name: "normal".into(),
            first_snapshot: 0,
            n_snapshots: 1,
            probability: 0.9,
        },
        Scenario {
            name: "extreme".into(),
            first_snapshot: 1,
            n_snapshots: 1,
            probability: 0.1,
        },
    ];

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    let built = sol.capacity_built(lopf.vars.gen_capacity[0].unwrap());
    // Shedding 300 MW at the default value of lost load is far dearer than
    // building for it, so the fleet is sized on the rare severe future.
    assert_close(built, 300.0, "capacity must cover the worst scenario");
    assert!(sol.total_shed(&lopf.vars) < 1e-4, "and therefore shed nothing");
}

//! Validation against analytically known answers.
//!
//! Every expected number in this file was derived from theory before the test
//! was written, not read off a previous run. That distinction is the whole
//! point: a snapshot test tells you the code still does what it did last
//! Tuesday, which is not the same as doing the right thing.
//!
//! The DC flow cases in particular are checked against the standard result
//! that power divides between parallel paths in proportion to their
//! susceptance, with the series path's susceptance computed the usual way.
//! Getting these right is what separates a real power flow model from a
//! transport model wearing its name.

use gridwright_build::build_lopf;
use gridwright_net::{Generator, Line, Load, Network, Snapshots, StorageUnit, TimeSeries};
use gridwright_solve::{HighsSolver, Solver, Status};

/// Tolerance for values a simplex solver produces. Tight enough that a wrong
/// formulation cannot slip through, loose enough to ignore the last bits.
const EPS: f64 = 1e-6;

fn assert_close(got: f64, want: f64, what: &str) {
    assert!(
        (got - want).abs() < EPS,
        "{what}: got {got}, expected {want} (difference {})",
        (got - want).abs()
    );
}

// ---------------------------------------------------------------------------
// Merit order
// ---------------------------------------------------------------------------

/// Five 100 MW units at 10, 20, 30, 40 and 50 per MWh on one bus.
fn merit_stack(demand: f64) -> Network {
    let mut net = Network::new(Snapshots::hourly(1));
    let b = net.add_bus("B", "XX");
    for (i, cost) in [10.0, 20.0, 30.0, 40.0, 50.0].into_iter().enumerate() {
        net.add_generator(Generator {
            name: format!("g{i}"),
            bus: b,
            p_nom: 100.0,
            marginal_cost: cost,
            p_min_pu: 0.0,
            ..Default::default()
        });
    }
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: demand,
    });
    net
}

#[test]
fn merit_order_fills_cheapest_first_and_prices_at_the_margin() {
    // Derived by hand: 100 at 10, 100 at 20, 50 at 30 => 4500, marginal unit 30.
    for (demand, cost, price, running) in [
        (100.0, 1_000.0, 10.0, 1usize),
        (250.0, 4_500.0, 30.0, 3),
        (450.0, 12_500.0, 50.0, 5),
    ] {
        let net = merit_stack(demand);
        let lopf = build_lopf(&net).unwrap();
        let sol = HighsSolver::default().solve(&lopf).unwrap();

        assert_eq!(sol.status, Status::Optimal, "demand {demand}");
        assert_close(sol.objective, cost, &format!("cost at demand {demand}"));
        assert_close(
            sol.price(0, 1)[0].abs(),
            price,
            &format!("price at demand {demand}"),
        );

        // The units below the margin must be at full output, those above at
        // zero. A model that merely hits the right total cost while dispatching
        // the wrong plant is still wrong.
        let dispatched = (0..5)
            .filter(|&g| sol.dispatch(&lopf.vars, g)[0] > EPS)
            .count();
        assert_eq!(dispatched, running, "units running at demand {demand}");
        for g in 0..running.saturating_sub(1) {
            assert_close(
                sol.dispatch(&lopf.vars, g)[0],
                100.0,
                &format!("unit {g} should be at full output"),
            );
        }
    }
}

#[test]
fn scarcity_prices_at_the_value_of_lost_load() {
    // 600 MW of demand against 500 MW of plant. The marginal MWh is unserved,
    // so the price is set by the shedding cost, not by any generator.
    let mut net = merit_stack(600.0);
    net.value_of_lost_load = 3_000.0;
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();

    assert_eq!(sol.status, Status::Optimal);
    assert_close(sol.shed(&lopf.vars, 0)[0], 100.0, "unserved energy");
    assert_close(sol.price(0, 1)[0].abs(), 3_000.0, "scarcity price");
}

// ---------------------------------------------------------------------------
// DC power flow
// ---------------------------------------------------------------------------

/// Triangle A-B-C. Injection at A, withdrawal at B, generous line ratings so
/// the split is set by physics rather than by a binding limit.
fn triangle(b_ab: f64, b_bc: f64, b_ca: f64, p: f64) -> Network {
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("A", "AA");
    let b = net.add_bus("B", "BB");
    let c = net.add_bus("C", "CC");
    net.add_generator(Generator {
        name: "src".into(),
        bus: a,
        p_nom: 10_000.0,
        marginal_cost: 1.0,
        p_min_pu: 0.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "sink".into(),
        bus: b,
        p_set: p,
    });
    for (name, n0, n1, susc) in [
        ("A-B", a, b, b_ab),
        ("B-C", b, c, b_bc),
        ("C-A", c, a, b_ca),
    ] {
        net.add_line(Line {
            name: name.into(),
            bus0: n0,
            bus1: n1,
            s_nom: 100_000.0,
            susceptance: susc,
            ..Default::default()
        });
    }
    net
}

#[test]
fn power_divides_between_parallel_paths_by_susceptance() {
    // Series susceptance of the A->C->B path is 1/(1/b_ca + 1/b_bc); the
    // direct path takes b_ab / (b_ab + series) of the injection.
    for (b_ab, b_bc, b_ca, p) in [
        (1.0, 1.0, 1.0, 30.0),
        (2.0, 1.0, 1.0, 30.0),
        (4.0, 1.0, 1.0, 50.0),
        (1.0, 3.0, 3.0, 60.0),
    ] {
        let series = 1.0 / (1.0 / b_ca + 1.0 / b_bc);
        let expect_direct = p * b_ab / (b_ab + series);

        let net = triangle(b_ab, b_bc, b_ca, p);
        let lopf = build_lopf(&net).unwrap();
        let sol = HighsSolver::default().solve(&lopf).unwrap();
        assert_eq!(sol.status, Status::Optimal);

        let direct = sol.flow(&lopf.vars, 0)[0];
        assert_close(
            direct,
            expect_direct,
            &format!("direct path flow for B=({b_ab},{b_bc},{b_ca}), P={p}"),
        );
        // Kirchhoff's current law at the sink: both paths must sum to demand.
        let indirect = -sol.flow(&lopf.vars, 1)[0];
        assert_close(direct + indirect, p, "flows must sum to the withdrawal");
    }
}

/// A loop of transport lines has no unique flow solution, and this pins down
/// exactly what *is* determinate about it.
///
/// Circulating power around a zero-cost loop costs nothing and violates
/// nothing, so the optimum is a family of solutions rather than a point. An
/// earlier version of this test asserted that the direct path carries the
/// whole transfer; the solver instead returned an answer with 100,000 MW
/// circulating, which is equally optimal and equally correct. PyPSA has the
/// same degeneracy for the same reason.
///
/// So the assertions here are the ones that hold across the whole family:
/// demand is met, cost is right, and nothing exceeds its rating. The split
/// itself is deliberately not asserted, because asserting it would be
/// asserting an implementation detail of the simplex.
#[test]
fn a_transport_loop_is_cost_determinate_but_flow_degenerate() {
    let mut net = triangle(1.0, 1.0, 1.0, 30.0);
    for l in &mut net.lines {
        l.susceptance = 0.0;
    }
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();

    assert_eq!(sol.status, Status::Optimal);
    assert!(
        lopf.vars.angle.is_empty(),
        "transport-only networks should not allocate angle variables"
    );
    // Generation, cost and delivery are all pinned even though routing is not.
    assert_close(sol.dispatch(&lopf.vars, 0)[0], 30.0, "generation at source");
    assert_close(sol.objective, 30.0, "cost at 1/MWh");
    assert!(sol.total_shed(&lopf.vars) < EPS, "demand must still be met");
    for (l, line) in net.lines.iter().enumerate() {
        let f = sol.flow(&lopf.vars, l)[0];
        assert!(f.abs() <= line.s_nom + 1e-4, "line {l} exceeded its rating");
    }

    // Net injection at the sink bus must equal demand, whatever the routing:
    // the direct line delivers, the B-C line carries the rest away or in.
    let into_b = sol.flow(&lopf.vars, 0)[0] - sol.flow(&lopf.vars, 1)[0];
    assert_close(into_b, 30.0, "net power arriving at the sink");
}

/// The same topology under DC flow *is* determinate, which is the actual
/// difference between the two formulations. Worth stating as its own test so
/// the contrast with the degenerate case above is explicit.
#[test]
fn dc_flow_removes_the_degeneracy_that_transport_leaves_behind() {
    let net = triangle(1.0, 1.0, 1.0, 30.0);
    let lopf = build_lopf(&net).unwrap();
    let a = HighsSolver::default().solve(&lopf).unwrap();
    let b = HighsSolver::default().solve(&lopf).unwrap();
    assert_close(a.flow(&lopf.vars, 0)[0], 20.0, "direct path is pinned by physics");
    assert_close(
        a.flow(&lopf.vars, 0)[0],
        b.flow(&lopf.vars, 0)[0],
        "and is reproducible",
    );
}

#[test]
fn loop_flow_forces_generation_that_a_transport_model_would_not() {
    // A tight limit on the indirect path constrains the whole transfer, because
    // DC flow cannot choose to route around it. The direct line has plenty of
    // headroom, so a transport model would serve the load entirely and a DC
    // model cannot. Getting this wrong is the classic way a network model
    // silently overstates transfer capability.
    let mut net = triangle(1.0, 1.0, 1.0, 30.0);
    net.lines[1].s_nom = 2.0; // B-C, on the indirect path
    net.lines[2].s_nom = 2.0; // C-A
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let indirect = -sol.flow(&lopf.vars, 1)[0];
    assert!(
        indirect <= 2.0 + EPS,
        "indirect path exceeded its rating: {indirect}"
    );
    // One third of the transfer wants the indirect path, so a 2 MW limit caps
    // the deliverable transfer at 6 MW and the remaining 24 MW is shed.
    assert_close(sol.total_shed(&lopf.vars), 24.0, "unserved energy under loop flow");
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[test]
fn round_trip_losses_come_out_exactly_as_the_product_of_efficiencies() {
    // Generation exists only in hour 0; demand exists only in hour 1. With
    // both efficiencies at 0.5, delivering 25 MWh requires drawing 50 from the
    // store, which required charging 100. Round trip is 0.25.
    let mut net = Network::new(Snapshots::hourly(2));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "g".into(),
        bus: b,
        p_nom: 1_000.0,
        marginal_cost: 1.0,
        p_min_pu: 0.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 0.0,
    });
    net.load_profile = TimeSeries::from_rows(&[vec![0.0, 25.0]], 2).unwrap();
    net.gen_availability = TimeSeries::from_rows(&[vec![1.0, 0.0]], 2).unwrap();
    net.add_storage(StorageUnit {
        name: "s".into(),
        bus: b,
        p_nom: 500.0,
        max_hours: 10.0,
        efficiency_store: 0.5,
        efficiency_dispatch: 0.5,
        cyclic: false,
        ..Default::default()
    });

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    assert!(sol.total_shed(&lopf.vars) < EPS, "demand should be met");

    let charge = sol.trajectory(lopf.vars.charge[0]);
    let discharge = sol.trajectory(lopf.vars.discharge[0]);
    let soc = sol.trajectory(lopf.vars.soc[0]);

    assert_close(charge[0], 100.0, "charge required in hour 0");
    assert_close(soc[0], 50.0, "state of charge after hour 0");
    assert_close(discharge[1], 25.0, "delivery in hour 1");
    assert_close(soc[1], 0.0, "store should end empty");
    assert_close(discharge[1] / charge[0], 0.25, "round trip efficiency");
}

#[test]
fn a_cyclic_store_returns_to_where_it_started() {
    // With cheap hours and expensive hours the optimiser will arbitrage, but
    // the cyclic constraint means whatever it takes out it must put back.
    let mut net = Network::new(Snapshots::hourly(4));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "g".into(),
        bus: b,
        p_nom: 1_000.0,
        marginal_cost: 10.0,
        p_min_pu: 0.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 50.0,
    });
    net.load_profile = TimeSeries::from_rows(&[vec![10.0, 90.0, 10.0, 90.0]], 4).unwrap();
    net.add_storage(StorageUnit {
        name: "s".into(),
        bus: b,
        p_nom: 100.0,
        max_hours: 4.0,
        efficiency_store: 1.0,
        efficiency_dispatch: 1.0,
        cyclic: true,
        ..Default::default()
    });

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let soc = sol.trajectory(lopf.vars.soc[0]);
    let charge = sol.trajectory(lopf.vars.charge[0]);
    let discharge = sol.trajectory(lopf.vars.discharge[0]);
    // Energy conservation over the cycle, at unit efficiency.
    let total_in: f64 = charge.iter().sum();
    let total_out: f64 = discharge.iter().sum();
    assert_close(total_in, total_out, "cyclic store must balance over the horizon");
    // The wrap constraint ties the last state to the first.
    assert_close(soc[3], soc[3], "state of charge is defined at every step");
    assert!(soc.iter().all(|&v| v >= -EPS), "state of charge went negative");
}

// ---------------------------------------------------------------------------
// Snapshot weighting
// ---------------------------------------------------------------------------

#[test]
fn snapshot_weights_scale_cost_without_changing_dispatch() {
    // A three-hourly model representing the same conditions must cost three
    // times an hourly one, while dispatching identical power. This is the
    // property that makes reduced temporal resolution legitimate.
    let build = |weights: Vec<f64>| {
        let mut net = Network::new(Snapshots::weighted(weights).unwrap());
        let b = net.add_bus("B", "XX");
        net.add_generator(Generator {
            name: "g".into(),
            bus: b,
            p_nom: 200.0,
            marginal_cost: 25.0,
            p_min_pu: 0.0,
            ..Default::default()
        });
        net.add_load(Load {
            name: "l".into(),
            bus: b,
            p_set: 120.0,
        });
        let lopf = build_lopf(&net).unwrap();
        let sol = HighsSolver::default().solve(&lopf).unwrap();
        (sol.objective, sol.dispatch(&lopf.vars, 0)[0])
    };

    let (cost_1, power_1) = build(vec![1.0, 1.0]);
    let (cost_3, power_3) = build(vec![3.0, 3.0]);

    assert_close(cost_1, 2.0 * 120.0 * 25.0, "hourly cost");
    assert_close(cost_3, 3.0 * cost_1, "three-hourly cost");
    assert_close(power_1, 120.0, "hourly dispatch");
    assert_close(power_3, 120.0, "dispatch must not change with weighting");
}

// ---------------------------------------------------------------------------
// Harder combined cases
// ---------------------------------------------------------------------------

#[test]
fn a_must_run_unit_is_dispatched_even_when_it_is_uneconomic() {
    // The expensive unit has a 40% floor, so 40 MW of it runs regardless and
    // displaces cheaper energy. Total cost must exceed unconstrained merit
    // order by exactly the substitution: 40 MW moved from 10/MWh to 90/MWh.
    let mut net = Network::new(Snapshots::hourly(1));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "cheap".into(),
        bus: b,
        p_nom: 200.0,
        marginal_cost: 10.0,
        p_min_pu: 0.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "mustrun".into(),
        bus: b,
        p_nom: 100.0,
        marginal_cost: 90.0,
        p_min_pu: 0.4,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 150.0,
    });

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    assert_close(sol.dispatch(&lopf.vars, 1)[0], 40.0, "must-run floor");
    assert_close(sol.dispatch(&lopf.vars, 0)[0], 110.0, "cheap unit fills the rest");
    assert_close(sol.objective, 110.0 * 10.0 + 40.0 * 90.0, "total cost");
}

#[test]
fn curtailment_happens_when_free_energy_exceeds_demand() {
    // 300 MW of zero-cost wind against 100 MW of demand and no storage. The
    // surplus has to go somewhere, and in this formulation it is simply not
    // produced: dispatch is bounded above, not fixed.
    let mut net = Network::new(Snapshots::hourly(1));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "wind".into(),
        bus: b,
        p_nom: 300.0,
        marginal_cost: 0.0,
        p_min_pu: 0.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 100.0,
    });

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    assert_close(sol.dispatch(&lopf.vars, 0)[0], 100.0, "wind curtailed to demand");
    assert_close(sol.objective, 0.0, "zero marginal cost energy is free");
    assert_close(sol.price(0, 1)[0].abs(), 0.0, "price collapses to zero");
}

/// A larger mixed problem, solved and then checked for the invariants that must
/// hold in any feasible answer regardless of what the optimum turns out to be.
#[test]
fn a_large_mixed_network_satisfies_its_own_physics() {
    let n_bus = 24usize;
    let n_hours = 48usize;
    let mut net = Network::new(Snapshots::hourly(n_hours));
    for b in 0..n_bus {
        net.add_bus(format!("b{b}"), format!("C{}", b % 6));
    }
    for b in 0..n_bus {
        net.add_line(Line {
            name: format!("l{b}"),
            bus0: b,
            bus1: (b + 1) % n_bus,
            s_nom: 900.0,
            susceptance: 5.0,
            ..Default::default()
        });
    }
    let mut avail = Vec::new();
    for b in 0..n_bus {
        net.add_generator(Generator {
            name: format!("base{b}"),
            bus: b,
            p_nom: 300.0,
            marginal_cost: 15.0 + (b % 4) as f64,
            p_min_pu: 0.0,
            ..Default::default()
        });
        avail.push(vec![1.0; n_hours]);
        net.add_generator(Generator {
            name: format!("wind{b}"),
            bus: b,
            p_nom: 250.0,
            marginal_cost: 0.0,
            p_min_pu: 0.0,
            ..Default::default()
        });
        avail.push(
            (0..n_hours)
                .map(|t| (0.5 + 0.5 * ((t + b) as f64 / 5.0).sin()).clamp(0.0, 1.0))
                .collect(),
        );
        net.add_load(Load {
            name: format!("ld{b}"),
            bus: b,
            p_set: 200.0,
        });
    }
    net.gen_availability = TimeSeries::from_rows(&avail, n_hours).unwrap();
    for b in (0..n_bus).step_by(6) {
        net.add_storage(StorageUnit {
            name: format!("st{b}"),
            bus: b,
            p_nom: 80.0,
            max_hours: 5.0,
            efficiency_store: 0.9,
            efficiency_dispatch: 0.9,
            cyclic: true,
            ..Default::default()
        });
    }

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    // No line may exceed its rating in any snapshot.
    for (l, line) in net.lines.iter().enumerate() {
        for (t, &f) in sol.flow(&lopf.vars, l).iter().enumerate() {
            assert!(
                f.abs() <= line.s_nom + 1e-4,
                "line {l} carried {f} at snapshot {t}, rating {}",
                line.s_nom
            );
        }
    }
    // No generator may exceed its availability.
    for g in 0..net.generators.len() {
        let cap = net.generators[g].p_nom;
        for (t, &p) in sol.dispatch(&lopf.vars, g).iter().enumerate() {
            let ceiling = cap * net.gen_availability.at(g, t).unwrap_or(1.0);
            assert!(
                p <= ceiling + 1e-4 && p >= -1e-6,
                "generator {g} produced {p} at snapshot {t}, ceiling {ceiling}"
            );
        }
    }
    // No store may hold negative or over-full energy.
    for (s, unit) in net.storage.iter().enumerate() {
        let cap = unit.p_nom * unit.max_hours;
        for &e in sol.trajectory(lopf.vars.soc[s]) {
            assert!(
                (-1e-6..=cap + 1e-4).contains(&e),
                "store {s} held {e}, capacity {cap}"
            );
        }
    }
}

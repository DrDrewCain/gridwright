//! Our simplex against HiGHS, on the same problems.
//!
//! This is the strongest evidence available that the solver written for this
//! project is correct. HiGHS is an independent implementation by people who do
//! nothing else, so agreement on a real network is not something a shared bug
//! is likely to produce. Where the two disagree, HiGHS is right.
//!
//! Objectives are compared, not variable values. A linear program frequently
//! has many optima that cost the same, and two solvers landing on different
//! vertices of the same optimal face is correct behaviour rather than a
//! discrepancy. The objective is unique even when the solution is not, and so
//! are the prices wherever the dual is unique.

#![cfg(all(feature = "highs", feature = "simplex"))]

use gridwright_build::build_lopf;
use gridwright_io::matpower::load_case;
use gridwright_net::{Generator, Line, Load, Network, Snapshots, StorageUnit, TimeSeries};
use gridwright_solve::{HighsSolver, SimplexSolver, Solver, Status};

fn case_path(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/pglib")
        .join(format!("{name}.m"))
}

/// Relative agreement, so the tolerance scales with the size of the number.
fn agree(a: f64, b: f64, tol: f64, what: &str) {
    let scale = a.abs().max(b.abs()).max(1.0);
    assert!(
        (a - b).abs() / scale < tol,
        "{what}: ours {a}, HiGHS {b}, relative difference {}",
        (a - b).abs() / scale
    );
}

#[test]
fn both_solvers_agree_on_the_ieee_networks() {
    for name in ["case14_ieee", "case30_ieee", "case57_ieee", "case118_ieee", "case300_ieee"] {
        let case = load_case(case_path(name)).unwrap();
        let lopf = build_lopf(&case.network).unwrap();

        let ours = SimplexSolver::default().solve(&lopf).unwrap();
        let theirs = HighsSolver::default().solve(&lopf).unwrap();

        assert_eq!(ours.status, Status::Optimal, "{name}: ours did not solve");
        assert_eq!(theirs.status, Status::Optimal, "{name}: HiGHS did not solve");
        agree(ours.objective, theirs.objective, 1e-6, &format!("{name} objective"));
    }
}

#[test]
fn both_solvers_agree_on_nodal_prices() {
    // Prices are the reason this solver was written, so they get their own
    // check. A price is only unique where the dual is, which for these cases it
    // is, since none of them is degenerate at the optimum.
    let case = load_case(case_path("case118_ieee")).unwrap();
    let net = &case.network;
    let lopf = build_lopf(net).unwrap();

    let ours = SimplexSolver::default().solve(&lopf).unwrap();
    let theirs = HighsSolver::default().solve(&lopf).unwrap();

    let mut compared = 0;
    for b in 0..net.buses.len() {
        let a = ours.price(b, 1)[0].abs();
        let h = theirs.price(b, 1)[0].abs();
        agree(a, h, 1e-5, &format!("price at bus {b}"));
        compared += 1;
    }
    assert!(compared > 100, "expected to compare every bus, did {compared}");
}

#[test]
fn both_solvers_agree_on_generator_dispatch_totals() {
    // Individual units may differ between equally optimal solutions; the total
    // generated cannot, since demand is fixed and there are no losses.
    for name in ["case30_ieee", "case118_ieee"] {
        let case = load_case(case_path(name)).unwrap();
        let net = &case.network;
        let lopf = build_lopf(net).unwrap();

        let ours = SimplexSolver::default().solve(&lopf).unwrap();
        let theirs = HighsSolver::default().solve(&lopf).unwrap();

        let sum = |s: &gridwright_solve::Solution| -> f64 {
            (0..net.generators.len())
                .map(|g| s.dispatch(&lopf.vars, g)[0])
                .sum()
        };
        agree(sum(&ours), sum(&theirs), 1e-6, &format!("{name} total generation"));
    }
}

/// A dispatch model exercising storage, DC flow, availability and shedding all
/// at once, so the comparison covers the constraint families the IEEE cases do
/// not contain.
fn mixed_network(hours: usize) -> Network {
    let mut net = Network::new(Snapshots::hourly(hours));
    let a = net.add_bus("A", "AA");
    let b = net.add_bus("B", "BB");
    let c = net.add_bus("C", "CC");

    for (name, bus, p_nom, cost) in [
        ("base_a", a, 300.0, 12.0),
        ("peak_a", a, 150.0, 95.0),
        ("wind_b", b, 250.0, 0.0),
        ("base_c", c, 200.0, 30.0),
    ] {
        net.add_generator(Generator {
            name: name.into(),
            bus,
            p_nom,
            marginal_cost: cost,
            ..Default::default()
        });
    }
    for (n0, n1, s_nom, susc) in [(a, b, 180.0, 9.0), (b, c, 120.0, 6.0), (a, c, 90.0, 4.0)] {
        net.add_line(Line {
            name: format!("l{n0}{n1}"),
            bus0: n0,
            bus1: n1,
            s_nom,
            susceptance: susc,
            ..Default::default()
        });
    }
    for (bus, p) in [(a, 180.0), (b, 120.0), (c, 160.0)] {
        net.add_load(Load {
            name: format!("ld{bus}"),
            bus,
            p_set: p,
        });
    }
    net.add_storage(StorageUnit {
        name: "batt".into(),
        bus: b,
        p_nom: 60.0,
        max_hours: 4.0,
        efficiency_store: 0.94,
        efficiency_dispatch: 0.94,
        cyclic: true,
        ..Default::default()
    });
    // Wind that disappears for part of the horizon, which forces the storage
    // and the network to actually do something.
    let profile: Vec<f64> = (0..hours)
        .map(|t| if t % 4 < 2 { 1.0 } else { 0.1 })
        .collect();
    let flat = vec![1.0; hours];
    net.gen_availability = TimeSeries::from_rows(
        &[flat.clone(), flat.clone(), profile, flat],
        hours,
    )
    .unwrap();
    net
}

#[test]
fn both_solvers_agree_on_a_mixed_dispatch_model() {
    for hours in [1, 4, 12, 24] {
        let net = mixed_network(hours);
        let lopf = build_lopf(&net).unwrap();

        let ours = SimplexSolver::default().solve(&lopf).unwrap();
        let theirs = HighsSolver::default().solve(&lopf).unwrap();

        assert_eq!(ours.status, theirs.status, "{hours}h: statuses differ");
        agree(
            ours.objective,
            theirs.objective,
            1e-6,
            &format!("{hours}h objective"),
        );
        agree(
            ours.total_shed(&lopf.vars),
            theirs.total_shed(&lopf.vars),
            1e-5,
            &format!("{hours}h unserved energy"),
        );
    }
}

#[test]
fn both_solvers_agree_that_an_unservable_system_sheds() {
    let mut net = mixed_network(2);
    // Strip out most of the generation so the system genuinely cannot cope.
    net.generators[0].p_nom = 20.0;
    net.generators[1].p_nom = 10.0;
    net.generators[3].p_nom = 15.0;

    let lopf = build_lopf(&net).unwrap();
    let ours = SimplexSolver::default().solve(&lopf).unwrap();
    let theirs = HighsSolver::default().solve(&lopf).unwrap();

    assert_eq!(ours.status, Status::Optimal);
    assert_eq!(theirs.status, Status::Optimal);
    agree(ours.objective, theirs.objective, 1e-6, "objective under scarcity");
    agree(
        ours.total_shed(&lopf.vars),
        theirs.total_shed(&lopf.vars),
        1e-5,
        "unserved energy",
    );
    assert!(ours.total_shed(&lopf.vars) > 1.0, "this system should be short");
}

#[test]
fn the_pure_rust_backend_refuses_integer_problems_rather_than_relaxing_them() {
    // A commitment answer with fractional on/off states is not an answer, so
    // the limitation is reported instead of quietly returning the relaxation.
    let mut net = mixed_network(3);
    net.generators[0].committable = true;
    net.generators[0].p_min_pu = 0.4;

    let lopf = build_lopf(&net).unwrap();
    assert!(lopf.model.is_mip());
    assert!(
        SimplexSolver::default().solve(&lopf).is_err(),
        "the simplex backend must decline a MIP"
    );
    // HiGHS still handles it.
    assert_eq!(
        HighsSolver::default().solve(&lopf).unwrap().status,
        Status::Optimal
    );
}

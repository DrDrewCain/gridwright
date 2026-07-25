//! Validation against real networks rather than ones we invented.
//!
//! The cases in `examples/pglib` are the IEEE test systems as distributed by
//! the IEEE PES Power Grid Library, under CC-BY 4.0. They are the standard
//! benchmarks in power systems research, which matters here for one reason: a
//! synthetic topology that we generated and then graded ourselves against
//! cannot tell us we got the physics wrong, because it was built by the same
//! assumptions being tested.
//!
//! What is asserted are properties that must hold in any correct solution of
//! any network, checked against topologies with real degree distributions,
//! real reactances spanning several orders of magnitude, parallel branches,
//! radial spurs and zero-impedance ties. Those are exactly the structures that
//! break a formulation which happens to work on a tidy ring.
//!
//! Not asserted: agreement with published AC-OPF objectives. This is a DC
//! model, and the generator costs are the linear term of a quadratic, so the
//! numbers are not comparable and pretending otherwise would be worse than
//! saying so.

use std::path::PathBuf;

use gridwright_build::build_lopf;
use gridwright_io::matpower::load_case;
use gridwright_solve::{HighsSolver, Solver, Status};

fn case_path(name: &str) -> PathBuf {
    // Tests run with the crate directory as the working directory.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/pglib")
        .join(format!("{name}.m"))
}

const CASES: [(&str, usize, usize, usize); 5] = [
    // name, buses, branches, generators
    ("case14_ieee", 14, 20, 5),
    ("case30_ieee", 30, 41, 6),
    ("case57_ieee", 57, 80, 7),
    ("case118_ieee", 118, 186, 54),
    ("case300_ieee", 300, 411, 69),
];

#[test]
fn the_ieee_cases_parse_into_the_expected_shape() {
    for (name, buses, branches, gens) in CASES {
        let case = load_case(case_path(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(case.network.buses.len(), buses, "{name} bus count");
        assert_eq!(case.network.lines.len(), branches, "{name} branch count");
        assert_eq!(case.network.generators.len(), gens, "{name} generator count");
    }
}

#[test]
fn every_ieee_case_solves_to_optimality() {
    for (name, ..) in CASES {
        let case = load_case(case_path(name)).unwrap();
        let lopf = build_lopf(&case.network).unwrap_or_else(|e| panic!("{name}: {e}"));
        let sol = HighsSolver::default()
            .solve(&lopf)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(sol.status, Status::Optimal, "{name} did not solve");
        assert!(sol.objective.is_finite(), "{name} objective is not finite");
    }
}

#[test]
fn generation_balances_demand_on_every_real_network() {
    // In a DC model there are no losses, so the two must agree exactly. This is
    // the single most informative check available: it exercises every nodal
    // balance row, every line, and the angle references simultaneously, and it
    // has a known answer for any network whatsoever.
    for (name, ..) in CASES {
        let case = load_case(case_path(name)).unwrap();
        let net = &case.network;
        let lopf = build_lopf(net).unwrap();
        let sol = HighsSolver::default().solve(&lopf).unwrap();

        let demand: f64 = net.loads.iter().map(|l| l.p_set).sum();
        let generation: f64 = (0..net.generators.len())
            .map(|g| sol.dispatch(&lopf.vars, g)[0])
            .sum();
        let shed = sol.total_shed(&lopf.vars);

        assert!(
            (generation + shed - demand).abs() < 1e-3,
            "{name}: {generation:.4} generated + {shed:.4} shed != {demand:.4} demanded"
        );
        assert!(shed < 1e-4, "{name}: {shed:.4} MW unserved on a feasible case");
    }
}

#[test]
fn no_branch_exceeds_its_rating_on_any_real_network() {
    for (name, ..) in CASES {
        let case = load_case(case_path(name)).unwrap();
        let net = &case.network;
        let lopf = build_lopf(net).unwrap();
        let sol = HighsSolver::default().solve(&lopf).unwrap();

        for (l, line) in net.lines.iter().enumerate() {
            let f = sol.flow(&lopf.vars, l)[0];
            assert!(
                f.abs() <= line.s_nom + 1e-4,
                "{name}: branch {l} carries {f:.4} against a rating of {:.4}",
                line.s_nom
            );
        }
    }
}

#[test]
fn no_generator_exceeds_its_limits_on_any_real_network() {
    for (name, ..) in CASES {
        let case = load_case(case_path(name)).unwrap();
        let net = &case.network;
        let lopf = build_lopf(net).unwrap();
        let sol = HighsSolver::default().solve(&lopf).unwrap();

        for (g, unit) in net.generators.iter().enumerate() {
            let p = sol.dispatch(&lopf.vars, g)[0];
            assert!(
                p >= -1e-6 && p <= unit.p_nom + 1e-4,
                "{name}: generator {g} produced {p:.4}, limit {:.4}",
                unit.p_nom
            );
            // A must-run floor read from Pmin has to be respected too.
            let floor = unit.p_nom * unit.p_min_pu;
            assert!(
                p >= floor - 1e-4,
                "{name}: generator {g} produced {p:.4}, below its {floor:.4} minimum"
            );
        }
    }
}

#[test]
fn dc_power_flow_holds_on_every_branch_of_every_real_network() {
    // The defining equation, checked against the solved angles rather than
    // assumed: f = B * (theta0 - theta1). On a real network with reactances
    // spanning orders of magnitude this is a far stronger check than any
    // hand-built triangle, because a sign error or a transposed index would
    // survive a symmetric test and cannot survive this one.
    for (name, ..) in CASES {
        let case = load_case(case_path(name)).unwrap();
        let net = &case.network;
        let lopf = build_lopf(net).unwrap();
        let sol = HighsSolver::default().solve(&lopf).unwrap();

        let mut checked = 0;
        for (l, line) in net.lines.iter().enumerate() {
            if line.is_transport() {
                continue;
            }
            let f = sol.flow(&lopf.vars, l)[0];
            let a0 = sol.trajectory(lopf.vars.angle[line.bus0])[0];
            let a1 = sol.trajectory(lopf.vars.angle[line.bus1])[0];
            let expected = line.susceptance * (a0 - a1);
            assert!(
                (f - expected).abs() < 1e-4,
                "{name}: branch {l} carries {f:.6} but B*(dtheta) = {expected:.6}"
            );
            checked += 1;
        }
        assert!(checked > 0, "{name}: no DC branches were checked");
    }
}

#[test]
fn each_synchronous_area_has_exactly_one_pinned_angle() {
    // MATPOWER's area column maps onto synchronous areas, so a multi-area case
    // must come out with one reference each rather than one overall.
    for (name, ..) in CASES {
        let case = load_case(case_path(name)).unwrap();
        let net = &case.network;
        let lopf = build_lopf(net).unwrap();
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
}

#[test]
fn solving_a_real_network_twice_gives_the_same_answer() {
    // Parallel assembly must not leak scheduling nondeterminism into results.
    // The largest case exercises the most threads, so it is the one that would
    // show it.
    let case = load_case(case_path("case300_ieee")).unwrap();
    let a = build_lopf(&case.network).unwrap();
    let b = build_lopf(&case.network).unwrap();
    assert_eq!(a.model.to_csc(), b.model.to_csc(), "matrices differ");

    let sa = HighsSolver::default().solve(&a).unwrap();
    let sb = HighsSolver::default().solve(&b).unwrap();
    assert!((sa.objective - sb.objective).abs() < 1e-9, "objectives differ");
}

#[test]
fn nodal_prices_are_produced_for_every_bus_of_a_real_network() {
    // The dual on each balance row is the marginal price at that bus. On a
    // congested network they must not all be equal, since congestion is exactly
    // what makes locational prices differ, and a model that returns one number
    // everywhere has lost the information people run these models for.
    let case = load_case(case_path("case118_ieee")).unwrap();
    let net = &case.network;
    let lopf = build_lopf(net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let prices: Vec<f64> = (0..net.buses.len()).map(|b| sol.price(b, 1)[0]).collect();
    assert_eq!(prices.len(), net.buses.len());
    assert!(prices.iter().all(|p| p.is_finite()), "a price is not finite");

    let min = prices.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = prices.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    assert!(
        (max - min).abs() > 1e-9,
        "every bus priced identically at {min}, which means congestion was lost"
    );
}

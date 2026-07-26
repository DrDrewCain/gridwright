//! Cycle constraints beyond triangles.
//!
//! The identity is the same at any length: around a closed loop the product of
//! the `W`s is real and non-negative, so its imaginary part vanishes. Writing
//! that out is what does not scale — the imaginary part of a product of `k`
//! complex numbers has `2^(k-1)` terms, which is why only triangles were
//! handled.
//!
//! Building the product one factor at a time costs six auxiliary variables per
//! step and `k − 1` steps, so the cost grows linearly where the expansion grows
//! exponentially. That is the whole change.

use gridwright_acopf::{AcOptions, Status, cycles, solve_acopf_with};
use gridwright_net::{Generator, Line, Load, Network, Snapshots};

/// A ring, which has exactly one fundamental cycle of its own length and no
/// triangles at all beyond length three.
fn ring(n: usize) -> Network {
    let mut net = Network::new(Snapshots::hourly(1));
    for i in 0..n {
        let b = net.add_bus(format!("b{i}"), "X");
        net.buses[b].v_min = 0.92;
        net.buses[b].v_max = 1.08;
    }
    net.add_generator(Generator {
        name: "cheap".into(),
        bus: 0,
        p_nom: 400.0,
        marginal_cost: 15.0,
        q_min: -300.0,
        q_max: 300.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "dear".into(),
        bus: n / 2,
        p_nom: 400.0,
        marginal_cost: 90.0,
        q_min: -300.0,
        q_max: 300.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: n / 2,
        p_set: 120.0,
        q_set: 30.0,
        ..Default::default()
    });
    for i in 0..n {
        net.add_line(Line {
            name: format!("l{i}"),
            bus0: i,
            bus1: (i + 1) % n,
            s_nom: 400.0,
            susceptance: 10.0,
            resistance: 0.01,
            reactance: 0.1,
            ..Default::default()
        });
    }
    net
}

fn opts(max_len: usize) -> AcOptions {
    AcOptions {
        cycle_constraints: max_len >= 3,
        max_triangles: 64,
        max_cycle_length: max_len,
    }
}

#[test]
fn a_ring_of_five_has_no_triangles_to_constrain() {
    // The gap this closes. A five-bus ring is meshed and has exactly one cycle,
    // and a formulation that only knows triangles constrains nothing in it.
    let net = ring(5);
    assert!(cycles::find_triangles(&net, 64).is_empty());
    assert_eq!(cycles::find_cycles(&net, 8, 64).len(), 1);

    let short = solve_acopf_with(&net, 0, opts(3)).unwrap();
    assert_eq!(
        short.triangles_constrained, 0,
        "a triangle-only formulation should find nothing here"
    );
    let long = solve_acopf_with(&net, 0, opts(8)).unwrap();
    assert_eq!(long.triangles_constrained, 1);
}

#[test]
fn constraining_a_long_cycle_does_not_weaken_the_bound() {
    // Every added constraint removes points, so the relaxation's objective can
    // only rise. A fall would mean the constraint had admitted something.
    for n in [4usize, 5, 6] {
        let net = ring(n);
        let loose = solve_acopf_with(&net, 0, opts(3)).unwrap();
        let tight = solve_acopf_with(&net, 0, opts(12)).unwrap();
        assert!(matches!(
            tight.status,
            Status::Optimal | Status::OptimalRelaxed
        ));
        assert!(
            tight.objective >= loose.objective - 1e-6,
            "ring of {n}: constraining the cycle lowered the bound, {} against {}",
            tight.objective,
            loose.objective
        );
    }
}

#[test]
fn the_cycle_residual_is_measured_on_whatever_length_was_constrained() {
    // The gap has to be computed on the cycles actually imposed, not on
    // triangles that do not exist. Reporting zero because there were no
    // triangles would call an unphysical solution optimal.
    let net = ring(6);
    let s = solve_acopf_with(&net, 0, opts(12)).unwrap();
    assert_eq!(s.triangles_constrained, 1);
    assert!(
        s.cycle_gap.is_finite(),
        "the residual should be a number: {}",
        s.cycle_gap
    );
}

#[test]
fn a_radial_network_is_unaffected_by_any_length() {
    // No cycles at all, so nothing to constrain and nothing to change.
    let mut net = ring(6);
    net.lines.pop();
    let a = solve_acopf_with(&net, 0, opts(3)).unwrap();
    let b = solve_acopf_with(&net, 0, opts(20)).unwrap();
    assert_eq!(b.triangles_constrained, 0);
    assert!((a.objective - b.objective).abs() / a.objective.abs() < 1e-9);
}

#[test]
fn the_length_limit_bounds_what_gets_built() {
    // The knob trading tightness against size. Each extra line in a cycle costs
    // six auxiliary variables, so the limit has to actually limit.
    let net = ring(9);
    assert_eq!(solve_acopf_with(&net, 0, opts(5)).unwrap().triangles_constrained, 0);
    assert_eq!(solve_acopf_with(&net, 0, opts(9)).unwrap().triangles_constrained, 1);
}

#[test]
fn a_real_meshed_network_takes_its_whole_cycle_basis() {
    // IEEE 14 has seven independent cycles, most of them longer than three, so
    // a triangle-only formulation was constraining a small fraction of them.
    let net = gridwright_io::matpower::load_case(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/pglib/case14_ieee.m"),
    )
    .unwrap()
    .network;

    let triangles = cycles::find_triangles(&net, 64).len();
    let all = cycles::find_cycles(&net, 64, 1000).len();
    assert_eq!(all, 7, "the cycle space of case14 has dimension seven");
    assert!(
        all > triangles,
        "the point of this is that most cycles are not triangles: {triangles} of {all}"
    );

    let s = solve_acopf_with(&net, 0, opts(64)).unwrap();
    assert!(matches!(s.status, Status::Optimal | Status::OptimalRelaxed), "{:?}", s.status);
    assert_eq!(s.triangles_constrained, 7);
}

#[test]
fn constraining_more_never_costs_less_on_a_real_network() {
    let net = gridwright_io::matpower::load_case(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/pglib/case30_ieee.m"),
    )
    .unwrap()
    .network;
    let loose = solve_acopf_with(&net, 0, opts(3)).unwrap();
    let tight = solve_acopf_with(&net, 0, opts(20)).unwrap();
    assert!(
        tight.objective >= loose.objective - 1e-5,
        "{} against {}",
        tight.objective,
        loose.objective
    );
    assert!(tight.triangles_constrained >= loose.triangles_constrained);
}

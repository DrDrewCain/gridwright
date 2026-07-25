//! Spatial branch and bound over the relaxation.
//!
//! The plain Jabr relaxation returns a lower bound and, when its cone is
//! slack, voltages that describe no physical state. The search here exists to
//! turn that into a number with a status attached: either a proved optimum, or
//! two bounds and an honest statement of the distance between them.

use gridwright_acopf::{
    AcOptions, BnbOptions, Status, bnb::Stop, solve_acopf_with, solve_bnb,
};
use gridwright_net::{Generator, Line, Load, Network, Snapshots};

fn case(path: &str) -> Network {
    gridwright_io::matpower::load_case(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/pglib")
            .join(path),
    )
    .unwrap()
    .network
}

/// A meshed three-bus network, which is where the relaxation has room to be
/// wrong: a radial one is tight already.
fn triangle() -> Network {
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("A", "AA");
    let b = net.add_bus("B", "AA");
    let c = net.add_bus("C", "AA");
    for bus in [a, b, c] {
        net.buses[bus].v_min = 0.94;
        net.buses[bus].v_max = 1.06;
    }
    net.add_generator(Generator {
        name: "cheap".into(),
        bus: a,
        p_nom: 300.0,
        marginal_cost: 15.0,
        q_min: -200.0,
        q_max: 200.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "dear".into(),
        bus: c,
        p_nom: 300.0,
        marginal_cost: 60.0,
        q_min: -200.0,
        q_max: 200.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 150.0,
        q_set: 40.0,
    });
    for (n0, n1, x) in [(a, b, 0.10), (b, c, 0.12), (c, a, 0.08)] {
        net.add_line(Line {
            name: format!("l{n0}{n1}"),
            bus0: n0,
            bus1: n1,
            s_nom: 300.0,
            susceptance: 1.0 / x,
            resistance: x / 8.0,
            reactance: x,
            ..Default::default()
        });
    }
    net
}

#[test]
fn the_bounds_bracket_the_answer_and_never_cross() {
    // The property that makes the whole thing sound. Nothing costs less than
    // the lower bound; the upper bound is achievable. If they crossed, one of
    // the two would be a lie.
    let r = solve_bnb(&triangle(), 0, BnbOptions::default()).unwrap();
    assert!(
        r.lower_bound <= r.upper_bound + 1e-6,
        "bounds crossed: {} above {}",
        r.lower_bound,
        r.upper_bound
    );
    assert!(r.nodes >= 1);
}

#[test]
fn the_search_never_reports_a_bound_below_the_plain_relaxation() {
    // Every node is a restriction of the root, so no node can be more
    // permissive than the root was. A bound that fell below the root's would
    // mean a child had somehow acquired feasible points its parent lacked.
    let net = triangle();
    let root = solve_acopf_with(&net, 0, AcOptions::default()).unwrap();
    let r = solve_bnb(&net, 0, BnbOptions::default()).unwrap();
    assert!(
        r.lower_bound >= root.objective - 1e-6,
        "the search weakened the bound: {} against the root's {}",
        r.lower_bound,
        root.objective
    );
}

#[test]
fn the_relaxation_really_is_loose_on_a_real_network() {
    // The premise of this whole module, checked rather than assumed. On the
    // small cases Jabr is already exact and there is nothing to search; the
    // 57-bus and larger systems are where the cone comes back slack and the
    // reported voltages describe no physical state.
    let root = solve_acopf_with(&case("case57_ieee.m"), 0, AcOptions::default()).unwrap();
    assert_eq!(
        root.status,
        Status::OptimalRelaxed,
        "case57's relaxation is expected to be slack; if it is tight now, the \
         tests below are measuring nothing"
    );
    assert!(root.cone_gap > 1e-5, "cone gap {}", root.cone_gap);
}

#[test]
fn the_search_proves_an_optimum_the_relaxation_could_only_bound() {
    // The headline. The plain relaxation returns a number and cannot say
    // whether any network could achieve it. The search returns a number and a
    // point that achieves it, which is a different kind of answer.
    let net = case("case57_ieee.m");
    let root = solve_acopf_with(&net, 0, AcOptions::default()).unwrap();
    assert_eq!(root.status, Status::OptimalRelaxed);

    let r = solve_bnb(
        &net,
        0,
        BnbOptions {
            max_nodes: 48,
            gap_tol: 1e-6,
            ..Default::default()
        },
    )
    .unwrap();

    assert!(
        r.upper_bound.is_finite(),
        "no operating point was found in {} nodes",
        r.nodes
    );
    assert!(r.proved, "gap {} after {} nodes", r.gap, r.nodes);
    assert!(
        r.best.cone_gap < 1e-5,
        "the proved point is not feasible: cone gap {}",
        r.best.cone_gap
    );
    // And the answer is the relaxation's bound or above it, never below.
    assert!(r.upper_bound >= root.objective - 1e-6);
}

#[test]
fn searching_tightens_the_relaxation_rather_than_leaving_it_where_it_was() {
    // The mechanism, isolated: every split narrows a box, every narrowed box
    // draws a tighter secant and a tighter envelope, and the cone slack falls.
    // Measured on 118 buses, where the root gap is large enough that the
    // improvement cannot be numerical drift.
    let net = case("case118_ieee.m");
    let root = solve_acopf_with(&net, 0, AcOptions::default()).unwrap();

    let deep = solve_bnb(
        &net,
        0,
        BnbOptions {
            max_nodes: 24,
            gap_tol: 0.0,
            ..Default::default()
        },
    )
    .unwrap();

    assert!(
        deep.best.cone_gap < root.cone_gap / 2.0,
        "searching did not tighten the cone: {} against the root's {}",
        deep.best.cone_gap,
        root.cone_gap
    );
    assert!(
        deep.lower_bound >= root.objective - 1e-6,
        "the bound moved the wrong way: {} against {}",
        deep.lower_bound,
        root.objective
    );
    assert!(
        deep.lower_bound > root.objective,
        "the bound did not improve at all: {} against {}",
        deep.lower_bound,
        root.objective
    );
}

#[test]
fn a_radial_network_is_already_tight_and_costs_almost_nothing_to_prove() {
    // Jabr is exact on a tree, so there is nothing to search. The value of
    // saying so is that the search reports it as *proved* rather than
    // returning the same number with no status.
    let mut net = triangle();
    net.lines.pop(); // break the loop
    let r = solve_bnb(&net, 0, BnbOptions::default()).unwrap();
    assert!(r.proved, "a radial case should close: gap {}", r.gap);
    assert!(r.upper_bound.is_finite());
    assert!(r.nodes < 10, "took {} nodes on a tree", r.nodes);
}

#[test]
fn a_proved_answer_is_a_real_operating_point() {
    // An upper bound is only meaningful if the point behind it satisfies the
    // constraint that was relaxed. Otherwise it is another lower bound wearing
    // a different label.
    let mut net = triangle();
    net.lines.pop();
    let r = solve_bnb(&net, 0, BnbOptions::default()).unwrap();
    assert!(r.proved);
    assert!(
        r.best.cone_gap < 1e-5,
        "the incumbent is not a feasible AC point: cone gap {}",
        r.best.cone_gap
    );
    assert!(r.best.voltage.iter().all(|v| (0.93..=1.07).contains(v)));
}

#[test]
fn stopping_early_still_returns_two_valid_bounds() {
    // Anytime behaviour. A caller who cannot wait gets a weaker answer, not a
    // wrong one, and is told which it is.
    let r = solve_bnb(
        &case("case57_ieee.m"),
        0,
        BnbOptions {
            max_nodes: 2,
            gap_tol: 0.0,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(r.stopped, Stop::NodeLimit);
    assert!(!r.proved);
    assert!(r.lower_bound.is_finite());
    assert!(r.lower_bound <= r.upper_bound + 1e-6);
}

#[test]
fn the_reason_the_search_stopped_is_reported() {
    let quick = solve_bnb(
        &triangle(),
        0,
        BnbOptions {
            gap_tol: 1.0, // absurdly loose, so it closes immediately
            ..Default::default()
        },
    )
    .unwrap();
    assert!(matches!(quick.stopped, Stop::GapClosed | Stop::Exhausted));

    let starved = solve_bnb(
        &case("case57_ieee.m"),
        0,
        BnbOptions {
            max_nodes: 2,
            gap_tol: 0.0,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(starved.stopped, Stop::NodeLimit);
}

#[test]
fn a_real_network_searches_without_falling_over() {
    // IEEE 14 is meshed and has transformers, so it exercises the parts a
    // hand-built triangle does not.
    let r = solve_bnb(
        &case("case14_ieee.m"),
        0,
        BnbOptions {
            max_nodes: 24,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(r.lower_bound.is_finite());
    assert!(r.lower_bound <= r.upper_bound + 1e-6);
    assert!(matches!(
        r.best.status,
        Status::Optimal | Status::OptimalRelaxed
    ));
}

#[test]
fn the_bound_is_below_the_dc_answer_or_above_it_and_either_is_allowed() {
    // Worth stating so it is not mistaken for a bug later. An AC lower bound
    // and a DC optimum are not ordered: DC ignores losses, which pushes it
    // down, and ignores reactive limits, which pushes it down again — but the
    // AC value here is a *relaxation*, which pushes it down too. Only the
    // relationship to the true AC optimum is guaranteed.
    let net = triangle();
    let r = solve_bnb(&net, 0, BnbOptions::default()).unwrap();
    assert!(r.lower_bound.is_finite() && r.lower_bound > 0.0);
}

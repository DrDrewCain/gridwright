//! Apparent power limits, as the cone they are.
//!
//! A line's rating bounds `√(P² + Q²)`, which is a circle in the complex plane.
//! A DC model has no reactive power and bounds the real part alone with a pair
//! of linear inequalities. Carrying that over to an AC formulation is wrong in
//! the direction that matters: a line carrying its full rating as reactive
//! power would read as entirely unloaded.
//!
//! The AC model carried no thermal limits at all before this, so it could route
//! as much as the impedances allowed.

use gridwright_acopf::{Status, solve_acopf};
use gridwright_net::{Generator, Line, Load, Network, Snapshots};

/// Two buses, all generation at one end, all demand at the other, so the line
/// carries everything and its rating is the only thing that can bind.
fn two_bus(rating: f64, q_demand: f64) -> Network {
    two_bus_with(rating, q_demand, 400.0)
}

/// `local_q` bounds how much reactive power the unit at the load can supply.
///
/// It matters more than it looks. Reactive power travels badly and is normally
/// supplied where it is consumed, so a local unit able to provide it means none
/// crosses the line at all — which is correct physics and useless for testing
/// whether the line's rating counts reactive flow. Denying it is what forces
/// the reactive power to come from the far end.
fn two_bus_with(rating: f64, q_demand: f64, local_q: f64) -> Network {
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("A", "AA");
    let b = net.add_bus("B", "AA");
    for bus in [a, b] {
        net.buses[bus].v_min = 0.9;
        net.buses[bus].v_max = 1.1;
    }
    net.add_generator(Generator {
        name: "far".into(),
        bus: a,
        p_nom: 400.0,
        marginal_cost: 20.0,
        q_min: -400.0,
        q_max: 400.0,
        ..Default::default()
    });
    // A dearer unit at the load, so constraining the line has somewhere to go
    // rather than simply making the problem infeasible.
    net.add_generator(Generator {
        name: "local".into(),
        bus: b,
        p_nom: 400.0,
        marginal_cost: 200.0,
        q_min: -local_q,
        q_max: local_q,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 150.0,
        q_set: q_demand,
        ..Default::default()
    });
    net.add_line(Line {
        name: "AB".into(),
        bus0: a,
        bus1: b,
        s_nom: rating,
        susceptance: 20.0,
        resistance: 0.005,
        reactance: 0.05,
        ..Default::default()
    });
    net
}

fn run(net: &Network) -> (Status, f64, f64, f64) {
    let s = solve_acopf(net, 0).unwrap();
    (s.status, s.objective, s.p_flow[0], s.q_flow[0])
}

#[test]
fn a_generous_rating_does_not_bind() {
    let (status, _, p, _) = run(&two_bus(1000.0, 0.0));
    assert!(matches!(status, Status::Optimal | Status::OptimalRelaxed));
    assert!(p > 140.0, "the cheap unit should carry the load: {p}");
}

#[test]
fn a_tight_rating_holds_the_flow_to_it() {
    // 80 MVA against 150 MW of demand, so the line cannot carry it all and the
    // dear local unit has to make up the difference.
    let (status, _, p, q) = run(&two_bus(80.0, 0.0));
    assert!(matches!(status, Status::Optimal | Status::OptimalRelaxed));
    let apparent = (p * p + q * q).sqrt();
    assert!(
        apparent <= 80.0 + 1e-3,
        "the line carries {apparent} MVA against a rating of 80"
    );
    assert!(p < 149.0, "the rating should have bitten: {p}");
}

#[test]
fn a_rating_costs_money_and_the_amount_is_the_redispatch() {
    let free = run(&two_bus(1000.0, 0.0)).1;
    let tight = run(&two_bus(80.0, 0.0)).1;
    assert!(
        tight > free,
        "constraining the line should cost something: {tight} against {free}"
    );
}

#[test]
fn reactive_power_counts_against_the_rating() {
    // The whole reason this is a cone rather than a pair of bounds. A line
    // already carrying reactive power has less room for real power, and a
    // formulation bounding only the real part would not notice.
    // No local reactive support, so the reactive demand has to cross the line.
    let (_, _, p_dry, _) = run(&two_bus_with(120.0, 0.0, 0.0));
    let (status, _, p_wet, q_wet) = run(&two_bus_with(120.0, 90.0, 0.0));
    assert!(matches!(status, Status::Optimal | Status::OptimalRelaxed));
    assert!(
        q_wet.abs() > 1.0,
        "the reactive demand should be flowing: {q_wet}"
    );
    assert!(
        p_wet < p_dry - 1.0,
        "reactive power should crowd out real power: {p_wet} against {p_dry}"
    );
    let apparent = (p_wet * p_wet + q_wet * q_wet).sqrt();
    assert!(
        apparent <= 120.0 + 1e-3,
        "{apparent} MVA against a rating of 120"
    );
}

#[test]
fn the_limit_is_circular_rather_than_square() {
    // A square limit would allow the corner: full rating on both axes at once,
    // which is √2 times the rating in apparent power. The cone must not.
    let (_, _, p, q) = run(&two_bus_with(100.0, 200.0, 0.0));
    let apparent = (p * p + q * q).sqrt();
    assert!(
        apparent <= 100.0 + 1e-3,
        "a square limit would have allowed up to 141 MVA; this is {apparent}"
    );
}

#[test]
fn an_unrated_line_is_left_unconstrained() {
    // Ratings arrive as a very large number when a file says "unlimited", and
    // adding a cone for one would cost a constraint per line for nothing.
    let net = two_bus(1e6, 0.0);
    let (status, _, p, _) = run(&net);
    assert!(matches!(status, Status::Optimal | Status::OptimalRelaxed));
    assert!(p > 140.0, "{p}");
}

#[test]
fn a_real_network_still_solves_with_its_ratings_applied() {
    // IEEE 14 has ratings on every branch, and they were being ignored
    // entirely until now.
    let net = gridwright_io::matpower::load_case(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/pglib/case14_ieee.m"),
    )
    .unwrap()
    .network;
    assert!(net.lines.iter().any(|l| l.s_nom < 1e5), "case14 has ratings");

    let s = solve_acopf(&net, 0).unwrap();
    assert!(matches!(s.status, Status::Optimal | Status::OptimalRelaxed), "{:?}", s.status);
    for (l, line) in net.lines.iter().enumerate() {
        if line.s_nom >= 1e5 {
            continue;
        }
        let apparent = (s.p_flow[l].powi(2) + s.q_flow[l].powi(2)).sqrt();
        assert!(
            apparent <= line.s_nom + 1e-2,
            "branch {l} carries {apparent} MVA against a rating of {}",
            line.s_nom
        );
    }
}

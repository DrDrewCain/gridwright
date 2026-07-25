//! Bus shunts and phase-shifting transformers.
//!
//! Both were read from files and ignored until now. Neither is exotic: a
//! shunt is how voltage is actually held up, and a phase shifter exists for
//! no reason other than to command a flow. A model that drops them answers
//! about a network with no reactive compensation and no controllable branches.

#![cfg(feature = "highs")]

use gridwright_build::build_lopf;
use gridwright_net::{Generator, Line, Load, Network, Snapshots};
use gridwright_solve::{HighsSolver, Solver, Status};

/// Two paths between the same pair of buses, with different impedances, so
/// the split between them is determined and worth commanding.
fn two_paths(shift: f64) -> Network {
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("A", "AA");
    let mid = net.add_bus("MID", "AA");
    let b = net.add_bus("B", "AA");
    net.add_generator(Generator {
        name: "g".into(),
        bus: a,
        p_nom: 400.0,
        marginal_cost: 10.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 120.0,
        ..Default::default()
    });
    // Direct path, and a longer way round through MID.
    net.add_line(Line {
        name: "direct".into(),
        bus0: a,
        bus1: b,
        s_nom: 500.0,
        susceptance: 10.0,
        reactance: 0.1,
        phase_shift: shift,
        ..Default::default()
    });
    net.add_line(Line {
        name: "via_mid_1".into(),
        bus0: a,
        bus1: mid,
        s_nom: 500.0,
        susceptance: 10.0,
        reactance: 0.1,
        ..Default::default()
    });
    net.add_line(Line {
        name: "via_mid_2".into(),
        bus0: mid,
        bus1: b,
        s_nom: 500.0,
        susceptance: 10.0,
        reactance: 0.1,
        ..Default::default()
    });
    net
}

fn flow(net: &Network, name: &str) -> f64 {
    let lopf = build_lopf(net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    let l = net.lines.iter().position(|x| x.name == name).unwrap();
    sol.flow(&lopf.vars, l)[0]
}

#[test]
fn without_a_shifter_the_split_is_whatever_the_impedances_say() {
    // The direct path has susceptance 10; the way round is two of the same in
    // series, so susceptance 5. Twice the susceptance carries twice the flow:
    // 80 MW direct and 40 MW round, out of 120.
    let net = two_paths(0.0);
    assert!((flow(&net, "direct") - 80.0).abs() < 1e-6, "{}", flow(&net, "direct"));
    assert!(
        (flow(&net, "via_mid_1") - 40.0).abs() < 1e-6,
        "{}",
        flow(&net, "via_mid_1")
    );
}

#[test]
fn a_phase_shifter_moves_power_off_the_path_it_sits_on() {
    // The whole purpose of the device. With flow = B·(θ₀ − θ₁ − shift) on the
    // direct path, a positive shift subtracts from the angle difference the
    // flow follows, so the direct path carries less and the way round takes
    // up the difference.
    //
    // Hand-derived. Let d = θ_A − θ_B. Direct carries 10(d − s); the round
    // path carries 5d; together they meet 120:
    //
    //     10(d − s) + 5d = 120   →   d = (120 + 10s)/15
    //     direct = 10·((120 + 10s)/15 − s) = 80 − (10/3)·s
    //
    // At s = 0.15 rad that is 80 − 0.5 = 79.5 MW direct and 40.5 round.
    let s = 0.15;
    let net = two_paths(s);
    let direct = flow(&net, "direct");
    let round = flow(&net, "via_mid_1");
    assert!((direct - (80.0 - 10.0 / 3.0 * s)).abs() < 1e-6, "direct {direct}");
    assert!((round - (40.0 + 10.0 / 3.0 * s)).abs() < 1e-6, "round {round}");
    assert!((direct + round - 120.0).abs() < 1e-9, "the two must still meet demand");
    assert!(direct < 80.0, "a positive shift should unload the branch it is on");
}

#[test]
fn the_shift_reverses_with_its_sign() {
    let up = flow(&two_paths(0.2), "direct");
    let down = flow(&two_paths(-0.2), "direct");
    let flat = flow(&two_paths(0.0), "direct");
    assert!(up < flat, "positive shift should unload: {up} against {flat}");
    assert!(down > flat, "negative shift should load: {down} against {flat}");
    // Symmetric about the unshifted case, since the term is linear.
    assert!(((up + down) / 2.0 - flat).abs() < 1e-6);
}

#[test]
fn a_shifter_can_be_used_to_respect_a_rating_the_impedances_would_violate() {
    // The reason a network operator installs one. Rate the direct path below
    // what it would naturally carry, and without a shifter the optimiser has
    // to redispatch or shed; with one, the flow is commanded down instead.
    let mut net = two_paths(0.0);
    net.lines[0].s_nom = 60.0;
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    let constrained = sol.flow(&lopf.vars, 0)[0];
    assert!(constrained <= 60.0 + 1e-6, "the rating must bind: {constrained}");

    // A shift large enough to push the natural flow under the rating means
    // the rating no longer binds. From the derivation above, direct is
    // 80 − (10/3)s, so s = 6 gives 60.
    let mut shifted = two_paths(6.0);
    shifted.lines[0].s_nom = 60.0;
    let lopf = build_lopf(&shifted).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    assert!((sol.flow(&lopf.vars, 0)[0] - 60.0).abs() < 1e-6);
}

#[test]
fn a_shunt_conductance_is_real_demand_and_has_to_be_generated() {
    // Not a rounding term. A shunt drawing 0.05 per unit on a 100 MVA base is
    // 5 MW that some generator has to produce, and a model that ignores it
    // understates both cost and emissions by that much.
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("A", "AA");
    net.buses[a].g_shunt = 0.05;
    net.add_generator(Generator {
        name: "g".into(),
        bus: a,
        p_nom: 400.0,
        marginal_cost: 10.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: a,
        p_set: 100.0,
        ..Default::default()
    });

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    let generated = sol.dispatch(&lopf.vars, 0)[0];
    assert!(
        (generated - 105.0).abs() < 1e-6,
        "100 MW of load plus 5 MW of shunt, got {generated}"
    );
    // And it costs what it costs.
    assert!((sol.objective - 1050.0).abs() < 1e-6, "{}", sol.objective);
}

#[test]
fn a_shunt_only_draws_at_the_bus_it_is_on() {
    let mut net = two_paths(0.0);
    let mid = net.buses.iter().position(|b| b.name == "MID").unwrap();
    net.buses[mid].g_shunt = 0.1; // 10 MW on a 100 MVA base

    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    let generated = sol.dispatch(&lopf.vars, 0)[0];
    assert!(
        (generated - 130.0).abs() < 1e-6,
        "120 MW of load plus 10 MW of shunt at MID, got {generated}"
    );
}

#[test]
fn the_ieee_300_bus_case_carries_both_and_they_arrive() {
    // The check that the readers are wired up, not just the formulation.
    use gridwright_io::matpower::load_case;
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/pglib/case300_ieee.m");
    let net = load_case(path).unwrap().network;

    let shunts = net.buses.iter().filter(|b| b.g_shunt != 0.0 || b.b_shunt != 0.0).count();
    assert!(shunts > 0, "case300 has bus shunts and none arrived");
    let shifters = net.lines.iter().filter(|l| l.phase_shift != 0.0).count();
    assert_eq!(shifters, 1, "case300 has exactly one phase shifter");

    // MATPOWER states the shift in degrees; every identity in the formulation
    // wants radians, and confusing the two is a factor of 57.
    let shifter = net.lines.iter().find(|l| l.phase_shift != 0.0).unwrap();
    assert!(
        shifter.phase_shift.abs() < 0.5,
        "{} radians is a degrees value that was never converted",
        shifter.phase_shift
    );
}

//! Demand that declines rather than moves when the price is high.
//!
//! Shedding prices unserved energy at the value of lost load, a number in the
//! thousands chosen to mean "never do this". Real demand is not all-or-nothing:
//! some of it would rather not be served at a high enough price and says so
//! through a bid curve. That turns dropping demand from a catastrophe with a
//! penalty into a choice with a price.

#![cfg(feature = "highs")]

use gridwright_build::build_lopf;
use gridwright_net::{Generator, Load, Network, Snapshots};
use gridwright_solve::{HighsSolver, Solver, Status};

/// One expensive generator and a load that may decline some of its demand.
fn elastic(tranches: Vec<(f64, f64)>) -> Network {
    let mut net = Network::new(Snapshots::hourly(1));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "cheap".into(),
        bus: b,
        p_nom: 60.0,
        marginal_cost: 20.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "dear".into(),
        bus: b,
        p_nom: 200.0,
        marginal_cost: 400.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "factory".into(),
        bus: b,
        p_set: 100.0,
        value_tranches: tranches,
        ..Default::default()
    });
    net
}

struct Run {
    status: Status,
    cost: f64,
    given_up: Vec<f64>,
    shed: f64,
}

fn run(net: &Network) -> Run {
    let lopf = build_lopf(net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    Run {
        status: sol.status,
        cost: sol.objective,
        given_up: lopf.vars.demand_tranche[0]
            .iter()
            .map(|b| sol.trajectory(*b)[0])
            .collect(),
        shed: sol.total_shed(&lopf.vars),
    }
}

#[test]
fn an_inelastic_load_is_served_whatever_it_costs() {
    // The behaviour every load had before, and still has without a curve.
    let r = run(&elastic(Vec::new()));
    assert_eq!(r.status, Status::Optimal);
    assert!(r.given_up.is_empty());
    assert!(r.shed < 1e-6, "nothing should be shed: {}", r.shed);
    // 60 MWh at 20, 40 at 400.
    assert!((r.cost - (60.0 * 20.0 + 40.0 * 400.0)).abs() < 1e-6, "{}", r.cost);
}

#[test]
fn demand_worth_less_than_generating_it_is_given_up() {
    // 30 MW valued at 200 against a generator charging 400. Serving it costs
    // more than it is worth, so it goes.
    let r = run(&elastic(vec![(30.0, 200.0)]));
    assert_eq!(r.status, Status::Optimal);
    assert!(
        (r.given_up[0] - 30.0).abs() < 1e-6,
        "the whole tranche should go: {:?}",
        r.given_up
    );
    // 60 at 20, 10 at 400, and 30 given up at 200.
    let want = 60.0 * 20.0 + 10.0 * 400.0 + 30.0 * 200.0;
    assert!((r.cost - want).abs() < 1e-6, "{} against {want}", r.cost);
}

#[test]
fn demand_worth_more_than_generating_it_is_served() {
    // The same tranche valued above the generator's price is worth serving.
    let r = run(&elastic(vec![(30.0, 900.0)]));
    assert_eq!(r.status, Status::Optimal);
    assert!(
        r.given_up[0] < 1e-6,
        "valuable demand should be served: {:?}",
        r.given_up
    );
    assert!((r.cost - (60.0 * 20.0 + 40.0 * 400.0)).abs() < 1e-6);
}

#[test]
fn the_cheapest_tranche_goes_first_whatever_order_it_was_given_in() {
    // A curve is a set of prices, not a sequence, and requiring a caller to
    // sort it would be a trap. Here only the 150 tranche is worth dropping
    // against a generator at 400, and it is listed second.
    let r = run(&elastic(vec![(20.0, 900.0), (25.0, 150.0)]));
    assert_eq!(r.status, Status::Optimal);
    assert!(r.given_up[0] < 1e-6, "the dear tranche was dropped: {:?}", r.given_up);
    assert!(
        (r.given_up[1] - 25.0).abs() < 1e-6,
        "the cheap tranche should go: {:?}",
        r.given_up
    );
}

#[test]
fn a_tranche_is_bounded_by_its_own_size() {
    // Twenty megawatts valued at nothing much is still only twenty megawatts.
    let r = run(&elastic(vec![(20.0, 1.0)]));
    assert!((r.given_up[0] - 20.0).abs() < 1e-6, "{:?}", r.given_up);
    assert!(r.shed < 1e-6, "the rest is served rather than shed: {}", r.shed);
}

#[test]
fn demand_beyond_the_curve_still_falls_back_on_the_value_of_lost_load() {
    // A curve says what a consumer will pay, not that they are indifferent past
    // its end. Strip the generation so the system genuinely cannot cope, and
    // what the curve does not cover must still be shed at the usual penalty
    // rather than given away.
    let mut net = elastic(vec![(20.0, 50.0)]);
    net.generators[0].p_nom = 10.0;
    net.generators[1].p_nom = 10.0;

    let r = run(&net);
    assert_eq!(r.status, Status::Optimal);
    assert!((r.given_up[0] - 20.0).abs() < 1e-6, "{:?}", r.given_up);
    // 100 demanded, 20 declined, 20 generated, so 60 unserved.
    assert!((r.shed - 60.0).abs() < 1e-6, "{} MWh shed", r.shed);
}

#[test]
fn elasticity_never_costs_more_than_inelasticity() {
    // Declining demand is an option, not an obligation, so having the option
    // cannot make the answer worse. If it did, the tranche would be entering
    // the objective with the wrong sign.
    let inelastic = run(&elastic(Vec::new())).cost;
    for value in [1.0, 100.0, 350.0, 400.0, 500.0, 5_000.0] {
        let with = run(&elastic(vec![(30.0, value)])).cost;
        assert!(
            with <= inelastic + 1e-6,
            "a curve at {value} made things worse: {with} against {inelastic}"
        );
    }
}

#[test]
fn a_tranche_priced_exactly_at_the_margin_is_indifferent() {
    // The break-even, worth pinning because it is where a sign error would
    // still look plausible either side.
    let r = run(&elastic(vec![(30.0, 400.0)]));
    assert_eq!(r.status, Status::Optimal);
    let inelastic = run(&elastic(Vec::new())).cost;
    assert!(
        (r.cost - inelastic).abs() < 1e-6,
        "serving and declining should cost the same here: {} against {inelastic}",
        r.cost
    );
}

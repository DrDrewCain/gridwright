//! Demand that moves in time rather than being served or shed.
//!
//! Load was previously one of two things: met where it stood, or shed at the
//! value of lost load. That leaves out demand response entirely, and a great
//! deal of load genuinely chooses when to run. A data centre is the extreme
//! case, and the marginal carbon intensity this engine already reports per bus
//! per snapshot is exactly the signal such a load would schedule against.
//!
//! The distinction from shedding is conservation: what leaves one snapshot has
//! to arrive in another.

#![cfg(feature = "highs")]

use gridwright_build::build_lopf;
use gridwright_net::{Generator, Load, Network, Snapshots, TimeSeries};
use gridwright_solve::{HighsSolver, Solver, Status};

/// Four hours, two of them expensive, and one load that may or may not move.
fn net(shiftable: f64, window: usize, cost: f64) -> Network {
    let mut net = Network::new(Snapshots::hourly(4));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "cheap".into(),
        bus: b,
        p_nom: 200.0,
        marginal_cost: 10.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "dear".into(),
        bus: b,
        p_nom: 200.0,
        marginal_cost: 100.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "flexible".into(),
        bus: b,
        p_set: 100.0,
        shiftable_pu: shiftable,
        shift_window: window,
        shift_cost: cost,
        ..Default::default()
    });
    // The cheap unit is only available in the first two hours, so meeting a
    // flat load costs far more in the last two.
    net.gen_availability =
        TimeSeries::from_rows(&[vec![1.0, 1.0, 0.0, 0.0], vec![1.0; 4]], 4).unwrap();
    net
}

fn run(net: &Network) -> (Status, f64, Vec<f64>) {
    let lopf = build_lopf(net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    let shift = lopf.vars.load_shift[0]
        .map(|b| sol.trajectory(b).to_vec())
        .unwrap_or_default();
    (sol.status, sol.objective, shift)
}

#[test]
fn a_fixed_load_cannot_move_and_pays_for_it() {
    let (status, cost, shift) = run(&net(0.0, 0, 0.0));
    assert_eq!(status, Status::Optimal);
    assert!(shift.is_empty(), "a fixed load should have no shift variable");
    // 200 MWh cheap at 10, 200 MWh dear at 100.
    assert!((cost - (200.0 * 10.0 + 200.0 * 100.0)).abs() < 1e-6, "{cost}");
}

#[test]
fn a_shiftable_load_moves_into_the_cheap_hours() {
    // Half the load may move. The dear hours give up 50 MW each and the cheap
    // hours take 50 MW each, which is the most the bound allows.
    let (status, cost, shift) = run(&net(0.5, 0, 0.0));
    assert_eq!(status, Status::Optimal);
    assert!(shift[0] > 1e-6 && shift[1] > 1e-6, "cheap hours should take more: {shift:?}");
    assert!(shift[2] < -1e-6 && shift[3] < -1e-6, "dear hours should give up: {shift:?}");
    let fixed = run(&net(0.0, 0, 0.0)).1;
    assert!(cost < fixed, "moving should save money: {cost} against {fixed}");
    // Hand-derived: 50 MWh moves out of each dear hour into each cheap one, so
    // 100 MWh changes hands at a saving of 90 per MWh.
    assert!((fixed - cost - 100.0 * 90.0).abs() < 1e-6, "saved {}", fixed - cost);
}

#[test]
fn the_energy_is_conserved_rather_than_deleted() {
    // The property that makes this shifting and not shedding. If the sum could
    // come out negative the optimiser would simply delete the expensive hours,
    // which is a much cheaper answer to a different question.
    let (_, _, shift) = run(&net(0.6, 0, 0.0));
    let total: f64 = shift.iter().sum();
    assert!(total.abs() < 1e-6, "energy went missing: {shift:?} sums to {total}");
}

#[test]
fn a_window_bounds_how_far_energy_can_travel() {
    // A load that may move within a day is a different thing from one that may
    // move within a year. With a window of two, the first pair and the second
    // pair must each balance on their own, so nothing can cross from the dear
    // half into the cheap half at all.
    let (status, cost, shift) = run(&net(0.5, 2, 0.0));
    assert_eq!(status, Status::Optimal);
    let first: f64 = shift[0..2].iter().sum();
    let second: f64 = shift[2..4].iter().sum();
    assert!(first.abs() < 1e-6, "first window did not balance: {shift:?}");
    assert!(second.abs() < 1e-6, "second window did not balance: {shift:?}");

    // And so it saves nothing, because within each window the hours are alike.
    let fixed = run(&net(0.0, 0, 0.0)).1;
    assert!(
        (cost - fixed).abs() < 1e-6,
        "a window that traps the load should not save: {cost} against {fixed}"
    );
}

#[test]
fn a_wider_window_lets_more_energy_travel() {
    let narrow = run(&net(0.5, 2, 0.0)).1;
    let wide = run(&net(0.5, 4, 0.0)).1;
    assert!(wide < narrow, "{wide} against {narrow}");
}

#[test]
fn the_amount_that_can_move_is_bounded_by_the_load() {
    // Sixty percent shiftable means sixty percent, not all of it.
    let (_, _, shift) = run(&net(0.6, 0, 0.0));
    for (t, s) in shift.iter().enumerate() {
        assert!(
            s.abs() <= 60.0 + 1e-6,
            "hour {t} moved {s}, beyond the 60 MW allowed"
        );
    }
    // And a load cannot be deferred below nothing.
    for (t, s) in shift.iter().enumerate() {
        assert!(100.0 + s >= -1e-6, "hour {t} was driven negative: {s}");
    }
}

#[test]
fn charging_for_movement_stops_pointless_shuffling() {
    // Without a cost, demand slides between equally priced snapshots for no
    // reason and the answer is arbitrary. A small charge makes it determinate.
    let mut flat = net(0.5, 0, 0.0);
    // Every hour alike, so there is nothing to gain by moving.
    flat.gen_availability = TimeSeries::from_rows(&[vec![1.0; 4], vec![1.0; 4]], 4).unwrap();
    let mut priced = flat.clone();
    priced.loads[0].shift_cost = 1.0;

    let (_, _, moved) = run(&priced);
    let total: f64 = moved.iter().map(|s| s.abs()).sum();
    assert!(
        total < 1e-6,
        "a charged load with nothing to gain should sit still: {moved:?}"
    );
}

#[test]
fn the_charge_is_paid_on_movement_in_either_direction() {
    // A signed variable cannot carry a cost in both directions, so the
    // magnitude is pinned separately. If only one direction were charged, the
    // optimiser would move in the free one and the cost would do nothing.
    let free = run(&net(0.5, 0, 0.0)).1;
    let charged = run(&net(0.5, 0, 5.0)).1;
    assert!(
        charged > free,
        "moving should cost more when movement is charged: {charged} against {free}"
    );
    // 200 MWh moves in total, half up and half down, at 5 each.
    assert!((charged - free - 200.0 * 5.0).abs() < 1e-6, "{}", charged - free);
}

#[test]
fn a_charge_that_exceeds_the_saving_stops_the_move() {
    // 90 per MWh is the price difference, so a charge above it makes moving a
    // loss and the load should stay put.
    let (_, _, shift) = run(&net(0.5, 0, 200.0));
    let total: f64 = shift.iter().map(|s| s.abs()).sum();
    assert!(total < 1e-6, "moving was not worth it and it moved anyway: {shift:?}");
}

#[test]
fn shifting_and_shedding_are_different_and_shifting_is_preferred() {
    // Shedding is priced at the value of lost load, which is far above any
    // generation cost, so a load able to move should always move rather than
    // go unserved.
    let mut tight = net(0.5, 0, 0.0);
    // Not enough capacity in the dear hours to meet a flat load.
    tight.generators[1].p_nom = 60.0;
    let lopf = build_lopf(&tight).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);
    let shed = sol.total_shed(&lopf.vars);
    let shift = sol.trajectory(lopf.vars.load_shift[0].unwrap());
    assert!(
        shift.iter().any(|s| s.abs() > 1e-6),
        "the load should have moved before anything was shed"
    );
    assert!(shed < 81.0, "{shed} MWh shed despite flexibility available");
}

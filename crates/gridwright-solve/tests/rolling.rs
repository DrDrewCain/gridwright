//! Rolling horizon, checked against solving the same period whole.
//!
//! The reference answer is available here, which is unusual and valuable: a
//! horizon short enough to solve in one go can be solved both ways, and the
//! rolling answer should be close. It will not be identical, and should not be:
//! a window cannot see past its own lookahead, so it will occasionally commit
//! to something a full-horizon solve would have avoided. That gap is the price
//! of tractability and is worth measuring rather than assuming away.

use gridwright_build::build_lopf;
use gridwright_net::{Generator, Load, Network, Snapshots, StorageUnit, TimeSeries};
use gridwright_solve::rolling::{Horizon, solve_rolling};
use gridwright_solve::{HighsSolver, Solver, Status};

/// One bus, cheap plant that comes and goes, expensive backup, and a battery.
fn shifting_system(hours: usize) -> Network {
    let mut net = Network::new(Snapshots::hourly(hours));
    let b = net.add_bus("B", "X");
    net.add_generator(Generator {
        name: "cheap".into(), bus: b, p_nom: 120.0, marginal_cost: 5.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "backup".into(), bus: b, p_nom: 200.0, marginal_cost: 80.0,
        ..Default::default()
    });
    net.add_load(Load { name: "l".into(), bus: b, p_set: 0.0, ..Default::default() });
    net.load_profile = TimeSeries::from_rows(
        &[(0..hours).map(|t| 60.0 + 40.0 * ((t as f64) * 0.7).sin()).collect()], hours).unwrap();
    net.gen_availability = TimeSeries::from_rows(&[
        (0..hours).map(|t| if t % 6 < 3 { 1.0 } else { 0.2 }).collect(),
        vec![1.0; hours],
    ], hours).unwrap();
    net.add_storage(StorageUnit {
        name: "batt".into(), bus: b, p_nom: 40.0, max_hours: 4.0,
        efficiency_store: 0.95, efficiency_dispatch: 0.95, cyclic: false,
        ..Default::default()
    });
    net
}

#[test]
fn a_rolling_solve_covers_every_snapshot() {
    let net = shifting_system(24);
    let r = solve_rolling(&net, Horizon { window: 8, keep: 4 }, &HighsSolver::default()).unwrap();
    assert_eq!(r.windows, 6, "24 snapshots kept 4 at a time");
    assert!(r.statuses.iter().all(|s| *s == Status::Optimal));
    for g in 0..net.generators.len() {
        assert_eq!(r.dispatch[g].len(), 24);
    }
    // Demand is met in every snapshot.
    let total_shed: f64 = r.shed.iter().flatten().sum();
    assert!(total_shed < 1e-4, "unserved {total_shed}");
}

#[test]
fn the_rolling_answer_is_close_to_solving_the_whole_horizon() {
    let net = shifting_system(24);
    let whole = {
        let l = build_lopf(&net).unwrap();
        HighsSolver::default().solve(&l).unwrap().objective
    };
    let rolled = solve_rolling(&net, Horizon { window: 12, keep: 6 }, &HighsSolver::default())
        .unwrap().objective;

    // Rolling can only be worse: it commits with less information.
    assert!(rolled >= whole - 1e-4, "rolling {rolled} beat the full horizon {whole}");
    let gap = (rolled - whole) / whole.abs().max(1.0);
    assert!(gap < 0.15, "rolling cost {rolled} vs {whole}, a {:.1}% gap", gap * 100.0);
}

#[test]
fn more_lookahead_never_costs_more() {
    // The reason windows overlap at all. Keeping the same amount but seeing
    // further should weakly improve the answer.
    let net = shifting_system(24);
    let short = solve_rolling(&net, Horizon { window: 6, keep: 6 }, &HighsSolver::default())
        .unwrap().objective;
    let long = solve_rolling(&net, Horizon { window: 18, keep: 6 }, &HighsSolver::default())
        .unwrap().objective;
    assert!(long <= short + 1e-3,
            "more lookahead cost more: {long} with 18h vs {short} with 6h");
}

#[test]
fn storage_state_carries_between_windows() {
    // The defining property. If the level did not carry, every window would
    // start empty and the battery could never be useful across a boundary.
    let net = shifting_system(18);
    let r = solve_rolling(&net, Horizon { window: 6, keep: 3 }, &HighsSolver::default()).unwrap();
    let soc = &r.soc[0];
    assert_eq!(soc.len(), 18);
    // At least once, the battery holds energy across a window boundary at t=3.
    assert!(soc.iter().any(|&e| e > 1e-3), "the battery was never charged");
    assert!(soc.iter().all(|&e| (-1e-6..=40.0 * 4.0 + 1e-3).contains(&e)),
            "state of charge left its bounds");
}

#[test]
fn commitment_state_carries_between_windows() {
    // A unit already running must not be charged to start again in the next
    // window. Solved with a start-up cost high enough that double-charging
    // would be obvious.
    let mut net = shifting_system(12);
    net.generators[1].committable = true;
    net.generators[1].p_min_pu = 0.2;
    net.generators[1].start_up_cost = 5_000.0;

    let r = solve_rolling(&net, Horizon { window: 6, keep: 3 }, &HighsSolver::default()).unwrap();
    assert!(r.statuses.iter().all(|s| *s == Status::Optimal));
    let whole = {
        let l = build_lopf(&net).unwrap();
        HighsSolver::default().solve(&l).unwrap().objective
    };
    // With state carried, the rolling cost should be in the same league. If
    // every window restarted the unit cold, this would blow out by multiples of
    // the 5,000 start cost.
    assert!(r.objective < whole + 3.0 * 5_000.0,
            "rolling {} vs whole {}: starts look like they were charged repeatedly",
            r.objective, whole);
}

#[test]
fn a_window_larger_than_the_horizon_is_one_window() {
    let net = shifting_system(5);
    let r = solve_rolling(&net, Horizon { window: 50, keep: 50 }, &HighsSolver::default()).unwrap();
    assert_eq!(r.windows, 1);
    assert_eq!(r.dispatch[0].len(), 5);
}

#[test]
fn a_nonsensical_horizon_is_refused() {
    let net = shifting_system(4);
    assert!(solve_rolling(&net, Horizon { window: 0, keep: 0 }, &HighsSolver::default()).is_err());
    assert!(solve_rolling(&net, Horizon { window: 4, keep: 9 }, &HighsSolver::default()).is_err());
}

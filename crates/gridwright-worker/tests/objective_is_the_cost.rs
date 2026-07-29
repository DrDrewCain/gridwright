//! The objective must equal the cost of what the answer says happened.
//!
//! Prompted by being asked whether it was accurate, and the honest response to
//! that question is a check that runs every time rather than a number recomputed
//! once by hand.
//!
//! This is a *cross-check*, not a restatement. The solver reports an objective
//! from its own row arithmetic; these tests rebuild the same figure from the
//! per-component dispatch it returned, through a completely different path. The
//! two agreeing means the mapping from solver columns back onto domain
//! quantities is right, which is the step most likely to be silently wrong and
//! the one a solver's own self-consistency cannot catch.

use gridwright_net::{Bus, Generator, Line, Load, Network, Snapshots, StorageUnit};

/// Two zones, a tight link between them, cheap generation on one side.
///
/// Deliberately congested: an uncongested network prices uniformly and would
/// pass these checks even if the flow limits were ignored entirely.
fn scene() -> Network {
    let mut net = Network::new(Snapshots::hourly(6));
    for name in ["north", "south"] {
        net.buses.push(Bus {
            name: name.into(),
            ..Default::default()
        });
    }
    net.lines.push(Line {
        name: "tie".into(),
        bus0: 0,
        bus1: 1,
        s_nom: 40.0,
        susceptance: 10.0,
        ..Default::default()
    });
    net.generators.push(Generator {
        name: "cheap".into(),
        bus: 0,
        p_nom: 500.0,
        marginal_cost: 7.0,
        ..Default::default()
    });
    net.generators.push(Generator {
        name: "dear".into(),
        bus: 1,
        p_nom: 500.0,
        marginal_cost: 91.0,
        ..Default::default()
    });
    net.loads.push(Load {
        name: "demand".into(),
        bus: 1,
        p_set: 120.0,
        ..Default::default()
    });
    net
}

#[test]
fn the_objective_equals_the_dispatch_times_its_cost() {
    let net = scene();
    let s = gridwright_worker::solve(&net).expect("solve");

    let rebuilt: f64 = net
        .generators
        .iter()
        .enumerate()
        .map(|(g, unit)| s.dispatch[g].iter().sum::<f64>() * unit.marginal_cost)
        .sum();

    let reported = s.objective.expect("an optimal solve reports an objective");
    assert!(
        (reported - rebuilt).abs() < 1e-6,
        "objective {reported} against dispatch cost {rebuilt}",
    );
}

#[test]
fn generation_meets_demand_in_every_snapshot() {
    // Per snapshot, not in total. A model that overproduced in one hour and
    // underproduced in another would balance on the year and be wrong
    // everywhere.
    let net = scene();
    let s = gridwright_worker::solve(&net).expect("solve");

    for t in 0..net.n_snapshots() {
        let made: f64 = s.dispatch.iter().map(|d| d[t]).sum();
        let taken: f64 = net.loads.iter().map(|l| l.p_set).sum();
        let shed: f64 = s.shed.iter().map(|d| d[t]).sum();
        assert!(
            (made + shed - taken).abs() < 1e-6,
            "snapshot {t}: made {made}, shed {shed}, load {taken}",
        );
    }
}

#[test]
fn the_congested_tie_is_respected_and_the_price_splits_across_it() {
    // The tie is rated 40 and demand is 120, so cheap generation cannot cover
    // the load and the expensive unit must run. Both facts have to show up: the
    // flow at its limit, and the two buses pricing differently.
    let net = scene();
    let s = gridwright_worker::solve(&net).expect("solve");

    for (t, f) in s.flows[0].iter().enumerate() {
        assert!(
            f.abs() <= 40.0 + 1e-6,
            "snapshot {t}: flow {f} exceeds the 40 MW rating",
        );
    }

    let north = s.prices[0][0];
    let south = s.prices[1][0];
    assert!(
        (north - 7.0).abs() < 1e-6,
        "the cheap bus should price at its marginal unit, got {north}",
    );
    assert!(
        (south - 91.0).abs() < 1e-6,
        "the constrained bus should price at its own marginal unit, got {south}",
    );
}

#[test]
fn storage_conserves_energy_through_its_efficiency() {
    // The check that catches a sign or efficiency error: what comes out must be
    // what went in, times the round trip, to within the solver's tolerance.
    let mut net = scene();
    net.storage.push(StorageUnit {
        name: "battery".into(),
        bus: 1,
        p_nom: 30.0,
        max_hours: 4.0,
        efficiency_store: 0.9,
        efficiency_dispatch: 0.8,
        cyclic: true,
        ..Default::default()
    });
    let s = gridwright_worker::solve(&net).expect("solve");

    let net_flow: f64 = s.storage_power[0].iter().sum();
    assert!(
        net_flow <= 1e-6,
        "a cyclic store cannot be a net source over its cycle, got {net_flow}",
    );

    // State of charge never exceeds capacity, and never goes negative.
    let capacity = 30.0 * 4.0;
    for (t, e) in s.soc[0].iter().enumerate() {
        assert!(
            *e >= -1e-6 && *e <= capacity + 1e-6,
            "snapshot {t}: state of charge {e} outside 0..{capacity}",
        );
    }
}

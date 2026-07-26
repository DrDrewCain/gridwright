//! Where a rolling horizon's window length stops paying.
//!
//! A rolling horizon trades foresight for tractability: a shorter window solves
//! faster and sees less of the future, so it commits to things a full-horizon
//! solve would have avoided. Everyone knows the trade exists. Nobody here had
//! established where it turns, and "use half the window as lookahead" was a
//! convention rather than a measurement.
//!
//! The question only means anything on a system with storage that matters,
//! because storage is the thing that couples snapshots. Without it every
//! snapshot is nearly independent, a window of one is nearly optimal, and the
//! measurement says nothing except that the test was badly chosen. So the
//! fixture below has a reservoir sized to shift energy across days rather than
//! hours, against a demand and availability pattern with a period longer than
//! the shortest windows.
//!
//! The reference is the same horizon solved whole, which is available here
//! precisely because the case is small enough for that. Every rolling answer is
//! reported as its cost penalty against that optimum, so the numbers say what a
//! window costs rather than merely what it does.
//!
//! Run it explicitly, since it solves the same year many times over:
//!
//! ```text
//! cargo test -p gridwright-solve --test horizon_window --release -- --ignored --nocapture
//! ```

#![cfg(feature = "highs")]

use gridwright_build::build_lopf;
use gridwright_net::{Generator, Load, Network, Snapshots, StorageUnit, TimeSeries};
use gridwright_solve::rolling::{Horizon, solve_rolling};
use gridwright_solve::{HighsSolver, Solver, Status};
use std::time::Instant;

/// A system whose storage genuinely couples distant snapshots.
///
/// Cheap generation arrives in multi-day spells rather than every few hours,
/// and the reservoir holds a week of it. That is what makes a short window
/// expensive: it can see the drought but not the spell that would have paid for
/// filling up before it.
fn seasonal(hours: usize) -> Network {
    let mut net = Network::new(Snapshots::hourly(hours));
    let b = net.add_bus("B", "X");
    net.add_generator(Generator {
        name: "wind".into(),
        bus: b,
        p_nom: 260.0,
        marginal_cost: 0.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "gas".into(),
        bus: b,
        p_nom: 300.0,
        marginal_cost: 90.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 0.0,
        ..Default::default()
    });

    // Demand with a daily shape on top of a weekly one, so no single window
    // length lines up with the whole pattern by accident.
    let day = std::f64::consts::TAU / 24.0;
    let week = std::f64::consts::TAU / (24.0 * 7.0);
    net.load_profile = TimeSeries::from_rows(
        &[(0..hours)
            .map(|t| {
                let t = t as f64;
                150.0 + 45.0 * (t * day).sin() + 25.0 * (t * week).cos()
            })
            .collect()],
        hours,
    )
    .unwrap();

    // Wind in spells lasting days: three days blowing, two days becalmed. A
    // window shorter than the calm spell cannot know to save for it.
    net.gen_availability = TimeSeries::from_rows(
        &[
            (0..hours)
                .map(|t| if (t / 24) % 5 < 3 { 0.95 } else { 0.10 })
                .collect(),
            vec![1.0; hours],
        ],
        hours,
    )
    .unwrap();

    // A week of storage at full output. Sized deliberately larger than the
    // shortest windows tested, because a reservoir that empties within one
    // window is one no window length can mismanage.
    net.add_storage(StorageUnit {
        name: "reservoir".into(),
        bus: b,
        p_nom: 90.0,
        max_hours: 168.0,
        efficiency_store: 0.92,
        efficiency_dispatch: 0.92,
        cyclic: false,
        soc_initial: Some(0.0),
        ..Default::default()
    });
    net
}

fn whole(net: &Network) -> (f64, std::time::Duration) {
    let lopf = build_lopf(net).unwrap();
    let t = Instant::now();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    let dt = t.elapsed();
    assert_eq!(sol.status, Status::Optimal);
    (sol.objective, dt)
}

#[test]
#[ignore = "solves the same horizon a dozen times; run explicitly for numbers"]
fn where_the_window_length_stops_paying() {
    let hours = 24 * 21;
    let net = seasonal(hours);

    let (optimum, whole_time) = whole(&net);
    println!("\n  {hours} snapshots, storage holding a week at full output");
    println!("  solved whole: {optimum:.0} in {whole_time:.1?}\n");
    println!("  window   keep   lookahead        cost   penalty     time   vs whole");

    // Windows from a day to a fortnight, each keeping half, which is the
    // convention this is meant to test rather than assume.
    for window in [24usize, 48, 72, 96, 120, 168, 240, 336] {
        if window > hours {
            continue;
        }
        let h = Horizon::new(window);
        let t = Instant::now();
        let Ok(r) = solve_rolling(&net, h, &HighsSolver::default()) else {
            println!("  {window:6}   {:4}   rejected", h.keep);
            continue;
        };
        let dt = t.elapsed();
        let penalty = (r.objective - optimum) / optimum.abs().max(1.0);
        println!(
            "  {window:6}   {:4}   {:9}   {:9.0}   {:6.2}%   {:>6.1?}   {:6.2}x",
            h.keep,
            window - h.keep,
            r.objective,
            penalty * 100.0,
            dt,
            dt.as_secs_f64() / whole_time.as_secs_f64().max(1e-9)
        );
    }

    // And the other axis, which the convention fixes without justification:
    // how much of a window is spent on lookahead rather than kept.
    println!("\n  at a fixed window of 168, varying how much is kept");
    println!("  keep   lookahead        cost   penalty     time");
    for keep in [24usize, 48, 84, 120, 144, 168] {
        let h = Horizon { window: 168, keep };
        let t = Instant::now();
        let Ok(r) = solve_rolling(&net, h, &HighsSolver::default()) else {
            println!("  {keep:4}   rejected");
            continue;
        };
        let dt = t.elapsed();
        let penalty = (r.objective - optimum) / optimum.abs().max(1.0);
        println!(
            "  {keep:4}   {:9}   {:9.0}   {:6.2}%   {:>6.1?}",
            168 - keep,
            r.objective,
            penalty * 100.0,
            dt
        );
    }
}

#[test]
fn a_rolling_answer_is_never_cheaper_than_the_optimum_it_approximates() {
    // The property that makes the penalties above meaningful. A window sees a
    // subset of the constraints the whole horizon does, but it must still
    // satisfy all of them over the snapshots it keeps, so its stitched answer
    // is feasible for the full problem and therefore cannot beat the optimum.
    //
    // A rolling answer that came out cheaper would mean a window had been
    // allowed to ignore something — most likely a reservoir level that was not
    // carried across the seam, which is exactly the bug this formulation exists
    // to avoid and exactly the kind that looks like good news.
    let hours = 24 * 5;
    let net = seasonal(hours);
    let (optimum, _) = whole(&net);

    for window in [24usize, 48, 72] {
        let r = solve_rolling(&net, Horizon::new(window), &HighsSolver::default()).unwrap();
        assert!(
            r.statuses.iter().all(|s| *s == Status::Optimal),
            "window {window}: {:?}",
            r.statuses
        );
        assert!(
            r.objective >= optimum - 1e-6 * optimum.abs().max(1.0),
            "window {window} came back at {} against a true optimum of {optimum}, \
             which means it was solving an easier problem than the one asked",
            r.objective
        );
    }
}

#[test]
fn a_window_covering_the_whole_horizon_reproduces_the_whole_answer() {
    // The degenerate end of the trade, and the calibration for everything
    // above: with nothing left outside the window there is no foresight to
    // lose, so the penalty must be zero. If this drifted, every penalty in the
    // table would be measuring the seam rather than the lookahead.
    let hours = 48;
    let net = seasonal(hours);
    let (optimum, _) = whole(&net);

    let r = solve_rolling(
        &net,
        Horizon {
            window: hours,
            keep: hours,
        },
        &HighsSolver::default(),
    )
    .unwrap();
    assert!(
        r.statuses.iter().all(|s| *s == Status::Optimal),
        "{:?}",
        r.statuses
    );
    assert!(
        (r.objective - optimum).abs() < 1e-6 * optimum.abs().max(1.0),
        "one window covering everything gave {} against {optimum}",
        r.objective
    );
}

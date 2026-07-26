//! How sensitive the solver is to how often it refactorises.
//!
//! Forrest-Tomlin updates the factors in place instead of appending elementary
//! matrices, and the reason to want it is that every solve lengthens with the
//! pivots since the last refactorisation. Whether that is worth building
//! depends entirely on how much of the time is actually spent applying those
//! updates — which this measures, by making them cheap and expensive and
//! seeing whether the total moves.
//!
//! # What it said
//!
//! At 9,216 rows the total is a shallow U: 3.55 s refactorising every 32
//! pivots, 3.30 at 64, 3.16 at 256, 3.20 at 512, and 5.41 s never
//! refactorising at all. Fitting `base + A/k + B·k` to those gives a base of
//! 3.03 s, and at the optimum the two variable terms are 0.06 s of
//! refactorisation and 0.07 s of update application.
//!
//! So applying the updates is **2.3% of runtime**. That is the whole of what
//! Forrest-Tomlin could address, and it would replace it with something rather
//! than nothing. The other 96% is the triangular solves and the pricing, which
//! Forrest-Tomlin does not touch.
//!
//! The measurement did buy something: the default interval moved from 64 to
//! 256, which is a measured 4%.

#![cfg(all(feature = "highs", feature = "simplex"))]

use gridwright_build::build_lopf;
use gridwright_net::{Generator, Line, Load, Network, Snapshots};
use gridwright_solve::{SimplexSolver, Solver};
use std::time::Instant;

fn ring(buses: usize, hours: usize) -> Network {
    let mut net = Network::new(Snapshots::hourly(hours));
    for i in 0..buses {
        net.add_bus(format!("b{i}"), "X");
    }
    for i in 0..buses {
        net.add_generator(Generator {
            name: format!("g{i}"),
            bus: i,
            p_nom: 400.0,
            marginal_cost: 20.0 + (i % 7) as f64,
            ..Default::default()
        });
        net.add_load(Load {
            name: format!("d{i}"),
            bus: i,
            p_set: 300.0,
            ..Default::default()
        });
        net.add_line(Line {
            name: format!("l{i}"),
            bus0: i,
            bus1: (i + 1) % buses,
            s_nom: 500.0,
            susceptance: 10.0,
            ..Default::default()
        });
    }
    net
}

#[test]
#[ignore = "a measurement, not a guard"]
fn how_much_the_refactorisation_interval_is_worth() {
    let lopf = build_lopf(&ring(48, 96)).unwrap();
    println!("\n  {} rows", lopf.model.num_rows());
    println!("  every  ratio   time");
    for (every, ratio) in [
        (32usize, 0.0f64),
        (64, 0.5),
        (128, 0.0),
        (256, 0.0),
        (512, 0.0),
        (100_000, 0.0),
    ] {
        let solver = SimplexSolver {
            options: gridwright_simplex::Options {
                refactor_every: every,
                refactor_fill_ratio: ratio,
                ..Default::default()
            },
            ..Default::default()
        };
        let t = Instant::now();
        let s = solver.solve(&lopf).unwrap();
        println!(
            "  {every:6} {ratio:5}   {:?}  {:?}",
            t.elapsed(),
            s.status
        );
    }
}

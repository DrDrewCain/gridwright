//! Where this actually stops, measured rather than asserted.
//!
//! Ignored by default: these take minutes, and their purpose is to produce
//! numbers for a human rather than to guard a behaviour. Run with
//! `cargo test -p gridwright-solve --test scale --release -- --ignored --nocapture`.

#![cfg(feature = "highs")]

use gridwright_build::build_lopf;
use gridwright_net::{Generator, Line, Load, Network, Snapshots, StorageUnit, TimeSeries};
use gridwright_solve::rolling::{Horizon, solve_rolling};
use gridwright_solve::{HighsSolver, Solver};
use std::time::Instant;

/// A synthetic ring with generation, demand and storage at every bus. Labelled
/// synthetic wherever it appears: it says nothing about whether the physics is
/// right, only about how the problem scales.
fn ring(buses: usize, hours: usize) -> Network {
    let mut net = Network::new(Snapshots::hourly(hours));
    for i in 0..buses {
        net.add_bus(format!("b{i}"), format!("c{}", i % 8));
    }
    for i in 0..buses {
        net.add_generator(Generator {
            name: format!("base{i}"),
            bus: i,
            p_nom: 400.0,
            marginal_cost: 20.0 + (i % 5) as f64,
            ..Default::default()
        });
        net.add_generator(Generator {
            name: format!("peak{i}"),
            bus: i,
            p_nom: 200.0,
            marginal_cost: 120.0,
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
        if i % 4 == 0 {
            net.add_storage(StorageUnit {
                name: format!("s{i}"),
                bus: i,
                p_nom: 100.0,
                max_hours: 6.0,
                efficiency_store: 0.94,
                efficiency_dispatch: 0.94,
                cyclic: true,
                ..Default::default()
            });
        }
    }
    // A profile that forces the storage and the network to do something,
    // rather than a flat year the optimiser can solve once and repeat.
    let rows: Vec<Vec<f64>> = (0..net.generators.len())
        .map(|g| {
            (0..hours)
                .map(|t| {
                    if g % 2 == 0 {
                        0.6 + 0.4 * ((t as f64 / 12.0).sin())
                    } else {
                        1.0
                    }
                })
                .collect()
        })
        .collect();
    net.gen_availability = TimeSeries::from_rows(&rows, hours).unwrap();
    net
}

#[test]
#[ignore = "minutes; run explicitly for numbers"]
fn where_the_whole_horizon_stops_being_solvable() {
    println!("\n  buses  cols        build      solve");
    // Runs to completion at every rung. An earlier version of this table
    // reported the largest as "did not finish in seven minutes", which was not
    // a property of the problem but the point at which the person running it
    // stopped waiting. Minutes are not a long time for an optimisation of this
    // size, and the whole ladder takes a few of them.
    for buses in [8, 16, 32, 64, 128] {
        let net = ring(buses, 8760);
        let t0 = Instant::now();
        let lopf = build_lopf(&net).unwrap();
        let build = t0.elapsed();
        let t1 = Instant::now();
        let sol = HighsSolver::default().solve(&lopf).unwrap();
        let solve = t1.elapsed();
        println!(
            "  {buses:5}  {:<10}  {:>7.1?}  {:>9.1?}  {:?}",
            lopf.model.num_cols(),
            build,
            solve,
            sol.status
        );
    }
}

#[test]
#[ignore = "minutes; run explicitly for numbers"]
fn the_same_year_through_a_rolling_horizon() {
    // The comparison that decides whether a fast builder is worth anything.
    // Solving a year whole is one enormous program; solving it in windows is
    // a hundred small ones, which means a hundred builds, and it is the only
    // way the large cases finish at all.
    println!("\n  buses  window  windows  total");
    for buses in [16, 32, 64, 128] {
        let net = ring(buses, 8760);
        let t0 = Instant::now();
        let r = solve_rolling(
            &net,
            Horizon {
                window: 96,
                keep: 72,
            },
            &HighsSolver::default(),
        );
        let elapsed = t0.elapsed();
        match r {
            Ok(sol) => println!(
                "  {buses:5}  {:6}  {:7}  {:?}",
                96,
                sol.windows,
                elapsed
            ),
            Err(e) => println!("  {buses:5}  failed: {e}"),
        }
    }
}

#[test]
#[ignore = "minutes; run explicitly for numbers"]
#[cfg(feature = "simplex")]
fn where_the_pure_rust_solver_stops() {
    // The one that decides what a browser can do on its own. The dense basis
    // inverse is O(m²) memory and O(m²) per pivot, so this ceiling is a
    // property of the factorisation, not of the machine.
    use gridwright_solve::SimplexSolver;
    println!("\n  buses  hours  rows      time");
    for (buses, hours) in [(8, 24), (16, 24), (24, 48), (32, 48), (48, 72), (64, 96), (96, 96)] {
        let net = ring(buses, hours);
        let lopf = build_lopf(&net).unwrap();
        let t0 = Instant::now();
        let r = SimplexSolver::default().solve(&lopf);
        let elapsed = t0.elapsed();
        match r {
            Ok(s) => println!(
                "  {buses:5}  {hours:5}  {:<8}  {:?}  {:?}",
                lopf.model.num_rows(),
                elapsed,
                s.status
            ),
            Err(e) => println!("  {buses:5}  {hours:5}  failed: {e}"),
        }
    }
}

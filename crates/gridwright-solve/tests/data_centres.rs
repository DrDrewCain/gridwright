//! A fleet of data centres as flexible demand, at the scale one would be.
//!
//! A data centre is a large load whose work is often indifferent to which hour
//! it happens in, which makes it all four kinds of flexible demand at once
//! depending on the workload: batch training can shift, inference cannot,
//! spot-priced capacity declines on a curve, and a contracted site can be
//! curtailed outright.
//!
//! The question this answers is whether that survives at fleet scale rather
//! than on the three-bus examples the formulation tests use. Three thousand
//! sites across a continental network, each with its own flexibility, is the
//! shape the question was asked in.

#![cfg(feature = "highs")]

use gridwright_build::build_lopf;
use gridwright_net::{Generator, Line, Load, Network, Snapshots, TimeSeries};
use gridwright_solve::{HighsSolver, Solver, Status};
use std::time::Instant;

/// `sites` data centres spread over `buses`, each flexible in a different way.
fn fleet(buses: usize, sites: usize, hours: usize) -> Network {
    let mut net = Network::new(Snapshots::hourly(hours));
    for i in 0..buses {
        net.add_bus(format!("b{i}"), format!("c{}", i % 8));
    }
    for i in 0..buses {
        net.add_generator(Generator {
            name: format!("base{i}"),
            bus: i,
            p_nom: 900.0,
            marginal_cost: 18.0 + (i % 5) as f64,
            carrier: "gas".into(),
            co2_emissions: 0.35,
            ..Default::default()
        });
        // Large enough that it covers the whole bus in a windy hour and
        // little of it in a calm one. Without that swing the same unit is
        // marginal in every hour, the price never moves, and flexibility has
        // nothing to chase — which is a fair description of a network nobody
        // would build flexibility for.
        net.add_generator(Generator {
            name: format!("wind{i}"),
            bus: i,
            p_nom: 2200.0,
            marginal_cost: 0.0,
            carrier: "wind".into(),
            ..Default::default()
        });
        net.add_line(Line {
            name: format!("l{i}"),
            bus0: i,
            bus1: (i + 1) % buses,
            s_nom: 2000.0,
            susceptance: 10.0,
            ..Default::default()
        });
        // Ordinary demand, which cannot move.
        net.add_load(Load {
            name: format!("town{i}"),
            bus: i,
            p_set: 400.0,
            ..Default::default()
        });
    }

    // The fleet. Four kinds in rotation, because a real one is not uniform.
    for s in 0..sites {
        let bus = s % buses;
        let mut load = Load {
            name: format!("dc{s}"),
            bus,
            p_set: 40.0,
            ..Default::default()
        };
        match s % 4 {
            // Batch work: moves within a day, at a small cost.
            0 => {
                load.shiftable_pu = 0.6;
                load.shift_window = 24;
                load.shift_cost = 2.0;
            }
            // Spot-priced capacity: declines rather than moves.
            1 => load.value_tranches = vec![(15.0, 60.0), (10.0, 250.0)],
            // Contracted: curtailable a bounded number of times.
            2 => {
                load.interruptible_mw = 20.0;
                load.max_interruptions = hours / 8;
                load.interruption_cost = 120.0;
            }
            // Inference: not flexible at all.
            _ => {}
        }
        net.add_load(load);
    }

    // Wind that comes and goes, so the flexibility has something to chase.
    let rows: Vec<Vec<f64>> = (0..net.generators.len())
        .map(|g| {
            if g % 2 == 1 {
                (0..hours)
                    .map(|t| if t % 4 < 2 { 0.9 } else { 0.05 })
                    .collect()
            } else {
                vec![1.0; hours]
            }
        })
        .collect();
    net.gen_availability = TimeSeries::from_rows(&rows, hours).unwrap();
    net
}

#[test]
#[ignore = "minutes; run explicitly for numbers"]
fn a_fleet_of_three_thousand_sites() {
    println!("\n  sites  hours  buses    cols      rows     build     solve");
    for (buses, sites, hours) in [
        (64usize, 500usize, 24usize),
        (128, 1500, 24),
        (256, 3000, 24),
        (256, 3000, 168),
    ] {
        let net = fleet(buses, sites, hours);
        let t0 = Instant::now();
        let lopf = build_lopf(&net).unwrap();
        let build = t0.elapsed();
        let t1 = Instant::now();
        let sol = HighsSolver::default().solve(&lopf).unwrap();
        let solve = t1.elapsed();
        println!(
            "  {sites:5}  {hours:5}  {buses:5}  {:8}  {:8}  {:>8.1?}  {:>8.1?}  {:?}",
            lopf.model.num_cols(),
            lopf.model.num_rows(),
            build,
            solve,
            sol.status
        );
    }
}

#[test]
fn three_thousand_flexible_sites_build_and_solve() {
    // Not a benchmark: a check that the four kinds of flexibility coexist at
    // fleet scale, since each adds its own variables and rows and the
    // interruptible ones add binaries.
    let net = fleet(64, 3000, 12);
    let lopf = build_lopf(&net).unwrap();
    assert!(lopf.model.is_mip(), "contracted sites make this an integer problem");

    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    // Every kind actually appears.
    let shifting = lopf.vars.load_shift.iter().filter(|b| b.is_some()).count();
    let tranched = lopf.vars.demand_tranche.iter().filter(|b| !b.is_empty()).count();
    let contracted = lopf.vars.interrupt.iter().filter(|b| b.is_some()).count();
    assert_eq!(shifting, 750);
    assert_eq!(tranched, 750);
    assert_eq!(contracted, 750);
}

#[test]
fn the_fleet_uses_its_flexibility_rather_than_holding_it() {
    // Flexibility nobody exercises is a variable, not a feature. With wind
    // coming and going, the shiftable sites should move demand into the windy
    // hours.
    let net = fleet(32, 400, 24);
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let moved: f64 = lopf
        .vars
        .load_shift
        .iter()
        .flatten()
        .map(|b| sol.trajectory(*b).iter().map(|x| x.abs()).sum::<f64>())
        .sum();
    assert!(moved > 1.0, "no demand moved at all: {moved}");
}

#[test]
fn marginal_intensity_is_available_per_site_and_hour() {
    // The signal a carbon-aware scheduler would actually consume, and the
    // reason this use keeps coming up: it has to exist per bus per snapshot,
    // not as a system average.
    use gridwright_emissions::account;
    let net = fleet(16, 100, 12);
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    let flows = sol.emissions_input(&net, &lopf);
    let e = account(&net, flows.as_slices()).unwrap();

    assert_eq!(e.marginal_intensity.len(), net.buses.len());
    assert_eq!(e.marginal_intensity[0].len(), net.n_snapshots());
    // Wind is on the margin in some hours and gas in others, so the signal has
    // to vary. A constant would be useless to schedule against.
    let all: Vec<f64> = e.marginal_intensity.iter().flatten().copied().collect();
    let lo = all.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = all.iter().cloned().fold(0.0, f64::max);
    assert!(hi > lo, "marginal intensity is constant at {hi}");
}

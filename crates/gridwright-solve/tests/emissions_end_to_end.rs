//! Solve a network, then account its emissions, with nothing assembled by hand.
//!
//! The accounting crate is deliberately solver-agnostic and takes plain
//! numbers. This checks the bridge between the two actually lines up: a wrong
//! variable index here would produce numbers that look entirely reasonable.

#![cfg(feature = "highs")]

use gridwright_build::build_lopf;
use gridwright_emissions::account;
use gridwright_io::matpower::load_case;
use gridwright_net::{Generator, Line, Load, Network, Snapshots};
use gridwright_solve::{HighsSolver, Solver, Status};

/// Two countries, one interconnector, and only one of them owns any plant.
fn two_country_net() -> Network {
    let mut net = Network::new(Snapshots::hourly(4));
    let a = net.add_bus("A", "Exporter");
    let b = net.add_bus("B", "Importer");
    net.add_generator(Generator {
        name: "lignite".into(),
        bus: a,
        p_nom: 400.0,
        marginal_cost: 15.0,
        co2_emissions: 1.1,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "gas".into(),
        bus: b,
        p_nom: 50.0,
        marginal_cost: 90.0,
        co2_emissions: 0.4,
        ..Default::default()
    });
    net.add_load(Load { name: "la".into(), bus: a, p_set: 150.0, ..Default::default() });
    net.add_load(Load { name: "lb".into(), bus: b, p_set: 120.0, ..Default::default() });
    net.add_line(Line {
        name: "AB".into(),
        bus0: a,
        bus1: b,
        s_nom: 400.0,
        susceptance: 10.0,
        ..Default::default()
    });
    net
}

#[test]
fn consumption_emissions_add_back_up_to_production_emissions() {
    // The conservation identity that any correct tracing must satisfy: carbon
    // is only moved between accounts, never created or destroyed. If the
    // bridge from solution to accounting were misindexed, this is the check it
    // would fail.
    let net = two_country_net();
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let flows = sol.emissions_input(&net, &lopf);
    let e = account(&net, flows.as_slices()).unwrap();

    let produced: f64 = e.production_by_country.iter().map(|(_, v)| v).sum();
    let consumed: f64 = e.consumption_by_country.iter().map(|(_, v)| v).sum();
    assert!(
        (produced - consumed).abs() / produced.max(1.0) < 1e-6,
        "produced {produced} but consumed {consumed}"
    );
    assert!((produced - e.total).abs() < 1e-6);
    assert!(e.untraced.is_empty(), "everything here is reachable");
}

#[test]
fn the_importer_is_charged_for_carbon_it_did_not_emit() {
    // The whole reason for consumption accounting. B burns almost nothing at
    // home — its own plant is dear enough to stay off — and still owns a real
    // share of the system's carbon, because it imported power that a lignite
    // unit produced.
    let net = two_country_net();
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    let flows = sol.emissions_input(&net, &lopf);
    let e = account(&net, flows.as_slices()).unwrap();

    let find = |v: &Vec<(String, f64)>, c: &str| {
        v.iter().find(|(k, _)| k == c).map_or(0.0, |(_, x)| *x)
    };
    let prod_b = find(&e.production_by_country, "Importer");
    let cons_b = find(&e.consumption_by_country, "Importer");

    assert!(prod_b < 1.0, "B should not be running its gas: {prod_b}");
    assert!(
        cons_b > 100.0,
        "B consumed 480 MWh of lignite power and should own its carbon, got {cons_b}"
    );
    // 120 MW * 4 h * 1.1 t/MWh.
    assert!((cons_b - 528.0).abs() < 1e-6, "got {cons_b}");
}

#[test]
fn a_real_network_accounts_without_losing_anything() {
    // case118 has 54 generators and a meshed 186-line network, so the tracing
    // system is solved on something that actually has loops in it rather than
    // a toy path.
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/pglib/case118_ieee.m");
    let mut case = load_case(path).unwrap();
    // MATPOWER carries no emissions data, so attach a plausible spread: the
    // point is the accounting arithmetic, not the fuel mix.
    for (i, g) in case.network.generators.iter_mut().enumerate() {
        g.co2_emissions = match i % 3 {
            0 => 0.0,
            1 => 0.35,
            _ => 0.9,
        };
    }
    let net = &case.network;
    let lopf = build_lopf(net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let flows = sol.emissions_input(net, &lopf);
    let e = account(net, flows.as_slices()).unwrap();

    assert!(e.total > 0.0, "some of this fleet emits");
    let consumed: f64 = e.consumption_by_country.iter().map(|(_, v)| v).sum();
    assert!(
        (e.total - consumed).abs() / e.total < 1e-5,
        "meshed tracing lost carbon: emitted {} but accounted {consumed}",
        e.total
    );
    // Intensity is a physical quantity with a physical ceiling: no bus can be
    // dirtier than the dirtiest plant feeding the system.
    let worst = net.generators.iter().map(|g| g.co2_emissions).fold(0.0, f64::max);
    for (b, row) in e.intensity.iter().enumerate() {
        for (s, &i) in row.iter().enumerate() {
            assert!(
                (-1e-9..=worst + 1e-6).contains(&i),
                "bus {b} snapshot {s} has intensity {i}, outside [0, {worst}]"
            );
        }
    }
}

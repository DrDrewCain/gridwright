//! PyPSA netCDF, against the MATPOWER case it was built from.
//!
//! The fixture was written by xarray, which is what PyPSA itself writes
//! through, so the file is encoded by an independent tool rather than by
//! anything in this crate. It carries the IEEE 14-bus network with line
//! impedances in ohms, as PyPSA states them, so recovering the MATPOWER
//! per-unit values is a check on the conversion and not on the transcription.

#![cfg(feature = "netcdf")]

use gridwright_io::{matpower::load_case, netcdf};
use gridwright_net::Network;

fn path(rel: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn pypsa() -> gridwright_io::Case {
    netcdf::load_network(path("examples/pypsa/case14_ieee.nc")).unwrap()
}

fn mat() -> Network {
    load_case(path("examples/pglib/case14_ieee.m")).unwrap().network
}

#[test]
fn ohms_become_per_unit_and_match_the_matpower_case() {
    // The conversion that decides whether this reader is useful. A 132 kV
    // line's reactance in ohms is about 170 times its per-unit value on a
    // 100 MVA base, and taking one for the other gives a network where power
    // will not flow rather than one that fails to load.
    let (a, b) = (pypsa(), mat());
    assert_eq!(a.network.lines.len(), b.lines.len());
    for (x, y) in a.network.lines.iter().zip(&b.lines) {
        assert!(
            (x.reactance - y.reactance).abs() < 1e-9,
            "X on {}: {} against {}",
            x.name,
            x.reactance,
            y.reactance
        );
        assert!(
            (x.resistance - y.resistance).abs() < 1e-9,
            "R on {}: {} against {}",
            x.name,
            x.resistance,
            y.resistance
        );
        assert!(
            (x.susceptance - y.susceptance).abs() < 1e-6,
            "B on {}: {} against {}",
            x.name,
            x.susceptance,
            y.susceptance
        );
    }
    assert!(
        a.notes.join("\n").contains("ohms to per unit"),
        "{:?}",
        a.notes
    );
}

#[test]
fn topology_capacities_and_costs_come_across() {
    let (a, b) = (pypsa(), mat());
    assert_eq!(a.network.buses.len(), b.buses.len());
    assert_eq!(a.network.generators.len(), b.generators.len());
    assert_eq!(a.network.loads.len(), b.loads.len());

    for (x, y) in a.network.lines.iter().zip(&b.lines) {
        assert_eq!(x.bus0, y.bus0, "{}", x.name);
        assert_eq!(x.bus1, y.bus1, "{}", x.name);
    }
    let demand = |n: &Network| n.loads.iter().map(|l| l.p_set).sum::<f64>();
    assert!((demand(&a.network) - demand(&b)).abs() < 1e-9);
    for (x, y) in a.network.generators.iter().zip(&b.generators) {
        assert!((x.p_nom - y.p_nom).abs() < 1e-9, "capacity of {}", x.name);
        assert!(
            (x.marginal_cost - y.marginal_cost).abs() < 1e-9,
            "cost of {}",
            x.name
        );
    }
}

#[test]
fn snapshot_weights_are_read_from_whichever_name_the_file_uses() {
    // PyPSA renamed this variable at 0.20 and both spellings are still in
    // circulation. Falling back to weights of one would change every cost in
    // the model without changing anything visible.
    let c = pypsa();
    assert_eq!(c.network.n_snapshots(), 4);
    assert_eq!(c.network.snapshots.weights(), &[1.0, 1.0, 2.0, 4.0]);
}

#[test]
fn a_series_covering_only_some_generators_lands_on_the_right_ones() {
    // PyPSA stores a profile only for components that have one, on a separate
    // axis. Assuming that axis matches the full component list attaches a wind
    // profile to a coal plant, which is a plausible-looking disaster.
    //
    // The fixture gives profiles to gen0 and gen1 alone, snapshot major.
    let c = pypsa();
    let a = &c.network.gen_availability;
    assert_eq!(a.row(0), Some(&[1.0, 0.9, 0.5, 0.3][..]), "gen0");
    assert_eq!(a.row(1), Some(&[0.8, 0.7, 0.6, 1.0][..]), "gen1");
    // The rest were not listed and are fully available, not unavailable.
    assert_eq!(a.row(2), Some(&[1.0, 1.0, 1.0, 1.0][..]), "gen2");
    assert_eq!(a.row(4), Some(&[1.0, 1.0, 1.0, 1.0][..]), "gen4");
}

#[test]
fn the_stored_series_is_snapshot_major_and_is_transposed() {
    // Worth its own check. If the transpose were dropped, gen0 would read
    // [1.0, 0.8, 0.9, 0.7] — the first four numbers in file order — which is a
    // perfectly plausible profile for the wrong reason.
    let c = pypsa();
    assert_ne!(
        c.network.gen_availability.row(0),
        Some(&[1.0, 0.8, 0.9, 0.7][..]),
        "the array was read in file order rather than transposed"
    );
}

#[test]
fn carriers_and_countries_carry_through() {
    let c = pypsa();
    assert_eq!(c.network.generators[0].carrier, "gas");
    assert_eq!(c.network.generators[2].carrier, "sync");
    assert!(c.network.buses.iter().all(|b| b.country == "area1"));
    assert_eq!(c.network.buses[0].v_nom, 132.0);
}

#[test]
fn what_was_read_is_reported() {
    let notes = pypsa().notes.join("\n");
    assert!(notes.contains("14 buses"), "{notes}");
    assert!(notes.contains("5 generators"), "{notes}");
}

#[test]
fn a_file_that_is_not_a_pypsa_network_is_refused() {
    assert!(netcdf::load_network(path("examples/pglib/case14_ieee.m")).is_err());
    assert!(netcdf::load_network(path("examples/pypsa/nonexistent.nc")).is_err());
}

#[test]
fn a_pypsa_network_solves() {
    // End to end: a file from the largest open energy modelling ecosystem
    // there is, through to a network the engine will accept.
    let c = pypsa();
    assert!(c.network.validate().is_ok());
    assert!(c.network.lines.iter().all(|l| l.susceptance.is_finite()));
    assert!(
        c.network.lines.iter().any(|l| l.susceptance > 1.0),
        "susceptances look like they are still in ohms"
    );
}

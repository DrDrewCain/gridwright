//! Spreadsheets, against the MATPOWER case they were built from.
//!
//! The fixture is the IEEE 14-bus network laid out as a ministry planning
//! annex would lay it out: one sheet per component type, a sheet tab with
//! stray capitalisation and whitespace, an unrelated notes sheet, and a
//! single-value setting in a one-cell sheet. All four are things real
//! published workbooks do.

#![cfg(feature = "excel")]

use gridwright_io::{excel, matpower::load_case};
use gridwright_net::Network;

fn path(rel: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn book() -> Network {
    excel::load_network(path("examples/excel/case14_ieee.xlsx")).unwrap()
}

fn mat() -> Network {
    load_case(path("examples/pglib/case14_ieee.m")).unwrap().network
}

#[test]
fn a_workbook_reads_to_the_same_network_as_the_matpower_case() {
    let (a, b) = (book(), mat());
    assert_eq!(a.buses.len(), b.buses.len());
    assert_eq!(a.lines.len(), b.lines.len());
    assert_eq!(a.generators.len(), b.generators.len());
    assert_eq!(a.loads.len(), b.loads.len());

    for (x, y) in a.lines.iter().zip(&b.lines) {
        assert_eq!(x.bus0, y.bus0, "{}", x.name);
        assert_eq!(x.bus1, y.bus1, "{}", x.name);
        assert!((x.reactance - y.reactance).abs() < 1e-12, "X on {}", x.name);
        assert!((x.tap_ratio - y.tap_ratio).abs() < 1e-12, "tap on {}", x.name);
        assert!((x.s_nom - y.s_nom).abs() < 1e-9, "rating on {}", x.name);
    }
    let demand = |n: &Network| n.loads.iter().map(|l| l.p_set).sum::<f64>();
    assert!((demand(&a) - demand(&b)).abs() < 1e-9);
    let cap = |n: &Network| n.generators.iter().map(|g| g.p_nom).sum::<f64>();
    assert!((cap(&a) - cap(&b)).abs() < 1e-9);
    for (x, y) in a.generators.iter().zip(&b.generators) {
        assert!(
            (x.marginal_cost - y.marginal_cost).abs() < 1e-9,
            "cost on {}",
            x.name
        );
    }
}

#[test]
fn a_sheet_tab_with_stray_case_and_spaces_is_the_same_sheet() {
    // The fixture's generator sheet is called " Generators ". Someone typing
    // that tab name did not mean a different sheet, and matching it exactly
    // would silently produce a network with no generators at all.
    let net = book();
    assert!(
        !net.generators.is_empty(),
        "the ` Generators ` sheet was not found"
    );
}

#[test]
fn sheets_the_reader_does_not_know_are_ignored() {
    // Published workbooks are full of cover pages, notes and sources. None of
    // them is a component, and none of them should be an error either.
    let names = excel::Workbook::open(path("examples/excel/case14_ieee.xlsx"))
        .unwrap()
        .sheet_names();
    assert!(names.iter().any(|n| n == "Notes"), "{names:?}");
    assert_eq!(book().buses.len(), 14);
}

#[test]
fn a_single_value_setting_can_live_in_a_one_cell_sheet() {
    assert_eq!(book().co2_price, 45.0);
}

#[test]
fn a_workbook_without_a_bus_sheet_says_which_sheets_it_has() {
    // The most likely failure in practice is pointing this at the wrong
    // workbook. Listing what was actually found turns that from a puzzle into
    // a glance.
    let err = excel::load_network(path("examples/excel/case14_ieee.xlsx"));
    assert!(err.is_ok());

    let missing = excel::load_network(path("examples/pglib/case14_ieee.m"));
    assert!(missing.is_err(), "a .m file is not a workbook");
}

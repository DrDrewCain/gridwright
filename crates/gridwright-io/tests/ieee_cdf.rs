//! IEEE Common Data Format, cross-validated against the same network read from
//! MATPOWER.
//!
//! `examples/ieee_cdf/ieee14cdf.txt` is the IEEE 14-bus system written back
//! into the format it was originally published in, with the bus and branch
//! values placed in the columns the 1973 specification defines. The numbers are
//! therefore not independent of `case14_ieee.m` — but the *column positions*
//! are, and a fixed-width parser gets nothing else wrong. A misread column puts
//! a turns ratio where a rating belongs, and the comparison below catches it.
//!
//! `examples/ieee_cdf/conventions.cdf` covers what the 14-bus case does not:
//! an MVA base that is not one hundred, explicit ratings, a phase shifter and
//! a bus shunt.

use gridwright_io::{ieee_cdf::load_cdf, matpower::load_case};
use gridwright_net::{Line, Network};
use std::collections::HashMap;

fn path(rel: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn case14() -> gridwright_io::Case {
    load_cdf(path("examples/ieee_cdf/ieee14cdf.txt")).unwrap()
}

fn conventions() -> gridwright_io::Case {
    load_cdf(path("examples/ieee_cdf/conventions.cdf")).unwrap()
}

/// Branches keyed by their endpoints, so the two files may list them in any
/// order. Parallel circuits would need the circuit id too; case14 has none.
fn branch_map(net: &Network) -> HashMap<(usize, usize), &Line> {
    net.lines
        .iter()
        .map(|l| ((l.bus0.min(l.bus1), l.bus0.max(l.bus1)), l))
        .collect()
}

#[test]
fn the_common_data_format_fixture_describes_the_same_network_as_the_matpower_case() {
    let cdf = case14();
    let mat = load_case(path("examples/pglib/case14_ieee.m")).unwrap();

    assert_eq!(
        cdf.network.buses.len(),
        mat.network.buses.len(),
        "bus count"
    );
    assert_eq!(cdf.network.lines.len(), mat.network.lines.len(), "branches");
    assert_eq!(cdf.network.loads.len(), mat.network.loads.len(), "loads");

    let (a, b) = (branch_map(&cdf.network), branch_map(&mat.network));
    assert_eq!(a.len(), b.len(), "endpoints collided");
    for (key, m) in &b {
        let c = a
            .get(key)
            .unwrap_or_else(|| panic!("branch {key:?} missing from the CDF"));
        assert!(
            (c.resistance - m.resistance).abs() < 1e-9,
            "R on {key:?}: {} vs {}",
            c.resistance,
            m.resistance
        );
        assert!(
            (c.reactance - m.reactance).abs() < 1e-9,
            "X on {key:?}: {} vs {}",
            c.reactance,
            m.reactance
        );
        assert!(
            (c.shunt_susceptance - m.shunt_susceptance).abs() < 1e-9,
            "line charging on {key:?}: {} vs {}",
            c.shunt_susceptance,
            m.shunt_susceptance
        );
        assert!(
            (c.tap_ratio - m.tap_ratio).abs() < 1e-9,
            "tap on {key:?}: {} vs {}",
            c.tap_ratio,
            m.tap_ratio
        );
    }

    let demand = |n: &Network| n.loads.iter().map(|l| l.p_set).sum::<f64>();
    assert!(
        (demand(&cdf.network) - demand(&mat.network)).abs() < 1e-9,
        "{} vs {}",
        demand(&cdf.network),
        demand(&mat.network)
    );
}

#[test]
fn a_blank_rating_does_not_shift_the_turns_ratio_that_follows_it() {
    // The published case leaves all three MVA rating fields blank, so between
    // the line charging susceptance and the turns ratio there are twenty-five
    // columns of nothing. Split the record on whitespace and the ratio lands
    // where the rating should be: every transformer comes out at 1.0, the file
    // loads without a complaint, and the network is not the IEEE 14-bus system.
    let mut taps: Vec<f64> = case14()
        .network
        .lines
        .iter()
        .map(|l| l.tap_ratio)
        .filter(|t| (t - 1.0).abs() > 1e-9)
        .collect();
    taps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    assert_eq!(taps.len(), 3, "expected three tap-changers, got {taps:?}");
    for (got, want) in taps.iter().zip([0.932, 0.969, 0.978]) {
        assert!((got - want).abs() < 1e-9, "got {taps:?}");
    }
}

#[test]
fn a_blank_rating_means_unlimited_not_forbidden() {
    // The archive cases state no thermal ratings at all; that is why every
    // conversion of them, MATPOWER's included, had to invent some. A blank
    // must not become a line of zero capacity.
    let c = case14();
    assert!(
        c.network.lines.iter().all(|l| l.s_nom > 1e5),
        "a blank rating became a limit"
    );
    assert!(
        c.notes.iter().any(|n| n.contains("no MVA rating")),
        "{:?}",
        c.notes
    );
}

#[test]
fn a_bus_shunt_is_already_per_unit_and_is_not_divided_by_the_base_again() {
    // Bus 9 carries a 19 MVAr capacitor. MATPOWER states that as `Bs = 19.0`
    // in MVAr and its reader divides by the 100 MVA base to reach 0.19 per
    // unit; CDF states the same shunt as 0.19 per unit directly. Dividing
    // again would leave 0.0019, a capacitor a hundred times too small.
    let cdf = case14();
    let mat = load_case(path("examples/pglib/case14_ieee.m")).unwrap();
    let shunt = |n: &Network| n.buses.iter().map(|b| b.b_shunt).sum::<f64>();
    assert!(
        (shunt(&cdf.network) - 0.19).abs() < 1e-12,
        "{}",
        shunt(&cdf.network)
    );
    assert!((shunt(&cdf.network) - shunt(&mat.network)).abs() < 1e-12);
}

#[test]
fn a_voltage_controlled_bus_becomes_a_generator_carrying_its_reactive_band() {
    // There is no generator section in CDF: a machine exists because the bus
    // record says the bus holds voltage. The 14-bus has two real plants and
    // three synchronous condensers, which is five machines.
    let c = case14();
    assert_eq!(
        c.network.generators.len(),
        5,
        "{:?}",
        c.network
            .generators
            .iter()
            .map(|g| &g.name)
            .collect::<Vec<_>>()
    );
    let at = |name: &str| {
        c.network
            .generators
            .iter()
            .find(|g| g.name == name)
            .unwrap_or_else(|| panic!("{name} is missing"))
    };
    // Bus 2 states +50.0 / -40.0 MVAr in columns 91-98 and 99-106.
    assert_eq!(at("gen2").q_max, 50.0);
    assert_eq!(at("gen2").q_min, -40.0);
    // Bus 1 states 0.0 for both, which is how the archive writes "not stated";
    // read literally it would forbid the swing bus any reactive power at all.
    assert!(at("gen1").q_max.is_infinite());
    assert!(at("gen1").q_min.is_infinite());
}

#[test]
fn the_mva_base_comes_from_the_title_card_and_is_not_assumed() {
    // `conventions.cdf` declares 250 MVA in columns 32-37. Everything per unit
    // in the file is on that base, and assuming a hundred would misread every
    // impedance in the case by a factor of two and a half.
    assert_eq!(case14().network.base_mva, 100.0);
    let c = conventions();
    assert_eq!(c.network.base_mva, 250.0);
    assert!(
        c.notes.iter().any(|n| n.contains("base 250 MVA")),
        "{:?}",
        c.notes
    );
}

#[test]
fn a_rating_abutting_the_field_before_it_is_still_its_own_field() {
    // A Fortran `F10.6` charging susceptance of 0.02 fills columns 41-50
    // exactly, so the 150 MVA rating in 51-55 touches it with no space
    // between. Whitespace splitting sees one token, `0.020000150`.
    let c = conventions();
    let l = c
        .network
        .lines
        .iter()
        .find(|l| l.name == "1-2-1")
        .expect("branch 1-2 is missing");
    assert!(
        (l.shunt_susceptance - 0.02).abs() < 1e-12,
        "{}",
        l.shunt_susceptance
    );
    assert!((l.s_nom - 150.0).abs() < 1e-12, "rating {}", l.s_nom);
}

#[test]
fn a_phase_shifter_carries_its_angle_in_radians() {
    // Columns 84-90 hold the final angle in degrees, and every trigonometric
    // identity in the formulation wants radians. Branch 3-4 shifts by -3.50
    // degrees, which is -3.5 * pi / 180.
    let c = conventions();
    let l = c
        .network
        .lines
        .iter()
        .find(|l| l.name == "3-4-1")
        .expect("the phase shifter is missing");
    assert!(
        (l.phase_shift - (-3.5f64).to_radians()).abs() < 1e-12,
        "shift {}",
        l.phase_shift
    );
    // And its fixed-tap neighbour keeps its ratio and no shift.
    let t = c
        .network
        .lines
        .iter()
        .find(|l| l.name == "2-3-1")
        .expect("the fixed-tap transformer is missing");
    assert!((t.tap_ratio - 1.025).abs() < 1e-12, "tap {}", t.tap_ratio);
    assert_eq!(t.phase_shift, 0.0);
}

#[test]
fn a_bus_shunt_conductance_and_susceptance_both_arrive() {
    // Columns 107-114 and 115-122, the last two numeric fields on the record,
    // which is exactly where a reader that has drifted by one column shows it.
    let c = conventions();
    let b = c
        .network
        .buses
        .iter()
        .find(|b| b.name.starts_with("WORKS"))
        .expect("bus 4 is missing");
    assert!((b.g_shunt - 0.02).abs() < 1e-12, "G {}", b.g_shunt);
    assert!((b.b_shunt - 0.15).abs() < 1e-12, "B {}", b.b_shunt);
    assert!((b.v_nom - 132.0).abs() < 1e-12, "base kV {}", b.v_nom);
}

#[test]
fn what_the_format_cannot_carry_is_reported_rather_than_filled_in() {
    // A reader that silently leaves every marginal cost at zero produces a
    // dispatch that looks like an answer and is arbitrary. Saying so is the
    // difference between a limitation and a lie.
    let notes = case14().notes.join("\n");
    assert!(notes.contains("no generation costs"), "{notes}");
    assert!(notes.contains("no generator capacity limits"), "{notes}");
    assert!(notes.contains("interchange"), "{notes}");
    assert!(
        case14()
            .network
            .generators
            .iter()
            .all(|g| g.marginal_cost == 0.0)
    );
}

#[test]
fn a_common_data_format_case_is_a_valid_network() {
    assert!(case14().network.validate().is_ok());
    assert!(conventions().network.validate().is_ok());
}

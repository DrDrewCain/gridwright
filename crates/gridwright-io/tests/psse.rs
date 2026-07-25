//! PSS/E RAW, cross-validated against the same network read from MATPOWER.
//!
//! The fixtures under `examples/psse` were written by placing IEEE 14-bus
//! values into the columns the format specification defines. The numbers are
//! therefore not independent of `case14_ieee.m` — but the *column positions*
//! are, and that is what a RAW parser gets wrong. A misread column puts a
//! reactance where a rating belongs, and the comparison below catches it.
//!
//! Both a v33 and a v29 fixture are checked. They are not cosmetic variants:
//! v29 keeps transformers in the branch section with the tap ratio inline and
//! keeps bus shunts on the bus record, so the two files exercise genuinely
//! different code paths and must still produce the same network.

use gridwright_io::{matpower::load_case, psse::load_raw};
use gridwright_net::{Line, Network};
use std::collections::HashMap;

fn path(rel: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
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
fn the_v33_fixture_describes_the_same_network_as_the_matpower_case() {
    let raw = load_raw(path("examples/psse/case14_v33.raw")).unwrap();
    let mat = load_case(path("examples/pglib/case14_ieee.m")).unwrap();

    assert_eq!(raw.network.buses.len(), mat.network.buses.len(), "bus count");
    assert_eq!(
        raw.network.lines.len(),
        mat.network.lines.len(),
        "branch count"
    );
    assert_eq!(raw.network.generators.len(), mat.network.generators.len());
    assert_eq!(raw.network.loads.len(), mat.network.loads.len());

    let (a, b) = (branch_map(&raw.network), branch_map(&mat.network));
    assert_eq!(a.len(), b.len(), "endpoints collided");
    for (key, m) in &b {
        let r = a
            .get(key)
            .unwrap_or_else(|| panic!("branch {key:?} missing from the RAW"));
        assert!(
            (r.resistance - m.resistance).abs() < 1e-9,
            "R on {key:?}: {} vs {}",
            r.resistance,
            m.resistance
        );
        assert!(
            (r.reactance - m.reactance).abs() < 1e-9,
            "X on {key:?}: {} vs {}",
            r.reactance,
            m.reactance
        );
        assert!(
            (r.shunt_susceptance - m.shunt_susceptance).abs() < 1e-6,
            "B on {key:?}"
        );
        assert!(
            (r.s_nom - m.s_nom).abs() < 1e-6,
            "rating on {key:?}: {} vs {}",
            r.s_nom,
            m.s_nom
        );
        assert!(
            (r.tap_ratio - m.tap_ratio).abs() < 1e-9,
            "tap on {key:?}: {} vs {}",
            r.tap_ratio,
            m.tap_ratio
        );
    }
}

#[test]
fn tap_ratios_survive_the_transformer_section() {
    // The three tap-changers are the reason the transformer section exists.
    // Reading them as 1.0 describes a network that is not the IEEE 14-bus.
    let raw = load_raw(path("examples/psse/case14_v33.raw")).unwrap();
    let mut taps: Vec<f64> = raw
        .network
        .lines
        .iter()
        .map(|l| l.tap_ratio)
        .filter(|t| (t - 1.0).abs() > 1e-9)
        .collect();
    taps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert_eq!(taps.len(), 3, "expected three tap-changers, got {taps:?}");
    for (got, want) in taps.iter().zip([0.932, 0.969, 0.978]) {
        assert!((got - want).abs() < 1e-9, "got {taps:?}");
    }
}

#[test]
fn loads_and_generation_match_the_matpower_case() {
    let raw = load_raw(path("examples/psse/case14_v33.raw")).unwrap();
    let mat = load_case(path("examples/pglib/case14_ieee.m")).unwrap();

    let demand = |n: &Network| n.loads.iter().map(|l| l.p_set).sum::<f64>();
    assert!(
        (demand(&raw.network) - demand(&mat.network)).abs() < 1e-6,
        "{} vs {}",
        demand(&raw.network),
        demand(&mat.network)
    );
    let cap = |n: &Network| n.generators.iter().map(|g| g.p_nom).sum::<f64>();
    assert!((cap(&raw.network) - cap(&mat.network)).abs() < 1e-6);
}

#[test]
fn the_v29_layout_reads_to_the_same_network_as_v33() {
    // v29 puts the tap ratio in branch column 9, where v30 onward keeps a
    // shunt conductance. Applying the wrong offsets gives every transformer a
    // ratio of 1.0 and a bogus conductance, and both files load without error,
    // which is exactly why this comparison exists.
    let old = load_raw(path("examples/psse/case14_v29.raw")).unwrap();
    let new = load_raw(path("examples/psse/case14_v33.raw")).unwrap();

    assert_eq!(old.network.buses.len(), new.network.buses.len());
    assert_eq!(old.network.lines.len(), new.network.lines.len());

    let (a, b) = (branch_map(&old.network), branch_map(&new.network));
    for (key, n) in &b {
        let o = a
            .get(key)
            .unwrap_or_else(|| panic!("branch {key:?} missing from v29"));
        assert!((o.reactance - n.reactance).abs() < 1e-9, "X on {key:?}");
        assert!(
            (o.tap_ratio - n.tap_ratio).abs() < 1e-9,
            "tap on {key:?}: v29 {} vs v33 {}",
            o.tap_ratio,
            n.tap_ratio
        );
        assert!((o.s_nom - n.s_nom).abs() < 1e-6, "rating on {key:?}");
    }
}

#[test]
fn what_was_dropped_is_reported() {
    let raw = load_raw(path("examples/psse/case14_v33.raw")).unwrap();
    let notes = raw.notes.join("\n");
    assert!(notes.contains("revision 33"), "{notes}");
    assert!(
        notes.contains("fixed shunt"),
        "the bus 9 shunt should be reported: {notes}"
    );
    assert!(notes.contains("no generator costs"), "{notes}");
}

#[test]
fn a_raw_case_is_a_valid_network() {
    let raw = load_raw(path("examples/psse/case14_v33.raw")).unwrap();
    assert!(raw.network.validate().is_ok());
    assert_eq!(raw.network.base_mva, 100.0);
}

// --- Conventions the IEEE 14-bus fixture does not exercise. ---

fn conventions() -> gridwright_io::Case {
    load_raw(path("examples/psse/conventions.raw")).unwrap()
}

#[test]
fn a_winding_voltage_in_kilovolts_becomes_a_per_unit_tap() {
    // CW = 2 gives winding voltages in kV. Read as CW = 1 this transformer
    // would have a tap of 525/230 = 2.28 instead of 1.05, which is not a
    // rounding difference: it is a different network.
    let c = conventions();
    let t = c
        .network
        .lines
        .iter()
        .find(|l| l.name.starts_with("CW2TAP"))
        .expect("the CW=2 transformer is missing");
    assert!((t.tap_ratio - 1.05).abs() < 1e-9, "tap {}", t.tap_ratio);
    assert!((t.reactance - 0.06).abs() < 1e-9);
}

#[test]
fn impedances_on_a_winding_base_are_rebased_onto_the_system_base() {
    // The three-winding transformer declares CZ = 2 with a 50 MVA winding
    // base against a 100 MVA system base, so every impedance doubles. Skipping
    // the rebase halves every reactance through this transformer and roughly
    // doubles the power it appears able to carry.
    //
    // Hand-derived. After rebasing: X12 = 0.10, X23 = 0.24, X31 = 0.20, so the
    // star arms are (X12+X31-X23)/2 = 0.03, (X12+X23-X31)/2 = 0.07 and
    // (X23+X31-X12)/2 = 0.17.
    let c = conventions();
    let mut arms: Vec<f64> = c
        .network
        .lines
        .iter()
        .filter(|l| l.name.starts_with("STAR3W_w"))
        .map(|l| l.reactance)
        .collect();
    assert_eq!(arms.len(), 3, "expected three star arms, got {arms:?}");
    arms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    for (got, want) in arms.iter().zip([0.03, 0.07, 0.17]) {
        assert!((got - want).abs() < 1e-9, "star arms {arms:?}");
    }
}

#[test]
fn a_three_winding_transformer_meets_at_a_point_that_is_not_a_bus() {
    // Its windings join at a star point. Collapsing that into three
    // bus-to-bus branches would let power enter on one winding and leave on
    // another without passing through the common impedance.
    let c = conventions();
    let star = c
        .network
        .buses
        .iter()
        .position(|b| b.name == "STAR3W_star")
        .expect("no star bus was created");
    let touching: Vec<&str> = c
        .network
        .lines
        .iter()
        .filter(|l| l.bus0 == star || l.bus1 == star)
        .map(|l| l.name.as_str())
        .collect();
    assert_eq!(touching.len(), 3, "star point has {touching:?}");
}

#[test]
fn an_hvdc_link_becomes_a_corridor_with_no_angle_relationship() {
    // A model of China or India that drops the HVDC is not a model of China
    // or India. The link must arrive, and it must arrive without a
    // susceptance, because that is the whole point of running it DC.
    let c = conventions();
    let dc = c
        .network
        .lines
        .iter()
        .find(|l| l.name.starts_with("dc_"))
        .expect("the DC link is missing");
    assert_eq!(dc.susceptance, 0.0, "a DC link imposes no angle relationship");
    assert!((dc.s_nom - 3000.0).abs() < 1e-9, "rating {}", dc.s_nom);
    assert!(dc.is_transport());
}

#[test]
fn out_of_service_and_disconnected_records_are_left_out() {
    let c = conventions();
    // Bus 9 is type 4, disconnected, and the branch reaching it goes with it.
    assert!(c.network.buses.iter().all(|b| b.name != "OFFLINE"));
    // The second generator at bus 4 is out of service.
    assert_eq!(c.network.generators.len(), 2, "{:?}",
               c.network.generators.iter().map(|g| &g.name).collect::<Vec<_>>());
    // One load record is out of service, and two at bus 3 merge into one.
    assert_eq!(c.network.loads.len(), 2);
    let total: f64 = c.network.loads.iter().map(|l| l.p_set).sum();
    assert!((total - 350.0).abs() < 1e-9, "demand {total}");
}

#[test]
fn several_load_records_at_one_bus_become_one_demand() {
    let c = conventions();
    let at3 = c
        .network
        .loads
        .iter()
        .find(|l| c.network.buses[l.bus].name == "DIST033")
        .unwrap();
    assert!((at3.p_set - 200.0).abs() < 1e-9, "got {}", at3.p_set);
}

#[test]
fn a_must_run_floor_survives_as_a_fraction_of_capacity() {
    let c = conventions();
    let g = c.network.generators.iter().find(|g| g.name.starts_with("gen1")).unwrap();
    assert!((g.p_nom - 1200.0).abs() < 1e-9);
    assert!((g.p_min_pu - 200.0 / 1200.0).abs() < 1e-9, "{}", g.p_min_pu);
}

#[test]
fn areas_carry_through_as_countries() {
    // In a multi-country RAW case the area code is how the countries are
    // distinguished, and dropping it makes every cross-border flow invisible.
    let c = conventions();
    let remote = c.network.buses.iter().find(|b| b.name == "REMOTE500").unwrap();
    assert_eq!(remote.country, "area2");
    let home = c.network.buses.iter().find(|b| b.name == "HV500").unwrap();
    assert_eq!(home.country, "area1");
}

#[test]
fn voltage_dependent_load_components_are_reported_not_silently_dropped() {
    let c = conventions();
    let notes = c.notes.join("\n");
    assert!(notes.contains("voltage-dependent"), "{notes}");
    assert!(notes.contains("star point"), "{notes}");
}

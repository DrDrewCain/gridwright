//! PSS/E RAWX, against the same network read from MATPOWER.
//!
//! RAWX is the same data model as RAW in a different encoding. Where RAW is
//! positional — a value's meaning depends on its column and on which revision
//! wrote the file — RAWX names every field, which removes the entire class of
//! problem the RAW reader spends most of its length guarding against.
//!
//! The fixture holds IEEE 14-bus values under their RAWX field names, so what
//! is being tested is whether the reader finds the right field, not whether the
//! numbers were copied correctly.

#![cfg(feature = "json")]

use gridwright_io::{Format, load_any, matpower::load_case, rawx};
use gridwright_net::Network;

fn path(rel: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn from_rawx() -> gridwright_io::Case {
    rawx::load_rawx(path("examples/rawx/case14_ieee.rawx")).unwrap()
}

fn from_matpower() -> Network {
    load_case(path("examples/pglib/case14_ieee.m")).unwrap().network
}

#[test]
fn a_rawx_case_describes_the_same_network_as_the_matpower_one() {
    let (a, b) = (from_rawx(), from_matpower());
    assert_eq!(a.network.buses.len(), b.buses.len());
    assert_eq!(a.network.lines.len(), b.lines.len());
    assert_eq!(a.network.generators.len(), b.generators.len());
    assert_eq!(a.network.loads.len(), b.loads.len());

    // Endpoints and impedances, keyed rather than positional, since the reader
    // takes lines and transformers from two different sections.
    let map = |n: &Network| {
        let mut v: Vec<(usize, usize, String, String)> = n
            .lines
            .iter()
            .map(|l| {
                (
                    l.bus0.min(l.bus1),
                    l.bus0.max(l.bus1),
                    format!("{:.9}", l.reactance),
                    format!("{:.6}", l.tap_ratio),
                )
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(map(&a.network), map(&b));

    let demand = |n: &Network| n.loads.iter().map(|l| l.p_set).sum::<f64>();
    assert!((demand(&a.network) - demand(&b)).abs() < 1e-6);
    let cap = |n: &Network| n.generators.iter().map(|g| g.p_nom).sum::<f64>();
    assert!((cap(&a.network) - cap(&b)).abs() < 1e-6);
}

#[test]
fn the_three_tap_changers_come_from_the_transformer_section() {
    // RAWX flattens the four-line RAW transformer record into one row, which is
    // the single largest simplification the format brings and the place a
    // reader is most likely to lose the ratio.
    let c = from_rawx();
    let mut taps: Vec<f64> = c
        .network
        .lines
        .iter()
        .map(|l| l.tap_ratio)
        .filter(|t| (t - 1.0).abs() > 1e-9)
        .collect();
    taps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert_eq!(taps.len(), 3, "{taps:?}");
    for (got, want) in taps.iter().zip([0.932, 0.969, 0.978]) {
        assert!((got - want).abs() < 1e-9, "{taps:?}");
    }
}

#[test]
fn fields_are_found_by_name_rather_than_by_position() {
    // The whole point of the format. A document listing its columns in a
    // different order, or carrying one this reader has never heard of, must
    // read identically.
    let text = std::fs::read_to_string(path("examples/rawx/case14_ieee.rawx")).unwrap();
    let mut doc: serde_json::Value = serde_json::from_str(&text).unwrap();

    let bus = doc["network"]["bus"].as_object_mut().unwrap();
    let fields: Vec<String> = bus["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let rows: Vec<Vec<serde_json::Value>> = bus["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_array().unwrap().clone())
        .collect();

    // Reverse the column order, and append a field nobody here knows.
    let mut reversed: Vec<String> = fields.iter().rev().cloned().collect();
    reversed.push("some_future_column".into());
    let new_rows: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let mut v: Vec<serde_json::Value> = r.iter().rev().cloned().collect();
            v.push(serde_json::json!("ignored"));
            serde_json::Value::Array(v)
        })
        .collect();
    bus["fields"] = serde_json::json!(reversed);
    bus["data"] = serde_json::Value::Array(new_rows);

    let shuffled = rawx::parse_rawx(&doc.to_string(), "shuffled").unwrap();
    let plain = from_rawx();
    assert_eq!(shuffled.network.buses.len(), plain.network.buses.len());
    for (a, b) in shuffled.network.buses.iter().zip(&plain.network.buses) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.v_nom, b.v_nom);
        assert_eq!(a.country, b.country);
    }
}

#[test]
fn three_json_dialects_are_told_apart() {
    // RAWX, PowerModels and this crate's own format all end in `.json` or are
    // handed over as bare buffers. Reading one as another gives a load failure
    // at best and a network whose demand is off by a hundredfold at worst.
    assert_eq!(
        gridwright_io::sniff(path("examples/rawx/case14_ieee.rawx")).unwrap(),
        Format::Rawx
    );
    assert_eq!(
        gridwright_io::sniff(path("examples/powermodels/case14_ieee.json")).unwrap(),
        Format::PowerModels
    );

    // And from bytes, with no extension to lean on.
    let bytes = std::fs::read(path("examples/rawx/case14_ieee.rawx")).unwrap();
    assert_eq!(
        gridwright_io::sniff_bytes(None, &bytes).unwrap(),
        Format::Rawx
    );
}

#[test]
fn a_rawx_case_loads_through_one_call() {
    let case = load_any(path("examples/rawx/case14_ieee.rawx")).unwrap();
    assert_eq!(case.network.buses.len(), 14);
    assert!(case.notes[0].contains("RAWX"), "{:?}", case.notes);
    assert!(
        case.notes.join("\n").contains("no generator costs"),
        "{:?}",
        case.notes
    );
}

#[test]
fn out_of_service_records_are_left_out() {
    let text = std::fs::read_to_string(path("examples/rawx/case14_ieee.rawx")).unwrap();
    let mut doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    // Switch off the first AC line.
    let stat = doc["network"]["acline"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .position(|v| v == "stat")
        .unwrap();
    doc["network"]["acline"]["data"][0][stat] = serde_json::json!(0);

    let c = rawx::parse_rawx(&doc.to_string(), "off").unwrap();
    assert_eq!(c.network.lines.len(), from_rawx().network.lines.len() - 1);
    assert!(c.notes.join("\n").contains("skipped"), "{:?}", c.notes);
}

#[test]
fn something_that_is_not_rawx_is_refused() {
    assert!(rawx::parse_rawx("{}", "x").is_err());
    assert!(rawx::parse_rawx("{\"network\": {}}", "x").is_err());
    assert!(rawx::parse_rawx("not json", "x").is_err());
}

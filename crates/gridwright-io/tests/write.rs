//! Writers, checked by reading back what they wrote.
//!
//! A writer's only real test is the round trip, and the strong version of it
//! uses the reader that already agrees with an independent source. The
//! MATPOWER reader is cross-validated against PSS/E and PyPSA encodings of the
//! same network, so a case that survives a write and a read is a case that
//! survived something.
//!
//! What a writer drops is as much a part of its contract as what it keeps, and
//! each says so rather than leaving it to be discovered by comparing files.

use gridwright_io::{load_any, matpower, psse, to_matpower, to_psse};
use gridwright_net::{Generator, Line, Load, Network, Snapshots, StorageUnit};

fn path(rel: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn case14() -> Network {
    load_any(path("examples/pglib/case14_ieee.m")).unwrap().network
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join("gridwright-write");
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

#[test]
fn a_matpower_case_survives_being_written_and_read() {
    let original = case14();
    let file = tmp("case14_out.m");
    let notes = gridwright_io::write_matpower(&original, &file).unwrap();
    let back = matpower::load_case(&file).unwrap().network;

    assert_eq!(back.buses.len(), original.buses.len());
    assert_eq!(back.lines.len(), original.lines.len());
    assert_eq!(back.generators.len(), original.generators.len());
    assert_eq!(back.loads.len(), original.loads.len(), "{notes:?}");

    for (a, b) in back.lines.iter().zip(&original.lines) {
        assert_eq!(a.bus0, b.bus0);
        assert_eq!(a.bus1, b.bus1);
        assert!((a.reactance - b.reactance).abs() < 1e-9, "X on {}", a.name);
        assert!((a.resistance - b.resistance).abs() < 1e-9, "R on {}", a.name);
        assert!((a.tap_ratio - b.tap_ratio).abs() < 1e-9, "tap on {}", a.name);
        assert!((a.s_nom - b.s_nom).abs() < 1e-6, "rating on {}", a.name);
    }
    for (a, b) in back.generators.iter().zip(&original.generators) {
        assert!((a.p_nom - b.p_nom).abs() < 1e-9);
        assert!(
            (a.marginal_cost - b.marginal_cost).abs() < 1e-9,
            "cost: {} against {}",
            a.marginal_cost,
            b.marginal_cost
        );
    }
    let demand = |n: &Network| n.loads.iter().map(|l| l.p_set).sum::<f64>();
    assert!((demand(&back) - demand(&original)).abs() < 1e-6);
}

#[test]
fn a_psse_case_survives_being_written_and_read() {
    let original = case14();
    let file = tmp("case14_out.raw");
    gridwright_io::write_psse(&original, &file).unwrap();
    let back = psse::load_raw(&file).unwrap().network;

    assert_eq!(back.buses.len(), original.buses.len());
    assert_eq!(back.lines.len(), original.lines.len());
    assert_eq!(back.generators.len(), original.generators.len());

    // Endpoints, since the writer splits lines and transformers across two
    // sections and getting either wrong loses branches.
    let key = |n: &Network| {
        let mut v: Vec<(usize, usize)> = n
            .lines
            .iter()
            .map(|l| (l.bus0.min(l.bus1), l.bus0.max(l.bus1)))
            .collect();
        v.sort();
        v
    };
    assert_eq!(key(&back), key(&original));

    let x = |n: &Network| {
        let mut v: Vec<String> = n.lines.iter().map(|l| format!("{:.9}", l.reactance)).collect();
        v.sort();
        v
    };
    assert_eq!(x(&back), x(&original), "reactances changed");
}

#[test]
fn the_three_tap_changers_survive_the_psse_transformer_section() {
    // The branches that go through the other section of the file. Losing their
    // ratio would describe a network that is not the IEEE 14-bus.
    let original = case14();
    let file = tmp("case14_taps.raw");
    gridwright_io::write_psse(&original, &file).unwrap();
    let back = psse::load_raw(&file).unwrap().network;

    let taps = |n: &Network| {
        let mut v: Vec<String> = n
            .lines
            .iter()
            .map(|l| l.tap_ratio)
            .filter(|t| (t - 1.0).abs() > 1e-9)
            .map(|t| format!("{t:.6}"))
            .collect();
        v.sort();
        v
    };
    assert_eq!(taps(&back), taps(&original));
    assert_eq!(taps(&back).len(), 3);
}

#[test]
fn what_a_format_cannot_hold_is_reported() {
    // The contract that matters as much as the round trip. A writer that
    // silently dropped storage would produce a file someone trusted.
    let mut net = case14();
    net.add_storage(StorageUnit {
        name: "batt".into(),
        bus: 0,
        p_nom: 50.0,
        max_hours: 4.0,
        ..Default::default()
    });
    net.loads[0].shiftable_pu = 0.3;

    let m = to_matpower(&net, "case");
    let joined = m.notes.join("\n");
    assert!(joined.contains("storage"), "{:?}", m.notes);
    assert!(joined.contains("shiftable"), "{:?}", m.notes);

    let p = to_psse(&net);
    let joined = p.notes.join("\n");
    assert!(joined.contains("no generator costs"), "{:?}", p.notes);
    assert!(joined.contains("storage"), "{:?}", p.notes);
}

#[test]
fn a_horizon_longer_than_one_snapshot_is_reported_not_silently_truncated() {
    // Both formats hold a single operating point. Writing the first snapshot
    // is the only thing to do; not saying so is not.
    let mut net = Network::new(Snapshots::hourly(24));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "g".into(),
        bus: b,
        p_nom: 100.0,
        marginal_cost: 10.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 50.0,
        ..Default::default()
    });
    assert!(to_matpower(&net, "x").notes.join("\n").contains("24 snapshots"));
    assert!(to_psse(&net).notes.join("\n").contains("24 snapshots"));
}

#[test]
fn every_synchronous_area_gets_exactly_one_reference_bus() {
    // A case with no angle datum will not solve in anyone else's tool either,
    // and two data in one area is the same problem from the other side.
    let mut net = Network::new(Snapshots::hourly(1));
    for (i, area) in ["east", "east", "west", "west"].iter().enumerate() {
        let b = net.add_bus(format!("b{i}"), "XX");
        net.buses[b].synchronous_area = (*area).into();
        net.add_generator(Generator {
            name: format!("g{i}"),
            bus: b,
            p_nom: 100.0,
            marginal_cost: 10.0,
            ..Default::default()
        });
        net.add_load(Load {
            name: format!("l{i}"),
            bus: b,
            p_set: 20.0,
            ..Default::default()
        });
    }
    net.add_line(Line {
        name: "e".into(),
        bus0: 0,
        bus1: 1,
        s_nom: 100.0,
        susceptance: 10.0,
        ..Default::default()
    });
    net.add_line(Line {
        name: "w".into(),
        bus0: 2,
        bus1: 3,
        s_nom: 100.0,
        susceptance: 10.0,
        ..Default::default()
    });

    let text = to_matpower(&net, "two_areas").text;
    let refs = text
        .lines()
        .skip_while(|l| !l.starts_with("mpc.bus"))
        .take_while(|l| !l.starts_with("];"))
        .filter(|l| l.split('\t').nth(2) == Some("3"))
        .count();
    assert_eq!(refs, 2, "one reference per area, got {refs}:\n{text}");
}

#[test]
fn written_numbers_are_never_in_exponential_notation() {
    // A MATPOWER or PSS/E parser is within its rights to reject `1e-05`, and
    // Rust's default formatting produces exactly that for small magnitudes.
    // Line resistances are routinely that small.
    let mut net = case14();
    net.lines[0].resistance = 0.0000123;
    net.buses[0].b_shunt = 1e-7;

    for text in [to_matpower(&net, "x").text, to_psse(&net).text] {
        assert!(
            !text.contains('e') || !text.lines().any(|l| l.contains("e-") || l.contains("e+")),
            "exponential notation was written"
        );
    }
}

#[test]
fn a_written_case_is_recognised_by_the_format_sniffer() {
    // Conversion is only useful if the result is usable, and the first thing
    // anything does with a file is work out what it is.
    let net = case14();
    let m = tmp("sniffed.m");
    gridwright_io::write_matpower(&net, &m).unwrap();
    assert_eq!(gridwright_io::sniff(&m).unwrap(), gridwright_io::Format::Matpower);

    let r = tmp("sniffed.raw");
    gridwright_io::write_psse(&net, &r).unwrap();
    assert_eq!(gridwright_io::sniff(&r).unwrap(), gridwright_io::Format::Psse);
}

#[test]
fn a_network_can_cross_between_two_formats_it_did_not_start_in() {
    // The point of writers existing at all: read PyPSA, write PSS/E, and hand
    // the result to somebody whose tools speak neither of the others.
    let start = load_any(path("examples/pypsa/case14_ieee.nc")).unwrap().network;
    let file = tmp("from_pypsa.raw");
    gridwright_io::write_psse(&start, &file).unwrap();
    let back = load_any(&file).unwrap().network;

    assert_eq!(back.buses.len(), start.buses.len());
    assert_eq!(back.lines.len(), start.lines.len());
    let demand = |n: &Network| n.loads.iter().map(|l| l.p_set).sum::<f64>();
    assert!((demand(&back) - demand(&start)).abs() < 1e-6);
}

#[test]
fn a_pypsa_directory_is_written_in_pypsa_column_names() {
    // The interop path that exists, since writing PyPSA's netCDF does not: a
    // conformant `.nc` needs HDF5 dimension scales, which need object
    // references, which the pure-Rust HDF5 library can describe and not emit.
    // A file xarray refuses to open would be worse than none.
    let net = case14();
    let dir = tmp("pypsa");
    let _ = std::fs::remove_dir_all(&dir);
    gridwright_io::write_pypsa_csv(&net, &dir).unwrap();

    for f in ["buses.csv", "generators.csv", "lines.csv", "loads.csv", "snapshots.csv"] {
        assert!(dir.join(f).exists(), "{f} was not written");
    }

    // PyPSA's names, not this crate's. Writing `susceptance` where PyPSA reads
    // `x` gives an empty column and a network of zero-impedance lines.
    let lines = std::fs::read_to_string(dir.join("lines.csv")).unwrap();
    let header = lines.lines().next().unwrap();
    for column in ["name", "bus0", "bus1", "x", "r", "s_nom"] {
        assert!(
            header.split(',').any(|c| c == column),
            "lines.csv has no `{column}` column: {header}"
        );
    }
    let buses = std::fs::read_to_string(dir.join("buses.csv")).unwrap();
    assert!(buses.lines().next().unwrap().contains("v_nom"));
}

/// The PSS/E fixture, which carries genuine base voltages.
///
/// PGLib normalises MATPOWER's `baseKV` to 1.0, so the IEEE cases are no use
/// for testing a conversion that depends on it. The RAW fixture states 132 kV
/// on the transmission core and 33 or 11 on the rest, which is what the network
/// is actually drawn at.
fn case14_with_voltages() -> Network {
    gridwright_io::psse::load_raw(path("examples/psse/case14_v33.raw"))
        .unwrap()
        .network
}

#[test]
fn impedance_is_written_back_in_ohms() {
    // The trap in reverse. PyPSA states impedance in ohms and everything here
    // is per unit, so a value written straight through describes a line that
    // is very nearly a short circuit — which will not fail, it will produce
    // answers.
    //
    // Hand-derived: at 132 kV on a 100 MVA base the impedance base is
    // 132² / 100 = 174.24 Ω, so the first branch's 0.05917 per unit is
    // 10.3098 Ω.
    let net = case14_with_voltages();
    assert!(net.buses[0].v_nom > 100.0, "the fixture should carry real voltages");
    let dir = tmp("pypsa-ohms");
    let _ = std::fs::remove_dir_all(&dir);
    gridwright_io::write_pypsa_csv(&net, &dir).unwrap();

    let text = std::fs::read_to_string(dir.join("lines.csv")).unwrap();
    let header: Vec<&str> = text.lines().next().unwrap().split(',').collect();
    let xi = header.iter().position(|c| *c == "x").unwrap();
    let first = text.lines().nth(1).unwrap();
    let x: f64 = first.split(',').nth(xi).unwrap().parse().unwrap();

    let z_base = 132.0 * 132.0 / 100.0;
    let want = net.lines[0].reactance * z_base;
    assert!(
        (x - want).abs() < 1e-6,
        "wrote {x} Ω where {want} was expected; per unit it is {}",
        net.lines[0].reactance
    );
    // And it is emphatically not the per-unit number.
    assert!(x > 1.0, "{x} looks like a per-unit value written into an ohms field");
}

#[test]
fn a_pypsa_directory_round_trips_through_this_crate_too() {
    // Not the point of the writer, but a cheap check that the values are
    // internally consistent: reading the ohms back with the netCDF reader's
    // conversion should recover the per-unit ones.
    let net = case14_with_voltages();
    let dir = tmp("pypsa-rt");
    let _ = std::fs::remove_dir_all(&dir);
    gridwright_io::write_pypsa_csv(&net, &dir).unwrap();

    let text = std::fs::read_to_string(dir.join("lines.csv")).unwrap();
    let header: Vec<&str> = text.lines().next().unwrap().split(',').collect();
    let xi = header.iter().position(|c| *c == "x").unwrap();
    let vi = header.iter().position(|c| *c == "v_nom").unwrap();

    for (row, line) in text.lines().skip(1).zip(&net.lines) {
        let cells: Vec<&str> = row.split(',').collect();
        let x_ohm: f64 = cells[xi].parse().unwrap();
        let kv: f64 = cells[vi].parse().unwrap();
        let back = x_ohm / (kv * kv / 100.0);
        assert!(
            (back - line.reactance).abs() < 1e-9,
            "{} came back as {back} from {x_ohm} Ω at {kv} kV",
            line.name
        );
    }
}

//! JSON in both dialects.
//!
//! The PowerModels fixture was produced by putting the IEEE 14-bus case into
//! the shape PowerModels.jl writes, including the per-unit division that
//! ecosystem applies. The test that matters is whether reading it back
//! recovers megawatts: a case whose demand is 2.59 rather than 259 solves
//! quite happily and answers nothing.

use gridwright_io::{json, matpower::load_case};
use gridwright_net::Network;

fn path(rel: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn pm() -> gridwright_io::Case {
    json::load_powermodels(path("examples/powermodels/case14_ieee.json")).unwrap()
}

fn mat() -> gridwright_io::Case {
    load_case(path("examples/pglib/case14_ieee.m")).unwrap()
}

#[test]
fn a_per_unit_case_comes_back_in_megawatts() {
    // The single most consequential thing this reader does. IEEE 14 has 259
    // MW of demand and the file says 2.59.
    let (a, b) = (pm(), mat());
    let demand = |n: &Network| n.loads.iter().map(|l| l.p_set).sum::<f64>();
    let got = demand(&a.network);
    assert!(
        (got - demand(&b.network)).abs() < 1e-6,
        "{got} against {}",
        demand(&b.network)
    );
    assert!(got > 250.0, "demand came back as {got}, which is per-unit");
}

#[test]
fn capacities_and_ratings_scale_too() {
    let (a, b) = (pm(), mat());
    let cap = |n: &Network| n.generators.iter().map(|g| g.p_nom).sum::<f64>();
    assert!((cap(&a.network) - cap(&b.network)).abs() < 1e-6);

    // Ratings live on the branches and are per-unit in the same way. A line
    // rated 4.72 rather than 472 constrains a network that does not exist.
    let rating = |n: &Network| {
        n.lines
            .iter()
            .map(|l| l.s_nom)
            .filter(|s| *s < 1e5)
            .sum::<f64>()
    };
    assert!(
        (rating(&a.network) - rating(&b.network)).abs() < 1e-6,
        "{} against {}",
        rating(&a.network),
        rating(&b.network)
    );
}

#[test]
fn costs_are_per_megawatt_hour_in_both_dialects() {
    // Cost coefficients in a per-unit case are per unit of power, so they
    // scale the opposite way from the quantities they multiply. Getting the
    // direction wrong makes every generator a hundred times too expensive,
    // and since it is uniform the dispatch looks plausible and the objective
    // is nonsense.
    let (a, b) = (pm(), mat());
    for (g, h) in a.network.generators.iter().zip(&b.network.generators) {
        assert!(
            (g.marginal_cost - h.marginal_cost).abs() < 1e-9,
            "{}: {} against {}",
            g.name,
            g.marginal_cost,
            h.marginal_cost
        );
    }
}

#[test]
fn impedances_and_taps_match_the_matpower_case() {
    let (a, b) = (pm(), mat());
    assert_eq!(a.network.lines.len(), b.network.lines.len());
    for (l, m) in a.network.lines.iter().zip(&b.network.lines) {
        assert!((l.reactance - m.reactance).abs() < 1e-9, "X on {}", l.name);
        assert!((l.resistance - m.resistance).abs() < 1e-9, "R on {}", l.name);
        assert!((l.tap_ratio - m.tap_ratio).abs() < 1e-9, "tap on {}", l.name);
        // PowerModels splits charging susceptance across the two ends; the
        // total has to come back the same.
        assert!(
            (l.shunt_susceptance - m.shunt_susceptance).abs() < 1e-9,
            "B on {}: {} against {}",
            l.name,
            l.shunt_susceptance,
            m.shunt_susceptance
        );
    }
}

#[test]
fn the_per_unit_conversion_is_reported() {
    let notes = pm().notes.join("\n");
    assert!(notes.contains("per-unit"), "{notes}");
}

#[test]
fn a_case_without_the_flag_is_taken_at_face_value_and_said_so() {
    // Some hand-written cases state megawatts directly. Guessing from the
    // magnitude of the numbers would be worse than believing the file and
    // saying which reading was used.
    let text = r#"{
        "baseMVA": 100.0,
        "bus": {"1": {"bus_i": 1, "bus_type": 3, "vmax": 1.1, "vmin": 0.9},
                "2": {"bus_i": 2, "bus_type": 1, "vmax": 1.1, "vmin": 0.9}},
        "load": {"1": {"load_bus": 2, "pd": 90.0, "qd": 30.0, "status": 1}},
        "gen": {"1": {"gen_bus": 1, "pmax": 200.0, "pmin": 0.0, "gen_status": 1,
                      "model": 2, "ncost": 2, "cost": [25.0, 0.0]}},
        "branch": {"1": {"f_bus": 1, "t_bus": 2, "br_r": 0.01, "br_x": 0.1,
                         "rate_a": 250.0, "br_status": 1, "tap": 1.0}}
    }"#;
    let c = json::parse_powermodels(text, "flat").unwrap();
    assert!((c.network.loads[0].p_set - 90.0).abs() < 1e-9);
    assert!((c.network.generators[0].marginal_cost - 25.0).abs() < 1e-9);
    assert!(c.notes.join("\n").contains("no per-unit flag"));
}

#[test]
fn a_native_network_survives_a_round_trip_unchanged() {
    // The lossless direction. Every other format drops something; this one
    // must not, because it is what a running interface will hand back and
    // forth.
    let original = mat().network;
    let text = json::to_string(&original).unwrap();
    let back = json::from_str(&text).unwrap();

    assert_eq!(back.buses.len(), original.buses.len());
    assert_eq!(back.lines.len(), original.lines.len());
    assert_eq!(back.generators.len(), original.generators.len());
    assert_eq!(back.n_snapshots(), original.n_snapshots());
    assert_eq!(back.base_mva, original.base_mva);
    for (a, b) in back.lines.iter().zip(&original.lines) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.reactance, b.reactance);
        assert_eq!(a.tap_ratio, b.tap_ratio);
        assert_eq!(a.s_nom, b.s_nom);
    }
    for (a, b) in back.generators.iter().zip(&original.generators) {
        assert_eq!(a.marginal_cost, b.marginal_cost);
        assert_eq!(a.p_nom, b.p_nom);
        assert_eq!(a.q_max, b.q_max);
    }
    // And again, so that anything the first pass quietly normalised shows up.
    let twice = json::from_str(&json::to_string(&back).unwrap()).unwrap();
    assert_eq!(
        json::to_string(&twice).unwrap(),
        json::to_string(&back).unwrap()
    );
}

#[test]
fn time_series_survive_the_round_trip() {
    // The part most likely to break: a component-major flat buffer with a
    // stride that has to be restored, not just a list of numbers.
    use gridwright_net::{Generator, Load, Snapshots, TimeSeries};
    let mut net = Network::new(Snapshots::weighted(vec![1.0, 2.0, 3.0]).unwrap());
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "wind".into(),
        bus: b,
        p_nom: 100.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 50.0,
        ..Default::default()
    });
    net.gen_availability = TimeSeries::from_rows(&[vec![0.9, 0.2, 0.7]], 3).unwrap();

    let back = json::from_str(&json::to_string(&net).unwrap()).unwrap();
    assert_eq!(back.gen_availability.row(0), Some(&[0.9, 0.2, 0.7][..]));
    assert_eq!(back.snapshots.weights(), &[1.0, 2.0, 3.0]);
}

#[test]
fn the_two_dialects_are_told_apart() {
    let pm_text = std::fs::read_to_string(path("examples/powermodels/case14_ieee.json")).unwrap();
    assert!(json::looks_like_powermodels(&pm_text));

    let native = json::to_string(&mat().network).unwrap();
    assert!(!json::looks_like_powermodels(&native));

    assert!(!json::looks_like_powermodels("not json at all"));
    assert!(!json::looks_like_powermodels("[1, 2, 3]"));
}

#[test]
fn a_document_with_no_buses_is_refused() {
    assert!(json::parse_powermodels(r#"{"baseMVA": 100.0}"#, "x").is_err());
    assert!(json::parse_powermodels("{ not json", "x").is_err());
}

#[test]
fn every_field_survives_serialisation() {
    // JSON has no infinity, and an unbounded capacity ceiling naturally is
    // one. Any f64 field that can hold an infinity must go through the
    // sentinel encoding. Built with every component type populated, so a
    // field added later that forgets to shows up here rather than as a load
    // failure much further downstream.
    use gridwright_net::{Generator, Line, Link, Load, Snapshots, StorageUnit};
    let mut net = Network::new(Snapshots::hourly(2));
    let a = net.add_bus("A", "AA");
    let b = net.add_bus("B", "BB");
    net.add_generator(Generator { name: "g".into(), bus: a, ..Default::default() });
    net.add_line(Line { name: "l".into(), bus0: a, bus1: b, s_nom: 100.0,
                        susceptance: 1.0, ..Default::default() });
    net.add_load(Load { name: "d".into(), bus: b, p_set: 10.0, ..Default::default() });
    net.add_storage(StorageUnit { name: "s".into(), bus: b, p_nom: 10.0,
                                  max_hours: 4.0, ..Default::default() });
    net.add_link(Link { name: "k".into(), bus0: a, bus1: b, p_nom: 10.0,
                        efficiency: 0.7, ..Default::default() });

    // Round-tripping and comparing the two encodings is the check that
    // generalises: an `Option` written as null returns as `None` and is fine,
    // whereas an infinity written as null cannot be read at all. Comparing
    // what comes back catches the second without flagging the first, and will
    // catch any other lossy field added later.
    let text = json::to_string(&net).unwrap();
    let back = json::from_str(&text).unwrap_or_else(|e| {
        panic!("a field did not survive serialisation: {e}\n{text}")
    });
    assert_eq!(
        json::to_string(&back).unwrap(),
        text,
        "the network changed on the way through"
    );
    assert!(back.generators[0].p_nom_max.is_infinite());
    assert!(back.generators[0].q_max.is_infinite());
    assert!(back.generators[0].q_min.is_infinite() && back.generators[0].q_min < 0.0);
}

#[test]
fn infinities_are_readable_in_either_spelling() {
    // Hand-written files say "Infinity" as often as "inf", and a number is
    // still a number.
    use gridwright_net::Generator;
    let mut net = Network::new(gridwright_net::Snapshots::hourly(1));
    let a = net.add_bus("A", "AA");
    net.add_generator(Generator { name: "g".into(), bus: a, p_nom: 1.0, ..Default::default() });
    net.add_load(gridwright_net::Load { name: "d".into(), bus: a, p_set: 1.0, ..Default::default() });

    let text = json::to_string(&net).unwrap();
    for spelling in ["\"inf\"", "\"Infinity\"", "\"INF\""] {
        let swapped = text.replacen("\"inf\"", spelling, 1);
        let back = json::from_str(&swapped)
            .unwrap_or_else(|e| panic!("{spelling} failed: {e}"));
        assert!(back.generators[0].p_nom_max.is_infinite());
    }
    let finite = text.replacen("\"inf\"", "500.0", 1);
    assert_eq!(json::from_str(&finite).unwrap().generators[0].p_nom_max, 500.0);
}

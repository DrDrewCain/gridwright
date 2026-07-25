//! Parquet, against the CSV reader it has to agree with.
//!
//! The two formats describe the same thing and share the same assembler, so
//! the test that matters is that a network written both ways reads back
//! identically. Anything Parquet gets uniquely wrong — a column type coerced
//! badly, a null read as zero, a wide series transposed the wrong way — shows
//! up as a difference from the CSV answer.

#![cfg(feature = "parquet")]

use gridwright_io::{csv::Table, load_network, matpower::load_case, parquet};
use gridwright_net::{Generator, Load, Network, Snapshots, StorageUnit, TimeSeries};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("gridwright-parquet-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn case14() -> Network {
    load_case(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/pglib/case14_ieee.m"),
    )
    .unwrap()
    .network
}

#[test]
fn a_real_network_survives_a_parquet_round_trip() {
    let original = case14();
    let dir = tmp("case14");
    parquet::write_network(&original, &dir).unwrap();
    let back = parquet::load_network(&dir).unwrap();

    assert_eq!(back.buses.len(), original.buses.len());
    assert_eq!(back.lines.len(), original.lines.len());
    assert_eq!(back.generators.len(), original.generators.len());
    assert_eq!(back.loads.len(), original.loads.len());

    for (a, b) in back.lines.iter().zip(&original.lines) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.bus0, b.bus0);
        assert_eq!(a.bus1, b.bus1);
        // Bit-exact, not merely close. A float written and read back through a
        // typed column has no reason to move, and a tolerance here would hide
        // a real precision loss in the text rendering path.
        assert_eq!(a.reactance, b.reactance, "X on {}", a.name);
        assert_eq!(a.resistance, b.resistance, "R on {}", a.name);
        assert_eq!(a.susceptance, b.susceptance, "B on {}", a.name);
        assert_eq!(a.tap_ratio, b.tap_ratio, "tap on {}", a.name);
        assert_eq!(a.s_nom, b.s_nom, "rating on {}", a.name);
    }
    for (a, b) in back.generators.iter().zip(&original.generators) {
        assert_eq!(a.marginal_cost, b.marginal_cost, "cost on {}", a.name);
        assert_eq!(a.p_nom, b.p_nom);
        assert_eq!(a.p_min_pu, b.p_min_pu);
    }
    for (a, b) in back.loads.iter().zip(&original.loads) {
        assert_eq!(a.p_set, b.p_set, "demand at {}", a.name);
    }
}

#[test]
fn parquet_and_csv_read_to_the_same_network() {
    // The two formats share an assembler, so any divergence is Parquet's
    // alone: a coerced type, a null taken as zero, a boolean spelled
    // differently.
    let original = case14();
    let pq = tmp("agree-pq");
    parquet::write_network(&original, &pq).unwrap();

    let csv = tmp("agree-csv");
    gridwright_io::write_network(&original, &csv).unwrap();

    let a = parquet::load_network(&pq).unwrap();
    let b = load_network(&csv).unwrap();

    assert_eq!(a.buses.len(), b.buses.len());
    assert_eq!(a.lines.len(), b.lines.len());
    for (x, y) in a.lines.iter().zip(&b.lines) {
        assert_eq!(x.name, y.name);
        assert!((x.reactance - y.reactance).abs() < 1e-12, "X on {}", x.name);
        assert!((x.s_nom - y.s_nom).abs() < 1e-9, "rating on {}", x.name);
    }
    for (x, y) in a.generators.iter().zip(&b.generators) {
        assert!(
            (x.marginal_cost - y.marginal_cost).abs() < 1e-9,
            "cost on {}",
            x.name
        );
        assert_eq!(x.p_nom_extendable, y.p_nom_extendable);
    }
}

#[test]
fn a_years_worth_of_hourly_data_goes_through_numerically() {
    // The reason this format exists. 8760 snapshots across 40 generators is
    // 350k values, which is small for a real study and already tedious as
    // text. Every one has to come back exactly where it went in, which is
    // what a transposition bug would break.
    let hours = 8760;
    let mut net = Network::new(Snapshots::hourly(hours));
    let b = net.add_bus("B", "XX");
    let n_gen = 40;
    for g in 0..n_gen {
        net.add_generator(Generator {
            name: format!("unit{g}"),
            bus: b,
            p_nom: 100.0,
            marginal_cost: g as f64,
            ..Default::default()
        });
    }
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 500.0,
        ..Default::default()
    });
    // A distinct value per (generator, hour), so a transposition or an offset
    // cannot pass by looking plausible.
    let rows: Vec<Vec<f64>> = (0..n_gen)
        .map(|g| {
            (0..hours)
                .map(|t| ((g * hours + t) % 1000) as f64 / 1000.0)
                .collect()
        })
        .collect();
    net.gen_availability = TimeSeries::from_rows(&rows, hours).unwrap();

    let dir = tmp("bigseries");
    parquet::write_network(&net, &dir).unwrap();
    let back = parquet::load_network(&dir).unwrap();

    assert_eq!(back.n_snapshots(), hours);
    for (g, want) in rows.iter().enumerate() {
        assert_eq!(
            back.gen_availability.row(g),
            Some(&want[..]),
            "generator {g} came back wrong"
        );
    }

    // And it is genuinely smaller than the text would be.
    let size = std::fs::metadata(dir.join("gen_availability.parquet"))
        .unwrap()
        .len();
    let as_text = (n_gen * hours * 6) as u64; // roughly "0.123," per value
    assert!(
        size < as_text / 2,
        "{size} bytes against roughly {as_text} as text"
    );
}

#[test]
fn columns_are_matched_by_name_not_by_position() {
    // A file whose columns are in a different order, or which covers only
    // some of the generators, still has to land each series on the right one.
    // Position matching would quietly swap two fleets.
    let mut net = Network::new(Snapshots::hourly(3));
    let b = net.add_bus("B", "XX");
    for name in ["wind", "solar", "gas"] {
        net.add_generator(Generator {
            name: name.into(),
            bus: b,
            p_nom: 100.0,
            ..Default::default()
        });
    }
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 50.0,
        ..Default::default()
    });
    net.gen_availability = TimeSeries::from_rows(
        &[
            vec![0.1, 0.2, 0.3],
            vec![0.4, 0.5, 0.6],
            vec![0.7, 0.8, 0.9],
        ],
        3,
    )
    .unwrap();

    let dir = tmp("byname");
    parquet::write_network(&net, &dir).unwrap();

    // Rewrite the series with the columns reversed and one omitted.
    let mut reduced = net.clone();
    reduced.generators.reverse();
    reduced.gen_availability =
        TimeSeries::from_rows(&[vec![0.7, 0.8, 0.9], vec![0.4, 0.5, 0.6]], 3).unwrap();
    reduced.generators.truncate(2);
    let other = tmp("byname-rev");
    parquet::write_network(&reduced, &other).unwrap();
    std::fs::copy(
        other.join("gen_availability.parquet"),
        dir.join("gen_availability.parquet"),
    )
    .unwrap();

    let back = parquet::load_network(&dir).unwrap();
    assert_eq!(
        back.gen_availability.row(0),
        Some(&[1.0, 1.0, 1.0][..]),
        "wind has no column in this file and a generator with no profile is \
         available at full capacity, not unavailable"
    );
    assert_eq!(back.gen_availability.row(1), Some(&[0.4, 0.5, 0.6][..]), "solar");
    assert_eq!(back.gen_availability.row(2), Some(&[0.7, 0.8, 0.9][..]), "gas");
}

#[test]
fn an_unbounded_ceiling_survives_as_unbounded() {
    // Parquet has no infinity either. It is written as a null and has to come
    // back as "no ceiling", not as zero, which would forbid building anything.
    let mut net = Network::new(Snapshots::hourly(1));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "wind".into(),
        bus: b,
        p_nom: 0.0,
        p_nom_extendable: true,
        capital_cost: 1.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 10.0,
        ..Default::default()
    });
    assert!(net.generators[0].p_nom_max.is_infinite());

    let dir = tmp("infinite");
    parquet::write_network(&net, &dir).unwrap();
    let back = parquet::load_network(&dir).unwrap();
    assert!(
        back.generators[0].p_nom_max.is_infinite(),
        "came back as {}",
        back.generators[0].p_nom_max
    );
    assert!(back.generators[0].p_nom_extendable);
}

#[test]
fn storage_survives_the_round_trip() {
    let mut net = Network::new(Snapshots::hourly(4));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "g".into(),
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
    net.add_storage(StorageUnit {
        name: "batt".into(),
        bus: b,
        p_nom: 60.0,
        max_hours: 4.0,
        efficiency_store: 0.93,
        efficiency_dispatch: 0.91,
        cyclic: true,
        ..Default::default()
    });
    let dir = tmp("storage");
    parquet::write_network(&net, &dir).unwrap();
    let back = parquet::load_network(&dir).unwrap();
    assert_eq!(back.storage.len(), 1);
    assert_eq!(back.storage[0].max_hours, 4.0);
    assert_eq!(back.storage[0].efficiency_store, 0.93);
    assert!(back.storage[0].cyclic);
}

#[test]
fn snapshot_weights_survive() {
    let mut net = Network::new(Snapshots::weighted(vec![3.0, 3.0, 2.0]).unwrap());
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "g".into(),
        bus: b,
        p_nom: 10.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 5.0,
        ..Default::default()
    });
    let dir = tmp("weights");
    parquet::write_network(&net, &dir).unwrap();
    let back = parquet::load_network(&dir).unwrap();
    assert_eq!(back.snapshots.weights(), &[3.0, 3.0, 2.0]);
}

#[test]
fn a_missing_required_table_is_an_error_not_an_empty_network() {
    let dir = tmp("empty");
    assert!(parquet::load_network(&dir).is_err());
}

#[test]
fn integer_and_boolean_columns_are_read_as_written() {
    // Files produced elsewhere use whatever type their writer picked. An
    // int64 bus rating and a boolean flag both have to read the same as the
    // float and the string a hand-written file would carry.
    let dir = tmp("types");
    let net = case14();
    parquet::write_network(&net, &dir).unwrap();
    let table = gridwright_io::parquet::ParquetDir(&dir);
    use gridwright_io::TableSource;
    let t: Table = table.table("generators").unwrap().unwrap();
    assert!(t.column("p_nom").is_some());
    assert_eq!(
        t.boolean(0, "p_nom_extendable", true).unwrap(),
        net.generators[0].p_nom_extendable
    );
    assert_eq!(t.number(0, "p_nom", -1.0).unwrap(), net.generators[0].p_nom);
}

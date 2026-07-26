//! Reading a year of hourly data without holding a year of hourly data.
//!
//! A continental model at hourly resolution is a few thousand components by
//! 8,760 snapshots, which is tens of millions of numbers in one file. The
//! destination is a `TimeSeries`, whose size is known from the header and is
//! therefore not the thing to argue with: what matters is whether anything of
//! comparable size exists *beside* it while the file is being read. A whole
//! file of text held as a `String`, a `Table` of one `String` per cell, or a
//! destination buffer grown by doubling all cost as much again or several times
//! as much, and none of them is necessary.
//!
//! The small tests below pin the behaviour that must not change while that is
//! taken out. The large ones are `#[ignore]`d because they want a fixture of a
//! few hundred megabytes; they exist to be run under `/usr/bin/time -l`, one
//! process per reader, which is the only way to see a peak that a test harness
//! sharing an allocator with a dozen other tests would hide:
//!
//! ```text
//! cargo test -p gridwright-io --all-features --release --test streaming --no-run
//! B=$(ls -t target/release/deps/streaming-* | grep -v '\.d$' | head -1)
//! "$B" --ignored --exact writes_the_large_fixture --nocapture
//! /usr/bin/time -l "$B" --ignored --exact reads_the_large_csv_fixture
//! /usr/bin/time -l "$B" --ignored --exact reads_the_large_parquet_fixture
//! ```
//!
//! `GRIDWRIGHT_BIG_FIXTURE`, `GRIDWRIGHT_BIG_GENS` and `GRIDWRIGHT_BIG_SNAPS`
//! move the fixture and change its shape.

use std::io::Write;

use gridwright_net::{Generator, Line, Load, Network, Snapshots, TimeSeries};

/// The value at generator `g`, snapshot `t`.
///
/// Deterministic and with six decimal places, so the CSV rendering is the width
/// a real availability profile is rather than the width a round number is, and
/// so a reader that lands a value in the wrong place is caught rather than
/// merely suspected.
fn value(g: usize, t: usize) -> f64 {
    ((g * 8_641 + t * 7) % 1_000_000) as f64 / 1_000_000.0
}

fn shape() -> (usize, usize) {
    let n = |var: &str, default: usize| {
        std::env::var(var)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    };
    (
        n("GRIDWRIGHT_BIG_GENS", 4_000),
        n("GRIDWRIGHT_BIG_SNAPS", 8_760),
    )
}

fn fixture_dir() -> std::path::PathBuf {
    match std::env::var("GRIDWRIGHT_BIG_FIXTURE") {
        Ok(d) => std::path::PathBuf::from(d),
        Err(_) => std::env::temp_dir().join("gridwright-big-fixture"),
    }
}

/// Everything but the time series: a bus, some generators, a load, a line.
fn skeleton(n_gen: usize, n_snap: usize) -> Network {
    let mut net = Network::new(Snapshots::hourly(n_snap));
    net.add_bus("north".to_string(), "DE".to_string());
    net.add_bus("south".to_string(), "DE".to_string());
    for g in 0..n_gen {
        net.add_generator(Generator {
            name: format!("g{g}"),
            bus: g % 2,
            p_nom: 100.0,
            marginal_cost: 40.0,
            ..Generator::default()
        });
    }
    net.add_load(Load {
        name: "l".to_string(),
        bus: 0,
        p_set: 50.0,
        ..Load::default()
    });
    net.add_line(Line {
        name: "n-s".to_string(),
        bus0: 0,
        bus1: 1,
        s_nom: 1_000.0,
        susceptance: 10.0,
        tap_ratio: 1.0,
        ..Line::default()
    });
    net
}

/// Write the wide CSV a value at a time, so building the fixture does not need
/// the memory the point of the exercise is to stop needing.
fn write_wide_csv(path: &std::path::Path, n_gen: usize, n_snap: usize) {
    let file = std::fs::File::create(path).unwrap();
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
    for g in 0..n_gen {
        if g > 0 {
            out.write_all(b",").unwrap();
        }
        write!(out, "g{g}").unwrap();
    }
    out.write_all(b"\n").unwrap();
    for t in 0..n_snap {
        for g in 0..n_gen {
            if g > 0 {
                out.write_all(b",").unwrap();
            }
            write!(out, "{:?}", value(g, t)).unwrap();
        }
        out.write_all(b"\n").unwrap();
    }
    out.flush().unwrap();
}

fn check(net: &Network, n_gen: usize, n_snap: usize) {
    assert_eq!(net.generators.len(), n_gen);
    assert_eq!(net.gen_availability.len(), n_gen * n_snap);
    // Corners and a diagonal: enough to catch a transpose that lost its
    // stride, which is the failure a spot check of one cell would pass.
    for (g, t) in [
        (0, 0),
        (0, n_snap - 1),
        (n_gen - 1, 0),
        (n_gen - 1, n_snap - 1),
        (n_gen / 3, n_snap / 7),
    ] {
        assert_eq!(
            net.gen_availability.at(g, t),
            Some(value(g, t)),
            "generator {g} at snapshot {t}"
        );
    }
}

/// A scratch directory holding a two-generator network and whatever wide file
/// the test wants to put in it.
fn small(name: &str, availability: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("gridwright-streaming-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let write = |file: &str, body: &str| std::fs::write(dir.join(file), body).unwrap();
    write("buses.csv", "name,country\nDE,DE\nFR,FR\n");
    write(
        "generators.csv",
        "name,bus,p_nom,marginal_cost\ncoal,DE,100,40\n\"nuke, big\",FR,200,10\n",
    );
    write(
        "lines.csv",
        "name,bus0,bus1,s_nom,susceptance\nDE-FR,DE,FR,50,0\n",
    );
    write("loads.csv", "name,bus,p_set\nl,DE,80\n");
    write("snapshots.csv", "weight\n1\n1\n1\n");
    write("gen_availability.csv", availability);
    dir
}

/// Read the same directory off the disk and out of a buffer.
///
/// The two go by different routes on purpose — the first a line at a time from
/// the file, the second through a table of strings — and they have to agree,
/// including on the error. This is the check that the streaming path was a
/// change of where the bytes live rather than of what they mean.
fn both_ways(dir: &std::path::Path) -> (Result<Network, String>, Result<Network, String>) {
    let from_disk = gridwright_io::load_network(dir).map_err(|e| e.to_string());
    let files: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    let from_bytes = gridwright_io::load_files(&gridwright_io::Files::new(files))
        .map(|c| c.network)
        .map_err(|e| e.to_string());
    (from_disk, from_bytes)
}

fn agree(dir: &std::path::Path) -> Network {
    let (disk, bytes) = both_ways(dir);
    match (disk, bytes) {
        (Ok(a), Ok(b)) => {
            assert_eq!(a.gen_availability.len(), b.gen_availability.len());
            for c in 0..a.generators.len() {
                assert_eq!(a.gen_availability.row(c), b.gen_availability.row(c));
            }
            a
        }
        (Err(a), Err(b)) => panic!("both failed: {a} / {b}"),
        (a, b) => panic!("the two paths disagreed: {a:?} / {b:?}"),
    }
}

#[test]
fn a_year_of_hourly_data_does_not_have_to_be_resident_all_at_once() {
    // The shape of the thing rather than its size: the file is read a line at
    // a time and every value still lands where transposing the whole document
    // would have put it. The size is what the ignored tests above are for.
    let dir = small(
        "streamed",
        "coal,\"nuke, big\"\n1.0,0.5\n0.25,0.75\n0.0,0.9\n",
    );
    let net = agree(&dir);
    assert_eq!(net.gen_availability.row(0).unwrap(), &[1.0, 0.25, 0.0]);
    assert_eq!(net.gen_availability.row(1).unwrap(), &[0.5, 0.75, 0.9]);
}

#[test]
fn line_endings_and_a_missing_final_newline_do_not_shift_the_values() {
    // `read_line` keeps the terminator where `str::lines` drops it, so a file
    // written on Windows would otherwise end every row with a value of
    // `0.9\r`, which does not parse and would fail loudly — or worse, would
    // parse if the last column were one this reader ignored.
    for body in [
        "coal,\"nuke, big\"\r\n1.0,0.5\r\n0.25,0.75\r\n0.0,0.9\r\n",
        "coal,\"nuke, big\"\n1.0,0.5\n0.25,0.75\n0.0,0.9",
        "coal,\"nuke, big\"\n1.0,0.5\n\n0.25,0.75\n\n0.0,0.9\n",
    ] {
        let dir = small("endings", body);
        let net = agree(&dir);
        assert_eq!(net.gen_availability.row(0).unwrap(), &[1.0, 0.25, 0.0]);
        assert_eq!(net.gen_availability.row(1).unwrap(), &[0.5, 0.75, 0.9]);
    }
}

#[test]
fn the_streamed_path_reports_the_same_faults_as_the_buffered_one() {
    for body in [
        // A column naming no generator.
        "coal,ghost\n1,1\n1,1\n1,1\n",
        // Too few rows, and too many.
        "coal\n1\n1\n",
        "coal\n1\n1\n1\n1\n",
        // A value that is not a number, on the third row.
        "coal\n1\n1\nbanana\n",
    ] {
        let dir = small("faults", body);
        let (disk, bytes) = both_ways(&dir);
        assert_eq!(disk.map(|_| ()), bytes.map(|_| ()), "disagreed on {body:?}");
    }
}

#[test]
fn an_empty_wide_file_is_an_empty_series_and_not_a_short_one() {
    // Pinned rather than chosen. A file without even a header line has no
    // header to disagree with, and the reader that walks it has always called
    // that "no series given" and left every component on its default. The
    // buffered path parses the document into a table first, sees zero rows
    // against three snapshots, and calls it short. They differed before this
    // test existed and they still differ; recording it here is what makes any
    // later reconciliation a deliberate act rather than a surprise.
    let dir = small("empty", "");
    let (disk, bytes) = both_ways(&dir);
    assert!(disk.unwrap().gen_availability.is_empty());
    assert!(
        bytes
            .unwrap_err()
            .contains("0 rows but there are 3 snapshots")
    );
}

#[cfg(feature = "parquet")]
#[test]
fn a_series_wider_than_one_slice_of_columns_still_lands_component_by_component() {
    // The Parquet reader takes a fixed number of columns at a time, so the
    // case that matters is a file with more components than that: the column
    // mapping is rebuilt per slice and a slice that mapped its columns as if
    // they started at zero would write every component after the first slice
    // into the wrong run.
    let n_gen = 300;
    let n_snap = 5;
    let mut net = skeleton(n_gen, n_snap);
    net.gen_availability = TimeSeries::from_flat(
        (0..n_gen)
            .flat_map(|g| (0..n_snap).map(move |t| value(g, t)))
            .collect(),
        n_gen,
        n_snap,
    )
    .unwrap();

    let dir = std::env::temp_dir().join("gridwright-streaming-slices");
    let _ = std::fs::remove_dir_all(&dir);
    gridwright_io::parquet::write_network(&net, &dir).unwrap();
    let back = gridwright_io::parquet::load_network(&dir).unwrap();

    for g in 0..n_gen {
        assert_eq!(
            back.gen_availability.row(g),
            net.gen_availability.row(g),
            "generator {g}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "writes a few hundred megabytes; run it by name"]
fn writes_the_large_fixture() {
    let (n_gen, n_snap) = shape();
    let dir = fixture_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut net = skeleton(n_gen, n_snap);
    net.gen_availability = TimeSeries::from_flat(
        (0..n_gen)
            .flat_map(|g| (0..n_snap).map(move |t| value(g, t)))
            .collect(),
        n_gen,
        n_snap,
    )
    .unwrap();

    #[cfg(feature = "parquet")]
    gridwright_io::parquet::write_network(&net, &dir).unwrap();

    // The CSV writer renders the whole document into one `String`, which for
    // this fixture is the thing under test; the series is dropped first and the
    // wide file written separately so the fixture can be built at any size.
    net.gen_availability = TimeSeries::empty();
    gridwright_io::write_network(&net, &dir).unwrap();
    write_wide_csv(&dir.join("gen_availability.csv"), n_gen, n_snap);

    let bytes = std::fs::metadata(dir.join("gen_availability.csv"))
        .unwrap()
        .len();
    println!(
        "fixture: {n_gen} generators x {n_snap} snapshots, gen_availability.csv {} MB",
        bytes / (1 << 20)
    );
}

#[test]
#[ignore = "wants the fixture; run it under /usr/bin/time -l"]
fn reads_the_large_csv_fixture() {
    let (n_gen, n_snap) = shape();
    let started = std::time::Instant::now();
    let net = gridwright_io::load_network(fixture_dir()).unwrap();
    let elapsed = started.elapsed();
    check(&net, n_gen, n_snap);
    println!("csv: {:.3} s", elapsed.as_secs_f64());
}

#[cfg(feature = "parquet")]
#[test]
#[ignore = "wants the fixture; run it under /usr/bin/time -l"]
fn reads_the_large_parquet_fixture() {
    let (n_gen, n_snap) = shape();
    let started = std::time::Instant::now();
    let net = gridwright_io::parquet::load_network(fixture_dir()).unwrap();
    let elapsed = started.elapsed();
    check(&net, n_gen, n_snap);
    println!("parquet: {:.3} s", elapsed.as_secs_f64());
}

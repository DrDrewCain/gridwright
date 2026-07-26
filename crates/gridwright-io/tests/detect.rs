//! Pointing at a file and getting a network back.
//!
//! Every fixture in `examples/` goes through one call. That is the whole
//! claim of the format layer: someone with a file downloaded from a TSO, a
//! ministry or a paper should not have to identify it first.

use gridwright_io::{Format, load_any, sniff};

fn path(rel: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

/// Every fixture, and what it is.
fn corpus() -> Vec<(&'static str, Format)> {
    let mut v = vec![
        ("examples/pglib/case14_ieee.m", Format::Matpower),
        ("examples/pglib/case118_ieee.m", Format::Matpower),
        ("examples/psse/case14_v33.raw", Format::Psse),
        ("examples/psse/case14_v29.raw", Format::Psse),
        ("examples/psse/conventions.raw", Format::Psse),
        ("examples/ieee_cdf/ieee14cdf.txt", Format::IeeeCdf),
        ("examples/ieee_cdf/conventions.cdf", Format::IeeeCdf),
        ("examples/ucte/mini.uct", Format::Ucte),
    ];
    if cfg!(feature = "json") {
        v.push((
            "examples/powermodels/case14_ieee.json",
            Format::PowerModels,
        ));
        v.push(("examples/rawx/case14_ieee.rawx", Format::Rawx));
    }
    if cfg!(feature = "netcdf") {
        v.push(("examples/pypsa/case14_ieee.nc", Format::Netcdf));
    }
    if cfg!(feature = "excel") {
        v.push(("examples/excel/case14_ieee.xlsx", Format::Excel));
    }
    if cfg!(feature = "cgmes") {
        v.push(("examples/cgmes", Format::Cgmes));
    }
    v
}

#[test]
fn every_fixture_is_recognised_for_what_it_is() {
    for (rel, want) in corpus() {
        let got = sniff(path(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert_eq!(got, want, "{rel} was read as {}", got.label());
    }
}

#[test]
fn every_fixture_loads_through_one_call() {
    for (rel, want) in corpus() {
        let case = load_any(path(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert!(!case.network.buses.is_empty(), "{rel} produced no buses");
        assert!(case.network.validate().is_ok(), "{rel} is not a valid network");
        assert!(
            case.notes[0].contains(want.label()),
            "{rel}: notes start with {:?}",
            case.notes[0]
        );
    }
}

#[test]
fn a_renamed_file_is_still_read_correctly() {
    // Files arrive named wrongly all the time — a MATPOWER case saved as
    // .txt, a RAW file with the extension stripped. Content is the more
    // reliable signal and is consulted first.
    let dir = std::env::temp_dir().join("gridwright-detect-renamed");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let m = dir.join("mystery.txt");
    std::fs::copy(path("examples/pglib/case14_ieee.m"), &m).unwrap();
    assert_eq!(sniff(&m).unwrap(), Format::Matpower);
    assert_eq!(load_any(&m).unwrap().network.buses.len(), 14);

    let r = dir.join("no_extension");
    std::fs::copy(path("examples/psse/case14_v33.raw"), &r).unwrap();
    assert_eq!(sniff(&r).unwrap(), Format::Psse);
    assert_eq!(load_any(&r).unwrap().network.buses.len(), 14);

    // The two fixed-width formats are the sharpest case of this: the IEEE
    // archive publishes its cases as `.txt` and nothing else, so the extension
    // never identified them in the first place.
    let c = dir.join("archive.dat");
    std::fs::copy(path("examples/ieee_cdf/ieee14cdf.txt"), &c).unwrap();
    assert_eq!(sniff(&c).unwrap(), Format::IeeeCdf);
    assert_eq!(load_any(&c).unwrap().network.buses.len(), 14);

    let u = dir.join("study");
    std::fs::copy(path("examples/ucte/mini.uct"), &u).unwrap();
    assert_eq!(sniff(&u).unwrap(), Format::Ucte);
    assert_eq!(load_any(&u).unwrap().network.buses.len(), 5);
}

#[test]
fn the_fixed_width_formats_read_from_a_buffer_as_well_as_from_a_path() {
    // The interface this is headed for runs in a browser, where a file picker
    // hands over a name and a buffer and there is no filesystem to open.
    for (rel, want) in [
        ("examples/ieee_cdf/ieee14cdf.txt", 14usize),
        ("examples/ucte/mini.uct", 5),
    ] {
        let bytes = std::fs::read(path(rel)).unwrap();
        let case = gridwright_io::load_bytes(None, &bytes).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert_eq!(case.network.buses.len(), want, "{rel}");
    }
}

#[test]
fn a_cdf_extension_belongs_to_the_common_data_format_rather_than_to_netcdf() {
    // `.cdf` is claimed by both, and a real netCDF4 file is HDF5 underneath
    // and is caught by its magic bytes long before any extension is read. A
    // text file called `.cdf` is the 1973 IEEE format.
    assert_eq!(
        sniff(path("examples/ieee_cdf/conventions.cdf")).unwrap(),
        Format::IeeeCdf
    );
    if cfg!(feature = "netcdf") {
        assert_eq!(
            sniff(path("examples/pypsa/case14_ieee.nc")).unwrap(),
            Format::Netcdf
        );
    }
}

#[test]
#[cfg(feature = "json")]
fn the_two_json_dialects_are_separated_by_content() {
    // Both are `.json`, and reading one as the other gives either a load
    // failure or, worse, a network whose demand is a hundredth of the truth.
    assert_eq!(
        sniff(path("examples/powermodels/case14_ieee.json")).unwrap(),
        Format::PowerModels
    );

    let dir = std::env::temp_dir().join("gridwright-detect-json");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let native = dir.join("native.json");
    let net = gridwright_io::matpower::load_case(path("examples/pglib/case14_ieee.m"))
        .unwrap()
        .network;
    gridwright_io::json::write_network(&net, &native).unwrap();
    assert_eq!(sniff(&native).unwrap(), Format::NativeJson);
    assert_eq!(load_any(&native).unwrap().network.buses.len(), 14);
}

#[test]
#[cfg(feature = "parquet")]
fn a_directory_is_identified_by_what_is_in_it() {
    let net = gridwright_io::matpower::load_case(path("examples/pglib/case14_ieee.m"))
        .unwrap()
        .network;

    let pq = std::env::temp_dir().join("gridwright-detect-pq");
    let _ = std::fs::remove_dir_all(&pq);
    gridwright_io::parquet::write_network(&net, &pq).unwrap();
    assert_eq!(sniff(&pq).unwrap(), Format::ParquetDirectory);
    assert_eq!(load_any(&pq).unwrap().network.lines.len(), 20);

    let csv = std::env::temp_dir().join("gridwright-detect-csv");
    let _ = std::fs::remove_dir_all(&csv);
    gridwright_io::write_network(&net, &csv).unwrap();
    assert_eq!(sniff(&csv).unwrap(), Format::CsvDirectory);
    assert_eq!(load_any(&csv).unwrap().network.lines.len(), 20);
}

#[test]
fn every_format_reads_the_same_network_to_the_same_answer() {
    // The strongest statement the format layer can make: the IEEE 14-bus
    // system arrives in six different encodings and comes out the same network
    // every time.
    let mut seen: Vec<(&str, usize, usize, f64)> = Vec::new();
    let mut check = |rel: &'static str| {
        let Ok(case) = load_any(path(rel)) else { return };
        let n = &case.network;
        let demand: f64 = n.loads.iter().map(|l| l.p_set).sum();
        seen.push((rel, n.buses.len(), n.lines.len(), demand));
    };
    check("examples/pglib/case14_ieee.m");
    check("examples/psse/case14_v33.raw");
    check("examples/psse/case14_v29.raw");
    check("examples/ieee_cdf/ieee14cdf.txt");
    if cfg!(feature = "json") {
        check("examples/powermodels/case14_ieee.json");
        check("examples/rawx/case14_ieee.rawx");
    }
    if cfg!(feature = "netcdf") {
        check("examples/pypsa/case14_ieee.nc");
    }
    if cfg!(feature = "excel") {
        check("examples/excel/case14_ieee.xlsx");
    }

    assert!(seen.len() >= 3, "expected several encodings, got {seen:?}");
    let (first, buses, lines, demand) = seen[0];
    for (rel, b, l, d) in &seen[1..] {
        assert_eq!(*b, buses, "{rel} has {b} buses, {first} has {buses}");
        assert_eq!(*l, lines, "{rel} has {l} lines, {first} has {lines}");
        assert!(
            (d - demand).abs() < 1e-6,
            "{rel} has {d} MW of demand, {first} has {demand}"
        );
    }
}

#[test]
fn something_unrecognisable_says_so_rather_than_guessing() {
    let dir = std::env::temp_dir().join("gridwright-detect-junk");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let junk = dir.join("notes.txt");
    std::fs::write(&junk, "just some prose about the grid\nnothing structured\n").unwrap();
    assert!(sniff(&junk).is_err());
    assert!(load_any(&junk).is_err());

    let empty = dir.join("empty.txt");
    std::fs::write(&empty, "").unwrap();
    assert!(sniff(&empty).is_err());

    // An empty directory is not a CSV directory.
    assert!(sniff(&dir).is_err());
}

#[test]
fn a_format_this_build_cannot_read_says_which_feature_it_needs() {
    // A build without the netCDF feature must not report a PyPSA file as
    // unrecognised: that sends someone looking for a problem with their data
    // rather than with their build.
    assert!(Format::Netcdf.label().contains("netCDF"));
    if !cfg!(feature = "netcdf") {
        let err = load_any(path("examples/pypsa/case14_ieee.nc"));
        let message = format!("{}", err.unwrap_err());
        assert!(message.contains("feature"), "{message}");
    }
}

#[test]
#[cfg(all(feature = "cgmes", feature = "excel"))]
fn two_formats_that_are_both_zips_are_told_apart_by_their_contents() {
    // A spreadsheet and a published CGMES model are both zip archives, and a
    // CGMES archive is as likely to be named for its operator as for its
    // contents, so the extension settles nothing.
    assert_eq!(
        sniff(path("examples/cgmes/mini_model.zip")).unwrap(),
        Format::Cgmes
    );
    assert_eq!(
        sniff(path("examples/excel/case14_ieee.xlsx")).unwrap(),
        Format::Excel
    );

    // And from bytes, where there is no extension at all to fall back on.
    let cgmes = std::fs::read(path("examples/cgmes/mini_model.zip")).unwrap();
    assert_eq!(gridwright_io::sniff_bytes(None, &cgmes).unwrap(), Format::Cgmes);
    let book = std::fs::read(path("examples/excel/case14_ieee.xlsx")).unwrap();
    assert_eq!(gridwright_io::sniff_bytes(None, &book).unwrap(), Format::Excel);
}

#[test]
#[cfg(feature = "cgmes")]
fn a_cgmes_archive_loads_through_one_call_and_through_bytes() {
    let from_path = load_any(path("examples/cgmes/mini_model.zip")).unwrap();
    assert_eq!(from_path.network.buses.len(), 3);
    assert!(from_path.notes[0].contains("CIM"), "{:?}", from_path.notes);

    let bytes = std::fs::read(path("examples/cgmes/mini_model.zip")).unwrap();
    let from_memory = gridwright_io::load_bytes(Some("model.zip"), &bytes).unwrap();
    assert_eq!(from_memory.network.buses.len(), from_path.network.buses.len());
    assert_eq!(from_memory.network.lines.len(), from_path.network.lines.len());
}

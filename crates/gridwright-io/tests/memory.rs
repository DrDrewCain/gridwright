//! Reading networks with no filesystem.
//!
//! The point of the format layer is that a program importing this crate can
//! take whatever a user hands it. For the interface this is heading towards
//! that program runs in a browser, where there is no filesystem at all and a
//! file picker gives out a name and a buffer. Everything below goes through
//! bytes, so all of it works on `wasm32`.

use gridwright_io::{Files, Format, load_any, load_bytes, load_files, sniff_bytes};

fn path(rel: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn bytes(rel: &str) -> Vec<u8> {
    std::fs::read(path(rel)).unwrap()
}

/// Every single-file fixture, and what it is.
fn single_files() -> Vec<(&'static str, Format)> {
    let mut v = vec![
        ("examples/pglib/case14_ieee.m", Format::Matpower),
        ("examples/psse/case14_v33.raw", Format::Psse),
        ("examples/psse/conventions.raw", Format::Psse),
    ];
    if cfg!(feature = "json") {
        v.push(("examples/powermodels/case14_ieee.json", Format::PowerModels));
    }
    if cfg!(feature = "netcdf") {
        v.push(("examples/pypsa/case14_ieee.nc", Format::Netcdf));
    }
    if cfg!(feature = "excel") {
        v.push(("examples/excel/case14_ieee.xlsx", Format::Excel));
    }
    v
}

#[test]
fn every_single_file_format_reads_from_a_buffer() {
    for (rel, want) in single_files() {
        let raw = bytes(rel);
        let name = rel.rsplit('/').next().unwrap();
        assert_eq!(
            sniff_bytes(Some(name), &raw).unwrap(),
            want,
            "{rel} was misidentified from its bytes"
        );
        let case = load_bytes(Some(name), &raw).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert!(!case.network.buses.is_empty(), "{rel} produced no buses");
        assert!(case.network.validate().is_ok());
    }
}

#[test]
fn a_buffer_reads_to_the_same_network_as_the_file_it_came_from() {
    // The check that matters: a browser and a command line must not disagree
    // about what a file says.
    for (rel, _) in single_files() {
        let from_disk = load_any(path(rel)).unwrap().network;
        let name = rel.rsplit('/').next().unwrap();
        let from_memory = load_bytes(Some(name), &bytes(rel)).unwrap().network;

        assert_eq!(from_memory.buses.len(), from_disk.buses.len(), "{rel}");
        assert_eq!(from_memory.lines.len(), from_disk.lines.len(), "{rel}");
        assert_eq!(
            from_memory.generators.len(),
            from_disk.generators.len(),
            "{rel}"
        );
        for (a, b) in from_memory.lines.iter().zip(&from_disk.lines) {
            assert_eq!(a.reactance, b.reactance, "{rel}: {}", a.name);
            assert_eq!(a.s_nom, b.s_nom, "{rel}: {}", a.name);
        }
        let demand = |n: &gridwright_net::Network| n.loads.iter().map(|l| l.p_set).sum::<f64>();
        assert!((demand(&from_memory) - demand(&from_disk)).abs() < 1e-9, "{rel}");
    }
}

#[test]
fn a_buffer_with_no_name_at_all_is_still_identified() {
    // A picker may hand over bytes and nothing else. Content is enough for
    // every format here except the ones that are only meaningful in a set.
    for (rel, want) in single_files() {
        if want == Format::Excel {
            // A spreadsheet is a zip and is recognised by its magic bytes.
            assert_eq!(sniff_bytes(None, &bytes(rel)).unwrap(), want);
            continue;
        }
        assert_eq!(sniff_bytes(None, &bytes(rel)).unwrap(), want, "{rel}");
    }
}

#[test]
fn a_set_of_csv_files_reads_as_a_directory_would() {
    // What a multiple-selection picker produces.
    let dir = std::env::temp_dir().join("gridwright-memory-csv");
    let _ = std::fs::remove_dir_all(&dir);
    let net = load_any(path("examples/pglib/case14_ieee.m")).unwrap().network;
    gridwright_io::write_network(&net, &dir).unwrap();

    let files: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| {
            (
                e.file_name().to_string_lossy().to_string(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    let case = load_files(&Files::new(files)).unwrap();

    assert_eq!(case.network.buses.len(), net.buses.len());
    assert_eq!(case.network.lines.len(), net.lines.len());
    for (a, b) in case.network.lines.iter().zip(&net.lines) {
        assert!((a.reactance - b.reactance).abs() < 1e-12, "{}", a.name);
    }
    assert!(case.notes[0].contains("CSV"), "{:?}", case.notes);
}

#[test]
#[cfg(feature = "parquet")]
fn a_set_of_parquet_files_reads_the_same_way() {
    use gridwright_net::TimeSeries;
    let mut net = load_any(path("examples/pglib/case14_ieee.m")).unwrap().network;
    // Include a wide series, since that is the path that stays numeric and is
    // the one most likely to break when the file is a buffer.
    let rows: Vec<Vec<f64>> = (0..net.generators.len())
        .map(|g| vec![0.5 + 0.1 * g as f64])
        .collect();
    net.gen_availability = TimeSeries::from_rows(&rows, 1).unwrap();

    let dir = std::env::temp_dir().join("gridwright-memory-pq");
    let _ = std::fs::remove_dir_all(&dir);
    gridwright_io::parquet::write_network(&net, &dir).unwrap();

    let files: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| {
            (
                e.file_name().to_string_lossy().to_string(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    let case = load_files(&Files::new(files)).unwrap();

    assert_eq!(case.network.buses.len(), net.buses.len());
    for (g, want) in rows.iter().enumerate() {
        assert_eq!(
            case.network.gen_availability.row(g),
            Some(&want[..]),
            "generator {g}"
        );
    }
}

#[test]
#[cfg(feature = "cgmes")]
fn cgmes_profiles_merge_from_buffers_as_they_do_from_a_directory() {
    // A published model is several documents and none of them is a network on
    // its own, which is exactly the case a single-buffer API cannot express.
    let from_disk = load_any(path("examples/cgmes")).unwrap().network;

    let files: Vec<(String, Vec<u8>)> = ["mini_EQ.xml", "mini_TP.xml"]
        .iter()
        .map(|n| {
            (
                n.to_string(),
                bytes(&format!("examples/cgmes/{n}")),
            )
        })
        .collect();
    let case = load_files(&Files::new(files)).unwrap();

    assert_eq!(case.network.buses.len(), from_disk.buses.len());
    assert_eq!(case.network.lines.len(), from_disk.lines.len());
    for (a, b) in case.network.lines.iter().zip(&from_disk.lines) {
        assert_eq!(a.name, b.name);
        assert!((a.reactance - b.reactance).abs() < 1e-12);
    }
}

#[test]
fn a_single_file_handed_to_the_set_reader_still_works() {
    // A caller should not have to decide which entry point applies.
    let files = Files::new(vec![(
        "case14_ieee.m".to_string(),
        bytes("examples/pglib/case14_ieee.m"),
    )]);
    assert_eq!(load_files(&files).unwrap().network.buses.len(), 14);
}

#[test]
fn a_directory_path_in_the_name_does_not_confuse_the_set() {
    // Pickers hand over paths as often as bare names.
    let files = Files::new(vec![(
        "some/where/case14_ieee.m".to_string(),
        bytes("examples/pglib/case14_ieee.m"),
    )]);
    assert_eq!(load_files(&files).unwrap().network.buses.len(), 14);
}

#[test]
fn one_table_out_of_a_directory_says_so_rather_than_failing_obscurely() {
    // Handing over `buses.csv` alone is a common mistake, and the file itself
    // parses perfectly. Reporting a parse error would send someone looking at
    // a file that is fine.
    let dir = std::env::temp_dir().join("gridwright-memory-lone");
    let _ = std::fs::remove_dir_all(&dir);
    let net = load_any(path("examples/pglib/case14_ieee.m")).unwrap().network;
    gridwright_io::write_network(&net, &dir).unwrap();

    let raw = std::fs::read(dir.join("buses.csv")).unwrap();
    let err = load_bytes(Some("buses.csv"), &raw).unwrap_err();
    let message = format!("{err}");
    assert!(
        message.contains("several"),
        "expected a message about needing the other files, got: {message}"
    );
}

#[test]
fn empty_and_unrecognisable_input_is_refused() {
    assert!(sniff_bytes(Some("x.txt"), b"").is_err());
    assert!(sniff_bytes(Some("x.txt"), b"just prose about the grid").is_err());
    assert!(load_bytes(None, b"").is_err());
    assert!(load_files(&Files::new(Vec::<(String, Vec<u8>)>::new())).is_err());
}

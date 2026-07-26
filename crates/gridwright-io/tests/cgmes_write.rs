//! The CGMES writer, checked by reading back what it wrote.
//!
//! A writer's only real test is the round trip, and for this format the strong
//! version of it starts somewhere else entirely: the IEEE 14-bus case is read
//! from MATPOWER, written as CGMES and read back. Nothing in that path is
//! shared between the two ends, so it cannot pass by two halves of one module
//! agreeing with each other about a convention that is wrong.
//!
//! The weaker version, round-tripping the CGMES fixtures, is here too, because
//! it is the one that catches a writer that has quietly stopped emitting
//! something the fixtures contain.
//!
//! What the format cannot hold is as much of the contract as what it keeps, and
//! for CIM the list is long: costs, storage, links, voltage limits, phase
//! shifts. Each is stated rather than left to be discovered by whoever solves
//! the file later and gets a different answer.

#![cfg(feature = "cgmes")]

use gridwright_io::{ModelOptions, cgmes, load_any, to_cgmes, to_cgmes_with};
use gridwright_net::{Generator, Line, Load, Network, Snapshots, StorageUnit};

fn path(rel: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join("gridwright-cgmes-write");
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

fn case14() -> Network {
    load_any(path("examples/pglib/case14_ieee.m"))
        .unwrap()
        .network
}

/// Write a network and read it straight back, without touching the filesystem.
fn round_trip(net: &Network, name: &str) -> (Network, Vec<String>) {
    let written = to_cgmes(net, name);
    let back = cgmes::parse_model(&written.documents, name)
        .unwrap_or_else(|e| panic!("the writer produced a model its own reader rejects: {e}"));
    (back.network, written.notes)
}

/// Everything a CGMES model is supposed to preserve, compared component by
/// component rather than in aggregate.
///
/// Indices are compared directly, not matched up by name. That is only possible
/// because the identifiers the writer derives sort into the order the reader
/// assembles in, and it is worth asserting: a network whose buses come back in
/// a different order is a different network to anything that indexes into it,
/// including the solved state a CGMES archive is usually read for.
fn assert_same_network(back: &Network, original: &Network, notes: &[String]) {
    assert_eq!(back.buses.len(), original.buses.len(), "buses: {notes:?}");
    assert_eq!(back.lines.len(), original.lines.len(), "lines: {notes:?}");
    assert_eq!(
        back.generators.len(),
        original.generators.len(),
        "generators: {notes:?}"
    );
    assert_eq!(back.loads.len(), original.loads.len(), "loads: {notes:?}");

    for (a, b) in back.buses.iter().zip(&original.buses) {
        assert_eq!(a.name, b.name, "bus names moved");
        assert!(
            (a.v_nom - b.v_nom).abs() < 1e-9,
            "{}: {} kV came back as {} kV",
            b.name,
            b.v_nom,
            a.v_nom
        );
    }
    for (a, b) in back.lines.iter().zip(&original.lines) {
        assert_eq!(a.name, b.name, "branch names moved");
        assert_eq!(
            (a.bus0, a.bus1),
            (b.bus0, b.bus1),
            "{} changed ends",
            b.name
        );
        assert!(
            (a.resistance - b.resistance).abs() < 1e-9,
            "{}: R {} came back as {}",
            b.name,
            b.resistance,
            a.resistance
        );
        assert!(
            (a.reactance - b.reactance).abs() < 1e-9,
            "{}: X {} came back as {}",
            b.name,
            b.reactance,
            a.reactance
        );
        assert!(
            (a.tap_ratio - b.tap_ratio).abs() < 1e-9,
            "{}: tap {} came back as {}",
            b.name,
            b.tap_ratio,
            a.tap_ratio
        );
        assert!(
            (a.s_nom - b.s_nom).abs() < 1e-6 * b.s_nom.max(1.0),
            "{}: rating {} came back as {}",
            b.name,
            b.s_nom,
            a.s_nom
        );
    }
    for (a, b) in back.generators.iter().zip(&original.generators) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.bus, b.bus, "{} moved bus", b.name);
        assert!((a.p_nom - b.p_nom).abs() < 1e-9, "{}: p_nom", b.name);
        assert!(
            (a.p_min_pu - b.p_min_pu).abs() < 1e-9,
            "{}: p_min_pu",
            b.name
        );
        assert_eq!(
            a.q_max.is_finite(),
            b.q_max.is_finite(),
            "{}: q_max",
            b.name
        );
        if b.q_max.is_finite() {
            assert!((a.q_max - b.q_max).abs() < 1e-9, "{}: q_max", b.name);
            assert!((a.q_min - b.q_min).abs() < 1e-9, "{}: q_min", b.name);
        }
    }
    for (a, b) in back.loads.iter().zip(&original.loads) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.bus, b.bus, "{} moved bus", b.name);
        assert!((a.p_set - b.p_set).abs() < 1e-9, "{}: p_set", b.name);
        assert!((a.q_set - b.q_set).abs() < 1e-9, "{}: q_set", b.name);
    }
}

#[test]
fn a_written_model_reads_back_as_the_network_it_came_from() {
    // The strong test, because the network arrives from MATPOWER and leaves
    // through CIM. No convention is shared between the two ends of it.
    let original = case14();
    let (back, notes) = round_trip(&original, "case14");
    assert_same_network(&back, &original, &notes);

    let demand = |n: &Network| n.loads.iter().map(|l| l.p_set).sum::<f64>();
    assert!(
        (demand(&back) - demand(&original)).abs() < 1e-6,
        "259 MW of demand went in and {} MW came out",
        demand(&back)
    );
}

#[test]
fn every_cgmes_fixture_survives_being_written_and_read() {
    // The fixtures are what the reader is tested against, so a writer that
    // cannot reproduce them is emitting something other than what this format
    // looks like. `cgmes_operated` is the interesting one: its hypothesis
    // switches a line off, so the network being written has one branch and
    // three nodes, one of which nothing reaches.
    for fixture in [
        "examples/cgmes",
        "examples/cgmes_operated",
        "examples/cgmes_solved",
    ] {
        let original = cgmes::load_model(path(fixture)).unwrap().network;
        let (back, notes) = round_trip(&original, "mini");
        assert_same_network(&back, &original, &notes);
        assert_eq!(
            back.buses[0].country, original.buses[0].country,
            "{fixture}"
        );
    }
}

#[test]
fn a_model_written_to_a_directory_is_read_back_by_the_reader_that_reads_published_ones() {
    // Not the same path as the in-memory round trip: this one goes through the
    // profile file names and the directory merge, which is how a published
    // model actually arrives.
    let original = case14();
    let dir = tmp("case14");
    let _ = std::fs::remove_dir_all(&dir);
    let notes = gridwright_io::write_cgmes(&original, &dir).unwrap();

    for f in ["case14_EQ.xml", "case14_TP.xml", "case14_SSH.xml"] {
        assert!(dir.join(f).exists(), "{f} was not written");
    }
    let back = cgmes::load_model(&dir).unwrap().network;
    assert_same_network(&back, &original, &notes);
}

#[test]
fn the_same_network_writes_the_same_bytes_every_time() {
    // A file assembled by walking a hash map differs run to run, which makes it
    // useless in version control and useless as the input to a comparison.
    let net = case14();
    let first = to_cgmes(&net, "case14");
    for _ in 0..4 {
        let again = to_cgmes(&net, "case14");
        assert_eq!(
            first.documents, again.documents,
            "two runs over the same network wrote different files"
        );
    }
    // And it is not accidentally deterministic by being empty.
    assert!(first.documents.iter().all(|(_, text)| text.len() > 1000));
}

#[test]
fn impedance_is_written_in_ohms_against_the_base_voltage() {
    // The characteristic failure of this format is a round trip that loses the
    // base, and it is silent: per-unit numbers in a field read as ohms describe
    // a network of near short circuits that solves and means nothing.
    //
    // Hand-derived from the PSS/E fixture, which carries genuine voltages where
    // PGLib normalises them to one: at 132 kV on a 100 MVA base the impedance
    // base is 132² / 100 = 174.24 Ω, so the first branch's 0.05917 per unit is
    // 10.309 Ω.
    let net = gridwright_io::psse::load_raw(path("examples/psse/case14_v33.raw"))
        .unwrap()
        .network;
    assert!(
        net.buses[0].v_nom > 100.0,
        "the fixture should carry real voltages"
    );
    let written = to_cgmes(&net, "case14");
    let eq = &written.documents[0].1;

    let want = net.lines[0].reactance * (132.0 * 132.0 / 100.0);
    let found: Vec<f64> = eq
        .lines()
        .filter(|l| l.contains("ACLineSegment.x"))
        .filter_map(|l| {
            l.split_once('>')
                .and_then(|(_, rest)| rest.split_once('<'))
                .and_then(|(v, _)| v.parse::<f64>().ok())
        })
        .collect();
    assert!(
        found.iter().any(|x| (x - want).abs() < 1e-6),
        "no reactance near {want} Ω was written; the file has {found:?}"
    );
    // And emphatically not the per-unit number, which is what a writer that
    // forgot the conversion would emit.
    assert!(
        found.iter().all(|x| *x > 0.5),
        "{found:?} looks like per-unit values written into an ohms field"
    );

    let (back, notes) = round_trip(&net, "case14");
    assert_same_network(&back, &net, &notes);
}

#[test]
fn a_rating_is_written_as_the_current_that_would_produce_it() {
    // The other conversion with a factor in it. The reader turns amps into MVA
    // through √3 · V · I, so the writer divides by the same thing; getting it
    // wrong by the √3 gives a network rated 73% too high, which is a plausible
    // number and therefore the dangerous kind of wrong.
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("A", "XX");
    let b = net.add_bus("B", "XX");
    net.buses[a].v_nom = 400.0;
    net.buses[b].v_nom = 400.0;
    net.add_line(Line {
        name: "AB".into(),
        bus0: a,
        bus1: b,
        s_nom: 1_039.230_484_541_326,
        susceptance: 80.0,
        reactance: 0.0125,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "g".into(),
        bus: a,
        p_nom: 100.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 50.0,
        ..Default::default()
    });

    let written = to_cgmes(&net, "amps");
    let eq = &written.documents[0].1;
    // 1039.23 MVA at 400 kV is 1500 A, which is a rating a 400 kV circuit
    // actually has. Compared as a number rather than as text, since the value
    // is written to the last bit that reads back rather than rounded to
    // something that looks tidy.
    let amps: f64 = eq
        .lines()
        .find(|l| l.contains("CurrentLimit.value"))
        .and_then(|l| l.split_once('>'))
        .and_then(|(_, rest)| rest.split_once('<'))
        .and_then(|(v, _)| v.parse().ok())
        .unwrap_or_else(|| panic!("no current limit was written:\n{eq}"));
    assert!(
        (amps - 1500.0).abs() < 1e-9,
        "{amps} A where 1500 was expected"
    );
    // Not the apparent power written into a field read as amps, and not out by
    // the √3 either, which would be 866 A and look entirely plausible.
    assert!((amps - net.lines[0].s_nom).abs() > 1.0);

    let (back, notes) = round_trip(&net, "amps");
    assert_same_network(&back, &net, &notes);
}

#[test]
fn a_transformer_carries_its_ratio_in_the_rated_voltages_of_its_windings() {
    // CIM has no tap ratio field. A fixed off-nominal ratio is the quotient of
    // what the two windings are rated at against what the two buses run at, and
    // a writer that emitted a single-step RatioTapChanger instead would be
    // describing a control the network never had.
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("HIGH", "XX");
    let b = net.add_bus("LOW", "XX");
    net.buses[a].v_nom = 400.0;
    net.buses[b].v_nom = 220.0;
    net.add_line(Line {
        name: "TX".into(),
        bus0: a,
        bus1: b,
        s_nom: 500.0,
        susceptance: 100.0,
        resistance: 0.0005,
        reactance: 0.01,
        tap_ratio: 0.95,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "g".into(),
        bus: a,
        p_nom: 100.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 50.0,
        ..Default::default()
    });

    let written = to_cgmes(&net, "tx");
    let eq = &written.documents[0].1;
    // 0.95 · 400 against 220: the high winding is rated below its bus by
    // exactly the ratio, and the low winding at its own.
    assert!(eq.contains("<cim:PowerTransformerEnd.ratedU>380</cim:PowerTransformerEnd.ratedU>"));
    assert!(eq.contains("<cim:PowerTransformerEnd.ratedU>220</cim:PowerTransformerEnd.ratedU>"));
    assert!(
        !eq.contains("RatioTapChanger"),
        "a tap changer was invented"
    );
    // The rating is the winding's own apparent power, not a current limit.
    assert!(eq.contains("<cim:PowerTransformerEnd.ratedS>500</cim:PowerTransformerEnd.ratedS>"));

    let (back, notes) = round_trip(&net, "tx");
    assert_same_network(&back, &net, &notes);
    assert!((back.lines[0].tap_ratio - 0.95).abs() < 1e-12);
}

#[test]
fn a_branch_between_two_voltages_is_a_transformer_even_with_no_tap() {
    // An ACLineSegment whose two terminals sit at different base voltages is
    // not something CIM allows, so the voltage step decides the class as much
    // as the ratio does. The fixture's 400/220 transformer has a ratio of one
    // and must not come back as a line.
    let net = cgmes::load_model(path("examples/cgmes")).unwrap().network;
    let tx = net
        .lines
        .iter()
        .position(|l| l.name == "TX 400/220")
        .expect("the fixture has a transformer");
    assert!((net.lines[tx].tap_ratio - 1.0).abs() < 1e-12);

    let written = to_cgmes(&net, "mini");
    let eq = &written.documents[0].1;
    assert!(
        eq.contains("<cim:PowerTransformer rdf:ID="),
        "no transformer:\n{eq}"
    );
    assert_eq!(
        eq.matches("<cim:ACLineSegment rdf:ID=").count(),
        1,
        "the transformer was written as a line segment"
    );
}

#[test]
fn equipment_never_names_its_bus_and_terminals_do() {
    // The indirection CIM requires, and the one way to get a file that is
    // correct in every component and describes nothing: equipment that names a
    // node directly, or terminals that name equipment and no node.
    let net = case14();
    let written = to_cgmes(&net, "case14");
    let eq = &written.documents[0].1;
    let tp = &written.documents[1].1;

    assert!(
        !eq.contains("TopologicalNode"),
        "the equipment profile names a topological node, which is the topology \
         profile's business"
    );
    // Every terminal points at its equipment in EQ and at its node in TP, and
    // there are as many of each.
    let in_eq = eq.matches("<cim:Terminal.ConductingEquipment").count();
    let in_tp = tp.matches("<cim:Terminal.TopologicalNode").count();
    assert_eq!(in_eq, in_tp, "{in_eq} terminals defined, {in_tp} placed");
    // 20 branches with two ends, 5 machines, 11 loads, one shunt.
    assert_eq!(in_eq, 20 * 2 + 5 + 11 + 1);

    // The topology profile updates terminals rather than redefining them. A
    // profile that claimed `rdf:ID` over an object another profile owns is the
    // RDF equivalent of two files declaring the same variable.
    assert!(tp.contains("<cim:Terminal rdf:about=\"#_"));
    assert!(!tp.contains("<cim:Terminal rdf:ID="));
}

#[test]
fn demand_lives_in_the_hypothesis_and_not_in_the_equipment_profile() {
    // Where CGMES puts it, and the reason the writer always emits three
    // profiles rather than two. An equipment profile with load in it is a
    // different standard's file.
    let net = case14();
    let written = to_cgmes(&net, "case14");
    let (eq, ssh) = (&written.documents[0].1, &written.documents[2].1);

    assert!(eq.contains("<cim:EnergyConsumer rdf:ID="));
    assert!(!eq.contains("EnergyConsumer.p"));
    assert!(ssh.contains("<cim:EnergyConsumer rdf:about=\"#_"));
    assert_eq!(
        ssh.matches("<cim:EnergyConsumer.p>").count(),
        net.loads.len()
    );

    // And the consequence, said out loud rather than left to be found.
    let joined = written.notes.join("\n");
    assert!(
        joined.contains("steady state hypothesis"),
        "{:?}",
        written.notes
    );

    // Reading the equipment and topology profiles alone gives the network with
    // its plant and no demand, which is what the standard says they mean.
    let eq_tp = vec![written.documents[0].clone(), written.documents[1].clone()];
    let designed = cgmes::parse_model(&eq_tp, "case14").unwrap().network;
    assert_eq!(designed.buses.len(), net.buses.len());
    assert_eq!(designed.lines.len(), net.lines.len());
    assert!(designed.loads.is_empty());
}

#[test]
fn each_profile_declares_what_it_is_and_what_it_depends_on() {
    // A CGMES document that does not say which profile it is cannot be merged
    // with anything, because the receiver has no way to know what it is holding.
    let net = case14();
    let written = to_cgmes(&net, "case14");
    let (eq, tp, ssh) = (
        &written.documents[0].1,
        &written.documents[1].1,
        &written.documents[2].1,
    );

    assert!(eq.contains("http://entsoe.eu/CIM/EquipmentCore/3/1"));
    assert!(
        eq.contains("http://entsoe.eu/CIM/EquipmentOperation/3/1"),
        "ratings were written, so the operation profile applies"
    );
    assert!(tp.contains("http://entsoe.eu/CIM/Topology/4/1"));
    assert!(ssh.contains("http://entsoe.eu/CIM/SteadyStateHypothesis/1/1"));

    // Both later profiles depend on the equipment one, and say so by naming its
    // header rather than by being in the same folder.
    let eq_id = eq
        .lines()
        .find(|l| l.contains("md:FullModel rdf:about"))
        .and_then(|l| l.split('"').nth(1))
        .expect("the equipment profile has a header")
        .to_string();
    assert!(eq_id.starts_with("urn:uuid:"));
    for later in [tp, ssh] {
        assert!(
            later.contains(&format!("md:Model.DependentOn rdf:resource=\"{eq_id}\"")),
            "a profile did not declare its dependency on the equipment file"
        );
    }
}

#[test]
fn what_cim_cannot_hold_is_reported() {
    // The contract that matters as much as the round trip. Silence here would
    // produce a file somebody trusted.
    let mut net = case14();
    net.add_storage(StorageUnit {
        name: "batt".into(),
        bus: 0,
        p_nom: 50.0,
        max_hours: 4.0,
        ..Default::default()
    });
    net.lines[0].phase_shift = 0.05;
    net.buses[3].b_shunt = 0.2;

    let written = to_cgmes(&net, "case14");
    let joined = written.notes.join("\n");
    for expected in [
        "storage",
        "phase shift",
        "generation costs",
        "shunt",
        // The two facts a network does not contain. Both are reported when the
        // caller did not supply them, rather than filled in from the clock or
        // from a load flow nobody ran.
        "Model.created",
        "no state variables profile was written",
        "no operating point in the hypothesis",
    ] {
        assert!(
            joined.contains(expected),
            "no note about {expected}: {:?}",
            written.notes
        );
    }
}

#[test]
fn a_bus_with_no_nominal_voltage_is_reported_rather_than_guessed_at() {
    // Ohms cannot be recovered from per unit without the base the per unit was
    // formed against. Writing the per-unit number into an ohms field is the one
    // thing that must not happen silently, so it is written and said.
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("A", "XX");
    let b = net.add_bus("B", "XX");
    net.add_line(Line {
        name: "AB".into(),
        bus0: a,
        bus1: b,
        s_nom: 100.0,
        susceptance: 20.0,
        reactance: 0.05,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "g".into(),
        bus: a,
        p_nom: 100.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 50.0,
        ..Default::default()
    });

    let (back, notes) = round_trip(&net, "novolts");
    let joined = notes.join("\n");
    assert!(joined.contains("no nominal voltage"), "{notes:?}");
    // The rating cannot be stated either, since a current limit needs a
    // voltage, and the reader gives the branch back as unlimited.
    assert!(
        joined.contains("current limit cannot be formed"),
        "{notes:?}"
    );
    assert!((back.lines[0].reactance - 0.05).abs() < 1e-12);
    assert!(back.lines[0].s_nom >= 1e6);
}

/// The network the checked-in fixture was written from.
///
/// The PSS/E encoding of the IEEE 14-bus rather than the MATPOWER one, because
/// PGLib normalises every nominal voltage to 1.0 and a CGMES model whose base
/// voltages are all one kilovolt exercises none of the containment: one base
/// voltage, one voltage level, and no branch that steps between two. This
/// fixture carries 132 kV across the transmission core and 33 or 11 kV below
/// it, so it has three base voltages, three voltage levels, and transformers
/// that are transformers because they change voltage.
/// The moment the checked-in fixture was made, in seconds since the epoch.
///
/// A constant rather than the clock, because `Model.created` is the one header
/// field that would otherwise make the fixture differ from itself on every run.
/// It is the real moment the fixture was first generated, which is what the
/// attribute means, and it stays put because the fixture does.
const FIXTURE_CREATED: i64 = 1_785_024_000; // 2026-07-26T00:00:00Z

fn fixture_options() -> ModelOptions<'static> {
    ModelOptions {
        created: Some(FIXTURE_CREATED),
        scenario_time: Some(FIXTURE_CREATED),
        state: None,
    }
}

fn fixture_source() -> Network {
    gridwright_io::psse::load_raw(path("examples/psse/case14_v33.raw"))
        .unwrap()
        .network
}

/// Regenerate `examples/cgmes_written`, which is not run in the ordinary suite.
///
/// Ignored because it writes into the repository rather than into a temporary
/// directory. Run it with `cargo test -p gridwright-io --all-features
/// regenerate -- --ignored` after any deliberate change to the writer, and read
/// the resulting diff: a change to what this emits should be legible as a
/// change to a CGMES model, and if it is not then something was emitted that
/// was not meant to be.
///
/// The fixture exists at all because it is the only test of the writer that
/// survives the process it ran in. Everything else here compares one run
/// against another run in the same executable, which cannot catch an identifier
/// derivation that is stable within a process and not across builds.
#[test]
#[ignore = "writes into the repository; run deliberately to refresh the fixture"]
fn regenerate_the_checked_in_fixtures() {
    for fixture in written_fixtures() {
        std::fs::create_dir_all(&fixture.dir).unwrap();
        for (file, text) in fixture.documents {
            std::fs::write(fixture.dir.join(file), text).unwrap();
        }
    }
}

/// One checked-in fixture: where it lives, and what the writer says it holds.
struct WrittenFixture {
    dir: std::path::PathBuf,
    documents: Vec<(String, String)>,
}

/// Every checked-in fixture, as the writer produces it.
///
/// One list, used both to write the files and to check them, so the two cannot
/// drift into disagreeing about which network a fixture came from.
fn written_fixtures() -> Vec<WrittenFixture> {
    // The model with no solved state, which is the ordinary case: a network
    // converted from somewhere else has plant and topology and nobody has run a
    // load flow on it.
    let plain = to_cgmes_with(&fixture_source(), "case14", &fixture_options());

    // And one with a state, so the state variables profile is on disk to be
    // read rather than only described. The state is the hand-derived one from
    // `examples/cgmes_solved`, where every number is checked in a comment, so
    // nothing in the SV file here was made up by this writer or by anything
    // else: it is that published answer, written back out.
    let (solved_case, solved_state) =
        cgmes::load_model_with_state(path("examples/cgmes_solved")).unwrap();
    let solved_state = solved_state.expect("the solved fixture publishes a state");
    let solved = to_cgmes_with(
        &solved_case.network,
        "mini",
        &ModelOptions {
            created: Some(FIXTURE_CREATED),
            scenario_time: Some(FIXTURE_CREATED),
            state: Some(&solved_state),
        },
    );

    vec![
        WrittenFixture {
            dir: path("examples/cgmes_written"),
            documents: plain.documents,
        },
        WrittenFixture {
            dir: path("examples/cgmes_written_solved"),
            documents: solved.documents,
        },
    ]
}

#[test]
fn the_checked_in_fixtures_are_byte_for_byte_what_this_writer_produces() {
    // Determinism across builds and not merely within one run. The identifiers
    // are derived from a hash written out by hand here rather than taken from
    // the standard library, whose hasher is explicitly allowed to change
    // between releases; this is the test that would catch it if that promise
    // were ever quietly broken by using one.
    for fixture in written_fixtures() {
        for (file, text) in fixture.documents {
            let on_disk = std::fs::read_to_string(fixture.dir.join(&file))
                .unwrap_or_else(|e| panic!("{file} is missing from the fixture: {e}"));
            assert_eq!(
                on_disk, text,
                "{file} differs from what the writer now produces; if the change was \
                 intended, refresh it with the ignored regeneration test in this file"
            );
        }
    }
}

#[test]
fn the_checked_in_fixture_reads_back_as_the_network_it_was_written_from() {
    // The round trip with a real file in the middle of it, which is the only
    // version of it that a reader written by somebody else could also be run
    // against.
    let original = fixture_source();
    let case = cgmes::load_model(path("examples/cgmes_written")).unwrap();
    assert_same_network(&case.network, &original, &case.notes);

    // And the file says what it is: three profiles, merged by the reader into
    // one network, with the demand arriving from the hypothesis.
    assert!(
        case.network.loads.iter().any(|l| l.p_set > 0.0),
        "the equipment profile alone would give a network with no demand in it"
    );
}

#[test]
fn a_solved_state_is_written_as_a_state_variables_profile_and_comes_back_unchanged() {
    // The strongest thing that can be said about the SV profile, and the reason
    // it is written from a state rather than from a network: the state that
    // goes out is the state that comes back, component for component, in CIM's
    // own signs. Nothing here was invented, because the state was read from a
    // published fixture that was worked out by hand.
    let (case, state) = cgmes::load_model_with_state(path("examples/cgmes_solved")).unwrap();
    let state = state.expect("the solved fixture publishes a state");

    let written = to_cgmes_with(
        &case.network,
        "mini",
        &ModelOptions {
            created: Some(FIXTURE_CREATED),
            scenario_time: Some(FIXTURE_CREATED),
            state: Some(&state),
        },
    );
    assert_eq!(written.documents.len(), 4, "no state variables profile");
    assert_eq!(written.documents[3].0, "mini_SV.xml");

    let (back_case, back) = cgmes::parse_model_with_state(&written.documents, "mini").unwrap();
    assert_same_network(&back_case.network, &case.network, &written.notes);
    let back = back.expect("the written model publishes a state");

    assert_eq!(back.voltages, state.voltages, "voltages moved");
    assert_eq!(back.branches, state.branches, "branch flows moved");
    assert_eq!(back.generators, state.generators, "machine flows moved");
    assert_eq!(back.loads, state.loads, "load flows moved");

    // 610 MW out of the machine, 610 into the line, and the line delivering 603
    // at the far end: the losses survived, which they would not have if either
    // end had been written at the wrong terminal.
    assert_eq!(back.branches[0].end0.map(|f| f.p), Some(610.0));
    assert_eq!(back.branches[0].end1.map(|f| f.p), Some(-603.0));

    // What was not written, said rather than left to be noticed by whoever
    // compares the tap positions and finds none.
    let joined = written.notes.join("\n");
    assert!(joined.contains("tap positions"), "{:?}", written.notes);
    assert!(joined.contains("TopologicalIsland"), "{:?}", written.notes);
}

#[test]
fn an_angle_goes_back_out_in_degrees() {
    // The reader stores radians because everything downstream of it wants
    // radians, and CIM publishes degrees. Writing radians into that field would
    // fail nowhere: it would make every angle difference wrong by a factor of
    // 57 and produce flows that look plausible.
    let (case, state) = cgmes::load_model_with_state(path("examples/cgmes_solved")).unwrap();
    let state = state.unwrap();
    // SOUTH 400 sits at -5 degrees in the fixture, which the reader holds as
    // -0.0873 radians.
    let radians = state.voltages[1].unwrap().angle;
    assert!((radians + 5f64.to_radians()).abs() < 1e-12);

    let written = to_cgmes_with(
        &case.network,
        "mini",
        &ModelOptions {
            state: Some(&state),
            ..Default::default()
        },
    );
    let sv = &written.documents[3].1;
    assert!(
        sv.contains("<cim:SvVoltage.angle>-5</cim:SvVoltage.angle>"),
        "the angle was not written back in degrees:\n{sv}"
    );
}

#[test]
fn a_machine_gets_an_operating_point_only_when_a_solved_state_supplies_one() {
    // A network states what a machine can do. What it is doing is a different
    // claim, and the hypothesis is where somebody would read it, so it is
    // written when it is known and reported when it is not.
    let (case, state) = cgmes::load_model_with_state(path("examples/cgmes_solved")).unwrap();
    let state = state.unwrap();

    let without = to_cgmes(&case.network, "mini");
    assert!(!without.documents[2].1.contains("RotatingMachine.p"));
    assert!(
        without
            .notes
            .join("\n")
            .contains("no operating point in the hypothesis")
    );

    let with = to_cgmes_with(
        &case.network,
        "mini",
        &ModelOptions {
            state: Some(&state),
            ..Default::default()
        },
    );
    // Negative because it is generating: CIM signs a terminal's flow into the
    // equipment, and the state was read in that convention and not flipped.
    assert!(
        with.documents[2]
            .1
            .contains("<cim:RotatingMachine.p>-610</cim:RotatingMachine.p>"),
        "{}",
        with.documents[2].1
    );
}

#[test]
fn a_stated_moment_reaches_every_header_and_an_unstated_one_is_reported() {
    // `Model.created` is required by CGMES and is not in a network. The
    // resolution is that the caller states it, which keeps the writer a
    // function of its arguments; what must not happen is a header quietly
    // stamped from the clock, since then the same network writes a different
    // file every run.
    let net = case14();
    let stated = to_cgmes_with(&net, "case14", &fixture_options());
    for (file, text) in &stated.documents {
        assert!(
            text.contains("<md:Model.created>2026-07-26T00:00:00Z</md:Model.created>"),
            "{file} has no created timestamp"
        );
        assert!(text.contains("<md:Model.scenarioTime>"), "{file}");
    }
    assert!(
        !stated.notes.join("\n").contains("Model.created"),
        "it was stated, so there is nothing to report: {:?}",
        stated.notes
    );
    // And stating it does not cost determinism, because it is an argument.
    assert_eq!(
        to_cgmes_with(&net, "case14", &fixture_options()).documents,
        stated.documents
    );

    let unstated = to_cgmes(&net, "case14");
    assert!(!unstated.documents[0].1.contains("Model.created"));
    assert!(unstated.notes.join("\n").contains("Model.created"));
}

#[test]
fn writing_to_a_directory_stamps_the_moment_it_wrote() {
    // The one place the clock is read, because a file being written to disk is
    // genuinely happening now and that is what the attribute records. Every
    // other entry point stays a function of its arguments.
    let dir = tmp("stamped");
    let _ = std::fs::remove_dir_all(&dir);
    gridwright_io::write_cgmes(&case14(), &dir).unwrap();
    let eq = std::fs::read_to_string(dir.join("stamped_EQ.xml")).unwrap();
    let created = eq
        .lines()
        .find(|l| l.contains("Model.created"))
        .expect("no created timestamp in a file written to disk");
    // Not a placeholder date: this century, and shaped like an xsd:dateTime.
    assert!(created.contains("T") && created.contains("Z"), "{created}");
    assert!(created.contains(">20"), "{created}");
}

#[test]
fn the_checked_in_solved_fixture_republishes_the_state_it_was_built_from() {
    // The fixture that has a state variables profile in it, read back off disk
    // with the reader that reads a published archive. Its numbers are the ones
    // derived by hand in `examples/cgmes_solved`, so this is a check that they
    // survived a write and a read rather than a check of the writer against
    // itself.
    let (original, state) = cgmes::load_model_with_state(path("examples/cgmes_solved")).unwrap();
    let state = state.unwrap();
    let (back, back_state) =
        cgmes::load_model_with_state(path("examples/cgmes_written_solved")).unwrap();
    assert_same_network(&back.network, &original.network, &back.notes);

    let back_state = back_state.expect("the fixture carries a state variables profile");
    assert_eq!(back_state.voltages, state.voltages);
    assert_eq!(back_state.branches, state.branches);
    assert_eq!(back_state.generators, state.generators);
    assert_eq!(back_state.loads, state.loads);

    // 410 kV on a 400 kV node is 1.025 per unit, which is the number the
    // original fixture derives in its own comment.
    let north = back_state.voltages[0].unwrap();
    assert_eq!(north.v_kv, 410.0);
    assert!((north.v_pu.unwrap() - 1.025).abs() < 1e-12);

    // The tap position and the shunt sections are not in the file, because
    // neither has an object here to hang off. Said in the notes rather than
    // discovered by finding an empty list.
    assert!(back_state.taps.is_empty());
    assert!(back_state.shunts.is_empty());
}

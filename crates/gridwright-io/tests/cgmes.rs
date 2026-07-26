//! CIM/CGMES, against values worked out by hand.
//!
//! There is no second reader to compare against here, so every expected number
//! is derived in the comment beside it. That is the same discipline the rest of
//! this crate uses where an independent parse is not available.
//!
//! The fixture is split across an equipment profile and a topology profile, as
//! a published model is. Neither file is a network on its own: the equipment
//! file has lines with terminals that reach nothing, and the topology file has
//! nodes with no equipment. Only merged do they describe anything, and that
//! merge is one of the things under test.

#![cfg(feature = "cgmes")]

use gridwright_io::cgmes;

fn path(rel: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn model() -> gridwright_io::Case {
    cgmes::load_model(path("examples/cgmes")).unwrap()
}

#[test]
fn equipment_finds_its_buses_through_terminals() {
    // Nothing in CIM says which bus a line is on. The line points at nothing;
    // two Terminal objects point at the line and at a node each. Getting this
    // wrong yields a network of correctly-specified components connected to
    // nothing at all.
    let c = model();
    assert_eq!(c.network.buses.len(), 3, "{:?}",
               c.network.buses.iter().map(|b| &b.name).collect::<Vec<_>>());
    assert_eq!(c.network.lines.len(), 2, "one line and one transformer");
    assert_eq!(c.network.generators.len(), 1);
    assert_eq!(c.network.loads.len(), 1);

    let north = c.network.buses.iter().position(|b| b.name == "NORTH 400").unwrap();
    let south4 = c.network.buses.iter().position(|b| b.name == "SOUTH 400").unwrap();
    let south2 = c.network.buses.iter().position(|b| b.name == "SOUTH 220").unwrap();

    let line = c.network.lines.iter().find(|l| l.name == "NORTH-SOUTH 1").unwrap();
    assert_eq!(
        (line.bus0.min(line.bus1), line.bus0.max(line.bus1)),
        (north.min(south4), north.max(south4))
    );
    let tx = c.network.lines.iter().find(|l| l.name == "TX 400/220").unwrap();
    assert_eq!(
        (tx.bus0.min(tx.bus1), tx.bus0.max(tx.bus1)),
        (south4.min(south2), south4.max(south2))
    );
    assert_eq!(c.network.generators[0].bus, north);
    assert_eq!(c.network.loads[0].bus, south2);
}

#[test]
fn ohms_become_per_unit_on_the_base_voltage() {
    // Hand-derived. At 400 kV on a 100 MVA base the impedance base is
    // 400² / 100 = 1600 Ω, so 20 Ω of reactance is 0.0125 per unit and the
    // susceptance is 80. Read as per unit already, this line would have a
    // susceptance of 0.05 and carry essentially nothing.
    let c = model();
    let line = c.network.lines.iter().find(|l| l.name == "NORTH-SOUTH 1").unwrap();
    assert!((line.reactance - 0.0125).abs() < 1e-12, "X {}", line.reactance);
    assert!((line.resistance - 0.00125).abs() < 1e-12, "R {}", line.resistance);
    assert!((line.susceptance - 80.0).abs() < 1e-9, "B {}", line.susceptance);
    assert!(c.notes.join("\n").contains("CIM/CGMES"), "{:?}", c.notes);
}

#[test]
fn transformer_impedance_comes_off_its_ends_and_is_rebased_per_end() {
    // The transformer itself carries no impedance; its two ends do, each on
    // its own rated voltage. End 1 has 16 Ω at 400 kV, which is 16/1600 =
    // 0.01 per unit; end 2 has zero at 220 kV. The sum is what the branch
    // carries, and its susceptance is 100.
    let c = model();
    let tx = c.network.lines.iter().find(|l| l.name == "TX 400/220").unwrap();
    assert!((tx.reactance - 0.01).abs() < 1e-12, "X {}", tx.reactance);
    assert!((tx.resistance - 0.0005).abs() < 1e-12, "R {}", tx.resistance);
    assert!((tx.susceptance - 100.0).abs() < 1e-9, "B {}", tx.susceptance);
    // Rated 400 on a 400 kV bus against rated 220 on a 220 kV bus is a ratio
    // of one: this transformer changes voltage but does not tap.
    assert!((tx.tap_ratio - 1.0).abs() < 1e-12, "tap {}", tx.tap_ratio);
}

#[test]
fn a_current_limit_becomes_an_apparent_power_rating() {
    // CIM rates lines in amps, and the optimisation is in megavolt-amperes.
    // √3 × 400 kV × 1500 A = 1039.23 MVA. Taking the amps as MVA would rate
    // this line at 1500 and let 44% more through it than it can carry.
    let c = model();
    let line = c.network.lines.iter().find(|l| l.name == "NORTH-SOUTH 1").unwrap();
    let want = 3f64.sqrt() * 400.0 * 1500.0 / 1000.0;
    assert!(
        (line.s_nom - want).abs() < 1e-6,
        "rating {} against {want}",
        line.s_nom
    );

    // The transformer has no current limit, so its rated apparent power is
    // what stands.
    let tx = c.network.lines.iter().find(|l| l.name == "TX 400/220").unwrap();
    assert!((tx.s_nom - 500.0).abs() < 1e-9, "rating {}", tx.s_nom);
}

#[test]
fn a_machine_takes_its_limits_from_the_generating_unit_behind_it() {
    // Only the SynchronousMachine sits on a terminal; the operating limits
    // live on the GeneratingUnit it points at, and the unit's own class is
    // where the fuel comes from.
    let c = model();
    let g = &c.network.generators[0];
    assert_eq!(g.name, "CCGT");
    assert!((g.p_nom - 800.0).abs() < 1e-9, "capacity {}", g.p_nom);
    assert!((g.p_min_pu - 0.25).abs() < 1e-12, "floor {}", g.p_min_pu);
    assert_eq!(g.carrier, "thermal");
    assert!((g.q_max - 300.0).abs() < 1e-9);
    assert!((g.q_min + 150.0).abs() < 1e-9);
}

#[test]
fn a_later_profile_extends_an_object_rather_than_replacing_it() {
    // The terminals are declared in the equipment profile with `rdf:ID` and
    // completed in the topology profile with `rdf:about`. If the second
    // definition replaced the first, every terminal would know its node and
    // have forgotten its equipment, and the network would have no branches.
    let c = model();
    assert_eq!(c.network.lines.len(), 2, "terminals lost their equipment");

    // And the same model read from the equipment profile alone has nodes it
    // cannot place, which is the honest failure rather than a silent empty
    // network.
    let eq_only = cgmes::load_model(path("examples/cgmes/mini_EQ.xml"));
    assert!(
        eq_only.is_err(),
        "an equipment profile with no topology defines no buses"
    );
}

#[test]
fn loads_carry_their_real_and_reactive_demand() {
    let c = model();
    assert!((c.network.loads[0].p_set - 600.0).abs() < 1e-9);
    assert!((c.network.loads[0].q_set - 100.0).abs() < 1e-9);
}

#[test]
fn the_absence_of_costs_is_stated() {
    // CIM describes plant, not markets. A reader that quietly left every
    // marginal cost at zero would produce a dispatch that looks like an
    // answer and is arbitrary.
    assert!(
        model().notes.join("\n").contains("no generation costs"),
        "{:?}",
        model().notes
    );
}

#[test]
fn a_model_reads_the_same_way_twice() {
    // Objects come out of a hash map, so anything depending on iteration
    // order would give a different bus numbering each run and make every
    // downstream result irreproducible.
    let a = model().network;
    let b = model().network;
    let names = |n: &gridwright_net::Network| {
        n.buses.iter().map(|x| x.name.clone()).collect::<Vec<_>>()
    };
    assert_eq!(names(&a), names(&b));
    let lines = |n: &gridwright_net::Network| {
        n.lines
            .iter()
            .map(|l| (l.name.clone(), l.bus0, l.bus1))
            .collect::<Vec<_>>()
    };
    assert_eq!(lines(&a), lines(&b));
}

#[test]
fn something_that_is_not_a_cim_model_is_refused() {
    assert!(cgmes::load_model(path("examples/pglib/case14_ieee.m")).is_err());
}

// --- The steady state hypothesis, and the form models are published in. ---

/// The same network with a steady state hypothesis added, in its own
/// directory so that the tests above keep describing the equipment merge
/// rather than the operating state.
fn operated() -> gridwright_io::Case {
    cgmes::load_model(path("examples/cgmes_operated")).unwrap()
}

#[test]
fn the_steady_state_hypothesis_supplies_the_demand() {
    // The equipment profile describes plant; the SSH says what it is doing. A
    // reader that stops at equipment produces a network with correct topology
    // and no load in it, which will solve and mean nothing.
    let with_ssh = operated();
    let load = with_ssh
        .network
        .loads
        .iter()
        .find(|l| l.name == "CITY LOAD")
        .expect("the load is missing");
    assert!(
        (load.p_set - 750.0).abs() < 1e-9,
        "the SSH says 750 MW and the equipment profile said 600: got {}",
        load.p_set
    );
    assert!((load.q_set - 120.0).abs() < 1e-9);
}

#[test]
fn equipment_the_hypothesis_switched_off_is_left_out() {
    // A model built as designed rather than as operated is a different and
    // usually more capable network.
    let c = operated();
    assert!(
        c.network.lines.iter().all(|l| l.name != "NORTH-SOUTH 1"),
        "an out-of-service line was included: {:?}",
        c.network.lines.iter().map(|l| &l.name).collect::<Vec<_>>()
    );
    assert!(
        c.notes.join("\n").contains("switched off"),
        "{:?}",
        c.notes
    );
}

#[test]
fn a_zip_archive_reads_as_the_unpacked_directory_does() {
    // The form ENTSO-E actually publishes. Requiring someone to unpack first
    // is a small tax on exactly the people this exists for.
    let zipped = cgmes::load_model(path("examples/cgmes/mini_model.zip")).unwrap();
    // The archive holds the equipment and topology profiles only, so it should
    // read exactly as that directory does.
    let unpacked = model();
    assert_eq!(zipped.network.lines.len(), unpacked.network.lines.len());
    assert_eq!(zipped.network.buses.len(), 3);
    let line = zipped
        .network
        .lines
        .iter()
        .find(|l| l.name == "NORTH-SOUTH 1")
        .unwrap();
    assert!((line.reactance - 0.0125).abs() < 1e-12);
}

// --- The state variables profile, which is an answer rather than a model. ---

/// The same three buses with a solved state published beside them, in its own
/// directory: the tests above are about assembling a network and these are
/// about reading somebody else's answer for it.
fn solved() -> (gridwright_io::Case, cgmes::SolvedState) {
    let (case, state) = cgmes::load_model_with_state(path("examples/cgmes_solved")).unwrap();
    let state = state.expect("the SV profile produced no state");
    (case, state)
}

/// Bus positions by name, since a solved state is indexed against the network
/// and a test that hard-coded the numbering would pass for the wrong reason.
fn bus(case: &gridwright_io::Case, name: &str) -> usize {
    case.network
        .buses
        .iter()
        .position(|b| b.name == name)
        .unwrap()
}

fn branch(case: &gridwright_io::Case, name: &str) -> usize {
    case.network
        .lines
        .iter()
        .position(|l| l.name == name)
        .unwrap()
}

#[test]
fn a_published_state_is_returned_beside_the_network_and_not_folded_into_it() {
    // The whole value of an SV profile is that it is an independent answer, and
    // it stops being independent the moment it is written into the model the
    // solver will be given. The network here has to be the same network the
    // equipment and topology profiles describe, unchanged.
    let (case, state) = solved();
    assert_eq!(case.network.buses.len(), 3);
    assert_eq!(case.network.lines.len(), 2);
    assert_eq!(case.network.generators.len(), 1);
    assert_eq!(case.network.loads.len(), 1);

    // The state says NORTH 400 was sitting at 410 kV. The bus is still 400 kV
    // nominal, because that is what it is rated at and not what it was doing.
    let north = bus(&case, "NORTH 400");
    assert!((case.network.buses[north].v_nom - 400.0).abs() < 1e-12);
    assert!((state.voltages[north].unwrap().v_kv - 410.0).abs() < 1e-12);

    // Likewise the load: the equipment profile's 600 MW stands, and the flow
    // measured at its terminal is reported separately even where they agree.
    assert!((case.network.loads[0].p_set - 600.0).abs() < 1e-9);
    assert!((state.loads[0].unwrap().p - 600.0).abs() < 1e-9);
}

#[test]
fn voltage_magnitudes_become_per_unit_on_the_bus_nominal_voltage() {
    // Hand-derived from the fixture: 410 / 400 = 1.025, 396 / 400 = 0.99 and
    // 214.5 / 220 = 0.975. Left as kilovolts these would be voltages of 410 and
    // 214.5 per unit, which no comparison against a solver would survive.
    let (case, state) = solved();
    let pu = |name: &str| state.voltages[bus(&case, name)].unwrap().v_pu.unwrap();
    assert!(
        (pu("NORTH 400") - 1.025).abs() < 1e-12,
        "{}",
        pu("NORTH 400")
    );
    assert!(
        (pu("SOUTH 400") - 0.99).abs() < 1e-12,
        "{}",
        pu("SOUTH 400")
    );
    assert!(
        (pu("SOUTH 220") - 0.975).abs() < 1e-12,
        "{}",
        pu("SOUTH 220")
    );
    // And the kilovolts are kept as published, since that is what a comparison
    // against the operator's own printout needs.
    assert!((state.voltages[bus(&case, "SOUTH 220")].unwrap().v_kv - 214.5).abs() < 1e-12);
}

#[test]
fn angles_arrive_in_degrees_and_are_stored_in_radians() {
    // CIM publishes degrees; every angle in this engine is radians, the same
    // convention the MATPOWER reader converts a phase shift into. The two
    // differ by a factor of 57.3, which does not fail anywhere — it produces
    // flows that look plausible and are wrong.
    let (case, state) = solved();
    let angle = |name: &str| state.voltages[bus(&case, name)].unwrap().angle;
    assert!((angle("NORTH 400") - 0.0).abs() < 1e-12);
    // -5 degrees is -5 × π / 180 = -0.08726646259971647 radians.
    let five = -5.0 * std::f64::consts::PI / 180.0;
    assert!(
        (angle("SOUTH 400") - five).abs() < 1e-15,
        "{}",
        angle("SOUTH 400")
    );
    // -8 degrees is -0.13962634015954636 radians.
    let eight = -8.0 * std::f64::consts::PI / 180.0;
    assert!(
        (angle("SOUTH 220") - eight).abs() < 1e-15,
        "{}",
        angle("SOUTH 220")
    );
    assert!(
        angle("SOUTH 220").abs() < 1.0,
        "{} is a degrees value that was never converted",
        angle("SOUTH 220")
    );
}

#[test]
fn a_branch_flow_is_matched_to_the_end_it_was_measured_at() {
    // The fixture lists each branch's receiving end first and names it so it
    // sorts first too. Filling the ends in the order they turn up would give
    // both branches a negative loss and a reversed direction, so each flow is
    // placed by the node its own terminal reaches.
    let (case, state) = solved();
    let line = &state.branches[branch(&case, "NORTH-SOUTH 1")];
    // 610 MW leaves NORTH, 603 MW arrives at SOUTH: 7 MW of losses.
    assert!(
        (line.end0.unwrap().p - 610.0).abs() < 1e-9,
        "{:?}",
        line.end0
    );
    assert!(
        (line.end1.unwrap().p + 603.0).abs() < 1e-9,
        "{:?}",
        line.end1
    );
    assert!((line.end0.unwrap().p + line.end1.unwrap().p - 7.0).abs() < 1e-9);
    // And 110 MVAr in against 85 MVAr out is 25 MVAr of reactive loss.
    assert!((line.end0.unwrap().q + line.end1.unwrap().q - 25.0).abs() < 1e-9);

    let tx = &state.branches[branch(&case, "TX 400/220")];
    // The transformer's bus0 is its 400 kV end, which is where the 603 MW
    // enters; 600 MW leaves at 220 kV, so it loses 3 MW and 15 MVAr.
    assert!((tx.end0.unwrap().p - 603.0).abs() < 1e-9, "{:?}", tx.end0);
    assert!((tx.end1.unwrap().p + 600.0).abs() < 1e-9, "{:?}", tx.end1);
    assert!((tx.end0.unwrap().p + tx.end1.unwrap().p - 3.0).abs() < 1e-9);
    assert!((tx.end0.unwrap().q + tx.end1.unwrap().q - 15.0).abs() < 1e-9);

    // The transformer's own ends were resolved through their TransformerEnd
    // terminals, so end0 really is the 400 kV side.
    let idx = branch(&case, "TX 400/220");
    assert_eq!(case.network.lines[idx].bus0, bus(&case, "SOUTH 400"));
    assert_eq!(case.network.lines[idx].bus1, bus(&case, "SOUTH 220"));
}

#[test]
fn a_generating_machine_reads_negative_because_the_flow_is_into_it() {
    // CIM signs every terminal flow into the equipment. The machine is
    // producing 610 MW, so the flow at its terminal is -610: the network's view
    // of the machine, not the machine's view of itself. A reader that silently
    // flipped this to match the engine's generator convention would leave the
    // branch flows disagreeing with the file they came from.
    let (_, state) = solved();
    let g = state.generators[0].unwrap();
    assert!((g.p + 610.0).abs() < 1e-9, "{g:?}");
    assert!((g.q + 110.0).abs() < 1e-9, "{g:?}");
}

#[test]
fn every_node_in_a_published_state_balances() {
    // The point of keeping CIM's signs: the terminals at a node sum to zero,
    // and that is a check on the reader's end-matching that needs no knowledge
    // of the network at all. Active power only — SOUTH 220 also carries a
    // capacitor bank, whose reactive injection is real equipment this reader
    // does not model and therefore does not index.
    let (case, state) = solved();
    let mut p = vec![0.0; case.network.buses.len()];
    for (i, b) in state.branches.iter().enumerate() {
        if let Some(f) = b.end0 {
            p[case.network.lines[i].bus0] += f.p;
        }
        if let Some(f) = b.end1 {
            p[case.network.lines[i].bus1] += f.p;
        }
    }
    for (i, g) in state.generators.iter().enumerate() {
        if let Some(f) = g {
            p[case.network.generators[i].bus] += f.p;
        }
    }
    for (i, l) in state.loads.iter().enumerate() {
        if let Some(f) = l {
            p[case.network.loads[i].bus] += f.p;
        }
    }
    for (i, total) in p.iter().enumerate() {
        assert!(
            total.abs() < 1e-9,
            "{} does not balance: {total} MW",
            case.network.buses[i].name
        );
    }
}

#[test]
fn a_tap_position_names_the_branch_it_drives() {
    // The path is SvTapStep to RatioTapChanger to PowerTransformerEnd to
    // PowerTransformer, and only then to a branch in the network. Reported and
    // not applied: the transformer's ratio still comes from its rated
    // voltages, because a published tap is a fact about the solved state and
    // not about the equipment.
    let (case, state) = solved();
    assert_eq!(state.taps.len(), 1, "{:?}", state.taps);
    let tap = &state.taps[0];
    assert_eq!(tap.name, "TX 400/220 TAP");
    // Three steps above the changer's neutral of 10.
    assert!((tap.position - 13.0).abs() < 1e-12, "{}", tap.position);
    assert_eq!(tap.branch, Some(branch(&case, "TX 400/220")));
    assert!((case.network.lines[branch(&case, "TX 400/220")].tap_ratio - 1.0).abs() < 1e-12);
}

#[test]
fn shunt_sections_say_where_a_compensator_was_left() {
    // Two of the capacitor bank's four sections were in. This reader builds no
    // shunt compensator, so there is nothing to fold the setting into and the
    // only honest thing to do is report it against the bus it sits on.
    let (case, state) = solved();
    assert_eq!(state.shunts.len(), 1, "{:?}", state.shunts);
    let shunt = &state.shunts[0];
    assert_eq!(shunt.name, "SOUTH CAP");
    assert!((shunt.sections - 2.0).abs() < 1e-12, "{}", shunt.sections);
    assert_eq!(shunt.bus, Some(bus(&case, "SOUTH 220")));
}

#[test]
fn the_coverage_of_a_solved_state_is_stated_in_the_notes() {
    // A caller holding only the `Case` would otherwise never learn that the
    // archive contained the operator's own answer and that it was deliberately
    // not applied.
    let (case, state) = solved();
    assert_eq!(state.buses_covered(), 3);
    assert_eq!(state.branches_covered(), 2);
    let notes = case.notes.join("\n");
    assert!(notes.contains("state variables"), "{:?}", case.notes);
    assert!(notes.contains("3 of 3 buses"), "{:?}", case.notes);
    assert!(notes.contains("2 of 2"), "{:?}", case.notes);
}

#[test]
fn a_model_that_nobody_has_solved_reports_no_state() {
    // Equipment and topology alone describe a network, not an answer, and a
    // state full of zeroes would be a claim the model never made.
    let (case, state) = cgmes::load_model_with_state(path("examples/cgmes")).unwrap();
    assert!(state.is_none(), "invented a solved state out of EQ and TP");
    assert!(
        !case.notes.join("\n").contains("state variables"),
        "{:?}",
        case.notes
    );
}

#[test]
fn a_solved_state_survives_the_archive_it_was_published_in() {
    // ENTSO-E ships the SV profile as one more file in the same zip, so it has
    // to arrive by that route as well as from an unpacked folder.
    let (from_zip, zipped) =
        cgmes::load_model_with_state(path("examples/cgmes_solved/mini_solved.zip")).unwrap();
    let (_, unpacked) = solved();
    assert_eq!(zipped, Some(unpacked));
    assert_eq!(from_zip.network.buses.len(), 3);
}

#[test]
fn a_solved_state_reads_the_same_way_twice() {
    // Everything here comes out of hash maps, and a state whose ends or whose
    // tap ordering changed between runs would make every comparison against it
    // irreproducible.
    let (_, a) = solved();
    let (_, b) = solved();
    assert_eq!(a, b);
}

#[test]
fn the_ordinary_reader_is_unchanged_by_a_state_being_present() {
    // The solved fixture is the plain one with an SV profile and two extra
    // pieces of equipment nothing reads, so the network it yields has to match
    // the one the equipment tests already pin down.
    let plain = model();
    let (solved_case, _) = solved();
    let shape = |c: &gridwright_io::Case| {
        c.network
            .lines
            .iter()
            .map(|l| (l.name.clone(), l.bus0, l.bus1, l.reactance))
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&plain), shape(&solved_case));
}

#[test]
fn an_archive_that_holds_no_xml_is_refused() {
    let dir = std::env::temp_dir().join("gridwright-cgmes-empty");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("empty.zip");
    // A minimal empty archive.
    std::fs::write(&path, b"PK\x05\x06\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00").unwrap();
    assert!(cgmes::load_model(&path).is_err());
}

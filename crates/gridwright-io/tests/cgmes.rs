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

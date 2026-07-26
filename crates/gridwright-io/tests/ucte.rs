//! UCTE-DEF, against a fixture whose every number is derived here.
//!
//! There is no second encoding of this network to compare against, so the
//! fixture is small enough that every value can be worked out by hand from the
//! ohms, microsiemens and amps in the file. That is the point: UCTE-DEF is
//! stated entirely in physical units and the formulation is entirely per unit,
//! so the conversions are the reader, and a test that only checked the network
//! loaded would check nothing.
//!
//! `examples/ucte/mini.uct` is four substations across two countries: a
//! generating node, a two-busbar load substation with a 400/110 transformer
//! under a tap changer, an out-of-service circuit, a closed busbar coupler and
//! a cross-border phase-shifting transformer.

use gridwright_io::ucte::load_ucte;
use gridwright_net::Line;

fn path(rel: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn mini() -> gridwright_io::Case {
    load_ucte(path("examples/ucte/mini.uct")).unwrap()
}

fn branch(case: &gridwright_io::Case, name: &str) -> Line {
    case.network
        .lines
        .iter()
        .find(|l| l.name == name)
        .unwrap_or_else(|| panic!("branch {name} is missing"))
        .clone()
}

/// The impedance base at a given nominal voltage: kV squared over MVA is ohms.
fn z_base(kv: f64) -> f64 {
    kv * kv / 100.0
}

#[test]
fn ohms_become_per_unit_on_the_voltage_the_node_code_encodes() {
    // `DGEN__11` and `DLOAD_11` both end in level code 1, which is 380 kV, so
    // Z_base = 380^2 / 100 = 1444 ohms. The circuit between them is 2.00 ohms
    // of resistance and 20.00 of reactance.
    let l = branch(&mini(), "DGEN__11-DLOAD_11-1");
    assert!(
        (l.resistance - 2.00 / z_base(380.0)).abs() < 1e-12,
        "R {} should be 2/1444",
        l.resistance
    );
    assert!(
        (l.reactance - 20.00 / z_base(380.0)).abs() < 1e-12,
        "X {} should be 20/1444",
        l.reactance
    );
    // And the DC susceptance is the reciprocal of the per-unit reactance, not
    // of the ohms: 1/0.013850 = 72.2, where 1/20 would give 0.05.
    assert!(
        (l.susceptance - z_base(380.0) / 20.00).abs() < 1e-9,
        "{}",
        l.susceptance
    );
}

#[test]
fn microsiemens_become_per_unit_by_multiplying_by_the_impedance_base() {
    // The admittance base is the reciprocal of the impedance base, so a
    // susceptance goes the other way from an impedance: 300 uS is
    // 300e-6 S * 1444 ohms = 0.4332 per unit. Dividing instead would give
    // 2.1e-10, a line with no charging at all.
    let l = branch(&mini(), "DGEN__11-DLOAD_11-1");
    assert!(
        (l.shunt_susceptance - 300e-6 * z_base(380.0)).abs() < 1e-12,
        "B {}",
        l.shunt_susceptance
    );
    assert!((l.shunt_susceptance - 0.4332).abs() < 1e-12);
}

#[test]
fn a_current_limit_in_amps_becomes_a_rating_in_megavolt_amperes() {
    // Three-phase apparent power from a line current limit:
    // S[MVA] = sqrt(3) * V[kV] * I[A] / 1000. At 380 kV, 1500 A is 987 MVA.
    // Reading 1500 A as 1500 MVA would rate the circuit 52 per cent high, and
    // nothing in the file would look wrong.
    let l = branch(&mini(), "DGEN__11-DLOAD_11-1");
    let want = 3f64.sqrt() * 380.0 * 1500.0 / 1000.0;
    assert!(
        (l.s_nom - want).abs() < 1e-9,
        "{} should be {want}",
        l.s_nom
    );
    assert!((l.s_nom - 987.268_960_314_26).abs() < 1e-9, "{}", l.s_nom);
}

#[test]
fn a_blank_field_does_not_shift_the_ones_after_it() {
    // The interconnector's charging susceptance is blank and its current limit
    // is 2000 A. Split the record on whitespace and the 2000 lands in the
    // susceptance field: the circuit loses its rating and gains a shunt
    // admittance of 2000e-6 * 1444 = 2.888 per unit, four thousand times what
    // it should be, and the file still parses.
    let l = branch(&mini(), "DGEN__11-FIMP__11-1");
    assert_eq!(l.shunt_susceptance, 0.0, "the blank field must stay blank");
    let want = 3f64.sqrt() * 380.0 * 2000.0 / 1000.0;
    assert!(
        (l.s_nom - want).abs() < 1e-9,
        "{} should be {want}",
        l.s_nom
    );
}

#[test]
fn a_transformer_rating_is_already_in_megavolt_amperes_and_is_not_converted() {
    // The contrast that makes the ampere conversion above dangerous: a line
    // states amps and a transformer states MVA, in a field eleven columns
    // earlier. Putting the 500 MVA nominal power through sqrt(3)*V*I/1000
    // would claim 329 GVA at 380 kV, or 95 GVA at 110.
    let t = branch(&mini(), "DLOAD_12-DLOAD_51-1");
    assert!((t.s_nom - 500.0).abs() < 1e-12, "rating {}", t.s_nom);
    assert!(
        mini()
            .notes
            .iter()
            .any(|n| n.contains("already in MVA and were not converted")),
        "{:?}",
        mini().notes
    );
}

#[test]
fn a_transformer_impedance_is_referred_to_the_regulated_winding() {
    // The 400/110 transformer's ohms belong to the node 2 winding, so the base
    // is 110^2/100 = 121 ohms, not 380^2/100 = 1444. Using node 1's would make
    // every impedance through it twelve times too small and the transformer
    // look twelve times stiffer than it is.
    //
    // The tap table supplies the impedance at the tap actually in use, which is
    // position 4: 0.60 and 16.50 ohms, replacing the 0.50 and 15.00 on the
    // transformer's own record.
    let t = branch(&mini(), "DLOAD_12-DLOAD_51-1");
    assert!(
        (t.resistance - 0.60 / z_base(110.0)).abs() < 1e-12,
        "R {} should be 0.60/121",
        t.resistance
    );
    assert!(
        (t.reactance - 16.50 / z_base(110.0)).abs() < 1e-12,
        "X {} should be 16.50/121",
        t.reactance
    );
    assert!(
        (t.reactance - 0.13636363636363635).abs() < 1e-12,
        "X {}",
        t.reactance
    );
    // Against node 1's base it would have been 16.50/1444 = 0.0114.
    assert!(
        (t.reactance - 16.50 / z_base(380.0)).abs() > 0.1,
        "the wrong winding's base was used"
    );
}

#[test]
fn the_tap_the_changer_is_sitting_on_moves_the_ratio() {
    // The regulation record gives a step of 1.25 per cent and a current tap of
    // 4, so the regulated winding has 5 per cent more turns: 110 kV rated
    // becomes 115.5. The per-unit ratio is then
    // (400/380) / (115.5/110) = 1.052632 / 1.05 = 1.002506.
    //
    // Ignoring the regulation record entirely would leave 1.052632, a five per
    // cent error in the voltage across the transformer.
    let t = branch(&mini(), "DLOAD_12-DLOAD_51-1");
    let want = (400.0 / 380.0) / (115.5 / 110.0);
    assert!(
        (t.tap_ratio - want).abs() < 1e-12,
        "{} should be {want}",
        t.tap_ratio
    );
    assert!(
        (t.tap_ratio - 1.0025062656641603).abs() < 1e-12,
        "{}",
        t.tap_ratio
    );
}

#[test]
fn an_angle_regulator_becomes_a_phase_shift() {
    // The cross-border transformer has an asymmetrical quadrature regulator:
    // three steps of one per cent at 90 degrees, so alpha = 0.03 and the
    // regulated winding's phasor is multiplied by 1 + j0.03. Its magnitude is
    // sqrt(1.0009) and its argument atan(0.03) = 1.7179 degrees.
    //
    // The tap changer sits on node 2 and this is the ratio applied at node 1,
    // so both are inverted: the magnitude divides and the angle is negated.
    let t = branch(&mini(), "DLOAD_11-FIMP__11-1");
    assert!(
        (t.tap_ratio - 1.0 / 1.0009_f64.sqrt()).abs() < 1e-12,
        "tap {}",
        t.tap_ratio
    );
    assert!(
        (t.phase_shift + 0.03_f64.atan()).abs() < 1e-12,
        "shift {} should be -{}",
        t.phase_shift,
        0.03_f64.atan()
    );
    // A transformer with no angle regulation must come out at a clean zero
    // rather than at a negative one.
    assert_eq!(branch(&mini(), "DLOAD_12-DLOAD_51-1").phase_shift, 0.0);
}

#[test]
fn generation_is_negative_in_the_file_and_positive_in_the_network() {
    // The node record is written in the load convention throughout, so a plant
    // producing 700 MW appears as -700.0 and its permissible band as -900.0 to
    // -200.0. Flipping the sign also swaps the ends: the *minimum* permissible
    // generation is the most negative number and therefore the *largest*
    // output. Taken at face value every plant would have negative capacity.
    let c = mini();
    let g = c
        .network
        .generators
        .iter()
        .find(|g| g.name == "gen_DGEN__11")
        .expect("the generating node produced no machine");
    assert!((g.p_nom - 900.0).abs() < 1e-12, "capacity {}", g.p_nom);
    assert!(
        (g.p_min_pu - 200.0 / 900.0).abs() < 1e-12,
        "must-run floor {}",
        g.p_min_pu
    );
    // The reactive band flips the same way: -300.0 to +300.0 in the file is a
    // machine that can produce 300 MVAr and absorb 300.
    assert!((g.q_max - 300.0).abs() < 1e-12, "q_max {}", g.q_max);
    assert!((g.q_min + 300.0).abs() < 1e-12, "q_min {}", g.q_min);
    assert!(c.network.generators.iter().all(|g| g.p_nom >= 0.0));
}

#[test]
fn a_node_code_gives_its_voltage_level_and_its_country() {
    // Neither is a numeric field. The seventh character of the eight-character
    // code is the voltage level — 1 is 380 kV, 5 is 110 — and the country
    // comes from the `##Z` header the node sits under.
    let c = mini();
    let bus = |name: &str| {
        c.network
            .buses
            .iter()
            .find(|b| b.name == name)
            .unwrap_or_else(|| panic!("{name} is missing"))
    };
    assert_eq!(bus("DGEN__11").v_nom, 380.0);
    assert_eq!(bus("DLOAD_51").v_nom, 110.0);
    assert_eq!(bus("DGEN__11").country, "DE");
    assert_eq!(bus("FIMP__11").country, "FR");
    // And every one of them is in the same synchronous area, because they are:
    // an AC interconnector between two of them would otherwise be rejected.
    assert_eq!(c.network.synchronous_areas().len(), 1);
}

#[test]
fn an_out_of_service_circuit_is_left_out_and_a_closed_coupler_is_kept() {
    // Status 8 is a real element out of operation and status 2 a closed busbar
    // coupler. Keeping the first would add a corridor the file says is not
    // there; dropping the second would split a substation in two.
    let c = mini();
    assert!(
        !c.network
            .lines
            .iter()
            .any(|l| l.name == "DLOAD_11-FIMP__11-1" && l.reactance > 0.02),
        "the out-of-service circuit was kept"
    );
    let coupler = branch(&c, "DLOAD_11-DLOAD_12-1");
    assert_eq!(coupler.reactance, 0.0);
    assert!(
        coupler.is_transport(),
        "a zero-impedance coupler cannot carry an infinite susceptance"
    );
    let notes = c.notes.join("\n");
    assert!(notes.contains("out-of-service"), "{notes}");
    assert!(notes.contains("busbar couplers"), "{notes}");
}

#[test]
fn what_the_format_cannot_carry_is_reported_rather_than_filled_in() {
    // A reader that silently leaves every marginal cost at zero produces a
    // dispatch that looks like an answer and is arbitrary.
    let c = mini();
    let notes = c.notes.join("\n");
    assert!(notes.contains("no generation costs"), "{notes}");
    assert!(notes.contains("no time series"), "{notes}");
    // And the assumed base, since the format declares none and every per-unit
    // number above depends on the choice.
    assert!(notes.contains("assumed base of 100 MVA"), "{notes}");
    assert!(c.network.generators.iter().all(|g| g.marginal_cost == 0.0));
    assert_eq!(c.network.base_mva, 100.0);
}

#[test]
fn a_ucte_file_is_a_valid_network() {
    let c = mini();
    assert!(c.network.validate().is_ok());
    assert_eq!(c.network.buses.len(), 5);
    // Two circuits, one coupler and two transformers; the out-of-service
    // circuit is not among them.
    assert_eq!(c.network.lines.len(), 5);
    let demand: f64 = c.network.loads.iter().map(|l| l.p_set).sum();
    assert!((demand - 720.0).abs() < 1e-12, "demand {demand}");
}

//! Bus shunts and phase shifts in the AC relaxation.
//!
//! In the DC model a shunt is a constant and a phase shift moves to the right
//! hand side. Neither simplification holds here: a shunt draws in proportion
//! to `|V|²`, which is the decision variable, and a phase shift rotates the
//! coupling between the two ends of a branch rather than offsetting it.

use gridwright_acopf::{AcOptions, BnbOptions, Status, solve_acopf, solve_acopf_with, solve_bnb};
use gridwright_net::{Generator, Line, Load, Network, Snapshots};

/// A two-bus network with room to move: enough generation, a line that is not
/// binding, and a voltage band wide enough that the answer is about the
/// physics rather than about the bounds.
fn two_bus() -> Network {
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("A", "AA");
    let b = net.add_bus("B", "AA");
    for bus in [a, b] {
        net.buses[bus].v_min = 0.9;
        net.buses[bus].v_max = 1.1;
    }
    net.add_generator(Generator {
        name: "g".into(),
        bus: a,
        p_nom: 500.0,
        marginal_cost: 20.0,
        q_min: -300.0,
        q_max: 300.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 100.0,
        q_set: 40.0,
        ..Default::default()
    });
    net.add_line(Line {
        name: "AB".into(),
        bus0: a,
        bus1: b,
        s_nom: 400.0,
        susceptance: 10.0,
        resistance: 0.01,
        reactance: 0.1,
        ..Default::default()
    });
    net
}

#[test]
fn a_shunt_conductance_adds_real_demand_that_scales_with_voltage() {
    let base = solve_acopf(&two_bus(), 0).unwrap();
    assert!(matches!(base.status, Status::Optimal | Status::OptimalRelaxed));

    let mut with_shunt = two_bus();
    with_shunt.buses[1].g_shunt = 0.2; // 20 MW at one per unit, on a 100 MVA base
    let after = solve_acopf(&with_shunt, 0).unwrap();
    assert!(matches!(after.status, Status::Optimal | Status::OptimalRelaxed));

    let extra = after.p_gen[0] - base.p_gen[0];
    assert!(
        extra > 10.0,
        "a 20 MW shunt should show up as real generation, got {extra} MW"
    );
    // Proportional to |V|², so it is not exactly 20 MW unless the voltage
    // happens to sit at one. It has to be in the right neighbourhood, and the
    // direction of the discrepancy has to follow the voltage.
    let v = after.voltage[1];
    let predicted = 0.2 * v * v * 100.0;
    assert!(
        (extra - predicted).abs() < 1.0,
        "drew {extra} MW at |V| = {v}, which predicts {predicted}"
    );
}

#[test]
fn a_capacitor_bank_supplies_reactive_power_the_generator_then_does_not_have_to() {
    // What shunt compensation is for. Reactive power does not travel well, so
    // supplying it locally is the entire reason capacitor banks are installed
    // at load, and a model without them makes the generator carry all of it.
    let base = solve_acopf(&two_bus(), 0).unwrap();

    let mut compensated = two_bus();
    compensated.buses[1].b_shunt = 0.3;
    let after = solve_acopf(&compensated, 0).unwrap();
    assert!(matches!(after.status, Status::Optimal | Status::OptimalRelaxed));

    assert!(
        after.q_gen[0] < base.q_gen[0],
        "the bank should relieve the generator: {} against {}",
        after.q_gen[0],
        base.q_gen[0]
    );
}

#[test]
fn a_zero_phase_shift_changes_nothing() {
    // The rotation is applied unconditionally, so it has to be exactly the
    // identity at zero. Anything else would mean every ordinary line in every
    // network had quietly acquired a different admittance.
    let base = solve_acopf(&two_bus(), 0).unwrap();
    let mut explicit = two_bus();
    explicit.lines[0].phase_shift = 0.0;
    let same = solve_acopf(&explicit, 0).unwrap();
    assert!((base.objective - same.objective).abs() < 1e-9);
    assert!((base.p_gen[0] - same.p_gen[0]).abs() < 1e-9);
    assert!((base.q_gen[0] - same.q_gen[0]).abs() < 1e-9);
}

#[test]
fn a_phase_shift_on_a_radial_branch_changes_nothing_and_should_not() {
    // Worth pinning down, because it looks at first like the shift failing to
    // reach the formulation.
    //
    // Two facts meet here. Physically, a shifter on a radial branch cannot
    // redirect power: there is nowhere else for it to go, and the flow is
    // whatever the load demands. Algebraically, the Jabr cone
    // `R² + I² ≤ u_i u_j` is invariant under rotation of `(R, I)`, and a phase
    // shift is exactly such a rotation, so it maps the feasible set onto
    // itself and carries the optimum with it.
    //
    // Both say the same thing, which is the reassuring part.
    let base = solve_acopf(&two_bus(), 0).unwrap();
    let mut shifted = two_bus();
    shifted.lines[0].phase_shift = 0.25;
    let after = solve_acopf(&shifted, 0).unwrap();
    assert!(matches!(after.status, Status::Optimal | Status::OptimalRelaxed));
    assert!(
        (after.p_gen[0] - base.p_gen[0]).abs() < 1e-6,
        "a radial shifter moved power it had nowhere to move: {} against {}",
        after.p_gen[0],
        base.p_gen[0]
    );
}

/// A triangle: three buses, three branches, generation at one and load at
/// another, so there are two paths and a genuine split to command.
fn triangle() -> Network {
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("A", "AA");
    let b = net.add_bus("B", "AA");
    let c = net.add_bus("C", "AA");
    for bus in [a, b, c] {
        net.buses[bus].v_min = 0.9;
        net.buses[bus].v_max = 1.1;
    }
    net.add_generator(Generator {
        name: "g".into(),
        bus: a,
        p_nom: 500.0,
        marginal_cost: 20.0,
        q_min: -300.0,
        q_max: 300.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "l".into(),
        bus: b,
        p_set: 120.0,
        q_set: 30.0,
        ..Default::default()
    });
    for (n0, n1, x) in [(a, b, 0.10), (b, c, 0.12), (c, a, 0.08)] {
        net.add_line(Line {
            name: format!("l{n0}{n1}"),
            bus0: n0,
            bus1: n1,
            s_nom: 400.0,
            susceptance: 1.0 / x,
            resistance: x / 10.0,
            reactance: x,
            ..Default::default()
        });
    }
    net
}

#[test]
fn a_phase_shift_bites_once_the_loop_is_actually_enforced() {
    // The rotation that leaves a radial branch alone is broken by a cycle,
    // because the cycle constraint is a statement about the voltages
    // themselves — `Im(W₁W₂W₃) = 0` — and rotating one branch's `(R, I)` no
    // longer satisfies it.
    //
    // Which means a plain Jabr relaxation *cannot see a phase shifter at all*,
    // on any network. That is not a defect in this code: it is precisely the
    // looseness around loops that the cycle constraints exist to remove, now
    // visible as a physical device the relaxation would otherwise ignore.
    // Adding the cycle constraint is not enough on its own: over the root box
    // its McCormick envelopes are so wide that they bind nothing, so the
    // rotation symmetry survives and the shifter stays invisible. It is the
    // spatial search, narrowing those boxes until the envelopes have teeth,
    // that finally makes the device visible to the model.
    let opts = BnbOptions {
        max_nodes: 40,
        gap_tol: 0.0,
        ac: AcOptions {
            cycle_constraints: true,
            max_triangles: 64,
            max_cycle_length: 3,
        },
        ..Default::default()
    };
    let base = solve_bnb(&triangle(), 0, opts).unwrap();
    let mut shifted = triangle();
    shifted.lines[0].phase_shift = 0.3;
    let after = solve_bnb(&shifted, 0, opts).unwrap();

    let d = (after.lower_bound - base.lower_bound).abs() / base.lower_bound.abs();
    assert!(
        d > 1e-8,
        "once the loop is enforced the shift must reach the answer: {} against {}",
        after.lower_bound,
        base.lower_bound
    );
}

#[test]
fn without_cycle_constraints_the_same_shift_is_invisible() {
    // The companion to the test above, stated so the reason is on record
    // rather than rediscovered. A relaxation that cannot distinguish a network
    // with a phase shifter from one without is a relaxation that is not
    // enforcing anything around the loop.
    let opts = AcOptions {
        cycle_constraints: false,
        max_triangles: 0,
            max_cycle_length: 3,
        };
    let base = solve_acopf_with(&triangle(), 0, opts).unwrap();
    let mut shifted = triangle();
    shifted.lines[0].phase_shift = 0.3;
    let after = solve_acopf_with(&shifted, 0, opts).unwrap();
    // Relative, because these are costs in the thousands and the solver's own
    // noise floor is far above an absolute microunit.
    assert!(
        (after.objective - base.objective).abs() / base.objective.abs() < 1e-8,
        "the plain relaxation is rotation-invariant and should not have noticed: \
         {} against {}",
        after.objective,
        base.objective
    );
}

#[test]
fn the_relaxation_stays_well_formed_with_both_present() {
    // Both terms enter rows the cone constraint also touches, so the check
    // that matters is that the problem is still solvable and the cone is still
    // as tight as it was.
    let mut net = two_bus();
    net.buses[1].g_shunt = 0.05;
    net.buses[1].b_shunt = 0.2;
    net.lines[0].phase_shift = 0.1;
    let sol = solve_acopf(&net, 0).unwrap();
    assert!(matches!(sol.status, Status::Optimal | Status::OptimalRelaxed));
    assert!(sol.cone_gap < 1e-4, "cone gap {}", sol.cone_gap);
    assert!(sol.voltage.iter().all(|v| (0.89..=1.11).contains(v)));
}

#[test]
fn a_real_network_with_shunts_still_solves() {
    // case300 carries both, and it is the case where ignoring them was
    // costing real megawatts.
    use gridwright_io::matpower::load_case;
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/pglib/case300_ieee.m");
    let net = load_case(path).unwrap().network;
    assert!(net.buses.iter().any(|b| b.b_shunt != 0.0));

    let sol = solve_acopf(&net, 0).unwrap();
    assert!(
        matches!(sol.status, Status::Optimal | Status::OptimalRelaxed),
        "{:?}",
        sol.status
    );
}

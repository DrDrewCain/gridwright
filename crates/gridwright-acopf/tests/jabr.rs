//! The Jabr relaxation, checked where its answer is known.

use gridwright_acopf::{Status, solve_acopf};
use gridwright_net::{Generator, Line, Load, Network, Snapshots};

fn two_bus(r: f64, x: f64, demand: f64, cost: f64) -> Network {
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("A", "X");
    let b = net.add_bus("B", "X");
    net.add_generator(Generator {
        name: "g".into(), bus: a, p_nom: 10.0, marginal_cost: cost,
        q_min: -10.0, q_max: 10.0, ..Default::default()
    });
    net.add_load(Load { name: "l".into(), bus: b, p_set: demand, q_set: 0.0 });
    net.add_line(Line {
        name: "AB".into(), bus0: a, bus1: b, s_nom: 100.0,
        resistance: r, reactance: x, susceptance: 1.0 / x, ..Default::default()
    });
    net
}

#[test]
fn a_radial_network_solves_and_the_relaxation_is_tight() {
    let net = two_bus(0.01, 0.1, 1.0, 10.0);
    let s = solve_acopf(&net, 0).unwrap();
    assert_eq!(s.status, Status::Optimal, "gap was {}", s.cone_gap);
    assert!(s.cone_gap < 1e-5, "radial network should be exact, gap {}", s.cone_gap);
}

#[test]
fn generation_exceeds_demand_by_the_losses() {
    let low = solve_acopf(&two_bus(0.005, 0.1, 1.0, 10.0), 0).unwrap();
    let high = solve_acopf(&two_bus(0.05, 0.1, 1.0, 10.0), 0).unwrap();
    assert!(low.p_gen[0] > 1.0, "lossless is not physical: {}", low.p_gen[0]);
    assert!(high.p_gen[0] > low.p_gen[0],
            "more resistance should mean more losses: {} vs {}", high.p_gen[0], low.p_gen[0]);
}

#[test]
fn voltages_stay_inside_their_band() {
    let mut net = two_bus(0.01, 0.1, 1.0, 10.0);
    for b in net.buses.iter_mut() { b.v_min = 0.95; b.v_max = 1.05; }
    let s = solve_acopf(&net, 0).unwrap();
    assert_eq!(s.status, Status::Optimal);
    for (i, &v) in s.voltage.iter().enumerate() {
        assert!((0.95 - 1e-4..=1.05 + 1e-4).contains(&v), "bus {i} at {v} pu");
    }
}

#[test]
fn reactive_limits_are_respected() {
    let mut net = two_bus(0.01, 0.1, 1.0, 10.0);
    net.generators[0].q_min = -0.05;
    net.generators[0].q_max = 0.05;
    let s = solve_acopf(&net, 0).unwrap();
    if matches!(s.status, Status::Optimal | Status::OptimalRelaxed) {
        assert!((-0.05 - 1e-4..=0.05 + 1e-4).contains(&s.q_gen[0]),
                "reactive output {} left its limits", s.q_gen[0]);
    }
}

#[test]
fn a_line_without_impedance_is_refused() {
    let mut net = two_bus(0.0, 0.0, 1.0, 10.0);
    net.lines[0].susceptance = 10.0;
    assert!(solve_acopf(&net, 0).is_err());
}

#[test]
fn the_ac_answer_exceeds_a_lossless_dc_one() {
    let net = two_bus(0.05, 0.1, 1.0, 10.0);
    let ac = solve_acopf(&net, 0).unwrap();
    assert!(ac.objective > 10.0, "AC {} should exceed lossless DC 10", ac.objective);
}

// ---------------------------------------------------------------------------
// Real networks
// ---------------------------------------------------------------------------

fn case(name: &str) -> gridwright_net::Network {
    gridwright_io::matpower::load_case(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/pglib")
            .join(format!("{name}.m")),
    )
    .unwrap()
    .network
}

#[test]
fn the_ieee_cases_solve_as_ac_problems() {
    for name in ["case14_ieee", "case30_ieee", "case57_ieee"] {
        let net = case(name);
        let s = solve_acopf(&net, 0).unwrap();
        assert!(
            matches!(s.status, Status::Optimal | Status::OptimalRelaxed),
            "{name} ended {:?}",
            s.status
        );
        assert!(s.objective.is_finite(), "{name} objective is not finite");
        // Voltages must sit inside the band the case declares.
        for (b, &v) in s.voltage.iter().enumerate() {
            let bus = &net.buses[b];
            assert!(
                v >= bus.v_min - 1e-3 && v <= bus.v_max + 1e-3,
                "{name}: bus {b} at {v} pu, band [{}, {}]",
                bus.v_min,
                bus.v_max
            );
        }
    }
}

#[test]
fn the_ac_model_accounts_for_losses_the_dc_model_cannot() {
    // An earlier version of this test asserted the AC relaxation could not come
    // in below the DC answer. That is not a theorem and it is not true: the
    // relaxation bounds the true *AC* optimum from below, while DC is a
    // different approximation that drops losses and reactive limits alike. A
    // relaxation permitting states physics forbids can and does land cheaper.
    //
    // What is genuinely checkable is that the AC model sees losses at all,
    // which is the thing a DC model structurally cannot: total generation must
    // exceed total demand, and the difference is resistive loss.
    for name in ["case14_ieee", "case30_ieee"] {
        let net = case(name);
        let s = solve_acopf(&net, 0).unwrap();
        assert!(matches!(s.status, Status::Optimal | Status::OptimalRelaxed));

        let demand: f64 = net.loads.iter().map(|l| l.p_set).sum();
        let generated: f64 = s.p_gen.iter().sum();
        assert!(
            generated > demand,
            "{name}: generated {generated:.2} MW against demand {demand:.2} MW, \
             so the model found no losses at all"
        );
        // Transmission losses on these networks are a few percent, not tens.
        let loss_fraction = (generated - demand) / demand;
        assert!(
            loss_fraction < 0.15,
            "{name}: losses came to {:.1}% of demand, which is not plausible",
            loss_fraction * 100.0
        );
    }
}

#[test]
fn tightness_is_reported_rather_than_assumed() {
    // The status distinguishes an answer from a bound. Whichever a meshed case
    // turns out to be, the cone gap and the status must agree with each other,
    // because a caller deciding whether to trust the voltages relies on it.
    for name in ["case14_ieee", "case30_ieee", "case57_ieee"] {
        let s = solve_acopf(&case(name), 0).unwrap();
        match s.status {
            Status::Optimal => assert!(
                s.cone_gap <= 1e-5,
                "{name} claims exactness with a gap of {}",
                s.cone_gap
            ),
            Status::OptimalRelaxed => assert!(
                s.cone_gap > 1e-5,
                "{name} claims looseness with a gap of {}",
                s.cone_gap
            ),
            other => panic!("{name} ended {other:?}"),
        }
    }
}

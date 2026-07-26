//! Interruptible supply contracts.
//!
//! Neither shedding, nor shifting, nor declining on a price curve. A large
//! consumer signs away the right to be cut a bounded number of times at an
//! agreed compensation, in exchange for a cheaper tariff. The energy does not
//! move to another hour and it is not valued on a curve: it is simply not
//! delivered, and the consumer is paid for that.
//!
//! The bound on how often is the entire contract. Without it this is expensive
//! shedding with extra steps, and it is also the only part that cannot be
//! written linearly, which is why a contract makes the model an integer one.

#![cfg(feature = "highs")]

use gridwright_build::build_lopf;
use gridwright_net::{Generator, Load, Network, Snapshots, TimeSeries};
use gridwright_solve::{HighsSolver, Solver, Status};

/// Four hours, three of them scarce, and one contract that may cover some.
fn contracted(mw: f64, times: usize, cost: f64) -> Network {
    let mut net = Network::new(Snapshots::hourly(4));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "cheap".into(),
        bus: b,
        p_nom: 120.0,
        marginal_cost: 10.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "peaker".into(),
        bus: b,
        p_nom: 200.0,
        marginal_cost: 500.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "smelter".into(),
        bus: b,
        p_set: 100.0,
        interruptible_mw: mw,
        max_interruptions: times,
        interruption_cost: cost,
        ..Default::default()
    });
    // The cheap unit is short in the last three hours, so the peaker or the
    // contract has to cover the gap.
    net.gen_availability =
        TimeSeries::from_rows(&[vec![1.0, 0.5, 0.5, 0.5], vec![1.0; 4]], 4).unwrap();
    net
}

struct Run {
    status: Status,
    cost: f64,
    cut: Vec<f64>,
    calls: Vec<f64>,
}

fn run(net: &Network) -> Run {
    let lopf = build_lopf(net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    Run {
        status: sol.status,
        cost: sol.objective,
        cut: lopf.vars.interrupt_mw[0]
            .map(|b| sol.trajectory(b).to_vec())
            .unwrap_or_default(),
        calls: lopf.vars.interrupt[0]
            .map(|b| sol.trajectory(b).to_vec())
            .unwrap_or_default(),
    }
}

#[test]
fn a_contract_makes_the_problem_an_integer_one() {
    // The count of interruptions is discrete, and saying so is better than
    // pretending: a contract called 0.6 times is not a contract.
    assert!(build_lopf(&contracted(40.0, 2, 100.0)).unwrap().model.is_mip());
    assert!(!build_lopf(&contracted(0.0, 0, 0.0)).unwrap().model.is_mip());
}

#[test]
fn a_contract_is_called_when_it_is_cheaper_than_the_peaker() {
    // 40 MW at 100 per MWh against a peaker at 500. Calling the contract is
    // the cheaper way to cover the gap.
    let r = run(&contracted(40.0, 3, 100.0));
    assert_eq!(r.status, Status::Optimal);
    assert!(
        r.cut.iter().sum::<f64>() > 1e-6,
        "the contract should have been called: {:?}",
        r.cut
    );
    for (t, c) in r.calls.iter().enumerate() {
        assert!((c - c.round()).abs() < 1e-6, "call {t} is {c}, not a decision");
    }
}

#[test]
fn a_contract_dearer_than_the_peaker_is_left_alone() {
    let r = run(&contracted(40.0, 3, 900.0));
    assert_eq!(r.status, Status::Optimal);
    assert!(
        r.cut.iter().sum::<f64>() < 1e-6,
        "an expensive contract should not be called: {:?}",
        r.cut
    );
}

#[test]
fn the_agreed_number_of_interruptions_is_respected() {
    // The heart of the contract. Three hours are scarce and the contract may
    // only be called in one of them, so the peaker covers the other two even
    // though calling would be cheaper.
    let r = run(&contracted(40.0, 1, 100.0));
    assert_eq!(r.status, Status::Optimal);
    let calls: f64 = r.calls.iter().sum();
    assert!(
        calls <= 1.0 + 1e-6,
        "called {calls} times against an agreed one: {:?}",
        r.calls
    );
    // And it was worth calling once, so it did.
    assert!((calls - 1.0).abs() < 1e-6, "{:?}", r.calls);
}

#[test]
fn more_interruptions_allowed_never_costs_more() {
    // Being permitted to call a contract is an option, not an obligation.
    let mut previous = f64::INFINITY;
    for times in [0usize, 1, 2, 3, 4] {
        let cost = run(&contracted(40.0, times, 100.0)).cost;
        assert!(
            cost <= previous + 1e-6,
            "allowing {times} calls cost {cost}, more than allowing fewer at {previous}"
        );
        previous = cost;
    }
}

#[test]
fn energy_flows_only_in_the_hours_the_contract_was_called() {
    // The tie between the continuous quantity and the discrete decision. Energy
    // not delivered without a call would be shedding wearing a contract's name.
    let r = run(&contracted(40.0, 2, 100.0));
    for (t, (&mw, &called)) in r.cut.iter().zip(&r.calls).enumerate() {
        if mw > 1e-6 {
            assert!(
                called > 0.5,
                "hour {t} cut {mw} MW without the contract being called"
            );
        }
    }
}

#[test]
fn the_contract_is_bounded_by_its_own_size() {
    let r = run(&contracted(25.0, 4, 50.0));
    for (t, &mw) in r.cut.iter().enumerate() {
        assert!(mw <= 25.0 + 1e-6, "hour {t} cut {mw} MW against a 25 MW contract");
    }
}

#[test]
fn interruption_is_preferred_to_shedding() {
    // Shedding is priced at the value of lost load, far above any contract, so
    // a system able to interrupt should interrupt rather than fail.
    let mut net = contracted(60.0, 4, 200.0);
    net.generators[1].p_nom = 0.0; // no peaker at all
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let cut: f64 = sol.trajectory(lopf.vars.interrupt_mw[0].unwrap()).iter().sum();
    assert!(cut > 1e-6, "the contract should have been called before shedding");
    assert!(
        sol.total_shed(&lopf.vars) < 60.0,
        "shed {} MWh with a contract available",
        sol.total_shed(&lopf.vars)
    );
}

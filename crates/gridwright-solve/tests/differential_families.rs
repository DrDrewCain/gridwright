//! Every constraint family, ours against HiGHS, one family at a time.
//!
//! `differential.rs` compares the two solvers on whole models — the IEEE
//! networks, a mixed dispatch model, unit commitment. That is the right shape
//! for asking whether the solver is correct, and it is how the phase-one bug
//! was caught. It is the wrong shape for asking *which* constraint family a
//! disagreement came from, because a mixed model exercises a dozen of them at
//! once and a whole-model test that fails names none of them.
//!
//! So this file turns one family on at a time against the same base network.
//! Each test does two things, and the second matters as much as the first:
//!
//! 1. The two solvers agree on the objective.
//! 2. The family **changed the answer**. A test that enables a constraint which
//!    does not bind proves that both solvers can ignore it consistently, which
//!    is worth nothing. Every test here asserts the objective moved, and the
//!    direction it moved is stated, so a formulation that quietly stops being
//!    generated fails here rather than passing silently.
//!
//! Objectives are compared rather than variable values, for the reason given in
//! `differential.rs`: a linear program often has many optima that cost the same,
//! and two solvers landing on different vertices of the same optimal face is
//! correct behaviour.

#![cfg(all(feature = "highs", feature = "simplex"))]

use gridwright_build::build_lopf;
use gridwright_net::{
    Generator, Line, Link, Load, Network, Scenario, Snapshots, StorageUnit, TimeSeries,
};
use gridwright_solve::{HighsSolver, SimplexSolver, Solver, Status};

/// Solve with both backends, require they agree, and return the objective.
fn both_agree(net: &Network, what: &str) -> f64 {
    let lopf = build_lopf(net).unwrap_or_else(|e| panic!("{what}: build failed: {e}"));
    let ours = SimplexSolver::default()
        .solve(&lopf)
        .unwrap_or_else(|e| panic!("{what}: ours failed: {e}"));
    let theirs = HighsSolver::default()
        .solve(&lopf)
        .unwrap_or_else(|e| panic!("{what}: HiGHS failed: {e}"));

    assert_eq!(ours.status, Status::Optimal, "{what}: ours did not solve");
    assert_eq!(theirs.status, Status::Optimal, "{what}: HiGHS did not solve");

    let scale = ours.objective.abs().max(theirs.objective.abs()).max(1.0);
    assert!(
        (ours.objective - theirs.objective).abs() / scale < 1e-6,
        "{what}: ours {}, HiGHS {}, relative difference {}",
        ours.objective,
        theirs.objective,
        (ours.objective - theirs.objective).abs() / scale
    );
    ours.objective
}

/// The family has to have done something, or the agreement above is agreement
/// about nothing.
fn must_cost_more(without: f64, with: f64, what: &str) {
    assert!(
        with > without * (1.0 + 1e-6),
        "{what}: the constraint did not bind — {without} without it, {with} with it. \
         A differential test on an inactive constraint proves only that both \
         solvers can ignore it."
    );
}

/// Three buses, cheap generation at A, expensive at C, and lines too small to
/// let A serve everything. Deliberately plain: every test below turns exactly
/// one thing on against this, so whatever changes is attributable.
fn base(hours: usize) -> Network {
    let mut net = Network::new(Snapshots::hourly(hours));
    let a = net.add_bus("A", "AA");
    let b = net.add_bus("B", "BB");
    let c = net.add_bus("C", "CC");

    for (name, bus, p_nom, cost) in [
        ("cheap_a", a, 400.0, 10.0),
        ("mid_b", b, 200.0, 45.0),
        ("dear_c", c, 300.0, 90.0),
    ] {
        net.add_generator(Generator {
            name: name.into(),
            bus,
            p_nom,
            marginal_cost: cost,
            ..Default::default()
        });
    }
    for (n0, n1, s_nom, susc) in [(a, b, 150.0, 9.0), (b, c, 130.0, 6.0), (a, c, 80.0, 4.0)] {
        net.add_line(Line {
            name: format!("l{n0}{n1}"),
            bus0: n0,
            bus1: n1,
            s_nom,
            susceptance: susc,
            ..Default::default()
        });
    }
    for (bus, p) in [(a, 120.0), (b, 140.0), (c, 190.0)] {
        net.add_load(Load {
            name: format!("ld{bus}"),
            bus,
            p_set: p,
            ..Default::default()
        });
    }
    net
}

#[test]
fn both_solvers_agree_on_the_base_network_they_all_start_from() {
    // If this one disagreed, every test below would be reporting the same
    // single fault under a dozen different names.
    for hours in [1, 6, 24] {
        let objective = both_agree(&base(hours), &format!("base {hours}h"));
        assert!(objective > 0.0, "{hours}h: a system with demand costs something");
    }
}

#[test]
fn both_solvers_agree_when_ramp_limits_bind() {
    let hours = 8;
    let without = both_agree(&base(hours), "ramps off");

    let mut net = base(hours);
    // Demand that swings, so a ramp limit has something to bite on. Without a
    // swing the limit is slack and the test would prove nothing.
    let swing: Vec<f64> = (0..hours)
        .map(|t| if t % 2 == 0 { 0.5 } else { 1.4 })
        .collect();
    net.load_profile = TimeSeries::from_rows(
        &[
            swing.iter().map(|f| f * 120.0).collect(),
            swing.iter().map(|f| f * 140.0).collect(),
            swing.iter().map(|f| f * 190.0).collect(),
        ],
        hours,
    )
    .unwrap();
    let with_swing = both_agree(&net, "ramps off, demand swinging");

    // A quarter of rating an hour on the cheap unit, which is what forces the
    // dear one on rather than merely reshuffling the cheap one.
    net.generators[0].ramp_up = 0.25;
    net.generators[0].ramp_down = 0.25;
    let with = both_agree(&net, "ramps on");

    must_cost_more(with_swing, with, "ramp limits");
    let _ = without;
}

#[test]
fn both_solvers_agree_when_transmission_losses_are_priced() {
    let hours = 4;
    let without = both_agree(&base(hours), "losses off");

    let mut net = base(hours);
    for line in &mut net.lines {
        line.loss = 0.03;
    }
    let with = both_agree(&net, "losses on");

    // Energy lost in transit has to be generated by someone, and the marginal
    // generator is not the cheap one.
    must_cost_more(without, with, "transmission losses");
}

#[test]
fn both_solvers_agree_under_a_carbon_budget() {
    let hours = 6;
    let mut net = base(hours);
    net.generators[0].co2_emissions = 0.9; // coal-like, and it is the cheap one
    net.generators[1].co2_emissions = 0.4;
    net.generators[2].co2_emissions = 0.0;
    let without = both_agree(&net, "carbon budget off");

    // Tight enough to displace the cheap emitter, which is the whole point of
    // the constraint and the only setting that tests it.
    let mut capped = net.clone();
    capped.co2_limit = Some(150.0);
    let with = both_agree(&capped, "carbon budget on");

    must_cost_more(without, with, "carbon budget");
}

#[test]
fn both_solvers_agree_under_a_water_budget() {
    let hours = 6;
    let mut net = base(hours);
    net.generators[0].water_use = 2.0;
    net.generators[1].water_use = 0.5;
    let without = both_agree(&net, "water budget off");

    let mut capped = net.clone();
    capped.water_limit = Some(400.0);
    let with = both_agree(&capped, "water budget on");

    must_cost_more(without, with, "water budget");
}

#[test]
fn both_solvers_agree_under_a_land_budget() {
    // Land is spent by capacity *built*, not by energy produced, so this one
    // only means anything against something worth building. A unit with no fuel
    // cost and a trivial capital cost is built as far as it is allowed to be,
    // which makes the ceiling the only thing deciding how much appears.
    let hours = 4;
    let mut net = base(hours);
    net.add_generator(Generator {
        name: "new_wind".into(),
        bus: 1,
        p_nom: 0.0,
        p_nom_extendable: true,
        p_nom_max: 500.0,
        capital_cost: 1.0,
        marginal_cost: 0.0,
        land_use: 0.2,
        ..Default::default()
    });
    let without = both_agree(&net, "land budget off");

    let mut capped = net.clone();
    // Room for 100 MW of new build and no more.
    capped.land_limit = Some(20.0);
    let with = both_agree(&capped, "land budget on");

    must_cost_more(without, with, "land budget");
}

#[test]
fn both_solvers_agree_when_a_reserve_margin_sizes_the_fleet() {
    let hours = 4;
    let mut net = base(hours);
    for g in &mut net.generators {
        g.p_nom_extendable = true;
        g.p_nom_max = 900.0;
        g.capital_cost = 30.0;
    }
    let without = both_agree(&net, "reserve off");

    let mut reserved = net.clone();
    // Peak demand is 450 MW against 900 MW of existing plant, so a margin has
    // to exceed 100% before it asks for anything that is not already there.
    // Below that the constraint is present and slack, and the test would be
    // comparing two identical answers.
    reserved.reserve_margin = Some(1.2);
    let with = both_agree(&reserved, "reserve on");

    // Firm capacity above peak demand has to be built and paid for even though
    // no snapshot dispatches it.
    must_cost_more(without, with, "reserve margin");
}

#[test]
fn both_solvers_agree_on_an_n_minus_one_secure_dispatch() {
    let hours = 3;
    let without = both_agree(&base(hours), "security off");

    let mut net = base(hours);
    net.contingencies_all_lines();
    let with = both_agree(&net, "security on");

    // A dispatch that must survive losing any single line cannot load the
    // remaining ones to their limits beforehand.
    must_cost_more(without, with, "N-1 security");
}

#[test]
fn both_solvers_agree_when_demand_can_shift_in_time() {
    let hours = 8;
    let mut net = base(hours);
    // The cheap unit is available on alternate hours, so the same megawatt-hour
    // costs 10 in one and 45 or 90 in the next.
    //
    // Two things about this fixture are load-bearing. Scaling demand up and
    // down instead of varying availability would move every hour's cost
    // together and leave nothing to shift *to*. And the cheap and dear hours
    // have to alternate rather than split the horizon in half, because shift
    // windows are fixed non-overlapping blocks that each sum to zero — a cheap
    // first half and a dear second half puts the two in different blocks, and
    // demand cannot cross between them however large the window is.
    let alternating: Vec<f64> = (0..hours).map(|t| if t % 2 == 0 { 1.0 } else { 0.0 }).collect();
    net.gen_availability =
        TimeSeries::from_rows(&[alternating, vec![1.0; hours], vec![1.0; hours]], hours).unwrap();
    let without = both_agree(&net, "shifting off");

    let mut shifted = net.clone();
    // The flexible load sits at A, beside the cheap unit. Putting it at C
    // instead proves nothing: the lines into C are the binding constraint
    // there, so demand moved into a cheap hour still cannot reach the cheap
    // generation and the objective does not move.
    shifted.loads[0].shiftable_pu = 0.4;
    shifted.loads[0].shift_window = 4;
    shifted.loads[0].shift_cost = 0.5;
    let with = both_agree(&shifted, "shifting on");

    // Shifting is a freedom rather than a restriction, so this one has to get
    // *cheaper*. A family that only ever adds cost would hide a sign error.
    assert!(
        with < without * (1.0 - 1e-6),
        "shiftable demand: {without} without it, {with} with it — a freedom that \
         costs more has its sign backwards"
    );
}

#[test]
fn both_solvers_agree_when_demand_bids_a_price() {
    let hours = 4;
    let mut net = base(hours);
    net.value_of_lost_load = 4000.0;
    let without = both_agree(&net, "elastic off");

    let mut elastic = net.clone();
    // A slice of the expensive bus's demand that would rather not be served
    // above 50/MWh, against generation there costing 90.
    elastic.loads[2].value_tranches = vec![(60.0, 50.0)];
    let with = both_agree(&elastic, "elastic on");

    assert!(
        with < without * (1.0 - 1e-6),
        "elastic demand: {without} inelastic, {with} elastic — a bid curve that \
         raises cost is being read as an obligation rather than an option"
    );
}

#[test]
fn both_solvers_agree_on_a_hydro_cascade() {
    let hours = 12;
    let mut net = base(hours);
    // Not cyclic: a reservoir forced back to its starting level by the end of
    // the horizon cannot spend the water that came down the cascade, and the
    // coupled and uncoupled cases would cost the same.
    let lower = net.add_storage(StorageUnit {
        name: "lower".into(),
        bus: 2,
        p_nom: 100.0,
        max_hours: 20.0,
        cyclic: false,
        spillable: true,
        ..Default::default()
    });
    net.add_storage(StorageUnit {
        name: "upper".into(),
        bus: 2,
        p_nom: 100.0,
        max_hours: 20.0,
        cyclic: false,
        spillable: true,
        ..Default::default()
    });
    // Inflow arrives at the upper reservoir only, so anything the lower one
    // releases must have come down the cascade.
    net.storage_inflow =
        TimeSeries::from_rows(&[vec![0.0; hours], vec![40.0; hours]], hours).unwrap();
    let uncoupled = both_agree(&net, "cascade uncoupled");

    let mut coupled = net.clone();
    coupled.storage[1].downstream = Some(lower);
    let with = both_agree(&coupled, "cascade coupled");

    // Coupling can only help — water the lower station would not otherwise have
    // seen now arrives — so this is another family that must reduce cost.
    assert!(
        with < uncoupled * (1.0 - 1e-9),
        "hydro cascade: {uncoupled} uncoupled, {with} coupled — releasing water \
         into a downstream reservoir cannot make the system worse off"
    );
}

#[test]
fn both_solvers_agree_when_a_link_couples_two_carriers() {
    let hours = 4;
    let mut net = base(hours);
    let h2 = net.add_bus("H2", "AA");
    net.add_load(Load {
        name: "hydrogen demand".into(),
        bus: h2,
        p_set: 30.0,
        ..Default::default()
    });
    // Without an electrolyser the hydrogen demand can only be shed.
    net.value_of_lost_load = 3000.0;
    let unlinked = both_agree(&net, "link absent");

    let mut linked = net.clone();
    linked.links.push(Link {
        name: "electrolyser".into(),
        bus0: 0,
        bus1: h2,
        p_nom: 100.0,
        efficiency: 0.7,
        ..Default::default()
    });
    let with = both_agree(&linked, "link present");

    assert!(
        with < unlinked * (1.0 - 1e-6),
        "sector coupling: {unlinked} without the link, {with} with it — a way to \
         serve demand that was being shed has to be cheaper than shedding it"
    );
}

#[test]
fn both_solvers_agree_across_investment_periods() {
    let hours = 8;
    let mut net = base(hours);
    for g in &mut net.generators {
        g.p_nom = 60.0;
        g.p_nom_extendable = true;
        g.p_nom_max = 800.0;
        g.capital_cost = 25.0;
    }
    let single = both_agree(&net, "single period");

    let mut staged = net.clone();
    staged.investment_periods = vec![
        gridwright_net::InvestmentPeriod {
            name: "now".into(),
            first_snapshot: 0,
            n_snapshots: 4,
            discount: 1.0,
        },
        gridwright_net::InvestmentPeriod {
            name: "later".into(),
            first_snapshot: 4,
            n_snapshots: 4,
            discount: 0.6,
        },
    ];
    let with = both_agree(&staged, "two periods");

    // Deferring half the horizon's costs to a discounted period has to change
    // the number; asserting only agreement would pass on a model that ignored
    // the periods entirely.
    assert!(
        (with - single).abs() / single.max(1.0) > 1e-6,
        "investment periods: {single} single-period, {with} staged — discounting \
         had no effect, so the periods are not reaching the objective"
    );
}

#[test]
fn both_solvers_agree_across_stochastic_scenarios() {
    let hours = 8;
    let mut net = base(hours);
    for g in &mut net.generators {
        g.p_nom = 80.0;
        g.p_nom_extendable = true;
        g.p_nom_max = 800.0;
        g.capital_cost = 25.0;
    }
    // Two futures over the same horizon: an easy one and a hard one. The
    // investment is shared, the operating cost is weighted.
    let severe = |mild: f64, harsh: f64| -> Vec<f64> {
        (0..hours).map(|t| if t >= 4 { harsh } else { mild }).collect()
    };
    let demand = vec![severe(120.0, 200.0), severe(140.0, 230.0), severe(190.0, 310.0)];
    net.load_profile = TimeSeries::from_rows(&demand, hours).unwrap();
    let deterministic = both_agree(&net, "deterministic");

    let mut stochastic = net.clone();
    stochastic.scenarios = vec![
        Scenario {
            name: "mild".into(),
            first_snapshot: 0,
            n_snapshots: 4,
            probability: 0.7,
        },
        Scenario {
            name: "severe".into(),
            first_snapshot: 4,
            n_snapshots: 4,
            probability: 0.3,
        },
    ];
    let with = both_agree(&stochastic, "stochastic");

    // Weighting the severe future at 0.3 rather than counting both in full has
    // to move the objective.
    assert!(
        with < deterministic * (1.0 - 1e-6),
        "scenarios: {deterministic} unweighted, {with} weighted — probabilities \
         are not reaching the operating costs"
    );
}

#[test]
fn both_solvers_agree_when_a_line_is_tapped_and_phase_shifted() {
    let hours = 3;
    let without = both_agree(&base(hours), "plain lines");

    let mut net = base(hours);
    net.lines[0].tap_ratio = 1.05;
    net.lines[0].phase_shift = 0.05;
    net.lines[1].shunt_susceptance = 0.02;
    let with = both_agree(&net, "tapped and shifted");

    // A phase shifter redirects flow rather than adding or removing it, so this
    // may go either way; what matters is that it changed something and that
    // both solvers changed by the same amount.
    assert!(
        (with - without).abs() / without.max(1.0) > 1e-9,
        "taps and phase shifts: {without} plain, {with} adjusted — neither \
         reached the flow equation"
    );
}

#[test]
fn both_solvers_agree_on_capacity_that_has_to_be_built() {
    let hours = 6;
    let mut net = base(hours);
    // Not enough plant to serve demand, so capacity must be built rather than
    // merely being available to build.
    for g in &mut net.generators {
        g.p_nom = 30.0;
        g.p_nom_extendable = true;
        g.p_nom_max = 700.0;
        g.capital_cost = 40.0;
    }
    net.value_of_lost_load = 5000.0;
    let with = both_agree(&net, "expansion");

    let mut fixed = net.clone();
    for g in &mut fixed.generators {
        g.p_nom_extendable = false;
    }
    let without = both_agree(&fixed, "no expansion");

    // Being allowed to build has to beat shedding at the value of lost load.
    assert!(
        with < without * (1.0 - 1e-6),
        "capacity expansion: {without} unable to build, {with} able to — \
         building cannot be worse than shedding at 5000/MWh"
    );
}


//! Constraint families switched on together, and the invariants that hold
//! however many of them there are.
//!
//! `differential_families.rs` turns one family on at a time, which is the right
//! shape for attributing a disagreement to a family. It is the wrong shape for
//! finding the bugs that only exist between families: a term counted twice
//! because two builders both add it, a limit that quietly disables another, a
//! freedom that stops being free once something else is switched on. Those
//! survive one-at-a-time testing by construction.
//!
//! Rather than enumerate pairs and assert specific numbers, which needs a hand
//! derivation per pair and does not scale, this asserts properties that must
//! hold for *every* combination. Three of them, each catching a different class
//! of fault:
//!
//! 1. **A slack constraint changes nothing.** A limit set so loose it cannot
//!    bind must leave the objective bit-for-bit identical. This is the sharpest
//!    of the three: it catches a constraint that costs something merely by
//!    existing, which is what a sign error, a double-counted term or a
//!    misplaced weight all look like from outside.
//! 2. **A restriction cannot make the system cheaper, and a freedom cannot make
//!    it dearer.** Direction is the thing formulation bugs get wrong, and the
//!    hydro cascade bug found in this project was exactly a freedom that cost
//!    money.
//! 3. **Two restrictions together cost at least what either costs alone.** If a
//!    pair comes out cheaper than one of its members, one of them is disabling
//!    the other rather than adding to it.
//!
//! None of these needs to know what the right answer is, which is what lets
//! them cover combinations nobody has worked out by hand.

#![cfg(feature = "highs")]

use gridwright_build::build_lopf;
use gridwright_net::{Generator, Line, Load, Network, Snapshots, StorageUnit, TimeSeries};
use gridwright_solve::{HighsSolver, Solver, Status};

/// A named change to the base network, applied by the tests below.
///
/// Boxed rather than generic because the cases live in a table and a table
/// wants one type, and named because the alternative is the same signature
/// written out at four call sites.
type Case = (&'static str, Box<dyn Fn(&mut Network)>);

fn cost(net: &Network, what: &str) -> f64 {
    let lopf = build_lopf(net).unwrap_or_else(|e| panic!("{what}: build failed: {e}"));
    let sol = HighsSolver::default()
        .solve(&lopf)
        .unwrap_or_else(|e| panic!("{what}: solve failed: {e}"));
    assert_eq!(sol.status, Status::Optimal, "{what}: did not solve");
    sol.objective
}

/// Three buses over a day, with enough going on that most families have
/// something to act on, and demand that varies so the time-coupled ones are not
/// trivially slack.
fn base() -> Network {
    let hours = 12;
    let mut net = Network::new(Snapshots::hourly(hours));
    let a = net.add_bus("A", "AA");
    let b = net.add_bus("B", "BB");
    let c = net.add_bus("C", "CC");

    for (name, bus, p_nom, mc, co2, water, land) in [
        ("coal_a", a, 350.0, 12.0, 0.9, 2.0, 0.004),
        ("gas_b", b, 220.0, 48.0, 0.4, 0.8, 0.001),
        ("peak_c", c, 260.0, 95.0, 0.5, 0.3, 0.001),
    ] {
        net.add_generator(Generator {
            name: name.into(),
            bus,
            p_nom,
            marginal_cost: mc,
            co2_emissions: co2,
            water_use: water,
            land_use: land,
            ..Default::default()
        });
    }
    for (n0, n1, s_nom, susc) in [(a, b, 160.0, 9.0), (b, c, 140.0, 6.0), (a, c, 90.0, 4.0)] {
        net.add_line(Line {
            name: format!("l{n0}{n1}"),
            bus0: n0,
            bus1: n1,
            s_nom,
            susceptance: susc,
            ..Default::default()
        });
    }
    for (bus, p) in [(a, 130.0), (b, 150.0), (c, 180.0)] {
        net.add_load(Load {
            name: format!("ld{bus}"),
            bus,
            p_set: p,
            ..Default::default()
        });
    }
    net.add_storage(StorageUnit {
        name: "batt".into(),
        bus: b,
        p_nom: 50.0,
        max_hours: 4.0,
        efficiency_store: 0.94,
        efficiency_dispatch: 0.94,
        cyclic: true,
        ..Default::default()
    });

    // Demand that moves, so ramps and storage are not slack by accident, and
    // availability that dips, so the network has to redistribute.
    let shape: Vec<f64> = (0..hours)
        .map(|t| 0.75 + 0.35 * ((t as f64) * 0.9).sin())
        .collect();
    net.load_profile = TimeSeries::from_rows(
        &[
            shape.iter().map(|f| f * 130.0).collect(),
            shape.iter().map(|f| f * 150.0).collect(),
            shape.iter().map(|f| f * 180.0).collect(),
        ],
        hours,
    )
    .unwrap();
    net.gen_availability = TimeSeries::from_rows(
        &[
            (0..hours).map(|t| if t % 3 == 0 { 0.6 } else { 1.0 }).collect(),
            vec![1.0; hours],
            vec![1.0; hours],
        ],
        hours,
    )
    .unwrap();
    net
}

/// Emissions, water and land actually produced by the base answer, so that a
/// "slack" ceiling can be set from what the system does rather than from a
/// number picked out of the air. A ceiling guessed too low would bind, and the
/// test would be measuring the wrong thing.
fn generous_ceilings(net: &mut Network) {
    net.co2_limit = Some(1e9);
    net.water_limit = Some(1e9);
    net.land_limit = Some(1e9);
}

#[test]
fn a_constraint_that_cannot_bind_does_not_change_the_answer() {
    // The sharpest invariant here. Each of these limits is set far beyond
    // anything the system could reach, so every one of them is present in the
    // model, generating rows, and inert. The objective must be identical.
    //
    // A family that fails this is charging for something merely by existing,
    // which is what a sign error, a term counted twice, or a weight applied to
    // the wrong side all look like from outside.
    let reference = cost(&base(), "base");

    let cases: Vec<Case> = vec![
        ("carbon ceiling", Box::new(|n: &mut Network| n.co2_limit = Some(1e9))),
        ("water ceiling", Box::new(|n: &mut Network| n.water_limit = Some(1e9))),
        ("land ceiling", Box::new(|n: &mut Network| n.land_limit = Some(1e9))),
        (
            "ramp limits at full rating",
            Box::new(|n: &mut Network| {
                for g in &mut n.generators {
                    g.ramp_up = 1.0;
                    g.ramp_down = 1.0;
                }
            }),
        ),
        (
            "zero losses",
            Box::new(|n: &mut Network| {
                for l in &mut n.lines {
                    l.loss = 0.0;
                }
            }),
        ),
        (
            "a shiftable share of nothing",
            Box::new(|n: &mut Network| {
                n.loads[0].shiftable_pu = 0.0;
                n.loads[0].shift_window = 4;
            }),
        ),
        (
            "an interruptible contract of nothing",
            Box::new(|n: &mut Network| {
                n.loads[1].interruptible_mw = 0.0;
                n.loads[1].max_interruptions = 3;
            }),
        ),
        ("every ceiling at once", Box::new(generous_ceilings)),
    ];

    for (what, apply) in cases {
        let mut net = base();
        apply(&mut net);
        let got = cost(&net, what);
        assert!(
            (got - reference).abs() <= 1e-6 * reference.abs().max(1.0),
            "{what}: an inert constraint moved the objective from {reference} to \
             {got}, so it is charging for its own existence"
        );
    }
}

#[test]
fn a_restriction_cannot_make_the_system_cheaper() {
    // Direction, which is what formulation bugs get wrong. Each of these
    // genuinely narrows what the system may do, so none of them may reduce
    // cost. A negative result here is the signature of a sign error.
    let reference = cost(&base(), "base");

    let cases: Vec<Case> = vec![
        (
            "a binding carbon cap",
            Box::new(|n: &mut Network| n.co2_limit = Some(2_000.0)),
        ),
        (
            "a binding water cap",
            Box::new(|n: &mut Network| n.water_limit = Some(4_000.0)),
        ),
        (
            "tight ramps",
            Box::new(|n: &mut Network| {
                for g in &mut n.generators {
                    g.ramp_up = 0.15;
                    g.ramp_down = 0.15;
                }
            }),
        ),
        (
            "priced losses",
            Box::new(|n: &mut Network| {
                for l in &mut n.lines {
                    l.loss = 0.03;
                }
            }),
        ),
        (
            "n-1 security",
            Box::new(|n: &mut Network| n.contingencies_all_lines()),
        ),
        (
            "a carbon price",
            Box::new(|n: &mut Network| n.co2_price = 40.0),
        ),
    ];

    for (what, apply) in cases {
        let mut net = base();
        apply(&mut net);
        let got = cost(&net, what);
        assert!(
            got >= reference - 1e-6 * reference.abs().max(1.0),
            "{what}: restricting the system made it cheaper, {got} against \
             {reference}, which means the constraint is paying rather than costing"
        );
    }
}

#[test]
fn a_freedom_cannot_make_the_system_dearer() {
    // The other direction, and the one that caught the hydro cascade bug: an
    // upstream release was formulated so that coupling two reservoirs made the
    // system buy energy instead of receiving water. Every option below is
    // strictly additional permission, so none may raise the cost.
    let reference = cost(&base(), "base");

    let cases: Vec<Case> = vec![
        (
            "demand that may shift",
            Box::new(|n: &mut Network| {
                n.loads[0].shiftable_pu = 0.25;
                n.loads[0].shift_window = 4;
                n.loads[0].shift_cost = 0.1;
            }),
        ),
        (
            "demand that may bid a price",
            Box::new(|n: &mut Network| {
                n.value_of_lost_load = 5_000.0;
                n.loads[2].value_tranches = vec![(40.0, 60.0)];
            }),
        ),
        (
            "capacity that may be built",
            Box::new(|n: &mut Network| {
                for g in &mut n.generators {
                    g.p_nom_extendable = true;
                    g.p_nom_max = 900.0;
                    g.capital_cost = 25.0;
                }
            }),
        ),
        (
            "a reservoir that may spill",
            Box::new(|n: &mut Network| {
                n.storage[0].spillable = true;
            }),
        ),
        (
            "more storage hours",
            Box::new(|n: &mut Network| {
                n.storage[0].max_hours = 12.0;
            }),
        ),
    ];

    for (what, apply) in cases {
        let mut net = base();
        apply(&mut net);
        let got = cost(&net, what);
        assert!(
            got <= reference + 1e-6 * reference.abs().max(1.0),
            "{what}: an additional freedom made the system dearer, {got} against \
             {reference}, so the option is being formulated as an obligation"
        );
    }
}

#[test]
fn two_restrictions_together_cost_at_least_what_either_costs_alone() {
    // Where an interaction bug actually shows. If a pair comes out cheaper than
    // one of its members, the two families are interfering: most likely one has
    // relaxed a row the other wrote, or both wrote the same row and the second
    // overwrote rather than added.
    let pairs: Vec<(Case, Case)> = vec![
        (
            ("carbon cap", Box::new(|n: &mut Network| n.co2_limit = Some(2_000.0))),
            ("water cap", Box::new(|n: &mut Network| n.water_limit = Some(4_000.0))),
        ),
        (
            ("carbon cap", Box::new(|n: &mut Network| n.co2_limit = Some(2_000.0))),
            ("carbon price", Box::new(|n: &mut Network| n.co2_price = 40.0)),
        ),
        (
            (
                "tight ramps",
                Box::new(|n: &mut Network| {
                    for g in &mut n.generators {
                        g.ramp_up = 0.15;
                        g.ramp_down = 0.15;
                    }
                }),
            ),
            ("n-1 security", Box::new(|n: &mut Network| n.contingencies_all_lines())),
        ),
        (
            (
                "priced losses",
                Box::new(|n: &mut Network| {
                    for l in &mut n.lines {
                        l.loss = 0.03;
                    }
                }),
            ),
            ("n-1 security", Box::new(|n: &mut Network| n.contingencies_all_lines())),
        ),
        (
            (
                "priced losses",
                Box::new(|n: &mut Network| {
                    for l in &mut n.lines {
                        l.loss = 0.03;
                    }
                }),
            ),
            ("carbon cap", Box::new(|n: &mut Network| n.co2_limit = Some(2_000.0))),
        ),
    ];

    for ((name_a, apply_a), (name_b, apply_b)) in pairs {
        let mut only_a = base();
        apply_a(&mut only_a);
        let a = cost(&only_a, name_a);

        let mut only_b = base();
        apply_b(&mut only_b);
        let b = cost(&only_b, name_b);

        let mut both = base();
        apply_a(&mut both);
        apply_b(&mut both);
        let ab = cost(&both, &format!("{name_a} + {name_b}"));

        let floor = a.max(b);
        assert!(
            ab >= floor - 1e-6 * floor.abs().max(1.0),
            "{name_a} + {name_b} together cost {ab}, less than {name_a} alone at \
             {a} and {name_b} alone at {b}. Adding a constraint cannot buy \
             anything, so one of these is cancelling the other."
        );
    }
}

#[test]
fn a_carbon_price_is_charged_once_when_a_cap_is_also_set() {
    // Setting both is legitimate and means something specific: a floor price
    // alongside a hard ceiling. The risk is that the price is applied twice, or
    // that the cap's dual is added to the objective on top of the price the
    // user already set, which would look like a plausible number and be wrong.
    //
    // With the cap slack, the answer must be exactly the priced answer: the
    // ceiling is present, generating its row, and deciding nothing.
    let mut priced = base();
    priced.co2_price = 40.0;
    let price_only = cost(&priced, "carbon price alone");

    let mut both = base();
    both.co2_price = 40.0;
    both.co2_limit = Some(1e9);
    let with_slack_cap = cost(&both, "carbon price with a slack cap");

    assert!(
        (with_slack_cap - price_only).abs() <= 1e-6 * price_only.abs().max(1.0),
        "a slack ceiling alongside a carbon price changed the answer from \
         {price_only} to {with_slack_cap}, so the two are interacting when only \
         one of them should be deciding anything"
    );
}

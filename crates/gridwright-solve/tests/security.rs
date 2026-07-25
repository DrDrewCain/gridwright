//! N-1 security, solved end to end.
//!
//! The LODF arithmetic is checked in `gridwright-build`. What matters here is
//! that the constraints change the answer: a dispatch that is optimal without
//! them and unsurvivable with them must actually be rejected.

use gridwright_build::build_lopf;
use gridwright_net::{Generator, Line, Load, Network, Snapshots};
use gridwright_solve::{HighsSolver, Solver, Status};

fn close(a: f64, b: f64) -> bool { (a - b).abs() < 1e-4 }

/// A triangle with cheap generation at A and demand at B. Losing the direct
/// A-B line pushes its whole flow onto the A-C-B path.
fn triangle(direct_rating: f64, detour_rating: f64, demand: f64) -> Network {
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("A", "X");
    let b = net.add_bus("B", "X");
    let c = net.add_bus("C", "X");
    net.add_generator(Generator {
        name: "cheap".into(), bus: a, p_nom: 1_000.0, marginal_cost: 5.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "local".into(), bus: b, p_nom: 1_000.0, marginal_cost: 90.0,
        ..Default::default()
    });
    net.add_load(Load { name: "l".into(), bus: b, p_set: demand, ..Default::default() });
    for (n0, n1, s) in [(a, b, direct_rating), (a, c, detour_rating), (c, b, detour_rating)] {
        net.add_line(Line {
            name: format!("{n0}{n1}"), bus0: n0, bus1: n1, s_nom: s,
            susceptance: 1.0, ..Default::default()
        });
    }
    net
}

#[test]
fn security_constraints_add_rows_without_adding_variables() {
    // The whole point of the LODF formulation: no duplicated flow variables.
    let base = triangle(200.0, 200.0, 90.0);
    let mut secure = base.clone();
    secure.contingencies_all_lines();

    let lb = build_lopf(&base).unwrap();
    let ls = build_lopf(&secure).unwrap();

    assert_eq!(lb.model.num_cols(), ls.model.num_cols(),
               "security must not introduce variables");
    assert!(ls.model.num_rows() > lb.model.num_rows(),
            "security should introduce rows");
}

#[test]
fn an_insecure_dispatch_is_rejected_once_n1_is_required() {
    // Without security the cheap generator serves everything: 60 MW down the
    // direct line, 30 round the detour. Losing the direct line would put all
    // 90 on a detour rated 40, so that dispatch cannot survive N-1 and the
    // expensive local unit has to run instead.
    let net = triangle(200.0, 40.0, 90.0);
    let insecure = {
        let l = build_lopf(&net).unwrap();
        HighsSolver::default().solve(&l).unwrap()
    };
    assert_eq!(insecure.status, Status::Optimal);
    assert!(close(insecure.objective, 90.0 * 5.0),
            "unsecured cost {}", insecure.objective);

    let mut secure_net = net.clone();
    secure_net.contingencies_all_lines();
    let ls = build_lopf(&secure_net).unwrap();
    let secure = HighsSolver::default().solve(&ls).unwrap();

    assert_eq!(secure.status, Status::Optimal);
    assert!(secure.objective > insecure.objective + 1.0,
            "security should cost something: {} vs {}", secure.objective, insecure.objective);
    // Some of the demand must now be met locally rather than imported.
    assert!(secure.dispatch(&ls.vars, 1)[0] > 1.0,
            "the expensive local unit should be running, got {}",
            secure.dispatch(&ls.vars, 1)[0]);
}

#[test]
fn a_well_reinforced_network_pays_nothing_for_security() {
    // Same topology with detour capacity to spare. N-1 is satisfied by the
    // economic dispatch already, so requiring it must not change the answer.
    let net = triangle(200.0, 200.0, 90.0);
    let insecure = {
        let l = build_lopf(&net).unwrap();
        HighsSolver::default().solve(&l).unwrap().objective
    };
    let mut secure_net = net.clone();
    secure_net.contingencies_all_lines();
    let ls = build_lopf(&secure_net).unwrap();
    let secure = HighsSolver::default().solve(&ls).unwrap();

    assert_eq!(secure.status, Status::Optimal);
    assert!(close(secure.objective, insecure),
            "security should be free here: {} vs {}", secure.objective, insecure);
}

#[test]
fn post_contingency_flows_stay_within_ratings() {
    // The property the constraints exist to guarantee, verified directly by
    // replaying every outage against the solved base case.
    let mut net = triangle(200.0, 60.0, 90.0);
    net.contingencies_all_lines();
    let lopf = build_lopf(&net).unwrap();
    let sol = HighsSolver::default().solve(&lopf).unwrap();
    assert_eq!(sol.status, Status::Optimal);

    let lodf = gridwright_build::security::compute_lodf(&net);
    for &k in &net.contingencies {
        if lodf.islanding.contains(&k) { continue; }
        let fk = sol.flow(&lopf.vars, k)[0];
        for l in 0..net.lines.len() {
            if l == k { continue; }
            let post = sol.flow(&lopf.vars, l)[0] + lodf.get(l, k) * fk;
            assert!(post.abs() <= net.lines[l].s_nom + 1e-3,
                    "losing line {k} put {post:.3} on line {l}, rated {}", net.lines[l].s_nom);
        }
    }
}

#[test]
fn security_on_a_real_network_solves_and_costs_more() {
    // IEEE 14 with every AC line as a contingency. The dispatch must remain
    // feasible and cannot be cheaper than the unsecured one.
    let case = gridwright_io::matpower::load_case(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/pglib/case14_ieee.m")).unwrap();

    let base = build_lopf(&case.network).unwrap();
    let unsecured = HighsSolver::default().solve(&base).unwrap();

    let mut net = case.network.clone();
    net.contingencies_all_lines();
    let secure_lopf = build_lopf(&net).unwrap();
    let secure = HighsSolver::default().solve(&secure_lopf).unwrap();

    assert_eq!(secure.status, Status::Optimal, "IEEE 14 should stay solvable under N-1");
    assert!(secure.objective >= unsecured.objective - 1e-6,
            "security cannot make dispatch cheaper: {} vs {}",
            secure.objective, unsecured.objective);
    assert!(secure_lopf.model.num_rows() > base.model.num_rows());
}

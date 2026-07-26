//! Correctness of the simplex, against answers derived by hand.

use gridwright_simplex::{Options, Problem, Status, solve};

const INF: f64 = f64::INFINITY;

/// Dense rows are far easier to read in a test than hand-written CSC.
struct B {
    cost: Vec<f64>,
    lo: Vec<f64>,
    up: Vec<f64>,
    rows: Vec<Vec<f64>>,
    rlo: Vec<f64>,
    rup: Vec<f64>,
}

impl B {
    fn new(cost: &[f64], lo: &[f64], up: &[f64]) -> Self {
        Self { cost: cost.into(), lo: lo.into(), up: up.into(),
               rows: vec![], rlo: vec![], rup: vec![] }
    }
    fn row(mut self, c: &[f64], lo: f64, up: f64) -> Self {
        self.rows.push(c.into()); self.rlo.push(lo); self.rup.push(up); self
    }
    fn go(&self) -> gridwright_simplex::Solution {
        let mut starts = vec![0u32]; let mut idx = vec![]; let mut val = vec![];
        for c in 0..self.cost.len() {
            for (r, row) in self.rows.iter().enumerate() {
                if row[c] != 0.0 { idx.push(r as u32); val.push(row[c]); }
            }
            starts.push(idx.len() as u32);
        }
        solve(Problem {
            n_cols: self.cost.len(), n_rows: self.rows.len(),
            col_starts: &starts, row_indices: &idx, values: &val,
            col_lower: &self.lo, col_upper: &self.up, col_cost: &self.cost,
            row_lower: &self.rlo, row_upper: &self.rup,
        }, Options::default()).unwrap()
    }
}

fn close(a: f64, b: f64) -> bool { (a - b).abs() < 1e-6 }

#[test]
fn a_two_variable_problem_with_a_known_answer() {
    // min 2x + 3y, x + y >= 10, x <= 6. Cheapest: x=6, y=4 => 24.
    let s = B::new(&[2.0, 3.0], &[0.0, 0.0], &[6.0, INF]).row(&[1.0, 1.0], 10.0, INF).go();
    assert_eq!(s.status, Status::Optimal);
    assert!(close(s.objective, 24.0), "objective {}", s.objective);
    assert!(close(s.col_value[0], 6.0) && close(s.col_value[1], 4.0), "{:?}", s.col_value);
}

#[test]
fn the_dual_of_a_binding_row_is_the_marginal_cost() {
    let s = B::new(&[2.0, 3.0], &[0.0, 0.0], &[6.0, INF]).row(&[1.0, 1.0], 10.0, INF).go();
    assert!(close(s.row_dual[0].abs(), 3.0), "dual {}", s.row_dual[0]);
}

#[test]
fn a_merit_order_prices_at_the_marginal_unit() {
    // 100 each at 10, 20, 30 meeting 250 => 4500, price 30.
    let s = B::new(&[10.0, 20.0, 30.0], &[0.0; 3], &[100.0; 3])
        .row(&[1.0, 1.0, 1.0], 250.0, 250.0).go();
    assert_eq!(s.status, Status::Optimal);
    assert!(close(s.objective, 4500.0), "objective {}", s.objective);
    assert!(close(s.row_dual[0].abs(), 30.0), "price {}", s.row_dual[0]);
}

#[test]
fn strong_duality_holds() {
    // min 4x + 6y, 2x + y >= 10, x + 3y >= 15.
    let s = B::new(&[4.0, 6.0], &[0.0, 0.0], &[INF, INF])
        .row(&[2.0, 1.0], 10.0, INF).row(&[1.0, 3.0], 15.0, INF).go();
    assert_eq!(s.status, Status::Optimal);
    let dual_obj = 10.0 * s.row_dual[0] + 15.0 * s.row_dual[1];
    assert!(close(dual_obj.abs(), s.objective),
            "dual {} vs primal {}", dual_obj.abs(), s.objective);
}

#[test]
fn a_slack_row_is_priced_at_zero() {
    let s = B::new(&[1.0], &[0.0], &[INF])
        .row(&[1.0], 5.0, INF).row(&[1.0], -INF, 100.0).go();
    assert_eq!(s.status, Status::Optimal);
    assert!(!close(s.row_dual[0], 0.0), "binding row must be priced");
    assert!(close(s.row_dual[1], 0.0), "slack row got {}", s.row_dual[1]);
}

#[test]
fn equality_rows_are_respected() {
    let s = B::new(&[1.0, 1.0], &[0.0, 0.0], &[INF, INF]).row(&[1.0, 2.0], 8.0, 8.0).go();
    assert_eq!(s.status, Status::Optimal);
    let lhs = s.col_value[0] + 2.0 * s.col_value[1];
    assert!(close(lhs, 8.0), "row value {lhs}");
}

#[test]
fn free_variables_may_go_negative() {
    // Voltage angles behave like this.
    let s = B::new(&[1.0], &[-INF], &[INF]).row(&[1.0], -20.0, -20.0).go();
    assert_eq!(s.status, Status::Optimal);
    assert!(close(s.col_value[0], -20.0), "got {}", s.col_value[0]);
}

#[test]
fn an_impossible_problem_is_infeasible() {
    let s = B::new(&[1.0], &[0.0], &[1.0]).row(&[1.0], 5.0, INF).go();
    assert_eq!(s.status, Status::Infeasible);
}

#[test]
fn an_unbounded_problem_is_unbounded() {
    let s = B::new(&[-1.0], &[0.0], &[INF]).row(&[1.0], 0.0, INF).go();
    assert_eq!(s.status, Status::Unbounded);
}

#[test]
fn variable_bounds_bind_without_any_row() {
    let s = B::new(&[1.0, -1.0], &[0.0, 0.0], &[10.0, 10.0]).go();
    assert_eq!(s.status, Status::Optimal);
    assert!(close(s.col_value[0], 0.0) && close(s.col_value[1], 10.0), "{:?}", s.col_value);
}

#[test]
fn a_ranged_row_binds_from_either_side() {
    for (lo, up, want) in [(2.0, 8.0, 2.0), (-INF, 8.0, 0.0)] {
        let s = B::new(&[1.0], &[0.0], &[INF]).row(&[1.0], lo, up).go();
        assert_eq!(s.status, Status::Optimal);
        assert!(close(s.col_value[0], want), "range [{lo},{up}] gave {}", s.col_value[0]);
    }
}

#[test]
fn a_degenerate_problem_terminates() {
    // Several rows binding at once is the classic way a naive simplex cycles.
    let s = B::new(&[1.0, 1.0, 1.0], &[0.0; 3], &[INF; 3])
        .row(&[1.0, 1.0, 0.0], 5.0, 5.0)
        .row(&[0.0, 1.0, 1.0], 5.0, 5.0)
        .row(&[1.0, 0.0, 1.0], 5.0, 5.0).go();
    assert_eq!(s.status, Status::Optimal);
    assert!(close(s.objective, 7.5), "objective {}", s.objective);
}

#[test]
fn a_two_bus_dispatch_prices_both_nodes() {
    // The engine's own smallest case: 80 MW of demand, 50 MW link, cheap
    // generation on the far side. Prices separate because the link saturates.
    // gen_de, gen_fr, flow
    let s = B::new(&[40.0, 10.0, 0.0], &[0.0, 0.0, -50.0], &[100.0, 200.0, 50.0])
        .row(&[1.0, 0.0, -1.0], 80.0, 80.0)   // DE balance
        .row(&[0.0, 1.0, 1.0], 0.0, 0.0)      // FR balance
        .go();
    assert_eq!(s.status, Status::Optimal);
    assert!(close(s.objective, 1700.0), "objective {}", s.objective);
    assert!(close(s.col_value[0], 30.0), "DE gen {}", s.col_value[0]);
    assert!(close(s.col_value[1], 50.0), "FR gen {}", s.col_value[1]);
    assert!(close(s.row_dual[0].abs(), 40.0), "DE price {}", s.row_dual[0]);
    assert!(close(s.row_dual[1].abs(), 10.0), "FR price {}", s.row_dual[1]);
}

#[test]
fn the_cost_crash_starts_variables_where_the_objective_wants_them() {
    // Maximise x + 2y subject to x + y <= 10, both in [0, 8], written as a
    // minimisation of the negated objective. Both costs are then negative, so
    // both variables start at their upper bounds — which is where the answer
    // very nearly is, and the simplex has less to do.
    //
    // The crash is inert on an ordinary energy model, where generation costs,
    // shedding penalties and capital costs are all non-negative and no variable
    // has a bound the objective prefers. It bites on a maximisation, which is
    // the same problem with every sign flipped.
    let starts = [0u32, 1, 2];
    let rows = [0u32, 0];
    let vals = [1.0f64, 1.0];
    let lower = [0.0f64, 0.0];
    let upper = [8.0f64, 8.0];
    let cost = [-1.0f64, -2.0];
    let row_lower = [f64::NEG_INFINITY];
    let row_upper = [10.0f64];
    let p = Problem {
        n_cols: 2,
        n_rows: 1,
        col_starts: &starts,
        row_indices: &rows,
        values: &vals,
        col_lower: &lower,
        col_upper: &upper,
        col_cost: &cost,
        row_lower: &row_lower,
        row_upper: &row_upper,
    };

    let crashed = solve(p, Options { cost_crash: true, ..Default::default() }).unwrap();
    let plain = solve(p, Options { cost_crash: false, ..Default::default() }).unwrap();

    assert_eq!(crashed.status, Status::Optimal);
    assert_eq!(plain.status, Status::Optimal);
    // y takes its full 8 and x takes what is left, so the objective is -(2+16).
    assert!((crashed.objective + 18.0).abs() < 1e-9, "{}", crashed.objective);
    assert!(
        (crashed.objective - plain.objective).abs() < 1e-9,
        "the crash changed the answer: {} against {}",
        crashed.objective,
        plain.objective
    );
    assert!(
        crashed.iterations <= plain.iterations,
        "the crash cost iterations rather than saving them: {} against {}",
        crashed.iterations,
        plain.iterations
    );
}

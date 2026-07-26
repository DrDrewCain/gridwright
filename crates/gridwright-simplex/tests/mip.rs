//! Branch and bound, against answers worked out by hand.
//!
//! Small enough problems that the optimum can be reasoned about rather than
//! trusted, which is the only way to test a search whose whole job is to find
//! something a relaxation cannot.

use gridwright_simplex::{MipOptions, Problem, Status, solve, solve_mip};

/// A problem in the shape the solver takes: compressed sparse column.
struct Lp {
    n_cols: usize,
    n_rows: usize,
    starts: Vec<u32>,
    rows: Vec<u32>,
    vals: Vec<f64>,
    lower: Vec<f64>,
    upper: Vec<f64>,
    cost: Vec<f64>,
    row_lower: Vec<f64>,
    row_upper: Vec<f64>,
}

impl Lp {
    fn problem(&self) -> Problem<'_> {
        Problem {
            n_cols: self.n_cols,
            n_rows: self.n_rows,
            col_starts: &self.starts,
            row_indices: &self.rows,
            values: &self.vals,
            col_lower: &self.lower,
            col_upper: &self.upper,
            col_cost: &self.cost,
            row_lower: &self.row_lower,
            row_upper: &self.row_upper,
        }
    }
}

/// Build from dense columns, which is clearer than hand-writing the offsets.
fn lp(cols: &[Vec<f64>], cost: &[f64], bounds: &[(f64, f64)], rows: &[(f64, f64)]) -> Lp {
    let n_rows = rows.len();
    let mut starts = vec![0u32];
    let mut ri = Vec::new();
    let mut vals = Vec::new();
    for col in cols {
        for (r, &v) in col.iter().enumerate() {
            if v != 0.0 {
                ri.push(r as u32);
                vals.push(v);
            }
        }
        starts.push(ri.len() as u32);
    }
    Lp {
        n_cols: cols.len(),
        n_rows,
        starts,
        rows: ri,
        vals,
        lower: bounds.iter().map(|b| b.0).collect(),
        upper: bounds.iter().map(|b| b.1).collect(),
        cost: cost.to_vec(),
        row_lower: rows.iter().map(|r| r.0).collect(),
        row_upper: rows.iter().map(|r| r.1).collect(),
    }
}

#[test]
fn a_relaxation_that_is_already_integral_needs_no_search() {
    //   minimise  -x - y
    //   subject to  x + y <= 3,  x, y in [0, 3] integer
    // The vertex is at (3, 0) or (0, 3), integral either way.
    let p = lp(
        &[vec![1.0], vec![1.0]],
        &[-1.0, -1.0],
        &[(0.0, 3.0), (0.0, 3.0)],
        &[(f64::NEG_INFINITY, 3.0)],
    );
    let r = solve_mip(p.problem(), &[true, true], MipOptions::default()).unwrap();
    assert_eq!(r.status, Status::Optimal);
    assert!((r.objective + 3.0).abs() < 1e-9, "{}", r.objective);
    assert!(r.proved);
    assert_eq!(r.nodes, 1, "an integral relaxation should not branch");
}

#[test]
fn a_fractional_relaxation_is_rounded_by_searching_not_by_rounding() {
    //   maximise  x + y   (as minimise -x - y)
    //   subject to  2x + 2y <= 3,  x, y in {0, 1}
    // The relaxation gives 1.5 and the integer optimum is 1. Rounding the
    // relaxation down componentwise would give (0.75, 0.75) -> (0, 0) and an
    // objective of 0, which is feasible and wrong.
    let p = lp(
        &[vec![2.0], vec![2.0]],
        &[-1.0, -1.0],
        &[(0.0, 1.0), (0.0, 1.0)],
        &[(f64::NEG_INFINITY, 3.0)],
    );
    let relaxed = solve(p.problem(), Default::default()).unwrap();
    assert!((relaxed.objective + 1.5).abs() < 1e-9, "{}", relaxed.objective);

    let r = solve_mip(p.problem(), &[true, true], MipOptions::default()).unwrap();
    assert_eq!(r.status, Status::Optimal);
    assert!((r.objective + 1.0).abs() < 1e-9, "{}", r.objective);
    assert!(r.proved, "gap {}", r.gap);
    for v in &r.col_value {
        assert!((v - v.round()).abs() < 1e-6, "{:?} is not integral", r.col_value);
    }
}

#[test]
fn a_knapsack_finds_the_answer_the_greedy_choice_misses() {
    // Capacity 10. Items of weight 6, 5, 5 and value 7, 5, 5.
    // Greedy by value density takes the first (7) and then nothing else fits.
    // The optimum takes the two fives for 10.
    let p = lp(
        &[vec![6.0], vec![5.0], vec![5.0]],
        &[-7.0, -5.0, -5.0],
        &[(0.0, 1.0); 3],
        &[(f64::NEG_INFINITY, 10.0)],
    );
    let r = solve_mip(p.problem(), &[true; 3], MipOptions::default()).unwrap();
    assert_eq!(r.status, Status::Optimal);
    assert!((r.objective + 10.0).abs() < 1e-9, "{} for {:?}", r.objective, r.col_value);
    assert!(r.col_value[0] < 0.5, "the greedy item should be left behind");
}

#[test]
fn a_mixed_problem_leaves_the_continuous_variables_alone() {
    //   minimise  x + 10y
    //   subject to  x + y >= 2.5,  y integer in [0, 5], x continuous in [0, 5]
    // x is cheap and continuous, so it takes the whole 2.5 and y stays at 0.
    let p = lp(
        &[vec![1.0], vec![1.0]],
        &[1.0, 10.0],
        &[(0.0, 5.0), (0.0, 5.0)],
        &[(2.5, f64::INFINITY)],
    );
    let r = solve_mip(p.problem(), &[false, true], MipOptions::default()).unwrap();
    assert_eq!(r.status, Status::Optimal);
    assert!((r.col_value[0] - 2.5).abs() < 1e-9, "{:?}", r.col_value);
    assert!(r.col_value[1].abs() < 1e-9, "{:?}", r.col_value);
    assert!((r.objective - 2.5).abs() < 1e-9);
}

#[test]
fn integrality_can_force_a_worse_answer_than_the_relaxation() {
    // The point of the bound. An integer optimum is never better than the
    // relaxation and is usually worse, and a search that reported otherwise
    // would be reporting an infeasible point.
    let p = lp(
        &[vec![3.0], vec![3.0]],
        &[-1.0, -1.0],
        &[(0.0, 2.0), (0.0, 2.0)],
        &[(f64::NEG_INFINITY, 7.0)],
    );
    let relaxed = solve(p.problem(), Default::default()).unwrap();
    let r = solve_mip(p.problem(), &[true, true], MipOptions::default()).unwrap();
    assert!(
        r.objective >= relaxed.objective - 1e-9,
        "the integer answer beat its own relaxation: {} against {}",
        r.objective,
        relaxed.objective
    );
    assert!(r.lower_bound <= r.objective + 1e-9, "the bound crossed the answer");
}

#[test]
fn an_infeasible_integer_problem_is_reported_as_such() {
    //   2x = 3 with x integer has no solution, though the relaxation gives 1.5.
    let p = lp(
        &[vec![2.0]],
        &[0.0],
        &[(0.0, 10.0)],
        &[(3.0, 3.0)],
    );
    let relaxed = solve(p.problem(), Default::default()).unwrap();
    assert_eq!(relaxed.status, Status::Optimal, "the relaxation is feasible");

    let r = solve_mip(p.problem(), &[true], MipOptions::default()).unwrap();
    assert_eq!(r.status, Status::Infeasible, "{:?}", r.col_value);
}

#[test]
fn an_infeasible_relaxation_is_passed_straight_through() {
    let p = lp(&[vec![1.0]], &[1.0], &[(0.0, 1.0)], &[(5.0, 5.0)]);
    let r = solve_mip(p.problem(), &[true], MipOptions::default()).unwrap();
    assert_eq!(r.status, Status::Infeasible);
    assert_eq!(r.nodes, 1, "nothing should be searched below an infeasible root");
}

#[test]
fn stopping_early_returns_the_best_found_and_says_it_is_not_proved() {
    // Anytime behaviour, which matters most where the budget is smallest.
    let p = lp(
        &[vec![7.0], vec![5.0], vec![4.0], vec![3.0]],
        &[-9.0, -6.0, -5.0, -4.0],
        &[(0.0, 1.0); 4],
        &[(f64::NEG_INFINITY, 11.0)],
    );
    let full = solve_mip(p.problem(), &[true; 4], MipOptions::default()).unwrap();
    assert!(full.proved);

    let starved = solve_mip(
        p.problem(),
        &[true; 4],
        MipOptions {
            max_nodes: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(starved.nodes <= 3);
    // Whatever it found is achievable, so it cannot be better than the truth.
    if starved.status == Status::Optimal {
        assert!(starved.objective >= full.objective - 1e-9);
    }
}

#[test]
fn the_bound_never_exceeds_the_answer() {
    // The invariant the whole search rests on. If the bound could pass the
    // incumbent, the pruning would be discarding solutions it should keep.
    for cap in [5.0, 7.0, 9.0, 11.0, 13.0] {
        let p = lp(
            &[vec![4.0], vec![3.0], vec![5.0]],
            &[-5.0, -4.0, -6.0],
            &[(0.0, 2.0); 3],
            &[(f64::NEG_INFINITY, cap)],
        );
        let r = solve_mip(p.problem(), &[true; 3], MipOptions::default()).unwrap();
        assert_eq!(r.status, Status::Optimal, "cap {cap}");
        assert!(
            r.lower_bound <= r.objective + 1e-6,
            "cap {cap}: bound {} above answer {}",
            r.lower_bound,
            r.objective
        );
        for v in &r.col_value {
            assert!((v - v.round()).abs() < 1e-6, "cap {cap}: {:?}", r.col_value);
        }
    }
}


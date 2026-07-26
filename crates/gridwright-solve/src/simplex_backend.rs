//! The pure-Rust backend, for WebAssembly and for builds without a C toolchain.
//!
//! Same [`Solver`] trait as HiGHS, so choosing between them is a feature flag
//! rather than a change of shape. The handoff is even simpler here than for
//! HiGHS: the simplex takes compressed sparse column arrays directly and uses
//! `u32` row indices, which is exactly what [`Model::to_csc`] produces, so the
//! `i32` widening the C API needs does not happen at all.
//!
//! What this backend cannot do is integer variables. Unit commitment therefore
//! needs HiGHS, and asking for it here is reported rather than silently solved
//! as a relaxation, because a commitment answer with fractional on/off states
//! is not an answer.

use gridwright_build::Lopf;
use gridwright_model::Sense;
use gridwright_simplex::{Options, Problem, Status as SxStatus, solve};

use crate::{SolveError, Solution, Solver, Status};

/// Solve with the built-in simplex.
#[derive(Debug, Clone)]
pub struct SimplexSolver {
    pub options: Options,
    /// Ceiling on branch-and-bound nodes for an integer model.
    ///
    /// Only consulted when the model has integers in it. Set low for an
    /// interactive setting, where an answer that arrives is worth more than one
    /// that is proved, and the status says which was obtained.
    pub max_nodes: usize,
}

impl Default for SimplexSolver {
    fn default() -> Self {
        Self {
            options: Options::default(),
            max_nodes: 5_000,
        }
    }
}

fn status_from(s: SxStatus) -> Status {
    match s {
        SxStatus::Optimal => Status::Optimal,
        SxStatus::Infeasible => Status::Infeasible,
        SxStatus::Unbounded => Status::Unbounded,
        SxStatus::IterationLimit => Status::Limit,
        SxStatus::NumericalFailure => Status::Other(-1),
    }
}

impl Solver for SimplexSolver {
    fn solve(&self, lopf: &Lopf) -> Result<Solution, SolveError> {
        let model = &lopf.model;
        let csc = model.matrix();
        let cols = model.columns();
        let (row_lower, row_upper) = model.row_bounds();

        // The simplex minimises. A maximisation is the same problem with the
        // objective negated, and the objective is negated back afterwards.
        let flip = matches!(model.sense, Sense::Maximize);
        let cost: Vec<f64> = if flip {
            cols.obj.iter().map(|c| -c).collect()
        } else {
            cols.obj.clone()
        };

        let problem = Problem {
            n_cols: model.num_cols(),
            n_rows: model.num_rows(),
            col_starts: &csc.starts,
            row_indices: &csc.rows,
            values: &csc.vals,
            col_lower: &cols.lower,
            col_upper: &cols.upper,
            col_cost: &cost,
            row_lower,
            row_upper,
        };

        let reject = |e| {
            SolveError::Rejected(match e {
                gridwright_simplex::SolveError::BadColumnStarts(_) => -2,
                gridwright_simplex::SolveError::BadBounds => -3,
                gridwright_simplex::SolveError::RowOutOfRange { .. } => -4,
                gridwright_simplex::SolveError::Basis(_) => -5,
            })
        };

        // An integer model goes through branch and bound over the same
        // relaxation. It used to be refused outright, which was honest and left
        // the browser build unable to run unit commitment at all, since a page
        // cannot call HiGHS.
        let s = if model.is_mip() {
            let r = gridwright_simplex::solve_mip(
                problem,
                &cols.integer,
                gridwright_simplex::MipOptions {
                    max_nodes: self.max_nodes,
                    lp: self.options,
                    ..Default::default()
                },
            )
            .map_err(reject)?;
            gridwright_simplex::Solution {
                // A search stopped on its node budget has found something
                // achievable but has not proved it optimal, and reporting
                // `Optimal` would be a claim the search did not make.
                status: if r.status == gridwright_simplex::Status::Optimal && !r.proved {
                    gridwright_simplex::Status::IterationLimit
                } else {
                    r.status
                },
                objective: r.objective,
                col_value: r.col_value,
                row_dual: r.row_dual,
                iterations: r.nodes,
                // Branch and bound counts nodes, not simplex phases; the
                // per-node split is not aggregated because the sum over nodes
                // would not mean what the field says.
                phase_one_iterations: 0,
            }
        } else {
            solve(problem, self.options).map_err(reject)?
        };

        Ok(Solution {
            status: status_from(s.status),
            objective: if flip { -s.objective } else { s.objective },
            col_value: s.col_value,
            row_dual: s.row_dual,
        })
    }
}

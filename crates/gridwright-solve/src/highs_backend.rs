//! HiGHS backend, bound through the C API.
//!
//! Everything unsafe in this project lives in [`HighsSolver::solve`], and it
//! is unsafe for one reason: the model is handed over as raw pointers so that
//! nothing is copied on the way in.

use std::ffi::CString;

use highs_sys as hs;
use gridwright_build::Lopf;

use crate::{SolveError, Solution, Solver, Status, sense_code};

/// Solve with HiGHS.
#[derive(Debug, Clone)]
pub struct HighsSolver {
    /// Emit the solver's own progress log.
    pub verbose: bool,
    /// Worker threads. Zero lets HiGHS decide.
    pub threads: i32,
    /// Wall clock limit in seconds. Non-finite means no limit.
    pub time_limit: f64,
}

impl Default for HighsSolver {
    fn default() -> Self {
        Self {
            verbose: false,
            threads: 0,
            time_limit: f64::INFINITY,
        }
    }
}

/// Owns the HiGHS instance so it is destroyed even if a later step fails.
struct Instance(*mut std::ffi::c_void);

impl Drop for Instance {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `Highs_create` and is destroyed once.
        unsafe { hs::Highs_destroy(self.0) };
    }
}

impl HighsSolver {
    fn apply_options(&self, h: *mut std::ffi::c_void) {
        let set_bool = |name: &str, value: bool| {
            if let Ok(c) = CString::new(name) {
                // SAFETY: `c` is a valid NUL terminated string for this call.
                unsafe { hs::Highs_setBoolOptionValue(h, c.as_ptr(), i32::from(value)) };
            }
        };
        let set_int = |name: &str, value: i32| {
            if let Ok(c) = CString::new(name) {
                // SAFETY: as above.
                unsafe { hs::Highs_setIntOptionValue(h, c.as_ptr(), value) };
            }
        };
        let set_double = |name: &str, value: f64| {
            if let Ok(c) = CString::new(name) {
                // SAFETY: as above.
                unsafe { hs::Highs_setDoubleOptionValue(h, c.as_ptr(), value) };
            }
        };

        set_bool("output_flag", self.verbose);
        if self.threads > 0 {
            set_int("threads", self.threads);
        }
        if self.time_limit.is_finite() {
            set_double("time_limit", self.time_limit);
        }
    }
}

/// Map a HiGHS model status onto ours.
///
/// The numeric constants are stable parts of the C API, but they are matched
/// by name here so a version bump that renumbers them is a compile error
/// rather than a silently wrong status.
fn status_from(code: i32) -> Status {
    match code {
        c if c == hs::kHighsModelStatusOptimal => Status::Optimal,
        c if c == hs::kHighsModelStatusInfeasible => Status::Infeasible,
        c if c == hs::kHighsModelStatusUnbounded => Status::Unbounded,
        c if c == hs::kHighsModelStatusUnboundedOrInfeasible => Status::Infeasible,
        c if c == hs::kHighsModelStatusTimeLimit || c == hs::kHighsModelStatusIterationLimit => {
            Status::Limit
        }
        other => Status::Other(other),
    }
}

impl Solver for HighsSolver {
    fn solve(&self, lopf: &Lopf) -> Result<Solution, SolveError> {
        let model = &lopf.model;
        let n_col = model.num_cols();
        let n_row = model.num_rows();

        if n_col > i32::MAX as usize {
            return Err(SolveError::TooManyColumns(n_col));
        }
        if n_row > i32::MAX as usize {
            return Err(SolveError::TooManyRows(n_row));
        }

        let csc = model.to_csc();
        let nnz = csc.nnz();
        if nnz > i32::MAX as usize {
            return Err(SolveError::TooManyNonzeros(nnz));
        }

        // The one unavoidable copy. HiGHS indexes with i32 and the model uses
        // u32, so the index arrays are widened here. Values and bounds are
        // already f64 and pass through untouched.
        let a_start: Vec<i32> = csc.starts.iter().map(|&v| v as i32).collect();
        let a_index: Vec<i32> = csc.rows.iter().map(|&v| v as i32).collect();

        let cols = model.columns();
        let (row_lower, row_upper) = model.row_bounds();

        // SAFETY: `Highs_create` returns an owned instance, wrapped immediately
        // so it is destroyed on every exit path.
        let inst = Instance(unsafe { hs::Highs_create() });
        if inst.0.is_null() {
            return Err(SolveError::Rejected(-1));
        }
        self.apply_options(inst.0);

        // SAFETY: every pointer below refers to a live local buffer whose
        // length matches the count passed alongside it:
        //   col_cost/col_lower/col_upper  -> n_col
        //   row_lower/row_upper           -> n_row
        //   a_start                       -> n_col + 1
        //   a_index/a_value               -> nnz
        // The matrix is column wise, which is what `to_csc` produced. There is
        // no Hessian, so the quadratic arguments are null and `q_num_nz` is 0,
        // and there is no integrality array because this is a pure LP. HiGHS
        // copies what it needs during this call, so the buffers only have to
        // outlive the call itself, which they do.
        let pass = unsafe {
            hs::Highs_passModel(
                inst.0,
                n_col as i32,
                n_row as i32,
                nnz as i32,
                0,
                hs::kHighsMatrixFormatColwise,
                0,
                sense_code(model.sense),
                0.0,
                cols.obj.as_ptr(),
                cols.lower.as_ptr(),
                cols.upper.as_ptr(),
                row_lower.as_ptr(),
                row_upper.as_ptr(),
                a_start.as_ptr(),
                a_index.as_ptr(),
                csc.vals.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if pass != hs::kHighsStatusOk {
            return Err(SolveError::Rejected(pass));
        }

        // SAFETY: the instance holds a model at this point.
        let run = unsafe { hs::Highs_run(inst.0) };
        if run != hs::kHighsStatusOk {
            return Err(SolveError::RunFailed(run));
        }

        // SAFETY: reads a scalar from a live instance.
        let status = status_from(unsafe { hs::Highs_getModelStatus(inst.0) });
        // SAFETY: as above.
        let objective = unsafe { hs::Highs_getObjectiveValue(inst.0) };

        let mut col_value = vec![0.0f64; n_col];
        let mut col_dual = vec![0.0f64; n_col];
        let mut row_value = vec![0.0f64; n_row];
        let mut row_dual = vec![0.0f64; n_row];

        // A model stopped at a limit, or proven infeasible, has no solution
        // vectors to retrieve, and asking for them is not meaningful.
        if matches!(status, Status::Optimal | Status::Limit) {
            // SAFETY: all four destinations are sized exactly as HiGHS expects,
            // n_col for the column arrays and n_row for the row arrays.
            unsafe {
                hs::Highs_getSolution(
                    inst.0,
                    col_value.as_mut_ptr(),
                    col_dual.as_mut_ptr(),
                    row_value.as_mut_ptr(),
                    row_dual.as_mut_ptr(),
                );
            }
        }

        Ok(Solution {
            status,
            objective,
            col_value,
            row_dual,
        })
    }
}

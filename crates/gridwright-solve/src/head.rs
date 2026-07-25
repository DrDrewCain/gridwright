//! Head-dependent hydro at a scale the exact formulation cannot reach.
//!
//! Head affects hydropower twice, and the second one is the awkward one:
//!
//! - **Capacity** is linear in the stored level and costs nothing to model.
//! - **Conversion** is bilinear, because the volume drawn per megawatt-hour
//!   goes as `1/head` and head depends on the level.
//!
//! [`gridwright_build`] models the second exactly, over bands of reservoir
//! level with a binary picking the band, following Borghetti, D'Ambrosio, Lodi
//! and Martello (2008). That formulation is correct and it does not scale: four
//! bands across a year at hourly resolution is 35,040 binaries for one
//! reservoir, and the hydro scheduling literature is candid that this is where
//! such models stop finishing.
//!
//! # The fixed point
//!
//! The alternative, which recent work leans on for exactly this reason, is to
//! stop treating head as a decision. Hold it fixed at a guess and the problem
//! is an ordinary linear program. Solve it, read the reservoir levels the
//! solution produced, recompute head from those levels, and solve again. Repeat
//! until the levels stop moving.
//!
//! What that buys and what it costs are both worth stating plainly. It buys a
//! model that finishes on a continental system, with no binaries at all and no
//! change to the solver. It costs the guarantee: this is a fixed point, not an
//! optimum, and it comes with no bound on how far from one it might be. When
//! both are affordable, the exact formulation is the one to trust, and this is
//! here for the cases where it will not return.
//!
//! Under-relaxation is applied between iterations, which is what stops the
//! level and the head chasing each other: a fuller reservoir converts better,
//! which encourages drawing on it, which empties it, which converts worse.
//! Taking a partial step damps that oscillation, and it is the same device
//! Borghetti uses.

use gridwright_build::{Lopf, build_lopf};
use gridwright_net::{Network, TimeSeries};

use crate::{SolveError, Solution, Solver, Status};

/// How to run the iteration.
#[derive(Debug, Clone, Copy)]
pub struct HeadOptions {
    /// Ceiling on iterations.
    ///
    /// Under-relaxation makes convergence geometric with ratio `1 - relaxation`,
    /// so at the default damping the residual halves each round and reaching a
    /// tolerance of `1e-4` from a standing start takes about a dozen. The
    /// ceiling is set well above that, since each iteration is one linear
    /// program and stopping early on a case that was nearly settled is the
    /// expensive mistake.
    pub max_iterations: usize,
    /// Largest change in any head, as a fraction, at which to stop.
    ///
    /// Measured on the step actually taken, which under-relaxation makes
    /// smaller than the distance remaining, so this is the conservative of the
    /// two readings.
    pub tolerance: f64,
    /// How far to step toward the newly computed head each round.
    ///
    /// One takes the new value outright and can oscillate. Values around a half
    /// damp that, at the cost of more iterations.
    pub relaxation: f64,
}

impl Default for HeadOptions {
    fn default() -> Self {
        Self {
            max_iterations: 40,
            tolerance: 1e-4,
            relaxation: 0.5,
        }
    }
}

/// The result of the iteration, with its own status attached.
#[derive(Debug, Clone)]
pub struct HeadSolution {
    pub solution: Solution,
    /// The heads the fixed point settled on, per storage unit and snapshot.
    pub head: TimeSeries,
    /// Iterations actually run.
    pub iterations: usize,
    /// Whether the heads stopped moving within tolerance.
    ///
    /// False means the answer is whatever the last iteration produced, which
    /// is a feasible dispatch under *some* head assumption but not a converged
    /// one. Reported rather than folded into the status, because it is a
    /// different kind of doubt from an infeasible model.
    pub converged: bool,
    /// Largest fractional change in head on the final iteration.
    pub residual: f64,
    /// The model as finally built, so a caller can read variables off the
    /// solution.
    pub lopf: Lopf,
}

/// Head at a given stored level, as a fraction of full head.
///
/// Linear between the empty-reservoir figure and one. A real head-to-volume
/// curve is not linear, since it depends on the shape of the valley, but the
/// data to do better is rarely published and a linear interpolation over the
/// working range is the standard approximation.
fn head_at(unit: &gridwright_net::StorageUnit, level: f64) -> f64 {
    let e_max = unit.p_nom * unit.max_hours;
    if e_max <= 0.0 {
        return 1.0;
    }
    let fill = (level / e_max).clamp(0.0, 1.0);
    (unit.head_min_pu + (1.0 - unit.head_min_pu) * fill).max(1e-6)
}

/// Whether a unit's head varies enough to be worth iterating over.
fn varies(unit: &gridwright_net::StorageUnit) -> bool {
    unit.head_min_pu < 1.0 && unit.p_nom > 0.0 && unit.max_hours > 0.0
}

/// Solve with head-dependent conversion, by iterating rather than branching.
///
/// Returns the converged dispatch, or the last iteration's if it did not
/// converge, with [`HeadSolution::converged`] saying which.
pub fn solve_head_iterated<S: Solver>(
    net: &Network,
    solver: &S,
    opts: HeadOptions,
) -> Result<HeadSolution, SolveError> {
    let t = net.n_snapshots();
    let n = net.storage.len();

    // Start from full head everywhere, which is what a model ignoring the
    // effect assumes, so the first iteration is the answer that was being
    // given before and every one after is an improvement on it.
    let mut head: Vec<f64> = vec![1.0; n * t];

    let mut working = net.clone();
    // The bands are the other treatment of the same physics. Running both at
    // once would count it twice.
    for unit in &mut working.storage {
        unit.head_bands = 0;
    }

    let mut iterations = 0;
    let mut residual = f64::INFINITY;
    let mut converged = false;
    let mut last: Option<(Solution, Lopf)> = None;

    for _ in 0..opts.max_iterations.max(1) {
        working.head_profile = TimeSeries::from_flat(head.clone(), n, t)
            .map_err(|e| SolveError::Build(e.to_string()))?;
        let lopf = build_lopf(&working).map_err(|e| SolveError::Build(e.to_string()))?;
        let solution = solver.solve(&lopf)?;
        iterations += 1;

        if solution.status != Status::Optimal {
            // No point iterating on a model that did not solve; the caller
            // needs to see the status rather than a fixed point over nonsense.
            return Ok(HeadSolution {
                head: TimeSeries::from_flat(head, n, t)
                    .map_err(|e| SolveError::Build(e.to_string()))?,
                solution,
                iterations,
                converged: false,
                residual,
                lopf,
            });
        }

        // Recompute head from the levels this solve produced, using the level
        // at the *start* of each period: water leaves at the head it had on
        // the way out.
        let mut next = head.clone();
        let mut worst: f64 = 0.0;
        for (s, unit) in working.storage.iter().enumerate() {
            if !varies(unit) {
                continue;
            }
            let soc = solution.trajectory(lopf.vars.soc[s]);
            for step in 0..t {
                let level = if step == 0 {
                    match (unit.soc_initial, unit.cyclic) {
                        (Some(e0), _) => e0,
                        (None, true) => soc[t - 1],
                        (None, false) => 0.0,
                    }
                } else {
                    soc[step - 1]
                };
                let target = head_at(unit, level);
                let i = s * t + step;
                let stepped = head[i] + opts.relaxation * (target - head[i]);
                worst = worst.max((stepped - head[i]).abs() / head[i].max(1e-9));
                next[i] = stepped;
            }
        }

        head = next;
        residual = worst;
        last = Some((solution, lopf));
        if worst <= opts.tolerance {
            converged = true;
            break;
        }
    }

    let (solution, lopf) = last.expect("at least one iteration runs");
    Ok(HeadSolution {
        solution,
        head: TimeSeries::from_flat(head, n, t)
            .map_err(|e| SolveError::Build(e.to_string()))?,
        iterations,
        converged,
        residual,
        lopf,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gridwright_net::StorageUnit;

    fn unit() -> StorageUnit {
        StorageUnit {
            p_nom: 100.0,
            max_hours: 10.0,
            head_min_pu: 0.6,
            ..Default::default()
        }
    }

    #[test]
    fn head_runs_from_the_floor_to_one_across_the_reservoir() {
        let u = unit();
        assert!((head_at(&u, 0.0) - 0.6).abs() < 1e-12);
        assert!((head_at(&u, 1000.0) - 1.0).abs() < 1e-12);
        assert!((head_at(&u, 500.0) - 0.8).abs() < 1e-12);
    }

    #[test]
    fn a_level_outside_the_reservoir_is_clamped_rather_than_extrapolated() {
        // Solver tolerance puts levels a hair outside their bounds routinely,
        // and extrapolating would give a head above one, which is free energy.
        let u = unit();
        assert!((head_at(&u, -5.0) - 0.6).abs() < 1e-12);
        assert!((head_at(&u, 1e9) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_unit_with_no_head_variation_sits_at_full_head() {
        let mut u = unit();
        u.head_min_pu = 1.0;
        assert!(!varies(&u));
        assert!((head_at(&u, 0.0) - 1.0).abs() < 1e-12);
    }
}

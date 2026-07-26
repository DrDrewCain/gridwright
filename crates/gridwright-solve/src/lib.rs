//! Solver backends.
//!
//! The handoff is the point of this crate. HiGHS exposes `Highs_passModel`,
//! which takes the constraint matrix as compressed sparse column arrays in one
//! call, and its own documentation notes this is faster than building the
//! model through `Highs_addRow`. That is exactly the shape `Model::to_csc`
//! already produces, so the matrix crosses the boundary as three pointers.
//!
//! The safe `highs` wrapper crate was deliberately not used. Its builder takes
//! rows one at a time through its own types, which would mean disassembling
//! the matrix we just assembled and rebuilding it inside someone else's
//! representation, discarding the entire reason this engine exists. Binding
//! the C API directly costs one `unsafe` block, confined to a single function,
//! and keeps the fast path intact.
//!
//! One copy does remain: HiGHS indexes with `HighsInt`, which is `i32`, while
//! the model uses `u32`. Converting is a linear pass over the nonzeros. That
//! is a real cost and it is measured rather than waved away, but it is a
//! single sequential scan against a solve that is superlinear in the same
//! data.

use gridwright_build::{Lopf, VarIndex};
use gridwright_model::Sense;

pub mod head;
pub mod rolling;

#[cfg(feature = "highs")]
mod highs_backend;

#[cfg(feature = "simplex")]
mod simplex_backend;

#[cfg(feature = "highs")]
pub use highs_backend::HighsSolver;

#[cfg(feature = "simplex")]
pub use simplex_backend::SimplexSolver;

/// How a solve ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Optimal,
    Infeasible,
    Unbounded,
    /// Stopped on a limit, so any solution present is not proven optimal.
    Limit,
    Other(i32),
}

impl Status {
    #[inline]
    pub fn is_optimal(self) -> bool {
        matches!(self, Status::Optimal)
    }
}

/// A solved model.
#[derive(Debug, Clone)]
pub struct Solution {
    pub status: Status,
    pub objective: f64,
    /// Primal value of every column, indexed as the model laid them out.
    pub col_value: Vec<f64>,
    /// Dual value of every row.
    ///
    /// For a nodal balance row this is the marginal cost of energy at that bus
    /// in that snapshot, which is the electricity price. It is the single most
    /// useful number an energy model produces and it falls out of the solve
    /// for free, so it is always retrieved.
    pub row_dual: Vec<f64>,
}

impl Solution {
    /// The trajectory of one component across every snapshot.
    ///
    /// Cheap because of how variables were allocated: one block per component
    /// spanning all snapshots means a trajectory is a contiguous slice rather
    /// than a gather.
    #[inline]
    pub fn trajectory(&self, block: gridwright_model::VarBlock) -> &[f64] {
        let s = block.start() as usize;
        &self.col_value[s..s + block.len() as usize]
    }

    /// Dispatch of generator `g` over time.
    pub fn dispatch(&self, vars: &VarIndex, g: usize) -> &[f64] {
        self.trajectory(vars.dispatch[g])
    }

    /// Signed flow on line `l`, positive from `bus0` toward `bus1`.
    pub fn flow(&self, vars: &VarIndex, l: usize) -> &[f64] {
        self.trajectory(vars.flow[l])
    }

    /// Unserved energy at bus `b`. Non-zero anywhere means the system could
    /// not physically meet demand there.
    pub fn shed(&self, vars: &VarIndex, b: usize) -> &[f64] {
        self.trajectory(vars.shed[b])
    }

    /// Marginal price at bus `b` per snapshot.
    ///
    /// Balance rows are emitted first and in bus order, so bus `b` at snapshot
    /// `t` is row `b * n_snapshots + t`. That contract is what the row
    /// ordering tests in `gridwright-build` protect, because a silent change to it
    /// would turn this into plausible nonsense rather than an error.
    pub fn price(&self, b: usize, n_snapshots: usize) -> &[f64] {
        let s = b * n_snapshots;
        &self.row_dual[s..s + n_snapshots]
    }

    /// Capacity added by the optimiser, summed over every investment period.
    ///
    /// This is *additional* capacity, not total. The variable is the decision,
    /// and the existing fleet is a constant the decision is added to, which is
    /// why a unit that already has 60 MW and should not grow reports zero here
    /// rather than sixty. Use [`Solution::total_capacity`] for the installed
    /// figure.
    pub fn capacity_built(&self, block: gridwright_model::VarBlock) -> f64 {
        self.trajectory(block).iter().sum()
    }

    /// Existing capacity plus everything built, which is what gets reported to
    /// a human and what the next model run would start from.
    pub fn total_capacity(
        &self,
        block: Option<gridwright_model::VarBlock>,
        existing: f64,
    ) -> f64 {
        existing + block.map_or(0.0, |b| self.capacity_built(b))
    }

    /// Capacity built in one specific period, for reading an investment path.
    pub fn capacity_built_in(&self, block: gridwright_model::VarBlock, period: usize) -> f64 {
        self.trajectory(block)[period]
    }

    /// Everything the emissions accounting needs, read out of the solution.
    ///
    /// The accounting crate takes plain numbers on purpose, so that it depends
    /// on no solver and can be fed results from anywhere. That leaves a gap
    /// between "solved" and "accounted" which every caller would otherwise
    /// close by hand, against variable-layout details they should not have to
    /// know. This closes it once.
    pub fn emissions_input(
        &self,
        net: &gridwright_net::Network,
        lopf: &gridwright_build::Lopf,
    ) -> gridwright_emissions::Flows {
        let vars = &lopf.vars;
        gridwright_emissions::Flows {
            dispatch: (0..net.generators.len())
                .map(|g| self.dispatch(vars, g).to_vec())
                .collect(),
            flows: (0..net.lines.len())
                .map(|l| self.flow(vars, l).to_vec())
                .collect(),
            shed: (0..net.buses.len())
                .map(|b| self.shed(vars, b).to_vec())
                .collect(),
            built: vars
                .gen_capacity
                .iter()
                .map(|block| block.map_or(0.0, |b| self.capacity_built(b)))
                .collect(),
            losses: vars
                .line_loss
                .iter()
                .map(|block| {
                    block.map_or_else(Vec::new, |b| self.trajectory(b).to_vec())
                })
                .collect(),
        }
    }

    /// Total unserved energy across the whole system.
    pub fn total_shed(&self, vars: &VarIndex) -> f64 {
        vars.shed
            .iter()
            .map(|&b| self.trajectory(b).iter().sum::<f64>())
            .sum()
    }
}

/// A backend that can solve a built model.
pub trait Solver {
    fn solve(&self, lopf: &Lopf) -> Result<Solution, SolveError>;
}

#[derive(Debug, thiserror::Error)]
pub enum SolveError {
    #[error("model has {0} nonzeros, which exceeds the solver's i32 index space")]
    TooManyNonzeros(usize),
    #[error("model has {0} columns, which exceeds the solver's i32 index space")]
    TooManyColumns(usize),
    #[error("model has {0} rows, which exceeds the solver's i32 index space")]
    TooManyRows(usize),
    #[error("could not build the model: {0}")]
    Build(String),
    #[error("solver rejected the model (status {0})")]
    Rejected(i32),
    #[error("solver failed to run (status {0})")]
    RunFailed(i32),
    #[error(
        "this backend cannot solve integer variables, and the model has {0}; \
         unit commitment needs the HiGHS backend"
    )]
    IntegerNotSupported(usize),
}

/// Objective sense as the solver's constant.
#[inline]
pub(crate) fn sense_code(sense: Sense) -> i32 {
    match sense {
        Sense::Minimize => 1,
        Sense::Maximize => -1,
    }
}

#[cfg(all(test, feature = "highs"))]
mod tests {
    use super::*;
    use gridwright_build::build_lopf;
    use gridwright_net::{Generator, Line, Load, Network, Snapshots};

    /// Two countries, one interconnector. France has cheap nuclear, Germany
    /// has expensive coal, and the load sits in Germany.
    ///
    /// The right answer here is arithmetic rather than opinion: import as much
    /// as the line allows, then cover the rest locally. That makes it a real
    /// correctness check on the whole pipeline instead of a smoke test.
    fn two_country(t: usize, load: f64, link: f64) -> Network {
        let mut n = Network::new(Snapshots::hourly(t));
        let de = n.add_bus("DE", "DE");
        let fr = n.add_bus("FR", "FR");
        n.add_generator(Generator {
            name: "de_coal".into(),
            bus: de,
            p_nom: 100.0,
            marginal_cost: 40.0,
            p_min_pu: 0.0,
            ..Default::default()
        });
        n.add_generator(Generator {
            name: "fr_nuclear".into(),
            bus: fr,
            p_nom: 200.0,
            marginal_cost: 10.0,
            p_min_pu: 0.0,
            ..Default::default()
        });
        n.add_line(Line {
            name: "DE-FR".into(),
            bus0: de,
            bus1: fr,
            s_nom: link,
            susceptance: 0.0,
            ..Default::default()
        });
        n.add_load(Load {
            name: "de_load".into(),
            bus: de,
            p_set: load,
            ..Default::default()
        });
        n
    }

    #[test]
    fn cheap_imports_displace_expensive_local_generation() {
        let net = two_country(4, 80.0, 50.0);
        let lopf = build_lopf(&net).unwrap();
        let sol = HighsSolver::default().solve(&lopf).unwrap();

        assert_eq!(sol.status, Status::Optimal);
        // 50 MW imported at 10, 30 MW local at 40, over 4 hours.
        let expected = 4.0 * (50.0 * 10.0 + 30.0 * 40.0);
        assert!(
            (sol.objective - expected).abs() < 1e-6,
            "objective {} != {expected}",
            sol.objective
        );

        let de_coal = sol.dispatch(&lopf.vars, 0);
        let fr_nuke = sol.dispatch(&lopf.vars, 1);
        for h in 0..4 {
            assert!((de_coal[h] - 30.0).abs() < 1e-6, "hour {h}: {}", de_coal[h]);
            assert!((fr_nuke[h] - 50.0).abs() < 1e-6, "hour {h}: {}", fr_nuke[h]);
        }
        assert!(sol.total_shed(&lopf.vars) < 1e-9, "nothing should be shed");
    }

    #[test]
    fn flow_runs_from_the_cheap_country_to_the_expensive_one() {
        let net = two_country(2, 80.0, 50.0);
        let lopf = build_lopf(&net).unwrap();
        let sol = HighsSolver::default().solve(&lopf).unwrap();
        // The line is oriented DE -> FR, so power arriving in DE is negative.
        for &f in sol.flow(&lopf.vars, 0) {
            assert!((f + 50.0).abs() < 1e-6, "expected -50, got {f}");
        }
    }

    #[test]
    fn a_fatter_interconnector_lets_more_cheap_power_through() {
        let solve = |link: f64| {
            let net = two_country(1, 80.0, link);
            let l = build_lopf(&net).unwrap();
            HighsSolver::default().solve(&l).unwrap().objective
        };
        let tight = solve(20.0);
        let loose = solve(80.0);
        // With 80 MW of link the whole load is served from France at 10/MWh.
        assert!((loose - 800.0).abs() < 1e-6, "loose = {loose}");
        assert!(loose < tight, "more transmission must not cost more");
    }

    #[test]
    fn prices_separate_when_the_interconnector_saturates() {
        let net = two_country(1, 80.0, 50.0);
        let lopf = build_lopf(&net).unwrap();
        let sol = HighsSolver::default().solve(&lopf).unwrap();
        let de = sol.price(0, 1)[0].abs();
        let fr = sol.price(1, 1)[0].abs();
        // Congestion is exactly the condition under which one market splits
        // into two prices, each set by the marginal unit on its own side.
        assert!((de - 40.0).abs() < 1e-6, "DE price {de}, expected 40");
        assert!((fr - 10.0).abs() < 1e-6, "FR price {fr}, expected 10");
    }

    #[test]
    fn unmeetable_demand_sheds_load_rather_than_reporting_infeasible() {
        // 400 MW of demand against 100 MW local and a 50 MW link.
        let net = two_country(1, 400.0, 50.0);
        let lopf = build_lopf(&net).unwrap();
        let sol = HighsSolver::default().solve(&lopf).unwrap();
        assert_eq!(sol.status, Status::Optimal, "should solve, not fail");
        let shed = sol.shed(&lopf.vars, 0)[0];
        assert!(
            (shed - 250.0).abs() < 1e-6,
            "expected 250 MW unserved, got {shed}"
        );
    }

    #[test]
    fn storage_shifts_energy_across_an_outage() {
        use gridwright_net::{StorageUnit, TimeSeries};
        // One bus, one generator whose availability vanishes in hour 1, and a
        // battery. The only way to serve hour 1 is to have charged in hour 0.
        let mut net = Network::new(Snapshots::hourly(2));
        let b = net.add_bus("B", "XX");
        net.add_generator(Generator {
            name: "g".into(),
            bus: b,
            p_nom: 100.0,
            marginal_cost: 5.0,
            p_min_pu: 0.0,
            ..Default::default()
        });
        net.add_load(Load {
            name: "l".into(),
            bus: b,
            p_set: 20.0,
            ..Default::default()
        });
        net.add_storage(StorageUnit {
            name: "batt".into(),
            bus: b,
            p_nom: 50.0,
            max_hours: 4.0,
            efficiency_store: 1.0,
            efficiency_dispatch: 1.0,
            cyclic: false,
            ..Default::default()
        });
        net.gen_availability = TimeSeries::from_rows(&[vec![1.0, 0.0]], 2).unwrap();

        let lopf = build_lopf(&net).unwrap();
        let sol = HighsSolver::default().solve(&lopf).unwrap();
        assert_eq!(sol.status, Status::Optimal);
        assert!(
            sol.total_shed(&lopf.vars) < 1e-6,
            "storage should have covered the outage, shed {}",
            sol.total_shed(&lopf.vars)
        );
        let discharge = sol.trajectory(lopf.vars.discharge[0]);
        assert!(
            (discharge[1] - 20.0).abs() < 1e-6,
            "expected 20 MW discharged in hour 1, got {}",
            discharge[1]
        );
    }

    #[test]
    fn dc_flow_splits_power_between_parallel_paths() {
        // A triangle with equal susceptance. Under DC flow, power injected at
        // one corner and withdrawn at another divides between the direct path
        // and the two-hop path in a 2:1 ratio. Transport lines would instead
        // send everything down the direct path, so this distinguishes the two
        // formulations rather than merely exercising the code.
        let mut net = Network::new(Snapshots::hourly(1));
        let a = net.add_bus("A", "AA");
        let b = net.add_bus("B", "BB");
        let c = net.add_bus("C", "CC");
        net.add_generator(Generator {
            name: "src".into(),
            bus: a,
            p_nom: 100.0,
            marginal_cost: 1.0,
            p_min_pu: 0.0,
            ..Default::default()
        });
        net.add_load(Load {
            name: "sink".into(),
            bus: b,
            p_set: 30.0,
            ..Default::default()
        });
        for (n0, n1) in [(a, b), (b, c), (c, a)] {
            net.add_line(Line {
                name: format!("{n0}-{n1}"),
                bus0: n0,
                bus1: n1,
                s_nom: 1000.0,
                susceptance: 1.0,
                ..Default::default()
            });
        }
        let lopf = build_lopf(&net).unwrap();
        let sol = HighsSolver::default().solve(&lopf).unwrap();
        assert_eq!(sol.status, Status::Optimal);
        let direct = sol.flow(&lopf.vars, 0)[0];
        assert!(
            (direct - 20.0).abs() < 1e-6,
            "expected 2/3 of 30 MW on the direct path, got {direct}"
        );
    }
}

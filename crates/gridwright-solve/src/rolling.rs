//! Rolling horizon: solving a long period as overlapping windows.
//!
//! A year of hourly unit commitment is a mixed integer problem with tens of
//! thousands of binaries, and nobody solves it whole. What operators and
//! planners actually do is solve a few days at a time, keep the first part of
//! each answer, and roll forward carrying the system state across.
//!
//! # Why the windows overlap
//!
//! A window solved in isolation knows nothing about what follows it, so it
//! behaves like the world ends at its final snapshot: reservoirs are emptied,
//! plant is shut down, and nothing is held back. Solving further than is kept
//! gives the optimiser a view past the decisions being committed, and the
//! discarded tail absorbs the end effect.
//!
//! The overlap is therefore a real cost paid for a real reason. A 48 hour
//! window keeping 24 hours solves every snapshot twice.
//!
//! # What carries across
//!
//! Reservoir levels and commitment states. Both are set explicitly on the next
//! window rather than left to the cyclic defaults, because a window that
//! returns its storage to where it started, or that assumes every unit begins
//! cold, is answering a different question than the one asked.

use gridwright_build::{BuildError, build_lopf};
use gridwright_net::{Network, TimeSeries};

use crate::{SolveError, Solver, Status};

/// How to cut the horizon into windows.
#[derive(Debug, Clone, Copy)]
pub struct Horizon {
    /// Snapshots solved at once.
    pub window: usize,
    /// Snapshots kept from each window before rolling forward. Must not exceed
    /// `window`; the difference is the lookahead that absorbs end effects.
    pub keep: usize,
}

impl Horizon {
    /// A window of `window` snapshots keeping half of it, which is the usual
    /// compromise between end effects and duplicated work.
    pub fn new(window: usize) -> Self {
        Self {
            window,
            keep: window / 2,
        }
    }
}

/// The stitched answer across every window.
#[derive(Debug, Clone)]
pub struct RollingSolution {
    /// Total cost over the kept snapshots only. Cost incurred in a lookahead
    /// tail is not counted, because those snapshots are solved again in the
    /// next window and counting both would double them.
    pub objective: f64,
    /// Generator dispatch, `[generator][snapshot]`.
    pub dispatch: Vec<Vec<f64>>,
    /// Signed line flows, `[line][snapshot]`.
    pub flows: Vec<Vec<f64>>,
    /// Marginal price at each bus, `[bus][snapshot]`.
    pub prices: Vec<Vec<f64>>,
    /// Unserved energy at each bus, `[bus][snapshot]`.
    pub shed: Vec<Vec<f64>>,
    /// Reservoir levels, `[storage][snapshot]`.
    pub soc: Vec<Vec<f64>>,
    /// How each window ended, in order. Anything other than `Optimal` means the
    /// state carried into the next window came from a solve that did not
    /// finish, which is worth knowing rather than averaging away.
    pub statuses: Vec<Status>,
    pub windows: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum RollingError {
    #[error("window must be at least 1 snapshot")]
    EmptyWindow,
    #[error("keep ({keep}) cannot exceed window ({window})")]
    KeepTooLarge { keep: usize, window: usize },
    #[error(transparent)]
    Build(#[from] BuildError),
    #[error(transparent)]
    Solve(#[from] SolveError),
    #[error("network is not valid: {0}")]
    Network(#[from] gridwright_net::NetError),
}

/// Slice a network down to a snapshot range, carrying state in.
fn window_of(
    net: &Network,
    start: usize,
    len: usize,
    soc: &[f64],
    on: &[bool],
    first: bool,
) -> Result<Network, RollingError> {
    let mut w = net.clone();
    let weights: Vec<f64> = net.snapshots.weights()[start..start + len].to_vec();
    w.snapshots = gridwright_net::Snapshots::weighted(weights)?;

    // Time series are component major, so a window is a strided slice of each
    // component's run rather than a contiguous block of the whole array.
    let slice = |ts: &TimeSeries, n: usize| -> Result<TimeSeries, RollingError> {
        if ts.is_empty() {
            return Ok(TimeSeries::empty());
        }
        let mut rows = Vec::with_capacity(n);
        for c in 0..n {
            let row = ts.row(c).unwrap_or(&[]);
            rows.push(row[start..start + len].to_vec());
        }
        Ok(TimeSeries::from_rows(&rows, len)?)
    };
    w.gen_availability = slice(&net.gen_availability, net.generators.len())?;
    w.load_profile = slice(&net.load_profile, net.loads.len())?;
    w.storage_inflow = slice(&net.storage_inflow, net.storage.len())?;

    // Windows are not cyclic: they inherit a state and hand one on. Only the
    // very first window may legitimately use the network's own starting rules.
    if !first {
        for (s, unit) in w.storage.iter_mut().enumerate() {
            unit.cyclic = false;
            unit.soc_initial = Some(soc[s]);
        }
        for (g, unit) in w.generators.iter_mut().enumerate() {
            if unit.committable {
                unit.initially_on = Some(on[g]);
            }
        }
    }

    // Investment periods and scenarios describe the whole horizon and do not
    // survive being cut up; a rolling solve is an operational question.
    w.investment_periods.clear();
    w.scenarios.clear();
    Ok(w)
}

/// Solve `net` as a sequence of overlapping windows.
pub fn solve_rolling<S: Solver>(
    net: &Network,
    horizon: Horizon,
    solver: &S,
) -> Result<RollingSolution, RollingError> {
    if horizon.window == 0 {
        return Err(RollingError::EmptyWindow);
    }
    if horizon.keep == 0 || horizon.keep > horizon.window {
        return Err(RollingError::KeepTooLarge {
            keep: horizon.keep,
            window: horizon.window,
        });
    }
    net.validate()?;

    let total = net.n_snapshots();
    let mut out = RollingSolution {
        objective: 0.0,
        dispatch: vec![vec![0.0; total]; net.generators.len()],
        flows: vec![vec![0.0; total]; net.lines.len()],
        prices: vec![vec![0.0; total]; net.buses.len()],
        shed: vec![vec![0.0; total]; net.buses.len()],
        soc: vec![vec![0.0; total]; net.storage.len()],
        statuses: Vec::new(),
        windows: 0,
    };

    let mut soc: Vec<f64> = net.storage.iter().map(|_| 0.0).collect();
    let mut on: Vec<bool> = net.generators.iter().map(|_| false).collect();
    let mut start = 0usize;
    let mut first = true;

    while start < total {
        let len = horizon.window.min(total - start);
        let keep = horizon.keep.min(len);
        let w = window_of(net, start, len, &soc, &on, first)?;

        let lopf = build_lopf(&w)?;
        let sol = solver.solve(&lopf)?;
        out.statuses.push(sol.status);
        out.windows += 1;

        // Copy the kept prefix into the stitched answer.
        for g in 0..net.generators.len() {
            let d = sol.dispatch(&lopf.vars, g);
            out.dispatch[g][start..start + keep].copy_from_slice(&d[..keep]);
        }
        for l in 0..net.lines.len() {
            let f = sol.flow(&lopf.vars, l);
            out.flows[l][start..start + keep].copy_from_slice(&f[..keep]);
        }
        for b in 0..net.buses.len() {
            let p = sol.price(b, len);
            out.prices[b][start..start + keep].copy_from_slice(&p[..keep]);
            let s = sol.shed(&lopf.vars, b);
            out.shed[b][start..start + keep].copy_from_slice(&s[..keep]);
        }
        for (s, level) in soc.iter_mut().enumerate() {
            let e = sol.trajectory(lopf.vars.soc[s]);
            out.soc[s][start..start + keep].copy_from_slice(&e[..keep]);
            // The level at the end of what is kept is where the next window
            // begins, not the level at the end of the lookahead.
            *level = e[keep - 1];
        }
        for (g, running) in on.iter_mut().enumerate() {
            if let Some(status) = lopf.vars.status[g] {
                *running = sol.trajectory(status)[keep - 1] > 0.5;
            }
        }

        // Cost of the kept snapshots only.
        let weights = w.snapshots.weights();
        for g in 0..net.generators.len() {
            let d = sol.dispatch(&lopf.vars, g);
            let c = net.generators[g].marginal_cost;
            for t in 0..keep {
                out.objective += c * d[t] * weights[t];
            }
        }
        for b in 0..net.buses.len() {
            let s = sol.shed(&lopf.vars, b);
            for t in 0..keep {
                out.objective += net.value_of_lost_load * s[t] * weights[t];
            }
        }

        start += keep;
        first = false;
    }

    Ok(out)
}

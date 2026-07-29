//! The engine behind a WebAssembly boundary.
//!
//! This crate is what a browser Web Worker loads. It exists because the main
//! thread may not block: a solve of a few thousand rows takes hundreds of
//! milliseconds and a large one takes seconds, and either would freeze the tab.
//! That is true regardless of threading, so the worker is not an optimisation,
//! it is the only correct place to run the engine.
//!
//! # Why the API is coarse
//!
//! Every call crosses a serialisation boundary, so this exposes whole
//! operations — load a file, solve a network — rather than accessors. A chatty
//! API here would spend more time marshalling than computing.
//!
//! # Why the protocol is JSON
//!
//! `gridwright-net` already derives serde for exactly this reason: its own
//! feature documentation says it is "for any caller that wants to hand a
//! network to a UI over a wire". Reusing it means the wire format is the
//! domain model rather than a parallel structure that can drift from it.
//!
//! Results are the exception worth watching. They are numeric and can be large,
//! and JSON is a poor carrier for a million floats. The current shape is honest
//! for the sizes a person edits interactively; a binary path is noted in
//! `TODO.md` for when it stops being.

use gridwright_build::build_lopf;
use gridwright_net::Network;
use gridwright_solve::{SimplexSolver, Solver, Status};
use serde::{Deserialize, Serialize};

/// What a file turned into, plus what reading it cost.
///
/// `notes` is not decoration. Every format carries more than a linear
/// optimisation model can hold, and each carries a different more, so the
/// readers record what they dropped. An interface that hides this is deciding
/// on the user's behalf what they did not need to know.
#[derive(Debug, Serialize, Deserialize)]
pub struct Loaded {
    pub name: String,
    pub notes: Vec<String>,
    pub network: Network,
}

/// A solved network, in the terms the network was described in.
///
/// Deliberately domain quantities rather than raw solver vectors: a column
/// index means nothing to an interface, and mapping it back is the engine's job
/// rather than the UI's.
#[derive(Debug, Serialize, Deserialize)]
pub struct Solved {
    /// `Optimal`, `Infeasible`, `Unbounded`, and so on, as a string.
    pub status: String,
    /// Absent unless the status is optimal, so a caller cannot read a cost off
    /// an answer that has none.
    pub objective: Option<f64>,
    /// Total unserved energy across the whole system. Zero on a healthy model;
    /// anything else means the shed vectors below are worth looking at.
    pub total_shed: f64,
    /// Simplex iterations, where the backend reports them. `None` from branch
    /// and bound, which counts nodes rather than iterations.
    pub iterations: Option<usize>,
    /// Of those, how many went on reaching feasibility rather than optimality.
    /// Surfaced because it is where the time goes — about three quarters of a
    /// solve on these models — and because it is exactly what a warm start
    /// would remove, which is the next thing an interactive edit loop wants.
    pub phase_one_iterations: Option<usize>,
    /// Per bus, per snapshot. The dual of a nodal balance row *is* the price of
    /// energy at that bus, which is the output this engine exists to produce.
    pub prices: Vec<Vec<f64>>,
    /// Per generator, per snapshot.
    pub dispatch: Vec<Vec<f64>>,
    /// Per line, per snapshot.
    pub flows: Vec<Vec<f64>>,
    /// Per storage unit, per snapshot: state of charge in MWh.
    ///
    /// Carried rather than left to be reconstructed from charge and discharge.
    /// Those are two series that only mean something together, and integrating
    /// them by hand loses the initial level and every rounding the solver made.
    pub soc: Vec<Vec<f64>>,
    /// Per storage unit, per snapshot: net power, discharge positive.
    ///
    /// One signed series rather than the two the model carries, because the two
    /// are complementary by construction -- a unit charging is not also
    /// discharging -- and a reader wants "is it absorbing or delivering".
    pub storage_power: Vec<Vec<f64>>,
    /// Per bus, per snapshot. Non-zero anywhere means the system could not be
    /// served, and *where* and *when* is the useful part of that.
    pub shed: Vec<Vec<f64>>,
    /// Capacity the model chose to build, where anything was extendable.
    pub built: Vec<(String, f64)>,
}

/// Anything that went wrong, in a form an interface can show a person.
#[derive(Debug, Serialize, Deserialize)]
pub struct Failure {
    pub kind: String,
    pub message: String,
}

impl Failure {
    fn new(kind: &str, message: impl std::fmt::Display) -> Self {
        Self {
            kind: kind.into(),
            message: message.to_string(),
        }
    }
}

/// Read any supported format from bytes.
///
/// Bytes rather than a path because a browser has no filesystem to hand us, and
/// `gridwright-io`'s `memory` module was written byte-first for this reason.
/// The name is optional and only helps format detection; content sniffing
/// decides when it is absent or unhelpful.
pub fn load(name: Option<&str>, bytes: &[u8]) -> Result<Loaded, Failure> {
    let case = gridwright_io::load_bytes(name, bytes)
        .map_err(|e| Failure::new("read", e))?;
    Ok(Loaded {
        name: case.name,
        notes: case.notes,
        network: case.network,
    })
}

/// Build and solve, returning domain quantities.
///
/// The solver is the pure-Rust simplex because it is the one that reaches this
/// target: HiGHS is C++ and cannot compile to `wasm32-unknown-unknown` at all.
/// A faster path through `highs-js` as a sibling wasm module is planned and
/// tracked in `TODO.md`; when it lands it goes behind a size-based choice here,
/// not in the interface.
pub fn solve(network: &Network) -> Result<Solved, Failure> {
    let lopf = build_lopf(network).map_err(|e| Failure::new("build", e))?;
    let sol = SimplexSolver::default()
        .solve(&lopf)
        .map_err(|e| Failure::new("solve", e))?;

    let n = network.n_snapshots();
    let owned = |s: &[f64]| s.to_vec();

    // Capacity decisions are the headline of an expansion run, so they are
    // reported rather than left for the caller to reconstruct from columns.
    let mut built = Vec::new();
    for (g, unit) in network.generators.iter().enumerate() {
        if let Some(cap) = lopf.vars.gen_capacity[g] {
            built.push((unit.name.clone(), sol.total_capacity(Some(cap), unit.p_nom)));
        }
    }
    for (l, line) in network.lines.iter().enumerate() {
        if let Some(cap) = lopf.vars.line_capacity[l] {
            built.push((line.name.clone(), sol.total_capacity(Some(cap), line.s_nom)));
        }
    }
    for (s, unit) in network.storage.iter().enumerate() {
        if let Some(cap) = lopf.vars.storage_capacity[s] {
            built.push((unit.name.clone(), sol.total_capacity(Some(cap), unit.p_nom)));
        }
    }

    Ok(Solved {
        status: format!("{:?}", sol.status),
        objective: (sol.status == Status::Optimal).then_some(sol.objective),
        total_shed: sol.total_shed(&lopf.vars),
        iterations: sol.iterations,
        phase_one_iterations: sol.phase_one_iterations,
        prices: (0..network.buses.len())
            .map(|b| owned(sol.price(b, n)))
            .collect(),
        dispatch: (0..network.generators.len())
            .map(|g| owned(sol.dispatch(&lopf.vars, g)))
            .collect(),
        flows: (0..network.lines.len())
            .map(|l| owned(sol.flow(&lopf.vars, l)))
            .collect(),
        shed: (0..network.buses.len())
            .map(|b| owned(sol.shed(&lopf.vars, b)))
            .collect(),
        soc: (0..network.storage.len())
            .map(|s| owned(sol.soc(&lopf.vars, s)))
            .collect(),
        storage_power: (0..network.storage.len())
            .map(|s| {
                let out = sol.discharge(&lopf.vars, s);
                let inn = sol.charge(&lopf.vars, s);
                out.iter().zip(inn).map(|(d, c)| d - c).collect()
            })
            .collect(),
        built,
    })
}

// ---------------------------------------------------------------------------
// The wasm boundary.
//
// Kept to a thin shell over the functions above, so everything of substance is
// testable natively without a browser. The JSON encoding lives here rather than
// in the functions themselves for the same reason.
// ---------------------------------------------------------------------------

#[cfg(target_arch = "wasm32")]
mod boundary {
    use super::*;
    use wasm_bindgen::prelude::*;

    fn encode<T: Serialize>(value: &T) -> String {
        // A failure to serialise our own types is a bug rather than a
        // condition, but panicking across the wasm boundary aborts the whole
        // instance and takes the worker with it, so it is reported instead.
        serde_json::to_string(value).unwrap_or_else(|e| {
            format!(r#"{{"kind":"encode","message":"{}"}}"#, e)
        })
    }

    /// Read a file. Returns `Loaded` as JSON, or `Failure` as JSON.
    #[wasm_bindgen]
    pub fn load_bytes(name: Option<String>, bytes: &[u8]) -> String {
        match super::load(name.as_deref(), bytes) {
            Ok(loaded) => encode(&loaded),
            Err(f) => encode(&f),
        }
    }

    /// Solve a network given as JSON. Returns `Solved` or `Failure` as JSON.
    #[wasm_bindgen]
    pub fn solve_json(network_json: &str) -> String {
        match serde_json::from_str::<Network>(network_json) {
            Ok(net) => match super::solve(&net) {
                Ok(solved) => encode(&solved),
                Err(f) => encode(&f),
            },
            Err(e) => encode(&Failure::new("decode", e)),
        }
    }
}

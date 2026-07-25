//! AC optimal power flow, through the Jabr second-order-cone relaxation.
//!
//! # What this is, and what it is not
//!
//! The AC optimal power flow problem is nonconvex. Nothing here solves it
//! exactly, and any library claiming otherwise for a general meshed network is
//! either using a local method that can miss the optimum or is not solving
//! AC-OPF. What this does is solve a **convex relaxation**: a larger problem
//! whose optimum is a rigorous lower bound on the true one.
//!
//! That distinction matters and is not a technicality:
//!
//! - The relaxation's cost is a **bound**, so a true AC solution can never be
//!   cheaper. If a DC answer comes in below it, the DC answer is infeasible.
//! - When the relaxation is **exact**, the answer is the AC optimum. Exactness
//!   is checkable after the fact rather than assumed, and this reports it.
//! - When it is **inexact**, the voltages returned do not correspond to any
//!   physical operating point, and saying so is the only honest thing to do.
//!
//! The relaxation is provably exact for radial networks under mild conditions,
//! which covers most distribution systems. Meshed transmission networks are
//! where it can loosen, and this reports the gap rather than hiding it.
//!
//! # The formulation
//!
//! The nonconvexity lives in the product of voltage magnitudes and the cosine
//! of their angle difference. Jabr's substitution removes the angles entirely:
//!
//! ```text
//!   u_i  = |V_i|²
//!   R_ij = |V_i||V_j| cos(θ_i − θ_j)
//!   I_ij = |V_i||V_j| sin(θ_i − θ_j)
//! ```
//!
//! Those three satisfy `R² + I² = u_i · u_j` exactly. Relaxing that equality to
//! an inequality is the entire trick, because `R² + I² ≤ u_i u_j` is a rotated
//! second-order cone and therefore convex:
//!
//! ```text
//!   ‖ (2R, 2I, u_i − u_j) ‖₂  ≤  u_i + u_j
//! ```
//!
//! Power flows become linear in the new variables, so everything else — nodal
//! balance in both real and reactive power, generator limits, voltage bands —
//! is an ordinary linear constraint.

use clarabel::algebra::CscMatrix;
use clarabel::solver::{
    DefaultSettings, DefaultSolver, IPSolver, SolverStatus, SupportedConeT,
};
use gridwright_net::{NetError, Network};

/// How an AC solve ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Solved, and the relaxation was tight: this is the AC optimum.
    Optimal,
    /// Solved, but the cone constraint is slack somewhere, so the answer is a
    /// lower bound rather than an operating point.
    OptimalRelaxed,
    Infeasible,
    Unbounded,
    Limit,
    Other,
}

/// The result of an AC solve.
#[derive(Debug, Clone)]
pub struct AcSolution {
    pub status: Status,
    /// Cost. A lower bound on the true AC optimum when `status` is
    /// `OptimalRelaxed`, and the optimum itself when `Optimal`.
    pub objective: f64,
    /// Voltage magnitude at each bus, per unit. Recovered as `√u`, and only
    /// physically meaningful when the relaxation was tight.
    pub voltage: Vec<f64>,
    /// Real power from each generator, MW.
    pub p_gen: Vec<f64>,
    /// Reactive power from each generator, MVAr.
    pub q_gen: Vec<f64>,
    /// Real power entering each line at its `bus0` end, MW.
    pub p_flow: Vec<f64>,
    /// Reactive power entering each line at its `bus0` end, MVAr.
    pub q_flow: Vec<f64>,
    /// Largest violation of `R² + I² = u_i u_j` across all lines.
    ///
    /// Zero means the relaxation is exact and the answer is a genuine AC
    /// solution. Anything meaningfully above zero means it is not, and the
    /// voltages describe no physical state.
    pub cone_gap: f64,
    pub iterations: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum AcError {
    #[error("network is not valid: {0}")]
    Network(#[from] NetError),
    #[error("network has no buses")]
    Empty,
    #[error(
        "line {0} has neither resistance nor reactance, so it has no admittance; \
         an AC model needs real impedance, not the DC susceptance shortcut"
    )]
    NoImpedance(usize),
}

/// Column layout of the conic problem.
///
/// Kept as one struct so the offsets are computed once and every constraint
/// builder asks the same object where a variable lives, rather than each
/// recomputing arithmetic that must agree.
struct Layout {
    n_bus: usize,
    n_line: usize,
    n_gen: usize,
}

impl Layout {
    #[inline]
    fn u(&self, b: usize) -> usize {
        b
    }
    #[inline]
    fn r(&self, l: usize) -> usize {
        self.n_bus + 2 * l
    }
    #[inline]
    fn i(&self, l: usize) -> usize {
        self.n_bus + 2 * l + 1
    }
    #[inline]
    fn pg(&self, g: usize) -> usize {
        self.n_bus + 2 * self.n_line + g
    }
    #[inline]
    fn qg(&self, g: usize) -> usize {
        self.n_bus + 2 * self.n_line + self.n_gen + g
    }
    #[inline]
    fn total(&self) -> usize {
        self.n_bus + 2 * self.n_line + 2 * self.n_gen
    }
}

/// Series admittance of a line, from its impedance.
fn admittance(line: &gridwright_net::Line, index: usize) -> Result<(f64, f64), AcError> {
    let (r, x) = (line.resistance, line.reactance);
    let denom = r * r + x * x;
    if denom < 1e-12 {
        return Err(AcError::NoImpedance(index));
    }
    // y = 1/z = (r - jx)/(r² + x²), so g = r/|z|² and b = −x/|z|².
    Ok((r / denom, -x / denom))
}

/// A row being accumulated for the conic problem.
#[derive(Default)]
struct Rows {
    /// Triplets, converted to CSC once at the end.
    entries: Vec<(usize, usize, f64)>,
    rhs: Vec<f64>,
    n_rows: usize,
}

impl Rows {
    fn push(&mut self, terms: &[(usize, f64)], b: f64) {
        let r = self.n_rows;
        for &(c, v) in terms {
            if v != 0.0 {
                self.entries.push((r, c, v));
            }
        }
        self.rhs.push(b);
        self.n_rows += 1;
    }
}

/// Build and solve the AC optimal power flow relaxation for one snapshot.
///
/// One snapshot because AC-OPF is an operating-point question. A time series of
/// them is a sequence of independent problems unless storage couples them, and
/// coupling storage to an AC relaxation is a different piece of work.
pub fn solve_acopf(net: &Network, snapshot: usize) -> Result<AcSolution, AcError> {
    net.validate()?;
    if net.buses.is_empty() {
        return Err(AcError::Empty);
    }

    // Everything below is per unit. Impedances already are; power is not, and
    // converting once here is far safer than remembering to divide at each of
    // the dozen places power enters a constraint.
    let base = if net.base_mva > 0.0 { net.base_mva } else { 100.0 };

    let lay = Layout {
        n_bus: net.buses.len(),
        n_line: net.lines.len(),
        n_gen: net.generators.len(),
    };
    let n = lay.total();

    // Objective: linear in generation. Clarabel minimises ½xᵀPx + qᵀx, and this
    // problem has no quadratic term.
    let mut q = vec![0.0; n];
    for (g, unit) in net.generators.iter().enumerate() {
        // Cost is per MWh but generation is now per unit, so the coefficient
        // absorbs the base and the reported objective stays in real money.
        q[lay.pg(g)] = unit.marginal_cost * base;
    }

    // Admittances, computed once.
    let mut y = Vec::with_capacity(lay.n_line);
    for (l, line) in net.lines.iter().enumerate() {
        y.push(admittance(line, l)?);
    }

    // --- Equality rows: nodal balance in real and reactive power. ---
    let mut eq = Rows::default();
    let gens_at = net.generators_by_bus();
    let loads_at = net.loads_by_bus();

    for b in 0..lay.n_bus {
        let mut p_terms: Vec<(usize, f64)> = Vec::new();
        let mut q_terms: Vec<(usize, f64)> = Vec::new();

        for &g in gens_at.of(b) {
            p_terms.push((lay.pg(g as usize), 1.0));
            q_terms.push((lay.qg(g as usize), 1.0));
        }

        for (l, line) in net.lines.iter().enumerate() {
            let (g_ij, b_ij) = y[l];
            let half_shunt = line.shunt_susceptance / 2.0;
            // A transformer scales what each end sees differently: the tapped
            // end by 1/tau^2 on its own voltage and 1/tau on the coupling, the
            // other end not at all on its own voltage. That asymmetry is the
            // whole of the model, and dropping it describes a different network.
            let tau = if line.tap_ratio > 0.0 { line.tap_ratio } else { 1.0 };
            let (t2, t1) = (tau * tau, tau);

            if line.bus0 == b {
                // Withdrawal at the tapped end: −P_ij and −Q_ij.
                p_terms.push((lay.u(b), -g_ij / t2));
                p_terms.push((lay.r(l), g_ij / t1));
                p_terms.push((lay.i(l), b_ij / t1));
                q_terms.push((lay.u(b), (b_ij + half_shunt) / t2));
                q_terms.push((lay.r(l), -b_ij / t1));
                q_terms.push((lay.i(l), g_ij / t1));
            } else if line.bus1 == b {
                // At the far end the angle difference reverses, which flips the
                // sign on the sine term and only that term.
                p_terms.push((lay.u(b), -g_ij));
                p_terms.push((lay.r(l), g_ij / t1));
                p_terms.push((lay.i(l), -b_ij / t1));
                q_terms.push((lay.u(b), b_ij + half_shunt));
                q_terms.push((lay.r(l), -b_ij / t1));
                q_terms.push((lay.i(l), -g_ij / t1));
            }
        }

        let mut pd = 0.0;
        let mut qd = 0.0;
        for &ld in loads_at.of(b) {
            let li = ld as usize;
            pd += net
                .load_profile
                .at(li, snapshot)
                .unwrap_or(net.loads[li].p_set);
            qd += net.loads[li].q_set;
        }
        eq.push(&p_terms, pd / base);
        eq.push(&q_terms, qd / base);
    }

    // --- Inequality rows: bounds, as a nonnegative cone. ---
    let mut ineq = Rows::default();
    let push_range = |rows: &mut Rows, col: usize, lo: f64, hi: f64| {
        if hi.is_finite() {
            rows.push(&[(col, 1.0)], hi);
        }
        if lo.is_finite() {
            rows.push(&[(col, -1.0)], -lo);
        }
    };

    for b in 0..lay.n_bus {
        let bus = &net.buses[b];
        push_range(
            &mut ineq,
            lay.u(b),
            bus.v_min * bus.v_min,
            bus.v_max * bus.v_max,
        );
    }
    for (g, unit) in net.generators.iter().enumerate() {
        let avail = net.gen_availability.at(g, snapshot).unwrap_or(1.0);
        push_range(
            &mut ineq,
            lay.pg(g),
            unit.p_nom * unit.p_min_pu * avail / base,
            unit.p_nom * avail / base,
        );
        push_range(&mut ineq, lay.qg(g), unit.q_min / base, unit.q_max / base);
    }

    // --- Cone rows: the relaxation itself. ---
    //
    // ‖(2R, 2I, u_i − u_j)‖₂ ≤ u_i + u_j, written in the form clarabel wants,
    // which is s = b − Ax ∈ SecondOrderCone with the bound first.
    let mut cone = Rows::default();
    let mut cone_dims = Vec::new();
    for (l, line) in net.lines.iter().enumerate() {
        let (i, j) = (lay.u(line.bus0), lay.u(line.bus1));
        cone.push(&[(i, -1.0), (j, -1.0)], 0.0);
        cone.push(&[(lay.r(l), -2.0)], 0.0);
        cone.push(&[(lay.i(l), -2.0)], 0.0);
        cone.push(&[(i, -1.0), (j, 1.0)], 0.0);
        cone_dims.push(4usize);
    }

    // --- Assemble. Clarabel stacks the cones in order: zero, nonnegative, SOC.
    let mut entries: Vec<(usize, usize, f64)> = Vec::new();
    let mut b_vec: Vec<f64> = Vec::new();
    let mut offset = 0usize;
    for part in [&eq, &ineq, &cone] {
        for &(r, c, v) in &part.entries {
            entries.push((r + offset, c, v));
        }
        b_vec.extend_from_slice(&part.rhs);
        offset += part.n_rows;
    }

    let mut cones: Vec<SupportedConeT<f64>> = Vec::new();
    if eq.n_rows > 0 {
        cones.push(SupportedConeT::ZeroConeT(eq.n_rows));
    }
    if ineq.n_rows > 0 {
        cones.push(SupportedConeT::NonnegativeConeT(ineq.n_rows));
    }
    for d in cone_dims {
        cones.push(SupportedConeT::SecondOrderConeT(d));
    }

    let a = triplets_to_csc(offset, n, &entries);
    let p = CscMatrix::<f64>::zeros((n, n));

    let settings = DefaultSettings::<f64> {
        verbose: false,
        ..Default::default()
    };

    let mut solver = DefaultSolver::new(&p, &q, &a, &b_vec, &cones, settings)
        .map_err(|_| AcError::Empty)?;
    solver.solve();

    let x = &solver.solution.x;
    let status = match solver.solution.status {
        SolverStatus::Solved | SolverStatus::AlmostSolved => Status::Optimal,
        SolverStatus::PrimalInfeasible | SolverStatus::AlmostPrimalInfeasible => {
            Status::Infeasible
        }
        SolverStatus::DualInfeasible | SolverStatus::AlmostDualInfeasible => Status::Unbounded,
        SolverStatus::MaxIterations | SolverStatus::MaxTime => Status::Limit,
        _ => Status::Other,
    };

    // Recover quantities, and measure how tight the relaxation actually was.
    let voltage: Vec<f64> = (0..lay.n_bus).map(|b| x[lay.u(b)].max(0.0).sqrt()).collect();
    // Back to MW and MVAr for the caller, who asked in those units.
    let p_gen: Vec<f64> = (0..lay.n_gen).map(|g| x[lay.pg(g)] * base).collect();
    let q_gen: Vec<f64> = (0..lay.n_gen).map(|g| x[lay.qg(g)] * base).collect();

    let mut cone_gap: f64 = 0.0;
    let mut p_flow = Vec::with_capacity(lay.n_line);
    let mut q_flow = Vec::with_capacity(lay.n_line);
    for (l, line) in net.lines.iter().enumerate() {
        let (g_ij, b_ij) = y[l];
        let tau = if line.tap_ratio > 0.0 { line.tap_ratio } else { 1.0 };
        let (t2, t1) = (tau * tau, tau);
        let (ui, uj) = (x[lay.u(line.bus0)], x[lay.u(line.bus1)]);
        let (rr, ii) = (x[lay.r(l)], x[lay.i(l)]);
        // The relaxation replaced an equality with an inequality; this is how
        // far apart the two ended up.
        let slack = ui * uj - (rr * rr + ii * ii);
        cone_gap = cone_gap.max(slack.abs() / (ui * uj).abs().max(1.0));
        p_flow.push((g_ij * ui / t2 - (g_ij * rr + b_ij * ii) / t1) * base);
        q_flow.push(
            (-(b_ij + line.shunt_susceptance / 2.0) * ui / t2 + (b_ij * rr - g_ij * ii) / t1)
                * base,
        );
    }

    let objective = net
        .generators
        .iter()
        .enumerate()
        .map(|(g, unit)| unit.marginal_cost * p_gen[g])
        .sum();

    // Tightness decides whether this is an answer or a bound, so it is folded
    // into the status rather than left for the caller to notice.
    let status = if status == Status::Optimal && cone_gap > 1e-5 {
        Status::OptimalRelaxed
    } else {
        status
    };

    Ok(AcSolution {
        status,
        objective,
        voltage,
        p_gen,
        q_gen,
        p_flow,
        q_flow,
        cone_gap,
        iterations: solver.solution.iterations,
    })
}

/// Triplets to compressed sparse column, summing duplicates.
///
/// Duplicates are expected rather than exceptional here: a bus with several
/// lines contributes to the same `(row, column)` once per line, and those
/// contributions must add rather than overwrite.
fn triplets_to_csc(rows: usize, cols: usize, entries: &[(usize, usize, f64)]) -> CscMatrix<f64> {
    let mut counts = vec![0usize; cols + 1];
    for &(_, c, _) in entries {
        counts[c + 1] += 1;
    }
    for c in 0..cols {
        counts[c + 1] += counts[c];
    }
    let nnz = entries.len();
    let mut rowval = vec![0usize; nnz];
    let mut nzval = vec![0.0f64; nnz];
    let mut cursor = counts.clone();
    for &(r, c, v) in entries {
        let k = cursor[c];
        rowval[k] = r;
        nzval[k] = v;
        cursor[c] += 1;
    }
    // Sort each column by row and merge duplicates.
    let mut colptr = vec![0usize; cols + 1];
    let mut out_rows = Vec::with_capacity(nnz);
    let mut out_vals = Vec::with_capacity(nnz);
    for c in 0..cols {
        let s = counts[c];
        let e = counts[c + 1];
        let mut pairs: Vec<(usize, f64)> =
            (s..e).map(|k| (rowval[k], nzval[k])).collect();
        pairs.sort_by_key(|p| p.0);
        let mut i = 0;
        while i < pairs.len() {
            let r = pairs[i].0;
            let mut acc = 0.0;
            while i < pairs.len() && pairs[i].0 == r {
                acc += pairs[i].1;
                i += 1;
            }
            if acc != 0.0 {
                out_rows.push(r);
                out_vals.push(acc);
            }
        }
        colptr[c + 1] = out_rows.len();
    }
    CscMatrix::new(rows, cols, colptr, out_rows, out_vals)
}

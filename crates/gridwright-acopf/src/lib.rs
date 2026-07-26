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

pub mod bnb;
pub mod cycles;

pub use bnb::{BnbOptions, BnbSolution, Stop, solve_bnb};

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
    /// How many triangles had cycle constraints applied. Zero when they were
    /// not requested, or when the network is radial and has none.
    pub triangles_constrained: usize,
    /// `Re(V_i · conj(V_j))` per line: the Jabr variable itself.
    ///
    /// Exposed because the spatial search branches on it, and because it is
    /// the only place the relaxation's remaining error is visible per branch
    /// rather than as one summary number.
    pub w_re: Vec<f64>,
    /// `Im(V_i · conj(V_j))` per line.
    pub w_im: Vec<f64>,
    /// `|V|²` per bus, before the square root.
    pub u: Vec<f64>,
    /// Slack in `R² + I² ≤ u_i u_j` on each line, normalised.
    ///
    /// Zero means that branch's relaxation is exact. The largest of these is
    /// [`AcSolution::cone_gap`], and the search splits whichever branch has
    /// the biggest.
    pub line_gap: Vec<f64>,
    /// Largest violation of `Im(W₁W₂W₃) = 0` over the constrained cycles.
    ///
    /// A separate question from [`AcSolution::cone_gap`], and one it cannot
    /// answer. The cone is a statement about each branch on its own; this is
    /// the statement that the angle differences add up around a loop. A
    /// solution can satisfy every branch exactly and still be unphysical,
    /// routing power around a cycle in a way no set of voltage angles could
    /// produce — and a reader looking only at the cone gap would call it
    /// optimal.
    ///
    /// Zero when no cycles were constrained, which means unmeasured rather
    /// than satisfied.
    pub cycle_gap: f64,
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
    solve_acopf_with(net, snapshot, AcOptions::default())
}

/// Knobs for the AC solve.
#[derive(Debug, Clone, Copy)]
pub struct AcOptions {
    /// Add cycle constraints for triangles, relaxed through McCormick
    /// envelopes. Tightens the relaxation on meshed networks at the cost of
    /// auxiliary variables; see [`cycles`].
    pub cycle_constraints: bool,
    /// Longest cycle to constrain.
    ///
    /// Three keeps the old behaviour. Longer cycles are where the relaxation is
    /// loosest on a real meshed network, and each costs six auxiliary variables
    /// per additional line, so this is the knob trading tightness against size.
    /// Not a correctness setting: a shorter limit means a looser bound, never a
    /// wrong one.
    pub max_cycle_length: usize,
    /// Cap on how many cycles to constrain. Each costs variables and rows,
    /// and a dense subnetwork has a great many, so this is a budget rather than
    /// a correctness setting: fewer triangles means a looser bound, never a
    /// wrong one.
    pub max_triangles: usize,
}

impl Default for AcOptions {
    fn default() -> Self {
        Self {
            cycle_constraints: false,
            max_triangles: 256,
            max_cycle_length: 3,
        }
    }
}

/// Build and solve the AC relaxation with explicit options.
pub fn solve_acopf_with(
    net: &Network,
    snapshot: usize,
    opts: AcOptions,
) -> Result<AcSolution, AcError> {
    net.validate()?;
    if net.buses.is_empty() {
        return Err(AcError::Empty);
    }
    let dom = cycles::Domain::root(net);
    solve_in_domain(net, snapshot, opts, &dom)
}

/// Solve the relaxation restricted to a domain.
///
/// Every node of the spatial search is this function over a smaller box. The
/// objective it returns is a valid lower bound for its own box, so the least
/// bound over any partition of the root box is a valid bound for the whole
/// problem — which is what makes the search sound.
pub fn solve_in_domain(
    net: &Network,
    snapshot: usize,
    opts: AcOptions,
    dom: &cycles::Domain,
) -> Result<AcSolution, AcError> {
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
    // Triangles are chosen before the column count is fixed, since each one
    // brings auxiliary variables with it.
    let cycles_found = if opts.cycle_constraints {
        cycles::find_cycles(net, opts.max_cycle_length, opts.max_triangles)
    } else {
        Vec::new()
    };
    // Six auxiliaries per multiplication: four products and the two parts of
    // the running result. A cycle of length `k` takes `k − 1` of them, so the
    // cost grows linearly in the length where writing the expansion out grows
    // exponentially.
    const AUX_PER_STEP: usize = 6;
    let mut cycle_offsets = Vec::with_capacity(cycles_found.len());
    let mut aux_used = 0usize;
    for c in &cycles_found {
        cycle_offsets.push(aux_used);
        aux_used += (c.len() - 1) * AUX_PER_STEP;
    }
    let aux_base = lay.total();
    let n = aux_base + aux_used;

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

        // A shunt at the node itself: real power drawn in proportion to |V|²,
        // reactive power injected in proportion to it. Unlike the DC case this
        // is not a constant, because |V|² is the decision variable `u`.
        if net.buses[b].g_shunt != 0.0 {
            p_terms.push((lay.u(b), -net.buses[b].g_shunt));
        }
        if net.buses[b].b_shunt != 0.0 {
            q_terms.push((lay.u(b), net.buses[b].b_shunt));
        }

        for (l, line) in net.lines.iter().enumerate() {
            let (g_ij, b_ij) = y[l];
            let half_shunt = line.shunt_susceptance / 2.0;
            // A phase shift rotates the coupling between the two ends without
            // touching either end's own voltage term. Writing the tap ratio as
            // the complex number it is, `a = τ·e^{jθ}`, the off-diagonal
            // admittance picks up `e^{∓jθ}`, and multiplying that out leaves
            // the series admittance rotated:
            //
            //   at bus0:  g' = g·cosθ − b·sinθ,   b' = b·cosθ + g·sinθ
            //   at bus1:  the same with θ negated
            //
            // At θ = 0 both collapse to (g, b) and every row below is
            // unchanged, which is what makes this safe to apply everywhere.
            let (cos, sin) = (line.phase_shift.cos(), line.phase_shift.sin());
            let (g0, b0) = (g_ij * cos - b_ij * sin, b_ij * cos + g_ij * sin);
            let (g1, b1) = (g_ij * cos + b_ij * sin, b_ij * cos - g_ij * sin);
            // A transformer scales what each end sees differently: the tapped
            // end by 1/tau^2 on its own voltage and 1/tau on the coupling, the
            // other end not at all on its own voltage. That asymmetry is the
            // whole of the model, and dropping it describes a different network.
            let tau = if line.tap_ratio > 0.0 { line.tap_ratio } else { 1.0 };
            let (t2, t1) = (tau * tau, tau);

            if line.bus0 == b {
                // Withdrawal at the tapped end: −P_ij and −Q_ij.
                p_terms.push((lay.u(b), -g_ij / t2));
                p_terms.push((lay.r(l), g0 / t1));
                p_terms.push((lay.i(l), b0 / t1));
                q_terms.push((lay.u(b), (b_ij + half_shunt) / t2));
                q_terms.push((lay.r(l), -b0 / t1));
                q_terms.push((lay.i(l), g0 / t1));
            } else if line.bus1 == b {
                // At the far end the angle difference reverses, which flips the
                // sign on the sine term and only that term.
                p_terms.push((lay.u(b), -g_ij));
                p_terms.push((lay.r(l), g1 / t1));
                p_terms.push((lay.i(l), -b1 / t1));
                q_terms.push((lay.u(b), b_ij + half_shunt));
                q_terms.push((lay.r(l), -b1 / t1));
                q_terms.push((lay.i(l), -g1 / t1));
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
        let _ = bus;
        push_range(&mut ineq, lay.u(b), dom.u[b].0, dom.u[b].1);
    }

    // --- The box, and the cut that closes the relaxation as it shrinks. ---
    //
    // Jabr relaxes `R² + I² = u_i u_j` to `≤`, and everything the relaxation
    // gets wrong lives in that inequality being slack. The missing half is
    // `R² + I² ≥ u_i u_j`, which is reverse-convex and cannot be written down
    // directly.
    //
    // Over a box it can be relaxed validly. `R² + I²` is convex, so the affine
    // function through the corner values lies above it; `u_i u_j` is bilinear,
    // so McCormick gives affine functions lying below it. Therefore
    //
    //     secant(R) + secant(I)  ≥  R² + I²  ≥  u_i u_j  ≥  McCormick(u_i, u_j)
    //
    // and the two ends of that chain are linear. Imposing them is implied by
    // the true constraint, so no feasible point is ever cut off — and as the
    // box closes, the secant collapses onto the parabola and the McCormick
    // bound onto the product, so the relaxation converges to the exact
    // condition. That convergence is what the search below is spending nodes
    // to buy.
    for (l, line) in net.lines.iter().enumerate() {
        if line.is_transport() {
            continue;
        }
        let b = dom.lines[l];
        push_range(&mut ineq, lay.r(l), b.r.0, b.r.1);
        push_range(&mut ineq, lay.i(l), b.i.0, b.i.1);

        let (sr, cr) = cycles::secant(b.r.0, b.r.1);
        let (si, ci) = cycles::secant(b.i.0, b.i.1);
        let (ui, uj) = (dom.u[line.bus0], dom.u[line.bus1]);
        // Both McCormick underestimators of the product; either alone is
        // valid, and together they are tighter.
        for (a, c) in [(ui.0, uj.0), (ui.1, uj.1)] {
            //  a·u_j + c·u_i − a·c  ≤  secant_R + secant_I
            //  →  a·u_j + c·u_i − sr·R − si·I  ≤  a·c + cr + ci
            ineq.push(
                &[
                    (lay.u(line.bus1), a),
                    (lay.u(line.bus0), c),
                    (lay.r(l), -sr),
                    (lay.i(l), -si),
                ],
                a * c + cr + ci,
            );
        }
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

    // --- Cycle constraints, if asked for. ---
    //
    // Around a closed loop the product of the `W`s telescopes to a real,
    // non-negative number, so its imaginary part vanishes. That is the algebraic
    // form of "the angle differences add up", with no arctangents in it.
    //
    // For a triangle the expansion is short enough to write out. For anything
    // longer it is not: the imaginary part of a product of `k` complex numbers
    // has `2^(k-1)` terms. So the product is built up instead, one factor at a
    // time:
    //
    //     (a + ib)(c + id) = (ac − bd) + i(ad + bc)
    //
    // Four bilinear products and two linear combinations per step, `k − 1`
    // steps, and the imaginary part of the last one is set to zero. That grows
    // linearly in the cycle length where the expansion grows exponentially,
    // which is the whole reason cycles longer than three were out of reach.
    //
    // Each bilinear product becomes an auxiliary variable between McCormick
    // bounds, drawn over this node's boxes, so a tighter box gives a tighter
    // constraint and the spatial search has something to work on.
    for (n_cyc, cyc) in cycles_found.iter().enumerate() {
        let k = cyc.len();
        let base_col = aux_base + cycle_offsets[n_cyc];

        // Bounds on each factor's real and imaginary parts, from the node's box.
        let bnd: Vec<(f64, f64)> = (0..k)
            .map(|q| {
                let b = dom.lines[cyc.lines[q]];
                (b.r.0.min(b.i.0), b.r.1.max(b.i.1))
            })
            .collect();

        let envelope = |rows: &mut Rows, x: usize, y: usize, w: usize,
                            xb: (f64, f64), yb: (f64, f64)| {
            for (a, b, c, rhs) in cycles::mccormick(xb.0, xb.1, yb.0, yb.1) {
                rows.push(&[(x, a), (y, b), (w, c)], rhs);
            }
        };

        // The running product, as the columns holding its real and imaginary
        // parts, and the bound on their magnitude.
        let sgn = |q: usize| if cyc.forward[q] { 1.0 } else { -1.0 };
        let mut acc_re = lay.r(cyc.lines[0]);
        let mut acc_im = lay.i(cyc.lines[0]);
        // Traversing a line backwards conjugates it. Rather than carry a sign
        // through every product, the first factor's sign is folded into the
        // accumulator's definition and later ones into their own terms.
        let mut acc_im_sign = sgn(0);
        let mut acc_bound = bnd[0].1.abs().max(bnd[0].0.abs());
        let mut next_aux = base_col;

        for (q, &fb) in bnd.iter().enumerate().take(k).skip(1) {
            let (fr, fi) = (lay.r(cyc.lines[q]), lay.i(cyc.lines[q]));
            let fmag = fb.1.abs().max(fb.0.abs());
            let ab = (-acc_bound, acc_bound);

            // Four products: ac, bd, ad, bc.
            let (p_ac, p_bd, p_ad, p_bc) =
                (next_aux, next_aux + 1, next_aux + 2, next_aux + 3);
            envelope(&mut ineq, acc_re, fr, p_ac, ab, fb);
            envelope(&mut ineq, acc_im, fi, p_bd, ab, fb);
            envelope(&mut ineq, acc_re, fi, p_ad, ab, fb);
            envelope(&mut ineq, acc_im, fr, p_bc, ab, fb);

            let (new_re, new_im) = (next_aux + 4, next_aux + 5);
            next_aux += 6;

            // The signs of the two conjugations meet here: `acc_im` carries the
            // accumulated one and this factor carries its own.
            let s = sgn(q);
            //  new_re = ac − bd
            eq.push(
                &[
                    (new_re, 1.0),
                    (p_ac, -1.0),
                    (p_bd, acc_im_sign * s),
                ],
                0.0,
            );
            //  new_im = ad + bc
            eq.push(
                &[
                    (new_im, 1.0),
                    (p_ad, -s),
                    (p_bc, -acc_im_sign),
                ],
                0.0,
            );

            acc_re = new_re;
            acc_im = new_im;
            acc_im_sign = 1.0;
            acc_bound *= fmag;
        }

        // The imaginary part of the whole product is zero.
        eq.push(&[(acc_im, 1.0)], 0.0);
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

    // --- Thermal limits, as the cone they actually are. ---
    //
    // A line's rating bounds its *apparent* power, `√(P² + Q²) ≤ S`, which is a
    // circle in the complex plane. A DC model has no reactive power and bounds
    // the real part alone with a pair of linear inequalities, and carrying that
    // over here would be wrong in the direction that matters: a line at its
    // rating on reactive power alone would read as entirely unloaded.
    //
    // Both `P` and `Q` are already linear in the decision variables, so the
    // constraint goes in directly as a three-dimensional second-order cone with
    // the rating as its bound, and needs no auxiliary variables at all.
    //
    // Written at the `bus0` end. The far end differs by the losses, which for a
    // line near its limit is a fraction of a percent; bounding both ends would
    // double the cones for a distinction the ratings themselves do not carry.
    for (l, line) in net.lines.iter().enumerate() {
        let rating = line.s_nom / base;
        // No rating, or one large enough to mean unlimited. A NaN falls through
        // the first test and is excluded, which is the safe direction: a cone
        // built on one would poison the whole solve.
        if !rating.is_finite() || rating <= 0.0 || rating >= 1e4 {
            continue;
        }
        let (g_ij, b_ij) = y[l];
        let (cos, sin) = (line.phase_shift.cos(), line.phase_shift.sin());
        let (g0, b0) = (g_ij * cos - b_ij * sin, b_ij * cos + g_ij * sin);
        let tau = if line.tap_ratio > 0.0 { line.tap_ratio } else { 1.0 };
        let (t2, t1) = (tau * tau, tau);
        let half_shunt = line.shunt_susceptance / 2.0;
        let ui = lay.u(line.bus0);

        // The bound comes first, then the two components, in clarabel's
        // `s = b − Ax` form: a zero row with the rating on the right, then each
        // expression negated.
        cone.push(&[], rating);
        //  P = g·u/τ² − (g'R + b'I)/τ
        cone.push(
            &[
                (ui, g_ij / t2),
                (lay.r(l), -g0 / t1),
                (lay.i(l), -b0 / t1),
            ],
            0.0,
        );
        //  Q = −(b + b_sh/2)·u/τ² + (b'R − g'I)/τ
        cone.push(
            &[
                (ui, -(b_ij + half_shunt) / t2),
                (lay.r(l), b0 / t1),
                (lay.i(l), -g0 / t1),
            ],
            0.0,
        );
        cone_dims.push(3usize);
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
    let mut line_gap = Vec::with_capacity(lay.n_line);
    let mut w_re = Vec::with_capacity(lay.n_line);
    let mut w_im = Vec::with_capacity(lay.n_line);
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
        let normalised = slack.abs() / (ui * uj).abs().max(1.0);
        cone_gap = cone_gap.max(normalised);
        line_gap.push(normalised);
        w_re.push(rr);
        w_im.push(ii);
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

    let u: Vec<f64> = (0..lay.n_bus).map(|b| x[lay.u(b)]).collect();

    // Cycle consistency, measured from the solution rather than trusted.
    //
    // Around a closed loop W_ij·W_jk·W_ki is real and non-negative, so its
    // imaginary part vanishing is the algebraic form of "the angle differences
    // sum to zero". The McCormick envelopes only relax that condition; whether
    // the point actually satisfies it is a separate matter, and the answer is
    // frequently no.
    let mut cycle_gap: f64 = 0.0;
    for cyc in &cycles_found {
        // Multiply the loop out in complex arithmetic, which is the same chain
        // the constraint builds and the honest way to check it: the imaginary
        // part of the product is what should have been driven to zero.
        let (mut re, mut im) = (1.0f64, 0.0f64);
        let mut scale = 1.0f64;
        for (q, &l) in cyc.lines.iter().enumerate() {
            let (a, b) = (
                x[lay.r(l)],
                if cyc.forward[q] { 1.0 } else { -1.0 } * x[lay.i(l)],
            );
            let (nr, ni) = (re * a - im * b, re * b + im * a);
            re = nr;
            im = ni;
            scale *= u[cyc.buses[q]].abs().max(1e-9);
        }
        let _ = re;
        cycle_gap = cycle_gap.max(im.abs() / scale.max(1e-9));
    }

    // Tightness decides whether this is an answer or a bound, so it is folded
    // into the status rather than left for the caller to notice.
    // Both conditions have to hold for the answer to describe a real operating
    // point. Reporting `Optimal` on the strength of the cone alone would call
    // a cycle-inconsistent solution physical.
    let status = if status == Status::Optimal && (cone_gap > 1e-5 || cycle_gap > 1e-5) {
        Status::OptimalRelaxed
    } else {
        status
    };

    Ok(AcSolution {
        cycle_gap,
        w_re,
        w_im,
        u,
        line_gap,
        status,
        triangles_constrained: cycles_found.len(),
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

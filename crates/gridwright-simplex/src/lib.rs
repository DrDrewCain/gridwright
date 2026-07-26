//! A bounded-variable revised simplex that returns row duals.
//!
//! # Why this exists
//!
//! The engine needs to run in a browser, and its usual solver, HiGHS, is C++.
//! The pure-Rust alternatives solve the problem but do not expose the duals,
//! and computing them from outside is impossible because they live in the
//! solver's private basis. For an energy model that is not a minor gap: the
//! dual of a nodal balance row *is* the price of energy at that bus, and it is
//! usually the reason the model was run.
//!
//! So this returns them. It exists to be the browser-side backend behind the
//! same `Solver` trait HiGHS sits behind, and to be small enough to reason
//! about rather than fast enough to compete.
//!
//! # Formulation
//!
//! Problems arrive as ranged rows over bounded variables, which is the shape
//! the engine already produces:
//!
//! ```text
//!   minimise    cᵀx
//!   subject to  rowlo ≤ Ax ≤ rowup
//!               collo ≤  x  ≤ colup
//! ```
//!
//! A slack per row turns the inequalities into equalities:
//!
//! ```text
//!   Ax − s = 0,   rowlo ≤ s ≤ rowup
//! ```
//!
//! Structural and slack variables are then treated identically, the basis is
//! square, and each row's dual falls out of `y` directly.
//!
//! # Finding a starting point
//!
//! Phase one uses **explicit artificial variables**: one per row, forming an
//! identity basis that is feasible by construction, priced at one while
//! everything else is priced at zero. Driving that objective to zero drives the
//! artificials out, and the basis left behind is a feasible starting point for
//! phase two.
//!
//! This is deliberately the textbook construction rather than a composite
//! objective that adjusts costs as variables cross their violated bounds. The
//! composite method is faster and I got it wrong: the subtlety is that an
//! infeasible basic variable may travel *through* the bound it violates, which
//! an ordinary ratio test forbids. Artificials have no such special case, and a
//! solver that is correct is worth more here than one that is quick.

// Row loops here index several parallel arrays at once (basis, direction,
// values, bounds). Iterating one of them and indexing the rest reads worse
// than indexing all of them by the row number they share.
#![allow(clippy::needless_range_loop)]

mod basis;
pub mod lu;
pub mod mip;

pub use mip::{MipOptions, MipSolution, solve_mip};

pub use basis::{Basis, BasisError};

/// Which bound a nonbasic variable is resting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    Lower,
    Upper,
    /// Free variables have no bound to rest on and sit at zero.
    Free,
}

/// How a solve ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Optimal,
    Infeasible,
    Unbounded,
    /// Stopped on the iteration limit, with no proof either way.
    IterationLimit,
    /// The basis became numerically unusable and refactorising did not help.
    NumericalFailure,
}

impl Status {
    #[inline]
    pub fn is_optimal(self) -> bool {
        matches!(self, Status::Optimal)
    }
}

/// The result of a solve.
#[derive(Debug, Clone)]
pub struct Solution {
    pub status: Status,
    pub objective: f64,
    /// Value of every structural variable.
    pub col_value: Vec<f64>,
    /// Dual of every row, in the order the rows were given.
    ///
    /// The rate at which the objective would improve per unit of relaxation in
    /// that row. Zero for a row that is not binding.
    pub row_dual: Vec<f64>,
    pub iterations: usize,
}

/// A linear program.
#[derive(Debug, Clone, Copy)]
pub struct Problem<'a> {
    pub n_cols: usize,
    pub n_rows: usize,
    /// Constraint matrix, compressed sparse column: `n_cols + 1` starts.
    pub col_starts: &'a [u32],
    pub row_indices: &'a [u32],
    pub values: &'a [f64],
    pub col_lower: &'a [f64],
    pub col_upper: &'a [f64],
    pub col_cost: &'a [f64],
    pub row_lower: &'a [f64],
    pub row_upper: &'a [f64],
}

/// Tolerances and limits.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub max_iterations: usize,
    /// A reduced cost smaller than this counts as zero.
    pub dual_tolerance: f64,
    /// A bound violation smaller than this counts as zero.
    pub primal_tolerance: f64,
    /// Pivots below this are refused as unsafe.
    pub pivot_tolerance: f64,
    /// Rebuild the inverse after this many updates, to stop rounding error
    /// accumulating through repeated rank-one updates.
    pub refactor_every: usize,
    /// Refactorise once the accumulated updates carry this multiple of the
    /// factorisation's own nonzeros.
    ///
    /// The count above is a backstop; this is the rule that actually fires,
    /// because what costs time in a solve is nonzeros rather than pivots. How
    /// fast updates fill in varies by orders of magnitude between problems, so
    /// a fixed count either refactorises a cheap run pointlessly or lets an
    /// expensive one drag. Zero disables it and leaves only the count.
    pub refactor_fill_ratio: f64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_iterations: 200_000,
            dual_tolerance: 1e-9,
            primal_tolerance: 1e-8,
            pivot_tolerance: 1e-9,
            refactor_every: 64,
            refactor_fill_ratio: 0.5,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SolveError {
    #[error("column starts must have n_cols + 1 entries, found {0}")]
    BadColumnStarts(usize),
    #[error("a bound array has the wrong length")]
    BadBounds,
    #[error("row index {row} is out of range for {n_rows} rows")]
    RowOutOfRange { row: u32, n_rows: usize },
    #[error(transparent)]
    Basis(#[from] BasisError),
}

/// Structurals occupy `0..n_cols`, slacks the next `n_rows`, artificials the
/// `n_rows` after that. Keeping them in one index space means the pivot logic
/// never has to ask which kind it is holding.
struct Tab<'a> {
    p: Problem<'a>,
    o: Options,
    n_struct: usize,
    n_slack: usize,
    m: usize,
    /// Total columns including artificials.
    n: usize,
    lower: Vec<f64>,
    upper: Vec<f64>,
    cost: Vec<f64>,
    basis: Vec<usize>,
    is_basic: Vec<bool>,
    at: Vec<At>,
    value: Vec<f64>,
    inv: Basis,
    since_refactor: usize,
    /// Sign of each artificial's column, chosen so it starts non-negative.
    artificial_sign: Vec<f64>,
}

impl<'a> Tab<'a> {
    fn new(p: Problem<'a>, o: Options) -> Self {
        let n_struct = p.n_cols;
        let m = p.n_rows;
        let n = n_struct + m + m;

        let mut lower = Vec::with_capacity(n);
        let mut upper = Vec::with_capacity(n);
        let mut cost = Vec::with_capacity(n);

        lower.extend_from_slice(p.col_lower);
        upper.extend_from_slice(p.col_upper);
        cost.extend_from_slice(p.col_cost);
        // Slacks carry the row ranges and no cost.
        lower.extend_from_slice(p.row_lower);
        upper.extend_from_slice(p.row_upper);
        cost.extend(std::iter::repeat_n(0.0, m));
        // Artificials are non-negative and unbounded above during phase one;
        // phase two pins them to zero rather than deleting them, which keeps
        // every index stable.
        lower.extend(std::iter::repeat_n(0.0, m));
        upper.extend(std::iter::repeat_n(f64::INFINITY, m));
        cost.extend(std::iter::repeat_n(0.0, m));

        let mut at = vec![At::Lower; n];
        let mut value = vec![0.0; n];
        // Every structural and slack starts nonbasic on whichever bound it has.
        for v in 0..(n_struct + m) {
            at[v] = if lower[v].is_finite() {
                At::Lower
            } else if upper[v].is_finite() {
                At::Upper
            } else {
                At::Free
            };
            value[v] = match at[v] {
                At::Lower => lower[v],
                At::Upper => upper[v],
                At::Free => 0.0,
            };
        }

        let basis: Vec<usize> = (n_struct + m..n).collect();
        let mut is_basic = vec![false; n];
        for &v in &basis {
            is_basic[v] = true;
        }

        Self {
            p,
            o,
            n_struct,
            n_slack: m,
            m,
            n,
            lower,
            upper,
            cost,
            basis,
            is_basic,
            at,
            value,
            inv: Basis::identity(m),
            since_refactor: 0,
            artificial_sign: vec![1.0; m],
        }
    }

    #[inline]
    fn is_artificial(&self, v: usize) -> bool {
        v >= self.n_struct + self.n_slack
    }

    /// Column `v` of `[A | −I | S]`, where `S` holds the artificial signs.
    fn column(&self, v: usize, out: &mut Vec<(usize, f64)>) {
        out.clear();
        if v < self.n_struct {
            let s = self.p.col_starts[v] as usize;
            let e = self.p.col_starts[v + 1] as usize;
            for k in s..e {
                out.push((self.p.row_indices[k] as usize, self.p.values[k]));
            }
        } else if v < self.n_struct + self.n_slack {
            out.push((v - self.n_struct, -1.0));
        } else {
            // Signed so the artificial starts non-negative; see `seed`.
            let r = v - self.n_struct - self.n_slack;
            out.push((r, self.artificial_sign[r]));
        }
    }

    /// Set artificial values so the initial basis is feasible by construction.
    ///
    /// The residual of row `r` under the starting nonbasic values is
    /// `t_r = (Ax)_r − s_r`. The artificial takes `|t_r|` and its column takes
    /// `−sign(t_r)`, so the row balances and the artificial is non-negative,
    /// which is what makes the identity basis a legal starting point.
    fn seed(&mut self) {
        let mut residual = vec![0.0; self.m];
        let mut col = Vec::new();
        for v in 0..(self.n_struct + self.n_slack) {
            let x = self.value[v];
            if x == 0.0 {
                continue;
            }
            self.column_no_artificial(v, &mut col);
            for &(r, a) in &col {
                residual[r] += a * x;
            }
        }
        for r in 0..self.m {
            let t = residual[r];
            self.artificial_sign[r] = if t > 0.0 { -1.0 } else { 1.0 };
            self.value[self.n_struct + self.n_slack + r] = t.abs();
        }
    }

    /// As `column`, but never asks about artificials; used while seeding them.
    fn column_no_artificial(&self, v: usize, out: &mut Vec<(usize, f64)>) {
        out.clear();
        if v < self.n_struct {
            let s = self.p.col_starts[v] as usize;
            let e = self.p.col_starts[v + 1] as usize;
            for k in s..e {
                out.push((self.p.row_indices[k] as usize, self.p.values[k]));
            }
        } else {
            out.push((v - self.n_struct, -1.0));
        }
    }

    /// Recompute basic values from the nonbasic ones.
    fn recompute(&mut self) -> Result<(), BasisError> {
        let mut rhs = vec![0.0; self.m];
        let mut col = Vec::new();
        for v in 0..self.n {
            if self.is_basic[v] {
                continue;
            }
            let x = self.value[v];
            if x == 0.0 {
                continue;
            }
            self.column(v, &mut col);
            for &(r, a) in &col {
                rhs[r] -= a * x;
            }
        }
        let xb = self.inv.solve(&rhs)?;
        for (r, &v) in self.basis.iter().enumerate() {
            self.value[v] = xb[r];
        }
        Ok(())
    }

    fn duals(&self) -> Result<Vec<f64>, BasisError> {
        let cb: Vec<f64> = self.basis.iter().map(|&v| self.cost[v]).collect();
        self.inv.solve_transpose(&cb)
    }

    fn refactor(&mut self) -> Result<(), BasisError> {
        let mut cols = Vec::with_capacity(self.m);
        let mut buf = Vec::new();
        for i in 0..self.m {
            let v = self.basis[i];
            self.column(v, &mut buf);
            cols.push(buf.clone());
        }
        self.inv = Basis::from_columns(self.m, &cols)?;
        self.since_refactor = 0;
        Ok(())
    }
}

/// Solve a linear program.
pub fn solve(p: Problem<'_>, o: Options) -> Result<Solution, SolveError> {
    validate(&p)?;
    let mut t = Tab::new(p, o);
    t.seed();
    // The starting basis is diag(artificial_sign), not the identity: `seed`
    // chooses a sign per row so each artificial starts non-negative. Leaving
    // `inv` as the identity silently negates every row whose residual was
    // positive, which made the very first basis infeasible on any problem whose
    // residuals were not all of one sign. Small test problems happened to be,
    // which is why this survived until a real network was tried.
    t.refactor()?;
    t.recompute()?;

    let mut iters = 0;

    // Phase one: minimise the artificials.
    for v in 0..t.n {
        t.cost[v] = if t.is_artificial(v) { 1.0 } else { 0.0 };
    }
    let s1 = iterate(&mut t, &mut iters)?;
    if s1 == Status::IterationLimit {
        return Ok(report(&t, Status::IterationLimit, iters, p));
    }

    // Judge feasibility against a freshly factorised basis. The incremental
    // values carried through phase one have accumulated rounding error by this
    // point, and accepting them meant phase two could begin from a basis that
    // does not actually satisfy the bounds, which is not a state the ratio test
    // can recover from: it protects feasibility, it does not restore it.
    t.refactor()?;
    t.recompute()?;

    let residual: f64 = (0..t.m)
        .map(|r| t.value[t.n_struct + t.n_slack + r].abs())
        .sum();
    if residual > t.o.primal_tolerance * (1.0 + t.m as f64) {
        return Ok(report(&t, Status::Infeasible, iters, p));
    }

    // Phase two: pin the artificials shut and optimise the real objective.
    for r in 0..t.m {
        let v = t.n_struct + t.n_slack + r;
        t.upper[v] = 0.0;
        if !t.is_basic[v] {
            t.value[v] = 0.0;
            t.at[v] = At::Lower;
        }
    }
    for v in 0..t.n {
        t.cost[v] = if v < t.n_struct { p.col_cost[v] } else { 0.0 };
    }
    t.refactor()?;
    t.recompute()?;

    let s2 = iterate(&mut t, &mut iters)?;
    Ok(report(&t, s2, iters, p))
}

fn validate(p: &Problem<'_>) -> Result<(), SolveError> {
    if p.col_starts.len() != p.n_cols + 1 {
        return Err(SolveError::BadColumnStarts(p.col_starts.len()));
    }
    if p.col_lower.len() != p.n_cols
        || p.col_upper.len() != p.n_cols
        || p.col_cost.len() != p.n_cols
        || p.row_lower.len() != p.n_rows
        || p.row_upper.len() != p.n_rows
    {
        return Err(SolveError::BadBounds);
    }
    if let Some(&r) = p.row_indices.iter().max()
        && r as usize >= p.n_rows
    {
        return Err(SolveError::RowOutOfRange {
            row: r,
            n_rows: p.n_rows,
        });
    }
    Ok(())
}

/// The primal simplex loop, shared by both phases.
fn iterate(t: &mut Tab<'_>, iters: &mut usize) -> Result<Status, BasisError> {
    let mut col = Vec::new();

    loop {
        if *iters >= t.o.max_iterations {
            return Ok(Status::IterationLimit);
        }
        *iters += 1;

        if t.since_refactor >= t.o.refactor_every
            || (t.o.refactor_fill_ratio > 0.0
                && t.inv.updates_outweigh_factors(t.o.refactor_fill_ratio))
        {
            t.refactor()?;
            t.recompute()?;
        }

        let y = t.duals()?;

        // Pricing. A variable on its lower bound helps by rising when its
        // reduced cost is negative; one on its upper bound helps by falling
        // when it is positive. Dantzig's rule: take the largest violation.
        let mut entering = None;
        let mut best = t.o.dual_tolerance;
        for v in 0..t.n {
            if t.is_basic[v] {
                continue;
            }
            // A variable pinned shut cannot move at all.
            if t.lower[v] >= t.upper[v] {
                continue;
            }
            t.column(v, &mut col);
            let mut d = t.cost[v];
            for &(r, a) in &col {
                d -= y[r] * a;
            }
            let (gain, up) = match t.at[v] {
                At::Lower => (-d, true),
                At::Upper => (d, false),
                At::Free => {
                    if d < 0.0 {
                        (-d, true)
                    } else {
                        (d, false)
                    }
                }
            };
            if gain > best {
                best = gain;
                entering = Some((v, up));
            }
        }

        let Some((enter, up)) = entering else {
            // Optimality is only believed against a fresh factorisation. The
            // incremental updates that make the revised simplex fast also let
            // rounding error accumulate, and a drifted inverse can price every
            // column as non-improving while the recorded basic values no longer
            // satisfy the constraints at all. Rebuilding and re-pricing costs
            // one extra factorisation per solve and converts a silently wrong
            // answer into a correct one.
            if t.since_refactor > 0 {
                t.refactor()?;
                t.recompute()?;
                t.since_refactor = 0;
                continue;
            }
            // Even with a clean basis, a basic variable outside its own bounds
            // means this is not a solution. Phase one is responsible for
            // feasibility, so reaching here in phase two indicates the basis
            // has gone bad rather than that the problem is infeasible.
            let worst = (0..t.m)
                .map(|r| {
                    let v = t.basis[r];
                    let x = t.value[v];
                    (t.lower[v] - x).max(x - t.upper[v]).max(0.0)
                })
                .fold(0.0f64, f64::max);
            if worst > t.o.primal_tolerance * 1e3 {
                return Ok(Status::NumericalFailure);
            }
            return Ok(Status::Optimal);
        };

        // Ratio test.
        t.column(enter, &mut col);
        let mut rhs = vec![0.0; t.m];
        for &(r, a) in &col {
            rhs[r] = a;
        }
        let dir = t.inv.solve(&rhs)?;
        let sign = if up { 1.0 } else { -1.0 };

        // The entering variable may travel at most its own range.
        let mut step = t.upper[enter] - t.lower[enter];
        if !step.is_finite() {
            step = f64::INFINITY;
        }
        let mut leaving: Option<(usize, bool)> = None;

        for r in 0..t.m {
            let rate = -sign * dir[r];
            if rate.abs() < t.o.pivot_tolerance {
                continue;
            }
            let v = t.basis[r];
            let x = t.value[v];
            let limit = if rate > 0.0 {
                if t.upper[v].is_finite() {
                    (t.upper[v] - x) / rate
                } else {
                    f64::INFINITY
                }
            } else if t.lower[v].is_finite() {
                (t.lower[v] - x) / rate
            } else {
                f64::INFINITY
            };
            let limit = limit.max(0.0);
            if limit < step - 1e-12 {
                step = limit;
                leaving = Some((r, rate > 0.0));
            }
        }


        if !step.is_finite() {
            return Ok(Status::Unbounded);
        }

        let delta = sign * step;
        t.value[enter] += delta;
        for r in 0..t.m {
            let v = t.basis[r];
            t.value[v] -= dir[r] * delta;
        }


        match leaving {
            // The entering variable reached its own opposite bound; it simply
            // changes sides and the basis is untouched.
            None => {
                t.at[enter] = if up { At::Upper } else { At::Lower };
            }
            Some((row, to_upper)) => {
                if dir[row].abs() < t.o.pivot_tolerance {
                    t.refactor()?;
                    t.recompute()?;
                    continue;
                }
                let leave = t.basis[row];
                t.inv.update(row, &dir)?;
                t.since_refactor += 1;

                t.is_basic[leave] = false;
                t.at[leave] = if to_upper { At::Upper } else { At::Lower };
                t.value[leave] = if to_upper {
                    t.upper[leave]
                } else {
                    t.lower[leave]
                };
                t.basis[row] = enter;
                t.is_basic[enter] = true;
            }
        }
    }
}

fn report(t: &Tab<'_>, status: Status, iterations: usize, p: Problem<'_>) -> Solution {
    let col_value = t.value[..t.n_struct].to_vec();
    let objective = col_value
        .iter()
        .zip(p.col_cost)
        .map(|(x, c)| x * c)
        .sum::<f64>();

    // Duals come off the final basis with the real costs in place. A failure
    // here means the basis is unusable, in which case zeros are honest.
    let row_dual = if matches!(status, Status::Optimal | Status::IterationLimit) {
        t.duals().unwrap_or_else(|_| vec![0.0; t.m])
    } else {
        vec![0.0; t.m]
    };

    Solution {
        status,
        objective,
        col_value,
        row_dual,
        iterations,
    }
}

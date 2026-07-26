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

pub use mip::{Branching, MipOptions, MipSolution, solve_mip};

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
    /// How many of those went on finding a feasible point rather than a good
    /// one.
    ///
    /// Worth reporting rather than hiding, because it is the number that says
    /// where the time goes. On the models here it is consistently about three
    /// quarters of the total: 33,670 of 45,205 iterations at 20,736 rows. Phase
    /// one exists because the starting basis is every artificial variable,
    /// which is feasible for a problem nobody asked about and a long way from
    /// one for the problem in hand.
    pub phase_one_iterations: usize,
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
    ///
    /// Two hundred and fifty-six, chosen by measurement rather than by
    /// tradition. Refactorising is expensive and letting the updates pile up is
    /// expensive, and the total is a shallow U in between: at 9,216 rows the
    /// curve reads 3.55 s at 32, 3.30 at 64, 3.16 at 256, 3.20 at 512 and 5.41
    /// if it never refactorises at all.
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
    /// How many columns to price before settling for the best seen.
    ///
    /// Defaults to everything, which is Dantzig's rule, because a window
    /// measured as no better here. Across 500, 2,000, 10,000, 50,000 and the
    /// full column count on a 9,216-row model the total ran 3.33, 3.31, 3.24,
    /// 3.32 and 3.30 seconds: indistinguishable. A cheaper scan buys a worse
    /// entering variable and the two cancel.
    ///
    /// That is a fact about the shape of these models rather than about partial
    /// pricing, and it is worth stating which. An energy system model has a few
    /// times as many columns as rows, so a scan is a small multiple of a solve.
    /// Partial pricing earns its keep where columns vastly outnumber rows —
    /// column generation, cutting stock, crew scheduling — and the knob is here
    /// for a caller whose problem looks like that.
    pub price_window: usize,
    /// Start each bounded variable on whichever bound the objective prefers.
    ///
    /// A crash in the loosest sense: it does not change the starting basis, so
    /// it cannot make one singular, and it costs one comparison per column.
    pub cost_crash: bool,
    /// Replace artificials in the starting basis with structural columns.
    ///
    /// Phase one is about three quarters of a solve, and it exists because the
    /// starting basis is every artificial variable. A triangular selection of
    /// structural columns starts much nearer feasible. Verified and abandoned
    /// wholesale if it would put a basic variable out of bounds, since phase
    /// one would not notice.
    pub structural_crash: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_iterations: 200_000,
            dual_tolerance: 1e-9,
            primal_tolerance: 1e-8,
            pivot_tolerance: 1e-9,
            refactor_every: 256,
            refactor_fill_ratio: 0.5,
            price_window: usize::MAX,
            cost_crash: true,
            structural_crash: true,
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
        //
        // Where it has both, the cost decides. A variable the objective wants
        // large starts large: the simplex would move it there anyway, and each
        // such move is an iteration, so starting it in the right place is an
        // iteration not spent. That is the cheapest crash there is — no
        // factorisation, no search, one comparison per column — and unlike a
        // triangular crash it cannot make the starting basis singular, because
        // it does not change which variables are basic at all.
        for v in 0..(n_struct + m) {
            at[v] = if lower[v].is_finite() && upper[v].is_finite() {
                if o.cost_crash && cost[v] < 0.0 {
                    At::Upper
                } else {
                    At::Lower
                }
            } else if lower[v].is_finite() {
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

    /// Replace artificials in the starting basis with structural columns, where
    /// that can be done safely.
    ///
    /// The starting basis is every artificial variable. That is feasible for a
    /// problem nobody asked about and a long way from one for the problem in
    /// hand, and getting from there to a feasible point is about three quarters
    /// of every solve: 33,670 of 45,205 iterations at 20,736 rows.
    ///
    /// A better start is available cheaply. A generator's column is a singleton
    /// in its bus's balance row, so putting it in the basis satisfies that row
    /// outright. Choosing columns that are singletons in the *remaining* rows,
    /// repeatedly, builds a basis that is triangular by construction and so
    /// cannot be singular — the same singleton cascade the factorisation's
    /// column ordering already relies on.
    ///
    /// # Why it verifies and reverts
    ///
    /// Phase one here penalises artificials and nothing else, on the assumption
    /// that every other basic variable sits within its bounds. A crashed
    /// structural landing outside its bounds would be invisible to it, and
    /// phase two would then start from a point that is not feasible at all —
    /// which is not a slower answer but a wrong one.
    ///
    /// So the crash is checked rather than trusted. Triangularity makes that
    /// cheap: the basic values follow by forward substitution in pivot order,
    /// with no factorisation needed. If any lands out of bounds the whole crash
    /// is abandoned and the all-artificial basis stands. Taking the good start
    /// only when it is provably safe is worth more than taking it usually.
    fn crash(&mut self) -> bool {
        let m = self.m;
        let mut assigned: Vec<Option<usize>> = vec![None; m];
        let mut row_done = vec![false; m];
        let mut used = vec![false; self.n_struct + self.n_slack];
        // Rows each column still touches, and the count, so a singleton is
        // recognised without rescanning.
        let mut col_rows: Vec<Vec<(usize, f64)>> = Vec::with_capacity(self.n_struct + self.n_slack);
        let mut buf = Vec::new();
        for v in 0..(self.n_struct + self.n_slack) {
            self.column_no_artificial(v, &mut buf);
            col_rows.push(buf.clone());
        }

        // Order of assignment, which is the order forward substitution follows.
        let mut order: Vec<usize> = Vec::with_capacity(m);
        let mut progress = true;
        while progress {
            progress = false;
            for v in 0..col_rows.len() {
                if used[v] || self.lower[v] >= self.upper[v] {
                    continue;
                }
                // A singleton among the rows not yet spoken for. Its other
                // entries then lie in rows already assigned, which is what
                // makes the whole selection triangular and so nonsingular.
                let mut live = col_rows[v].iter().filter(|&&(r, a)| !row_done[r] && a != 0.0);
                let Some(&(row, a)) = live.next() else {
                    continue;
                };
                if live.next().is_some() {
                    continue;
                }
                // Nonsingular is not the same as usable. A triangular basis of
                // small pivots is invertible and hopelessly conditioned, and the
                // failure arrives much later as a solve that will not converge
                // rather than as a factorisation that refuses.
                //
                // So the pivot is judged against the largest entry in its own
                // column rather than against an absolute floor, which is the
                // test crash procedures have used since Bixby. An absolute one
                // passed IEEE 300 and failed PEGASE 1354, whose coefficients
                // span a far wider range.
                let biggest = col_rows[v]
                    .iter()
                    .map(|&(_, x)| x.abs())
                    .fold(0.0f64, f64::max);
                if a.abs() < CRASH_PIVOT_THRESHOLD * biggest || a.abs() < 1e-8 {
                    continue;
                }
                used[v] = true;
                row_done[row] = true;
                assigned[row] = Some(v);
                order.push(row);
                progress = true;
            }
        }
        if order.is_empty() {
            return false;
        }

        // Forward substitution in pivot order. Every column assigned later
        // touches no row assigned earlier, by construction, so each value
        // follows from the rows already settled.
        let mut activity = vec![0.0; m];
        for v in 0..(self.n_struct + self.n_slack) {
            if used[v] || self.value[v] == 0.0 {
                continue;
            }
            for &(r, a) in &col_rows[v] {
                activity[r] += a * self.value[v];
            }
        }
        // Row by row, in *reverse* pivot order, which is the direction the
        // structure actually demands.
        //
        // A column is chosen when it is a singleton among the rows not yet
        // spoken for, so its other entries lie in rows assigned *earlier*.
        // Ordered by assignment, that puts every off-pivot entry above the
        // diagonal: the system is upper triangular, and upper triangular
        // systems are solved from the bottom up. Going forwards instead settles
        // a row and then keeps adding to it, which leaves every early row
        // unbalanced — and the resulting basis is not singular, so nothing
        // complains until the factorisation fails much later. IEEE 300 caught
        // it; the smaller cases did not.
        //
        // A column whose value would fall outside
        // its bounds is not a reason to abandon the whole crash: that row keeps
        // its artificial, the column stays where it was, and the rest of the
        // selection still stands. The basis remains triangular either way,
        // since an artificial is a unit vector in its own row.
        //
        // All-or-nothing was tried first and threw away a complete covering of
        // every row because one column out of nine thousand wanted 600 against
        // a bound of 400.
        let mut trial = vec![0.0; self.n_struct + self.n_slack];
        let mut accepted = 0usize;
        for &row in order.iter().rev() {
            let v = assigned[row].expect("assigned");
            let a = col_rows[v]
                .iter()
                .find(|&&(r, _)| r == row)
                .map(|&(_, a)| a)
                .expect("the pivot entry exists");
            // The row to satisfy is `A x − s + artificial = 0`, and `activity`
            // already carries the slack's own term, since slacks sit in the
            // same column range as the structurals and are summed with them.
            // So the target is zero: the column has to cancel what is there,
            // not reach the row's bound.
            //
            // Taking the slack's bound as the target instead counts it twice,
            // which produces values that are wrong but plausible and a basis
            // that factors perfectly well. It shows up as a solve that will not
            // converge, several thousand iterations later.
            let x = -activity[row] / a;
            let usable =
                x.is_finite() && x >= self.lower[v] - 1e-9 && x <= self.upper[v] + 1e-9;
            if usable {
                trial[v] = x;
                accepted += 1;
            } else {
                // Leave this row to its artificial, and put the column back
                // where it was so its contribution is still accounted for.
                assigned[row] = None;
                used[v] = false;
            }
            let contributed = if usable { x } else { self.value[v] };
            if contributed != 0.0 {
                for &(r, coeff) in &col_rows[v] {
                    if r != row || !usable {
                        activity[r] += coeff * contributed;
                    }
                }
            }
        }
        if accepted == 0 {
            return false;
        }

        // Accepted. Install it, leaving artificials basic on the rows the crash
        // could not reach.
        let mut covered = vec![false; m];
        for &row in &order {
            let Some(v) = assigned[row] else { continue };
            let artificial = self.n_struct + self.n_slack + row;
            self.basis[row] = v;
            self.is_basic[v] = true;
            self.is_basic[artificial] = false;
            self.value[v] = trial[v];
            self.at[artificial] = At::Lower;
            covered[row] = true;
        }

        // And re-derive the artificials against the configuration the crash
        // leaves behind, which is the step whose absence broke this.
        //
        // `seed` picks each artificial's sign so that it starts non-negative
        // given the residual of the all-nonbasic configuration. The crash moves
        // structural columns into the basis at new values, so those residuals
        // change — and an artificial left basic on an uncovered row may now
        // need a negative value, which its own bounds forbid and which phase
        // one, penalising artificials rather than repairing them, cannot escape
        // from. IEEE 300 found that; the smaller cases did not.
        self.reseed(&covered);
        true
    }

    /// Recompute artificial signs and values for the rows still relying on
    /// them.
    ///
    /// The same arithmetic as [`Tab::seed`], over whatever configuration
    /// currently holds, and skipping the rows a crash has already balanced —
    /// their artificials are nonbasic at zero and must stay there.
    fn reseed(&mut self, covered: &[bool]) {
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
            let artificial = self.n_struct + self.n_slack + r;
            if covered[r] {
                self.artificial_sign[r] = 1.0;
                self.value[artificial] = 0.0;
            } else {
                let t = residual[r];
                self.artificial_sign[r] = if t > 0.0 { -1.0 } else { 1.0 };
                self.value[artificial] = t.abs();
            }
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

/// How large a crash pivot must be against the biggest entry in its column.
///
/// A tenth, which is the usual figure. Higher rejects more columns and leaves
/// more artificials; lower admits pivots that make the basis unusable.
const CRASH_PIVOT_THRESHOLD: f64 = 0.1;

/// Solve a linear program.
pub fn solve(p: Problem<'_>, o: Options) -> Result<Solution, SolveError> {
    validate(&p)?;
    let mut t = Tab::new(p, o);
    t.seed();
    if o.structural_crash {
        t.crash();
    }
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
    let phase_one_iterations = iters;
    if s1 == Status::IterationLimit {
        return Ok(report(&t, Status::IterationLimit, iters, iters, p));
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
        return Ok(report(&t, Status::Infeasible, iters, iters, p));
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
    Ok(report(&t, s2, iters, phase_one_iterations, p))
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

    // Where the last partial scan stopped, so successive iterations sweep the
    // columns rather than re-examining the same window.
    let mut price_cursor = 0usize;
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
        // when it is positive.
        //
        // Dantzig's rule takes the largest violation, which means pricing every
        // column on every iteration — and pricing a column means materialising
        // it and taking a dot product, so the scan costs the whole matrix each
        // time. On a model with millions of columns that is most of the solve.
        //
        // Partial pricing scans a rotating window instead and takes the best
        // within it. A worse choice of entering variable costs iterations; a
        // cheaper scan saves time on every one, and on these problems the second
        // wins comfortably. Correctness is untouched, because optimality is
        // still only declared after a full scan finds nothing: the window is a
        // shortcut to a good candidate, never to a conclusion.
        let scan = |t: &Tab<'_>, from: usize, count: usize,
                        col: &mut Vec<(usize, f64)>| {
            let mut entering: Option<(usize, bool)> = None;
            let mut best = t.o.dual_tolerance;
            for k in 0..count {
                let v = (from + k) % t.n;
                if t.is_basic[v] || t.lower[v] >= t.upper[v] {
                    continue;
                }
                t.column(v, col);
                let mut d = t.cost[v];
                for &(r, a) in col.iter() {
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
            entering
        };

        let window = t.o.price_window.min(t.n).max(1);
        let mut entering = if window < t.n {
            scan(t, price_cursor, window, &mut col)
        } else {
            None
        };
        if entering.is_some() {
            price_cursor = (price_cursor + window) % t.n;
        } else {
            // Nothing in the window, so look at everything. This is also the
            // only path on which optimality can be concluded.
            entering = scan(t, 0, t.n, &mut col);
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

fn report(t: &Tab<'_>, status: Status, iterations: usize,
    phase_one_iterations: usize, p: Problem<'_>) -> Solution {
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
        phase_one_iterations,
    }
}

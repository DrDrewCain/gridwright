//! Rows added to the relaxation to tighten it, without removing any integer
//! point.
//!
//! # What a cut is for
//!
//! Branching splits a node's *bounds*. That is cheap — a node is a pair of
//! vectors and a re-solve — but it is also the only thing the search does, so
//! every fractional vertex has to be resolved by cutting the box in two and
//! exploring both halves. A cut does something branching cannot: it removes the
//! fractional vertex outright, from the whole subtree at once, by adding an
//! inequality that every integer point satisfies and the current relaxation
//! optimum does not.
//!
//! # Why only at the root
//!
//! A cut is a row, and the search's whole economy rests on a node being bounds
//! alone. Local cuts — valid only inside one subtree — would have to be carried
//! down every branch and dropped on the way back up, and a local cut that
//! escapes its subtree removes points that are feasible elsewhere. The answer
//! still looks like an answer, which is the worst way for a solver to be wrong.
//!
//! So everything here is generated at the **root**, where the bounds are the
//! caller's own and no branching has happened yet, and is added to the problem
//! once. A cut derived from the original rows, the original bounds and the
//! original integrality is valid everywhere in the tree by construction, and
//! there is nothing to scope, carry or drop.
//!
//! # Why a cut here cannot remove an integer point
//!
//! Both families below are derived only from facts that hold for every point of
//! the original mixed-integer program:
//!
//! - **Gomory mixed-integer cuts** start from a row of the optimal tableau,
//!   which is a linear combination of the equality system `Ax − s = 0` and so
//!   holds at every feasible point, fractional or not. Every nonbasic variable
//!   is rewritten as its distance from a bound it actually has, so the
//!   substituted variables are non-negative for every feasible point rather
//!   than only for the current one. The Gomory function is then applied only to
//!   variables whose integrality is asserted by the caller *and* whose resting
//!   bound is a whole number, so that the shifted variable is itself a
//!   non-negative integer; anything else is treated as continuous, which is the
//!   weaker and always-valid case. The inequality that comes out is the
//!   standard mixed-integer rounding argument on `x = β − Σ aⱼ tⱼ` with `x`
//!   integer and `tⱼ ≥ 0`, and it is satisfied by every integer point of that
//!   relation.
//! - **Cover cuts** are combinatorial. A row `Σ wⱼ zⱼ ≤ B` over binaries with
//!   `wⱼ > 0`, together with a set `C` whose weights already exceed `B`, cannot
//!   have all of `C` set to one; so `Σ_{j∈C} zⱼ ≤ |C| − 1`, and extending `C`
//!   by any variable at least as heavy as its heaviest member keeps that true.
//!   No arithmetic beyond a comparison of sums enters, and that comparison is
//!   made with a margin so that a tie cannot be decided by rounding.
//!
//! Neither derivation refers to the objective, to the incumbent, or to the
//! branching bounds. That is the whole safety argument, and it is why the cuts
//! are global.
//!
//! # Why the guards exist
//!
//! The argument above is exact. The arithmetic is not. A tableau row is
//! computed through a factorisation that has been updated some hundreds of
//! times, and a Gomory cut divides by the fractional part of a basic value —
//! so a row whose basic value sits a hair from a whole number produces
//! coefficients scaled by a hair's reciprocal, and those coefficients are
//! floating point estimates of numbers the derivation assumed were exact. A cut
//! built that way can be *invalid*, cutting off the optimum while still looking
//! like a cut. The thresholds in this file are what stands between the
//! derivation and that, and each is named where it is defined.

use crate::{At, Options, Problem, Solution, SolveError, Status, Tab, solve_keeping_basis};

/// Which families of cut to separate at the root.
///
/// # What the measurement says
///
/// Measured on two families of model, because cuts help very unevenly by
/// structure and one family would have concluded whatever that family happened
/// to say. On unit commitment, Gomory cuts are worth three to nineteen times
/// the wall-clock and cover cuts find nothing at all, there being no knapsack
/// row in the model to find one on; on a multidimensional knapsack the two
/// swap places and Gomory cuts cost up to three times. [`Cuts::Gomory`] is the
/// default on the first of those, since it is the shape this engine builds.
/// [`crate::MipOptions::cuts`] carries both tables in full and `tests/cuts.rs`
/// prints them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cuts {
    /// Nothing is added beyond the branching bounds.
    Off,
    /// Gomory mixed-integer cuts from the root tableau.
    Gomory,
    /// Cover cuts from the root's knapsack rows.
    Cover,
    /// Both families, at the root.
    Both,
}

impl Cuts {
    /// Whether Gomory cuts are wanted.
    fn wants_gomory(self) -> bool {
        matches!(self, Cuts::Gomory | Cuts::Both)
    }

    /// Whether cover cuts are wanted.
    fn wants_cover(self) -> bool {
        matches!(self, Cuts::Cover | Cuts::Both)
    }

    /// Whether anything is wanted at all.
    pub(crate) fn any(self) -> bool {
        !matches!(self, Cuts::Off)
    }
}

// ---------------------------------------------------------------------------
// Thresholds
// ---------------------------------------------------------------------------

/// How far a basic value must sit from a whole number before a Gomory cut is
/// taken from its row.
///
/// A hundredth. The Gomory coefficients carry `1/f₀` and `1/(1 − f₀)`, so a
/// basic value a millionth away from an integer produces a cut whose
/// coefficients are a million times the tableau entries that generated them —
/// and those entries are floating point estimates with perhaps ten significant
/// digits left in them after a few hundred basis updates. Multiplying an error
/// of `1e-10` by `1e6` gives a coefficient wrong in its fourth digit, which is
/// no longer a cut but a guess about where a face lies. A hundredth caps that
/// amplification at a hundred, and the rows it refuses are the ones whose cut
/// would have been almost tangent to the current vertex anyway.
const MIN_FRACTION: f64 = 0.01;

/// A tableau entry below this is treated as absent.
///
/// The tableau row comes out of a transposed solve against an updated
/// factorisation, so exact structural zeros arrive as values of order `1e-16`
/// and carrying them turns a sparse cut into a dense one. This is well above
/// the noise and well below anything the derivation cares about.
const TABLEAU_ZERO: f64 = 1e-11;

/// A cut coefficient this much smaller than the cut's largest is dropped.
///
/// Dropping a term is not free: it is only sound if the constraint is weakened
/// to cover every value the dropped variable could have taken, which is done
/// below and needs a finite bound on that variable. A billionth of the largest
/// coefficient is below the precision the tableau arithmetic delivered in the
/// first place, so the term carries no information — only fill.
const COEF_NEGLIGIBLE: f64 = 1e-9;

/// The widest spread of coefficient magnitudes a cut may have.
///
/// A million, which is the figure production solvers use. Dynamism is the ratio
/// of the largest coefficient to the smallest, and it is the single best
/// predictor of a cut that will misbehave: the row is added to a basis that
/// then has to be factorised, and a row spanning more orders of magnitude than
/// double precision has digits to spare will lose the small end of itself to
/// the large end. The cut is then not the cut that was derived. Rejecting is
/// always safe — a cut not added costs nodes, a bad cut costs correctness.
const MAX_DYNAMISM: f64 = 1e6;

/// How far the current relaxation optimum must lie on the wrong side of a cut
/// before the cut is worth adding.
///
/// Measured as violation over the Euclidean norm of the coefficients, which is
/// the distance from the point to the cut's hyperplane and so is invariant to
/// scaling the row — the same cut written twice as large must not look twice as
/// good. A ten-thousandth of a unit of distance is comfortably above the
/// solver's own primal tolerance of `1e-8`, so a cut that passes has genuinely
/// moved the vertex rather than jittered it, and one that fails would have cost
/// a row in every node solve for nothing.
const MIN_EFFICACY: f64 = 1e-4;

/// The most nonzeros a cut may have, as a fraction of the columns.
///
/// Gomory cuts fill in: the tableau row is a combination of every row the basis
/// touches, so a cut from a well-connected model can name most of the problem.
/// A dense row is a dense column in every basis it enters, and this solver
/// factorises from scratch on every node, so a handful of them can cost more
/// than the nodes they save. Half is generous — most useful cuts here come out
/// far sparser — and it exists to refuse the outliers.
const MAX_DENSITY: f64 = 0.5;

/// A cut naming at most this many columns is never refused for density.
///
/// Without a floor the fraction above means something absurd on a small model:
/// half of two columns is one, and a cut over both columns of a two-column
/// problem would be thrown out for being dense. Thirty-two nonzeros is not a
/// dense row by any measure, whatever the problem's width.
const DENSE_FLOOR: usize = 32;

/// How far a cover's weight must exceed its capacity before it counts as a
/// cover.
///
/// The whole validity of a cover cut rests on `Σ_{j∈C} wⱼ > B` being true
/// rather than nearly true, and a set whose weight equals its capacity to the
/// last bit is not a cover at all — its cut would remove a feasible point. The
/// margin is relative to the capacity so that it means the same thing in a
/// model priced in megawatts and one priced in watts.
const COVER_MARGIN: f64 = 1e-7;

/// How many rounds of cutting the root gets.
///
/// Each round costs a full relaxation solve, and this solver has no warm start,
/// so a round costs exactly what a node costs. Four is where the returns stop
/// on the models measured: the first round does most of the tightening, the
/// second most of the rest, and by the fifth the cuts being found are the ones
/// the efficacy guard is about to reject anyway.
const MAX_ROUNDS: usize = 4;

/// How many cuts a round may keep, given the row count.
///
/// Every cut is a row that every one of the thousands of node solves below it
/// then pays for, in a solver that refactorises from scratch each time. So the
/// budget is a fraction of the problem rather than a constant: a twentieth of
/// the rows per round, over four rounds, is at most a fifth more rows than the
/// model started with. The ceiling stops a very large model from spending its
/// whole root budget separating.
///
/// The floor was four, and four is what a fraction is for refusing. On a
/// five-row knapsack that budget added sixteen rows — the model *quadrupled*,
/// every node solve with it, and the measurement showed the search taking three
/// to four times as long at an unchanged node count. Nothing was wrong with the
/// cuts; there were simply far too many of them for the model they were being
/// added to. A floor of one still lets a small model be cut, which is the thing
/// the floor exists for, without letting the row count multiply.
fn round_budget(n_rows: usize) -> usize {
    (n_rows / 20).clamp(1, 32)
}

// ---------------------------------------------------------------------------
// A cut, and the problem with cuts in it
// ---------------------------------------------------------------------------

/// A cut, as a ranged row over the structural columns.
///
/// The same shape as every other row the solver takes, which is what makes
/// adding one to the problem an edit to three arrays rather than a change to
/// the solver.
struct Cut {
    /// Column index and coefficient. Neither order nor uniqueness is assumed
    /// by the solver, but these are built one column at a time and so come out
    /// in increasing column order.
    coef: Vec<(usize, f64)>,
    lower: f64,
    upper: f64,
}

/// A cut and how far the point that produced it lies on the wrong side of it.
struct Candidate {
    cut: Cut,
    efficacy: f64,
}

/// What separation produced, accumulated across rounds.
///
/// Two counts rather than one because they answer different questions. The
/// distance from `generated` to `survived` is how much of its own arithmetic
/// this file declined to trust; the distance from `survived` to what is
/// actually added is how much sound material the round budget turned away.
#[derive(Default, Clone, Copy)]
struct Tally {
    generated: usize,
    survived: usize,
}

/// The original problem with the root's cuts appended as extra rows.
///
/// Owned, because a `Problem` is a bundle of borrowed slices and the caller's
/// arrays are not ours to grow. The column count never changes, so the node
/// bounds the search carries are unaffected and a node stays a pair of vectors.
pub(crate) struct Tightened {
    starts: Vec<u32>,
    rows: Vec<u32>,
    vals: Vec<f64>,
    row_lower: Vec<f64>,
    row_upper: Vec<f64>,
    n_rows: usize,
    /// The relaxation of the tightened root, which is the search's first bound.
    pub(crate) root: Solution,
    /// Cut candidates attempted: tableau rows with a fractional integer basic
    /// variable, and covers found on knapsack rows.
    pub(crate) generated: usize,
    /// Those that survived the validity and stability guards.
    pub(crate) survived: usize,
    /// Those that were then within the round's budget and were added.
    ///
    /// Separate from `survived` because the two say different things: the gap
    /// between `generated` and `survived` is how much of the arithmetic this
    /// file refused to trust, and the gap between `survived` and `kept` is how
    /// much good material was left on the floor because the model could not
    /// afford the rows.
    pub(crate) kept: usize,
    /// Relaxation solves spent on cutting, which are nodes by any other name.
    pub(crate) solves: usize,
}

impl Tightened {
    /// The tightened problem, over the caller's columns.
    pub(crate) fn problem<'a>(&'a self, p: Problem<'a>) -> Problem<'a> {
        Problem {
            n_rows: self.n_rows,
            col_starts: &self.starts,
            row_indices: &self.rows,
            values: &self.vals,
            row_lower: &self.row_lower,
            row_upper: &self.row_upper,
            ..p
        }
    }
}

/// The matrix and row bounds, owned and growable.
struct Rows {
    n_cols: usize,
    n_rows: usize,
    starts: Vec<u32>,
    rows: Vec<u32>,
    vals: Vec<f64>,
    row_lower: Vec<f64>,
    row_upper: Vec<f64>,
}

impl Rows {
    fn new(p: Problem<'_>) -> Self {
        Self {
            n_cols: p.n_cols,
            n_rows: p.n_rows,
            starts: p.col_starts.to_vec(),
            rows: p.row_indices.to_vec(),
            vals: p.values.to_vec(),
            row_lower: p.row_lower.to_vec(),
            row_upper: p.row_upper.to_vec(),
        }
    }

    fn problem<'a>(&'a self, p: Problem<'a>) -> Problem<'a> {
        Problem {
            n_rows: self.n_rows,
            col_starts: &self.starts,
            row_indices: &self.rows,
            values: &self.vals,
            row_lower: &self.row_lower,
            row_upper: &self.row_upper,
            ..p
        }
    }

    /// Fold new rows into the compressed sparse column arrays.
    ///
    /// Compressed sparse column groups by column, so appending rows means
    /// splicing an entry into each column the new rows touch rather than
    /// pushing onto the end. Gathering per column and reflattening is the
    /// honest way to do that, and it happens at most [`MAX_ROUNDS`] times per
    /// solve.
    fn append(&mut self, cuts: &[Cut]) {
        let mut per_col: Vec<Vec<(u32, f64)>> = vec![Vec::new(); self.n_cols];
        for j in 0..self.n_cols {
            let s = self.starts[j] as usize;
            let e = self.starts[j + 1] as usize;
            for k in s..e {
                per_col[j].push((self.rows[k], self.vals[k]));
            }
        }
        for (i, cut) in cuts.iter().enumerate() {
            let row = (self.n_rows + i) as u32;
            for &(j, a) in &cut.coef {
                if j < self.n_cols {
                    per_col[j].push((row, a));
                }
            }
            self.row_lower.push(cut.lower);
            self.row_upper.push(cut.upper);
        }
        self.n_rows += cuts.len();

        self.rows.clear();
        self.vals.clear();
        self.starts.clear();
        self.starts.push(0);
        for col in &per_col {
            for &(r, a) in col {
                self.rows.push(r);
                self.vals.push(a);
            }
            self.starts.push(self.rows.len() as u32);
        }
    }
}

// ---------------------------------------------------------------------------
// The root cutting loop
// ---------------------------------------------------------------------------

/// Cut the root until it stops paying, and return the tightened problem.
///
/// `first` is the tableau of the root relaxation the caller has already solved,
/// so the first round of separation costs nothing beyond the separation itself.
///
/// `budget` is how many relaxation solves the caller's node budget still allows.
/// A cutting round is a relaxation solve and is charged as a node, so a caller
/// who asked for one node gets no cutting — anything else would spend a budget
/// it was told not to spend, and would quietly turn a search that could not
/// have proved anything into one that had.
///
/// `None` means no cuts survived, or something about the tightened problem
/// stopped making sense — in which case the caller searches the problem it
/// started with, which is always a correct thing to do.
pub(crate) fn tighten_root<'a>(
    p: Problem<'a>,
    first: &Tab<'a>,
    integer: &[bool],
    families: Cuts,
    o: Options,
    integrality_tolerance: f64,
    budget: usize,
) -> Result<Option<Tightened>, SolveError> {
    let mut rows = Rows::new(p);
    let mut tally = Tally::default();
    let mut kept = 0usize;
    let mut solves = 0usize;
    let mut answer: Option<Solution> = None;

    let mut pending = separate(first, integer, families, integrality_tolerance, &mut tally);
    let rounds = MAX_ROUNDS.min(budget);
    let mut round = 0usize;
    while !pending.is_empty() && round < rounds {
        kept += pending.len();
        rows.append(&pending);
        round += 1;

        // The tableau below borrows `rows`, so it has to be gone before the
        // next round appends to it. Keeping it inside the loop body is what
        // makes that true rather than a comment claiming it.
        let (solution, tab) = solve_keeping_basis(rows.problem(p), o)?;
        solves += 1;
        if solution.status != Status::Optimal {
            // A valid cut cannot make a feasible relaxation infeasible, so
            // reaching here means the arithmetic has gone somewhere the
            // derivation did not. Throwing away every cut and searching the
            // original problem is slower and cannot be wrong.
            return Ok(None);
        }
        answer = Some(solution);
        pending = if round < rounds {
            separate(&tab, integer, families, integrality_tolerance, &mut tally)
        } else {
            Vec::new()
        };
    }

    let Some(root) = answer else {
        return Ok(None);
    };
    Ok(Some(Tightened {
        starts: rows.starts,
        rows: rows.rows,
        vals: rows.vals,
        row_lower: rows.row_lower,
        row_upper: rows.row_upper,
        n_rows: rows.n_rows,
        root,
        generated: tally.generated,
        survived: tally.survived,
        kept,
        solves,
    }))
}

/// Separate one round of cuts from a solved relaxation.
///
/// The candidates are ranked by efficacy and the best of them kept, because a
/// round that adds everything it found adds mostly rows that move the vertex by
/// less than the rounding in the solve that found them. Ties fall to generation
/// order, which is row order, so the same model gives the same cuts on every
/// run — the search's determinism depends on that as much as the branching
/// rule's tiebreak does.
fn separate(
    t: &Tab<'_>,
    integer: &[bool],
    families: Cuts,
    integrality_tolerance: f64,
    tally: &mut Tally,
) -> Vec<Cut> {
    let by_row = ByRow::new(&t.p);
    let mut candidates = Vec::new();
    if families.wants_gomory() {
        gomory(
            t,
            &by_row,
            integer,
            integrality_tolerance,
            &mut candidates,
            &mut tally.generated,
        );
    }
    if families.wants_cover() {
        cover(
            &t.p,
            &by_row,
            integer,
            &t.value[..t.n_struct],
            &mut candidates,
            &mut tally.generated,
        );
    }
    tally.survived += candidates.len();

    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by(|&a, &b| {
        candidates[b]
            .efficacy
            .total_cmp(&candidates[a].efficacy)
            .then(a.cmp(&b))
    });
    order.truncate(round_budget(t.p.n_rows));
    order.sort_unstable();

    let mut kept = Vec::with_capacity(order.len());
    for (i, candidate) in candidates.into_iter().enumerate() {
        if order.binary_search(&i).is_ok() {
            kept.push(candidate.cut);
        }
    }
    kept
}

// ---------------------------------------------------------------------------
// The matrix, by row
// ---------------------------------------------------------------------------

/// The constraint matrix by row, which compressed sparse column does not give.
///
/// Needed twice: to substitute a slack out of a cut, since a slack is the row's
/// activity and the cut has to be written over the columns; and to recognise a
/// knapsack row, which is a statement about a row's entries.
struct ByRow {
    starts: Vec<u32>,
    cols: Vec<u32>,
    vals: Vec<f64>,
}

impl ByRow {
    fn new(p: &Problem<'_>) -> Self {
        let mut starts = vec![0u32; p.n_rows + 1];
        for &r in p.row_indices {
            let r = r as usize;
            if r < p.n_rows {
                starts[r + 1] += 1;
            }
        }
        for i in 0..p.n_rows {
            starts[i + 1] += starts[i];
        }
        let mut fill = starts.clone();
        let mut cols = vec![0u32; p.row_indices.len()];
        let mut vals = vec![0.0; p.row_indices.len()];
        for j in 0..p.n_cols {
            let s = p.col_starts[j] as usize;
            let e = p.col_starts[j + 1] as usize;
            for k in s..e {
                let r = p.row_indices[k] as usize;
                if r >= p.n_rows {
                    continue;
                }
                let at = fill[r] as usize;
                cols[at] = j as u32;
                vals[at] = p.values[k];
                fill[r] += 1;
            }
        }
        Self { starts, cols, vals }
    }

    fn row(&self, i: usize) -> impl Iterator<Item = (usize, f64)> + '_ {
        let s = self.starts[i] as usize;
        let e = self.starts[i + 1] as usize;
        (s..e).map(move |k| (self.cols[k] as usize, self.vals[k]))
    }
}

// ---------------------------------------------------------------------------
// Gomory mixed-integer cuts
// ---------------------------------------------------------------------------

/// Whether a value is exactly a whole number.
///
/// Exact rather than approximate on purpose. This decides whether a shifted
/// variable is an *integer* variable in the derivation, and an approximate
/// answer to that question is an approximate cut: a bound of 0.5 that passed a
/// tolerance would have the Gomory function applied to something that is not an
/// integer, and the result would not be valid. Failing the test costs strength
/// and never correctness, since the fallback is to treat the variable as
/// continuous.
#[inline]
fn whole(v: f64) -> bool {
    v.is_finite() && v == v.round()
}

/// Rows whose activity is a whole number at every integer point.
///
/// Worth knowing because the slack of such a row is then an integer variable in
/// the derivation, and the integer branch of the Gomory function is markedly
/// stronger than the continuous one. A row qualifies only if every column it
/// touches is an integer column with a whole coefficient, which is exactly the
/// shape a commitment or covering row has and exactly what a row containing a
/// continuous dispatch variable does not.
fn integral_rows(p: &Problem<'_>, by_row: &ByRow, integer: &[bool]) -> Vec<bool> {
    (0..p.n_rows)
        .map(|i| {
            by_row
                .row(i)
                .all(|(j, a)| whole(a) && integer.get(j).copied().unwrap_or(false))
        })
        .collect()
}

/// The Gomory mixed-integer function.
///
/// For the relation `x = β − Σ aⱼ tⱼ` with `x` integer and `tⱼ ≥ 0`, the cut is
/// `Σ g(aⱼ) tⱼ ≥ 1`. An integer `tⱼ` may be shifted by whole numbers without
/// leaving its own integrality, which is what lets its coefficient be taken to
/// the cheaper side of the fractional part; a continuous one may not, and pays
/// the full coefficient.
fn gomory_coefficient(a: f64, f0: f64, integral: bool) -> f64 {
    if integral {
        let f = a - a.floor();
        if f <= f0 {
            f / f0
        } else {
            (1.0 - f) / (1.0 - f0)
        }
    } else if a > 0.0 {
        a / f0
    } else {
        -a / (1.0 - f0)
    }
}

/// Separate Gomory mixed-integer cuts from the rows of the optimal tableau
/// whose basic variable is an integer column sitting between two integers.
fn gomory(
    t: &Tab<'_>,
    by_row: &ByRow,
    integer: &[bool],
    integrality_tolerance: f64,
    out: &mut Vec<Candidate>,
    generated: &mut usize,
) {
    let integral_row = integral_rows(&t.p, by_row, integer);
    let mut unit = vec![0.0; t.m];
    let mut column = Vec::new();

    for r in 0..t.m {
        let basic = t.basis[r];
        if basic >= t.n_struct || !integer.get(basic).copied().unwrap_or(false) {
            continue;
        }
        let beta = t.value[basic];
        if (beta - beta.round()).abs() <= integrality_tolerance {
            continue;
        }
        let f0 = beta - beta.floor();
        if !(MIN_FRACTION..=1.0 - MIN_FRACTION).contains(&f0) {
            continue;
        }

        // Row `r` of `B⁻¹`, which turns each column of the problem into its
        // entry in this row of the tableau.
        unit[r] = 1.0;
        let rho = t.inv.solve_transpose(&unit);
        unit[r] = 0.0;
        let Ok(rho) = rho else { continue };

        *generated += 1;

        // The cut over structurals and slacks, before the slacks are
        // substituted out. Moving each nonbasic variable back from the bound it
        // was measured against is what puts a constant on the right-hand side.
        let mut over_all = vec![0.0; t.n_struct + t.n_slack];
        let mut rhs = 1.0;
        let mut usable = true;
        for v in 0..t.n {
            if t.is_basic[v] || t.is_artificial(v) || t.lower[v] >= t.upper[v] {
                continue;
            }
            t.column(v, &mut column);
            let mut alpha = 0.0;
            for &(row, a) in &column {
                alpha += rho[row] * a;
            }
            if alpha.abs() < TABLEAU_ZERO {
                continue;
            }
            // A nonbasic variable resting on a bound is rewritten as its
            // distance from that bound, which is non-negative at every feasible
            // point rather than only at this one. A free nonbasic variable has
            // no bound to measure from, so there is no such substitution and no
            // cut from this row.
            let (at_lower, bound) = match t.at[v] {
                At::Lower => (true, t.lower[v]),
                At::Upper => (false, t.upper[v]),
                At::Free => {
                    usable = false;
                    break;
                }
            };
            if !bound.is_finite() {
                usable = false;
                break;
            }
            let a = if at_lower { alpha } else { -alpha };
            let shifted_is_integer = if v < t.n_struct {
                integer.get(v).copied().unwrap_or(false) && whole(bound)
            } else {
                integral_row.get(v - t.n_struct).copied().unwrap_or(false) && whole(bound)
            };
            let g = gomory_coefficient(a, f0, shifted_is_integer);
            if g == 0.0 {
                continue;
            }
            let d = if at_lower { g } else { -g };
            over_all[v] += d;
            rhs += d * bound;
        }
        if !usable {
            continue;
        }

        // A slack is the activity of its row, so a cut naming one is a cut
        // naming that row's columns.
        let mut coef = vec![0.0; t.n_struct];
        for (v, &d) in over_all.iter().enumerate() {
            if d == 0.0 {
                continue;
            }
            if v < t.n_struct {
                coef[v] += d;
            } else {
                for (j, a) in by_row.row(v - t.n_struct) {
                    coef[j] += d * a;
                }
            }
        }

        if let Some(candidate) = finish(&coef, rhs, &t.p, &t.value[..t.n_struct]) {
            out.push(candidate);
        }
    }
}

/// Turn a dense `Σ c x ≥ rhs` into a cut, or refuse it.
///
/// This is where every stability guard is applied, and where the cut is
/// weakened rather than falsified when a term has to go.
fn finish(coef: &[f64], rhs: f64, p: &Problem<'_>, x: &[f64]) -> Option<Candidate> {
    let largest = coef.iter().fold(0.0f64, |m, c| m.max(c.abs()));
    if !largest.is_finite() || largest <= 0.0 {
        return None;
    }

    let mut terms: Vec<(usize, f64)> = Vec::new();
    let mut rhs = rhs;
    let mut smallest = f64::INFINITY;
    for (j, &c) in coef.iter().enumerate() {
        if c == 0.0 {
            continue;
        }
        if c.abs() < COEF_NEGLIGIBLE * largest {
            // Dropping a term is only sound if the constraint is weakened to
            // allow for everything the dropped variable could have contributed,
            // so the right-hand side falls by the least that term could be. A
            // variable unbounded on that side has no least, and the cut goes
            // rather than the guarantee.
            let least = if c > 0.0 {
                c * p.col_lower[j]
            } else {
                c * p.col_upper[j]
            };
            if !least.is_finite() {
                return None;
            }
            rhs -= least;
            continue;
        }
        smallest = smallest.min(c.abs());
        terms.push((j, c));
    }
    if terms.is_empty() || !rhs.is_finite() {
        return None;
    }
    if largest / smallest > MAX_DYNAMISM {
        return None;
    }
    if terms.len() > DENSE_FLOOR && terms.len() as f64 > MAX_DENSITY * p.n_cols as f64 {
        return None;
    }

    // Scaled so the largest coefficient is one, which keeps the row on the
    // same scale as the rest of the matrix rather than on the scale the
    // fractional part happened to impose.
    let scale = 1.0 / largest;
    for term in &mut terms {
        term.1 *= scale;
    }
    let rhs = rhs * scale;

    let activity: f64 = terms
        .iter()
        .map(|&(j, c)| c * x.get(j).copied().unwrap_or(0.0))
        .sum();
    let norm = terms.iter().map(|&(_, c)| c * c).sum::<f64>().sqrt();
    if norm <= 0.0 {
        return None;
    }
    let efficacy = (rhs - activity) / norm;
    // Written to reject a value that is not a number rather than to accept one,
    // since every comparison against one is false and the wrong default here is
    // a cut nobody checked.
    if !efficacy.is_finite() || efficacy <= MIN_EFFICACY {
        return None;
    }

    Some(Candidate {
        cut: Cut {
            coef: terms,
            lower: rhs,
            upper: f64::INFINITY,
        },
        efficacy,
    })
}

// ---------------------------------------------------------------------------
// Cover cuts
// ---------------------------------------------------------------------------

/// One side of a row, read as a knapsack over binaries.
///
/// A row `Σ aⱼ xⱼ ≤ b` becomes `Σ wⱼ zⱼ ≤ B` with every weight positive by
/// complementing the negative columns: `xⱼ = 1 − zⱼ` moves `aⱼ xⱼ` to
/// `aⱼ + |aⱼ| zⱼ`, so the capacity absorbs the constant. Without that step a
/// row with mixed signs would be refused, and unit commitment rows are full of
/// mixed signs.
struct Knapsack {
    /// Column, weight, and whether the column was complemented.
    items: Vec<(usize, f64, bool)>,
    capacity: f64,
}

/// Read one sense of a row as a knapsack over binaries, if it is one.
fn knapsack(
    p: &Problem<'_>,
    by_row: &ByRow,
    integer: &[bool],
    row: usize,
    negate: bool,
) -> Option<Knapsack> {
    let bound = if negate {
        -p.row_lower[row]
    } else {
        p.row_upper[row]
    };
    if !bound.is_finite() {
        return None;
    }
    let sign = if negate { -1.0 } else { 1.0 };
    let mut items = Vec::new();
    let mut capacity = bound;
    for (j, a) in by_row.row(row) {
        let a = sign * a;
        if a == 0.0 {
            continue;
        }
        // Binary, and known to be so from the caller's own declaration rather
        // than inferred from the relaxation's values.
        let binary = integer.get(j).copied().unwrap_or(false)
            && p.col_lower[j] == 0.0
            && p.col_upper[j] == 1.0;
        if !binary {
            return None;
        }
        if a > 0.0 {
            items.push((j, a, false));
        } else {
            capacity += -a;
            items.push((j, -a, true));
        }
    }
    if items.len() < 2 || capacity < 0.0 {
        return None;
    }
    Some(Knapsack { items, capacity })
}

/// Separate cover cuts from the knapsack rows of the problem.
///
/// A cover is a set of items whose weights already exceed the capacity, so they
/// cannot all be taken. Finding the *most violated* one is itself a knapsack
/// problem, so this takes the standard greedy answer: prefer items the
/// relaxation has already nearly taken, and prefer heavy ones among those,
/// because both make a cover that the current point violates.
fn cover(
    p: &Problem<'_>,
    by_row: &ByRow,
    integer: &[bool],
    x: &[f64],
    out: &mut Vec<Candidate>,
    generated: &mut usize,
) {
    for row in 0..p.n_rows {
        for negate in [false, true] {
            let Some(k) = knapsack(p, by_row, integer, row, negate) else {
                continue;
            };
            // How far each item is from being taken, in the relaxation.
            let mut items: Vec<(usize, f64, bool, f64)> = k
                .items
                .iter()
                .map(|&(j, w, complemented)| {
                    let v = x.get(j).copied().unwrap_or(0.0);
                    let z = if complemented { 1.0 - v } else { v };
                    (j, w, complemented, (1.0 - z).max(0.0))
                })
                .collect();
            let total: f64 = items.iter().map(|i| i.1).sum();
            if total <= k.capacity {
                continue;
            }

            // Cheapest distance from being taken, per unit of weight bought.
            // Ties fall to the lower column index, so the same model gives the
            // same cover every run.
            items.sort_by(|a, b| (a.3 / a.1).total_cmp(&(b.3 / b.1)).then(a.0.cmp(&b.0)));

            let margin = COVER_MARGIN * k.capacity.abs().max(1.0);
            let mut chosen: Vec<usize> = Vec::new();
            let mut weight = 0.0;
            for (i, item) in items.iter().enumerate() {
                if weight > k.capacity + margin {
                    break;
                }
                weight += item.1;
                chosen.push(i);
            }
            if weight <= k.capacity + margin {
                continue;
            }

            // Minimal covers give stronger cuts, so drop whatever can go while
            // the set still weighs more than the capacity, heaviest distance
            // first. Walking the choices in reverse takes the least promising
            // out first, which is the order they were added in.
            let mut trimmed = chosen.clone();
            for &i in chosen.iter().rev() {
                if weight - items[i].1 > k.capacity + margin {
                    weight -= items[i].1;
                    trimmed.retain(|&c| c != i);
                }
            }
            let chosen = trimmed;
            if chosen.is_empty() {
                continue;
            }
            *generated += 1;

            // Extending by every item at least as heavy as the cover's heaviest
            // keeps the inequality valid and makes it name more of the problem,
            // which is the cheapest strengthening there is.
            let heaviest = chosen.iter().map(|&i| items[i].1).fold(0.0f64, f64::max);
            let size = chosen.len();
            let mut extended: Vec<usize> = chosen;
            for i in 0..items.len() {
                if !extended.contains(&i) && items[i].1 >= heaviest {
                    extended.push(i);
                }
            }

            // `Σ z ≤ |C| − 1` over the extended set, written back over the
            // columns: a complemented item contributes `1 − x`.
            let mut coef = vec![0.0; p.n_cols];
            let mut upper = (size as f64) - 1.0;
            for &i in &extended {
                let (j, _, complemented, _) = items[i];
                if complemented {
                    coef[j] -= 1.0;
                    upper -= 1.0;
                } else {
                    coef[j] += 1.0;
                }
            }

            // `finish` expects a `≥` cut, and this one is a `≤`; negating both
            // sides is the same inequality.
            let negated: Vec<f64> = coef.iter().map(|c| -c).collect();
            if let Some(candidate) = finish(&negated, -upper, p, x) {
                out.push(candidate);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small binary problem, dense columns turned into the shape the solver
    /// takes.
    struct Small {
        n_cols: usize,
        n_rows: usize,
        starts: Vec<u32>,
        rows: Vec<u32>,
        vals: Vec<f64>,
        lower: Vec<f64>,
        upper: Vec<f64>,
        cost: Vec<f64>,
        row_lower: Vec<f64>,
        row_upper: Vec<f64>,
    }

    impl Small {
        fn problem(&self) -> Problem<'_> {
            Problem {
                n_cols: self.n_cols,
                n_rows: self.n_rows,
                col_starts: &self.starts,
                row_indices: &self.rows,
                values: &self.vals,
                col_lower: &self.lower,
                col_upper: &self.upper,
                col_cost: &self.cost,
                row_lower: &self.row_lower,
                row_upper: &self.row_upper,
            }
        }
    }

    /// A deterministic stream, because a validity test that used a different
    /// problem on every run would report a different answer on every run.
    fn stream(seed: u64) -> impl FnMut(u64) -> u64 {
        let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        move |modulus| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) % modulus
        }
    }

    /// A small binary program with knapsack rows in both senses.
    ///
    /// Both senses on purpose: a `≤` row is a knapsack directly and a `≥` row
    /// is one after complementing, and the complementing is where a sign error
    /// would produce a cut that looks reasonable and is not.
    fn small(n: usize, seed: u64) -> Small {
        let mut next = stream(seed);
        let mut columns: Vec<Vec<f64>> = Vec::new();
        let mut cost = Vec::new();
        let n_rows = 4;
        for _ in 0..n {
            let mut col = vec![0.0; n_rows];
            for entry in col.iter_mut() {
                if next(3) > 0 {
                    *entry = (1 + next(9)) as f64;
                }
            }
            columns.push(col);
            cost.push(-((5 + next(45)) as f64));
        }

        let mut starts = vec![0u32];
        let mut rows = Vec::new();
        let mut vals = Vec::new();
        for col in &columns {
            for (r, &v) in col.iter().enumerate() {
                if v != 0.0 {
                    rows.push(r as u32);
                    vals.push(v);
                }
            }
            starts.push(rows.len() as u32);
        }

        let total = |r: usize| columns.iter().map(|c| c[r]).sum::<f64>();
        let row_lower = vec![
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
            (total(2) * 0.3).ceil(),
            (total(3) * 0.25).ceil(),
        ];
        let row_upper = vec![
            (total(0) * 0.45).floor(),
            (total(1) * 0.4).floor(),
            f64::INFINITY,
            f64::INFINITY,
        ];

        Small {
            n_cols: n,
            n_rows,
            starts,
            rows,
            vals,
            lower: vec![0.0; n],
            upper: vec![1.0; n],
            cost,
            row_lower,
            row_upper,
        }
    }

    /// Whether a point satisfies the rows and bounds of a problem.
    fn feasible(p: &Problem<'_>, x: &[f64]) -> bool {
        let mut activity = vec![0.0; p.n_rows];
        for j in 0..p.n_cols {
            let s = p.col_starts[j] as usize;
            let e = p.col_starts[j + 1] as usize;
            for k in s..e {
                activity[p.row_indices[k] as usize] += p.values[k] * x[j];
            }
        }
        (0..p.n_rows)
            .all(|i| activity[i] >= p.row_lower[i] - 1e-9 && activity[i] <= p.row_upper[i] + 1e-9)
    }

    /// Cut a problem's root, as `solve_mip` does.
    fn cut(p: Problem<'_>, integer: &[bool], families: Cuts) -> Option<Tightened> {
        let o = Options::default();
        let (_, tab) = solve_keeping_basis(p, o).ok()?;
        tighten_root(p, &tab, integer, families, o, 1e-6, usize::MAX).ok()?
    }

    #[test]
    fn a_cut_never_removes_an_integer_point() {
        // The thing that must not happen, checked exhaustively rather than
        // argued. Every assignment of the binaries is enumerated; those that
        // satisfy the original problem are exactly the points the search is
        // entitled to find, and every one of them has to satisfy every cut.
        //
        // A cut that removes one is the worst failure this code could have,
        // because the search would still return an answer, still call it
        // proved, and still be wrong — with nothing anywhere reporting that
        // anything had gone amiss.
        let n = 12;
        let integer = vec![true; n];
        let mut checked = 0usize;
        let mut cuts_seen = 0usize;
        for seed in 0..24u64 {
            let model = small(n, seed);
            let p = model.problem();
            for families in [Cuts::Gomory, Cuts::Cover, Cuts::Both] {
                let Some(t) = cut(p, &integer, families) else {
                    continue;
                };
                if t.n_rows == p.n_rows {
                    continue;
                }
                cuts_seen += t.n_rows - p.n_rows;
                let with_cuts = t.problem(p);
                for mask in 0..(1u32 << n) {
                    let x: Vec<f64> = (0..n).map(|j| f64::from((mask >> j) & 1)).collect();
                    if !feasible(&p, &x) {
                        continue;
                    }
                    checked += 1;
                    assert!(
                        feasible(&with_cuts, &x),
                        "seed {seed}, {families:?}: the cuts removed the integer \
                         point {x:?}, which the original problem allows"
                    );
                }
            }
        }
        assert!(
            cuts_seen > 0,
            "no cuts were generated, so nothing was tested"
        );
        assert!(
            checked > 0,
            "no feasible integer points, so nothing was tested"
        );
    }

    #[test]
    fn cutting_the_same_model_twice_gives_the_same_cuts() {
        // Determinism, at the level it is decided: the candidates are ranked by
        // a floating point efficacy and truncated to a budget, so a ranking
        // that did not break ties by generation order would keep a different
        // set of cuts on different runs — and then the node count, the time and
        // the tree would all move for no reason a caller could see.
        let model = small(14, 7);
        let integer = vec![true; 14];
        let first = cut(model.problem(), &integer, Cuts::Both).expect("cuts");
        for _ in 0..3 {
            let again = cut(model.problem(), &integer, Cuts::Both).expect("cuts");
            assert_eq!(first.n_rows, again.n_rows);
            assert_eq!(first.kept, again.kept);
            assert_eq!(first.generated, again.generated);
            assert_eq!(first.vals, again.vals);
            assert_eq!(first.rows, again.rows);
            assert_eq!(first.row_lower, again.row_lower);
            assert_eq!(first.row_upper, again.row_upper);
        }
    }

    #[test]
    fn cutting_never_loosens_the_root_bound() {
        // A cut can only remove points, so the relaxation it leaves behind
        // cannot be cheaper than the one it started from. A root bound that
        // fell would mean the cut had added feasible territory, which for a
        // minimisation is the signature of a sign error.
        let integer = vec![true; 14];
        for seed in 0..12u64 {
            let model = small(14, seed);
            let p = model.problem();
            let plain = solve_keeping_basis(p, Options::default())
                .map(|(s, _)| s)
                .expect("a solvable relaxation");
            let Some(t) = cut(p, &integer, Cuts::Both) else {
                continue;
            };
            assert_eq!(t.root.status, Status::Optimal, "seed {seed}");
            assert!(
                t.root.objective >= plain.objective - 1e-6,
                "seed {seed}: the root bound fell from {} to {}",
                plain.objective,
                t.root.objective
            );
        }
    }

    #[test]
    fn the_gomory_coefficient_of_a_whole_number_on_an_integer_variable_is_zero() {
        // A term that is already integral says nothing about the fractional
        // part being cut off, and the cut should not carry it.
        assert_eq!(gomory_coefficient(3.0, 0.4, true), 0.0);
        assert_eq!(gomory_coefficient(-2.0, 0.4, true), 0.0);
    }

    #[test]
    fn the_gomory_coefficient_takes_the_cheaper_side_for_an_integer_variable() {
        // Below f0 the coefficient rises with the fractional part; above it,
        // the variable is shifted up by one instead and the coefficient falls.
        let f0 = 0.4;
        assert!((gomory_coefficient(0.2, f0, true) - 0.5).abs() < 1e-12);
        assert!((gomory_coefficient(0.8, f0, true) - (0.2 / 0.6)).abs() < 1e-12);
    }

    #[test]
    fn a_continuous_coefficient_pays_the_whole_of_itself() {
        let f0 = 0.25;
        assert!((gomory_coefficient(0.5, f0, false) - 2.0).abs() < 1e-12);
        assert!((gomory_coefficient(-0.5, f0, false) - (0.5 / 0.75)).abs() < 1e-12);
    }

    #[test]
    fn every_gomory_coefficient_is_non_negative() {
        // The cut is `Σ g t ≥ 1` over non-negative `t`, so a negative
        // coefficient would let a feasible point buy its way out of the cut.
        for f0 in [0.05, 0.3, 0.5, 0.7, 0.95] {
            for a in [-3.7, -1.0, -0.2, 0.0, 0.2, 1.0, 3.7] {
                for integral in [false, true] {
                    assert!(
                        gomory_coefficient(a, f0, integral) >= 0.0,
                        "g({a}, {f0}, {integral})"
                    );
                }
            }
        }
    }

    #[test]
    fn a_cut_that_names_nothing_is_refused() {
        let p = Problem {
            n_cols: 2,
            n_rows: 0,
            col_starts: &[0, 0, 0],
            row_indices: &[],
            values: &[],
            col_lower: &[0.0, 0.0],
            col_upper: &[1.0, 1.0],
            col_cost: &[0.0, 0.0],
            row_lower: &[],
            row_upper: &[],
        };
        assert!(finish(&[0.0, 0.0], 1.0, &p, &[0.5, 0.5]).is_none());
    }

    #[test]
    fn a_cut_the_point_already_satisfies_is_refused() {
        // The point of a cut is to remove the vertex that produced it. One that
        // does not is a row every node solve pays for and nothing gets.
        let p = Problem {
            n_cols: 2,
            n_rows: 0,
            col_starts: &[0, 0, 0],
            row_indices: &[],
            values: &[],
            col_lower: &[0.0, 0.0],
            col_upper: &[1.0, 1.0],
            col_cost: &[0.0, 0.0],
            row_lower: &[],
            row_upper: &[],
        };
        // x0 + x1 >= 0.5 at (0.5, 0.5) is satisfied with room to spare.
        assert!(finish(&[1.0, 1.0], 0.5, &p, &[0.5, 0.5]).is_none());
        // The same cut at 1.5 is violated by half.
        assert!(finish(&[1.0, 1.0], 1.5, &p, &[0.5, 0.5]).is_some());
    }

    #[test]
    fn a_cut_spanning_too_many_orders_of_magnitude_is_refused() {
        let p = Problem {
            n_cols: 2,
            n_rows: 0,
            col_starts: &[0, 0, 0],
            row_indices: &[],
            values: &[],
            col_lower: &[0.0, 0.0],
            col_upper: &[1.0, 1.0],
            col_cost: &[0.0, 0.0],
            row_lower: &[],
            row_upper: &[],
        };
        // Eight orders of magnitude between the two coefficients, and violated,
        // so only the dynamism guard can refuse it. Eight rather than twelve
        // because the smaller coefficient has to stay above the threshold that
        // would have dropped it as noise: the two guards deal with different
        // things, and a term at a hundred-millionth of the largest is real
        // information about a face rather than the residue of a solve.
        assert!(finish(&[1.0, 1e-8], 1.5, &p, &[0.5, 0.5]).is_none());
    }

    #[test]
    fn dropping_a_negligible_term_weakens_the_cut_rather_than_falsifying_it() {
        // The dropped term is worth at most its coefficient times the bound it
        // could reach, and the right-hand side has to give that up. Here the
        // second column is negligible and non-negative, so it could contribute
        // as little as zero and the right-hand side is untouched — but the
        // guarantee is that no integer point is lost either way.
        let p = Problem {
            n_cols: 2,
            n_rows: 0,
            col_starts: &[0, 0, 0],
            row_indices: &[],
            values: &[],
            col_lower: &[0.0, -1.0],
            col_upper: &[1.0, 1.0],
            col_cost: &[0.0, 0.0],
            row_lower: &[],
            row_upper: &[],
        };
        let c = finish(&[1.0, 1e-12], 0.9, &p, &[0.0, 0.0]).expect("violated at the origin");
        assert_eq!(c.cut.coef.len(), 1, "the negligible term should be gone");
        // The dropped coefficient is positive, so its least contribution is at
        // that column's lower bound of −1, and the cut has to allow for it.
        assert!((c.cut.lower - (0.9 + 1e-12)).abs() < 1e-9);
    }

    #[test]
    fn a_row_with_a_continuous_column_is_not_a_knapsack() {
        // Cover cuts are an argument about binaries. A commitment row mixing a
        // continuous dispatch variable with a status binary is not one, and
        // treating it as one would be inventing a bound on the dispatch.
        let p = Problem {
            n_cols: 2,
            n_rows: 1,
            col_starts: &[0, 1, 2],
            row_indices: &[0, 0],
            values: &[1.0, -100.0],
            col_lower: &[0.0, 0.0],
            col_upper: &[100.0, 1.0],
            col_cost: &[0.0, 0.0],
            row_lower: &[f64::NEG_INFINITY],
            row_upper: &[0.0],
        };
        let by_row = ByRow::new(&p);
        assert!(knapsack(&p, &by_row, &[false, true], 0, false).is_none());
    }

    #[test]
    fn a_negative_coefficient_is_complemented_into_the_capacity() {
        // 3x - 2y <= 1 over binaries is 3x + 2(1-y) <= 3, so the capacity
        // absorbs the constant and both weights come out positive.
        let p = Problem {
            n_cols: 2,
            n_rows: 1,
            col_starts: &[0, 1, 2],
            row_indices: &[0, 0],
            values: &[3.0, -2.0],
            col_lower: &[0.0, 0.0],
            col_upper: &[1.0, 1.0],
            col_cost: &[0.0, 0.0],
            row_lower: &[f64::NEG_INFINITY],
            row_upper: &[1.0],
        };
        let by_row = ByRow::new(&p);
        let k = knapsack(&p, &by_row, &[true, true], 0, false).expect("a knapsack");
        assert!((k.capacity - 3.0).abs() < 1e-12);
        assert_eq!(k.items, vec![(0, 3.0, false), (1, 2.0, true)]);
    }

    #[test]
    fn the_row_view_agrees_with_the_column_view() {
        let p = Problem {
            n_cols: 3,
            n_rows: 2,
            col_starts: &[0, 2, 3, 4],
            row_indices: &[0, 1, 0, 1],
            values: &[1.0, 2.0, 3.0, 4.0],
            col_lower: &[0.0; 3],
            col_upper: &[1.0; 3],
            col_cost: &[0.0; 3],
            row_lower: &[0.0; 2],
            row_upper: &[1.0; 2],
        };
        let by_row = ByRow::new(&p);
        assert_eq!(by_row.row(0).collect::<Vec<_>>(), vec![(0, 1.0), (1, 3.0)]);
        assert_eq!(by_row.row(1).collect::<Vec<_>>(), vec![(0, 2.0), (2, 4.0)]);
    }
}

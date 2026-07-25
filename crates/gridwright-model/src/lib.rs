//! The linear model core.
//!
//! This crate exists because of one observation: building a large energy
//! system LP is a sparse matrix assembly problem, and the Python tooling
//! treats it as a dataframe problem. PyPSA and linopy construct labelled
//! intermediates and then convert them into the triplet form a solver
//! actually wants. The conversion is where the memory goes.
//!
//! So the model here is never a table. Variables are contiguous ranges handed
//! out by a bump counter, constraints are accumulated as CSR row batches that
//! each thread owns exclusively, and the only global operation is a merge that
//! knows every batch's size in advance and therefore allocates exactly once.
//!
//! The one genuinely interesting kernel is [`Model::to_csc`], which transposes
//! CSR to the column major form solvers expect. It is a counting sort, which
//! means it is two linear passes and no comparisons.

use std::ops::Range;

pub mod csc;

pub use csc::Csc;

/// Sense of the objective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sense {
    Minimize,
    Maximize,
}

/// A contiguous run of variables.
///
/// Energy system models allocate variables in regular blocks: one generator
/// dispatch variable per snapshot, one flow variable per line per snapshot.
/// Handing back a range rather than individual indices means the caller does
/// index arithmetic instead of hashing, and it means a block is described by
/// two integers no matter how many variables it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VarBlock {
    start: u32,
    len: u32,
}

impl VarBlock {
    #[inline]
    pub fn start(self) -> u32 {
        self.start
    }

    #[inline]
    pub fn len(self) -> u32 {
        self.len
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Index of the `i`th variable in this block.
    ///
    /// Debug builds check the bound. Release builds do not, because this sits
    /// in the innermost loop of constraint generation and the callers are all
    /// in this workspace.
    #[inline]
    pub fn at(self, i: u32) -> u32 {
        debug_assert!(i < self.len, "index {i} out of block of {}", self.len);
        self.start + i
    }

    #[inline]
    pub fn range(self) -> Range<u32> {
        self.start..self.start + self.len
    }
}

/// Variable bounds and objective coefficients, stored column wise.
#[derive(Debug, Default, Clone)]
pub struct Columns {
    pub lower: Vec<f64>,
    pub upper: Vec<f64>,
    pub obj: Vec<f64>,
}

impl Columns {
    #[inline]
    pub fn len(&self) -> usize {
        self.lower.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.lower.is_empty()
    }
}

/// A batch of constraint rows in CSR form.
///
/// Each thread building constraints owns one of these outright, so nothing is
/// shared and nothing is locked. Batches are merged at the end by
/// [`Model::absorb`], which can size the destination exactly because every
/// batch already knows its own length.
#[derive(Debug, Default, Clone)]
pub struct RowBatch {
    /// Offsets into `cols`/`vals`, length `rows + 1`, starting at zero.
    starts: Vec<u32>,
    cols: Vec<u32>,
    vals: Vec<f64>,
    lower: Vec<f64>,
    upper: Vec<f64>,
}

impl RowBatch {
    pub fn new() -> Self {
        Self {
            starts: vec![0],
            ..Default::default()
        }
    }

    /// Preallocate for a known shape. Constraint generation almost always
    /// knows both counts up front, and growing a few million element vector by
    /// doubling is pure waste when the final size is already derivable.
    pub fn with_capacity(rows: usize, nnz: usize) -> Self {
        let mut starts = Vec::with_capacity(rows + 1);
        starts.push(0);
        Self {
            starts,
            cols: Vec::with_capacity(nnz),
            vals: Vec::with_capacity(nnz),
            lower: Vec::with_capacity(rows),
            upper: Vec::with_capacity(rows),
        }
    }

    #[inline]
    pub fn rows(&self) -> usize {
        self.starts.len() - 1
    }

    #[inline]
    pub fn nnz(&self) -> usize {
        self.cols.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.rows() == 0
    }

    /// Append a row from an iterator of `(column, coefficient)` pairs.
    ///
    /// Ranged rather than one method per relational operator: an equality is
    /// `push_row(terms, v, v)`, a `<= u` is `push_row(terms, f64::NEG_INFINITY, u)`.
    /// Solvers model it this way too, so nothing has to be translated later.
    pub fn push_row<I>(&mut self, terms: I, lower: f64, upper: f64)
    where
        I: IntoIterator<Item = (u32, f64)>,
    {
        for (col, val) in terms {
            self.cols.push(col);
            self.vals.push(val);
        }
        self.starts.push(self.cols.len() as u32);
        self.lower.push(lower);
        self.upper.push(upper);
    }

    /// Equality row: `terms == rhs`.
    #[inline]
    pub fn push_eq<I>(&mut self, terms: I, rhs: f64)
    where
        I: IntoIterator<Item = (u32, f64)>,
    {
        self.push_row(terms, rhs, rhs);
    }

    /// Upper bounded row: `terms <= rhs`.
    #[inline]
    pub fn push_le<I>(&mut self, terms: I, rhs: f64)
    where
        I: IntoIterator<Item = (u32, f64)>,
    {
        self.push_row(terms, f64::NEG_INFINITY, rhs);
    }

    /// Lower bounded row: `terms >= rhs`.
    #[inline]
    pub fn push_ge<I>(&mut self, terms: I, rhs: f64)
    where
        I: IntoIterator<Item = (u32, f64)>,
    {
        self.push_row(terms, rhs, f64::INFINITY);
    }
}

/// A linear model: columns, rows, and an objective sense.
#[derive(Debug, Clone)]
pub struct Model {
    pub sense: Sense,
    cols: Columns,
    rows: RowBatch,
}

impl Default for Model {
    fn default() -> Self {
        Self::new()
    }
}

impl Model {
    pub fn new() -> Self {
        Self {
            sense: Sense::Minimize,
            cols: Columns::default(),
            rows: RowBatch::new(),
        }
    }

    #[inline]
    pub fn num_cols(&self) -> usize {
        self.cols.len()
    }

    #[inline]
    pub fn num_rows(&self) -> usize {
        self.rows.rows()
    }

    #[inline]
    pub fn nnz(&self) -> usize {
        self.rows.nnz()
    }

    #[inline]
    pub fn columns(&self) -> &Columns {
        &self.cols
    }

    /// Reserve column space for a model whose size is already known.
    pub fn reserve_cols(&mut self, n: usize) {
        self.cols.lower.reserve(n);
        self.cols.upper.reserve(n);
        self.cols.obj.reserve(n);
    }

    /// Allocate `n` variables sharing bounds and objective coefficient.
    ///
    /// The uniform case is overwhelmingly the common one (every hour of a
    /// generator's dispatch has the same bounds), so it gets the fast path:
    /// three `resize` calls, which are memsets, rather than `n` pushes.
    pub fn add_block(&mut self, n: u32, lower: f64, upper: f64, obj: f64) -> VarBlock {
        let start = self.cols.len() as u32;
        let new_len = self.cols.len() + n as usize;
        self.cols.lower.resize(new_len, lower);
        self.cols.upper.resize(new_len, upper);
        self.cols.obj.resize(new_len, obj);
        VarBlock { start, len: n }
    }

    /// Allocate a block whose bounds vary per element, e.g. a wind generator
    /// whose ceiling follows an hourly availability profile.
    pub fn add_block_with(
        &mut self,
        lower: &[f64],
        upper: &[f64],
        obj: f64,
    ) -> Result<VarBlock, ModelError> {
        if lower.len() != upper.len() {
            return Err(ModelError::BoundLengthMismatch {
                lower: lower.len(),
                upper: upper.len(),
            });
        }
        let start = self.cols.len() as u32;
        self.cols.lower.extend_from_slice(lower);
        self.cols.upper.extend_from_slice(upper);
        self.cols.obj.resize(self.cols.len(), obj);
        Ok(VarBlock {
            start,
            len: lower.len() as u32,
        })
    }

    /// Bounds as mutable slices, for callers that need to pin a variable after
    /// the fact. The slack bus angle is the motivating case: it is allocated
    /// like every other angle and then fixed.
    #[inline]
    pub fn columns_mut_lower(&mut self) -> &mut [f64] {
        &mut self.cols.lower
    }

    #[inline]
    pub fn columns_mut_upper(&mut self) -> &mut [f64] {
        &mut self.cols.upper
    }

    /// Set one objective coefficient across a whole block.
    ///
    /// The uniform case is worth its own method because it is a memset rather
    /// than a copy, and because it lets callers skip materialising a vector
    /// they would immediately throw away.
    pub fn fill_obj(&mut self, block: VarBlock, obj: f64) {
        let s = block.start as usize;
        self.cols.obj[s..s + block.len as usize].fill(obj);
    }

    /// Overwrite the objective coefficients of an existing block.
    pub fn set_obj(&mut self, block: VarBlock, obj: &[f64]) -> Result<(), ModelError> {
        if obj.len() != block.len() as usize {
            return Err(ModelError::BoundLengthMismatch {
                lower: obj.len(),
                upper: block.len() as usize,
            });
        }
        let s = block.start as usize;
        self.cols.obj[s..s + obj.len()].copy_from_slice(obj);
        Ok(())
    }

    /// Merge a batch of rows built elsewhere.
    ///
    /// This is the only place per-thread work rejoins the model. Because the
    /// batch carries its own length, the destination is reserved exactly once
    /// and the copy is three `extend_from_slice` calls plus an offset shift on
    /// the row starts. Nothing is reparsed and nothing is rehashed.
    pub fn absorb(&mut self, batch: &RowBatch) {
        let shift = self.rows.cols.len() as u32;
        self.rows.cols.extend_from_slice(&batch.cols);
        self.rows.vals.extend_from_slice(&batch.vals);
        self.rows.lower.extend_from_slice(&batch.lower);
        self.rows.upper.extend_from_slice(&batch.upper);
        // The batch's first start is its own zero, which is already
        // represented by the tail of ours, so it is skipped.
        self.rows
            .starts
            .extend(batch.starts[1..].iter().map(|&s| s + shift));
    }

    /// Merge many batches, reserving for all of them first.
    pub fn absorb_all(&mut self, batches: &[RowBatch]) {
        let rows: usize = batches.iter().map(RowBatch::rows).sum();
        let nnz: usize = batches.iter().map(RowBatch::nnz).sum();
        self.rows.cols.reserve(nnz);
        self.rows.vals.reserve(nnz);
        self.rows.lower.reserve(rows);
        self.rows.upper.reserve(rows);
        self.rows.starts.reserve(rows);
        for b in batches {
            self.absorb(b);
        }
    }

    pub fn row_bounds(&self) -> (&[f64], &[f64]) {
        (&self.rows.lower, &self.rows.upper)
    }

    /// Transpose the constraint matrix into the column major form solvers take.
    ///
    /// See [`csc::from_csr`]. This is the one operation whose cost is not
    /// linear in what the caller wrote, so it is the one worth measuring.
    pub fn to_csc(&self) -> Csc {
        csc::from_csr(
            &self.rows.starts,
            &self.rows.cols,
            &self.rows.vals,
            self.num_cols(),
        )
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelError {
    #[error("bound slices differ in length: lower has {lower}, upper has {upper}")]
    BoundLengthMismatch { lower: usize, upper: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_hand_out_contiguous_indices() {
        let mut m = Model::new();
        let a = m.add_block(3, 0.0, 10.0, 1.0);
        let b = m.add_block(2, -1.0, 1.0, 0.0);

        assert_eq!(a.start(), 0);
        assert_eq!(a.len(), 3);
        assert_eq!(b.start(), 3);
        assert_eq!(a.at(2), 2);
        assert_eq!(b.at(0), 3);
        assert_eq!(m.num_cols(), 5);
        assert_eq!(a.range(), 0..3);
    }

    #[test]
    fn uniform_block_fills_bounds_and_objective() {
        let mut m = Model::new();
        m.add_block(3, 0.0, 42.0, 7.0);
        assert_eq!(m.columns().lower, vec![0.0; 3]);
        assert_eq!(m.columns().upper, vec![42.0; 3]);
        assert_eq!(m.columns().obj, vec![7.0; 3]);
    }

    #[test]
    fn per_element_bounds_are_kept_in_order() {
        let mut m = Model::new();
        let up = [1.0, 5.0, 2.5];
        let lo = [0.0, 0.0, 0.0];
        let b = m.add_block_with(&lo, &up, 3.0).unwrap();
        assert_eq!(b.len(), 3);
        assert_eq!(m.columns().upper, up);
        assert_eq!(m.columns().obj, vec![3.0; 3]);
    }

    #[test]
    fn mismatched_bound_lengths_are_rejected() {
        let mut m = Model::new();
        let err = m.add_block_with(&[0.0, 0.0], &[1.0], 0.0).unwrap_err();
        assert_eq!(err, ModelError::BoundLengthMismatch { lower: 2, upper: 1 });
    }

    #[test]
    fn rows_survive_the_merge_with_shifted_offsets() {
        let mut one = RowBatch::new();
        one.push_eq([(0, 1.0), (1, -1.0)], 0.0);
        let mut two = RowBatch::new();
        two.push_le([(1, 2.0)], 5.0);
        two.push_ge([(0, 1.0), (1, 1.0)], 1.0);

        let mut m = Model::new();
        m.add_block(2, 0.0, 10.0, 1.0);
        m.absorb_all(&[one, two]);

        assert_eq!(m.num_rows(), 3);
        assert_eq!(m.nnz(), 5);
        let (lo, up) = m.row_bounds();
        assert_eq!(lo, &[0.0, f64::NEG_INFINITY, 1.0]);
        assert_eq!(up, &[0.0, 5.0, f64::INFINITY]);
        // Row starts must be cumulative across the batch boundary, not restart.
        assert_eq!(m.rows.starts, vec![0, 2, 3, 5]);
    }

    #[test]
    fn merging_empty_batches_changes_nothing() {
        let mut m = Model::new();
        m.add_block(2, 0.0, 1.0, 0.0);
        m.absorb_all(&[RowBatch::new(), RowBatch::new()]);
        assert_eq!(m.num_rows(), 0);
        assert_eq!(m.nnz(), 0);
        assert_eq!(m.rows.starts, vec![0]);
    }

    #[test]
    fn setting_objective_on_a_block_touches_only_that_block() {
        let mut m = Model::new();
        let a = m.add_block(2, 0.0, 1.0, 9.0);
        let _b = m.add_block(2, 0.0, 1.0, 9.0);
        m.set_obj(a, &[1.0, 2.0]).unwrap();
        assert_eq!(m.columns().obj, vec![1.0, 2.0, 9.0, 9.0]);
    }
}

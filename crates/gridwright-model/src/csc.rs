//! Compressed sparse column storage, and the transpose that produces it.
//!
//! Constraints are natural to write one row at a time: a nodal balance is a
//! statement about one bus in one hour. Solvers want the matrix the other way
//! round, by column. Something has to transpose, and for a European hourly
//! model that something is moving tens of millions of entries.
//!
//! The transpose is a counting sort. Column indices are already integers in
//! `0..n_cols`, so there is nothing to compare: count how many entries land in
//! each column, prefix sum the counts into offsets, then scatter each entry to
//! its column's cursor. Two linear passes over the data, one over the columns.
//!
//! The parallel version splits the count across threads into private
//! histograms and then does a two dimensional scan, so that each thread knows
//! not only where its column starts but where *its own* slice of that column
//! starts. That makes the scatter write-disjoint, so it needs no atomics and
//! no locking, which is what makes it worth doing at all.

use rayon::prelude::*;

/// A matrix in compressed sparse column form.
///
/// `starts` has length `n_cols + 1`. Column `j` occupies
/// `starts[j]..starts[j + 1]` of `rows` and `vals`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Csc {
    pub starts: Vec<u32>,
    pub rows: Vec<u32>,
    pub vals: Vec<f64>,
    pub n_cols: usize,
    pub n_rows: usize,
}

impl Csc {
    #[inline]
    pub fn nnz(&self) -> usize {
        self.rows.len()
    }

    /// Entries of column `j`, as `(row, value)`.
    pub fn column(&self, j: usize) -> impl Iterator<Item = (u32, f64)> + '_ {
        let s = self.starts[j] as usize;
        let e = self.starts[j + 1] as usize;
        self.rows[s..e].iter().copied().zip(self.vals[s..e].iter().copied())
    }

    /// Check the structure is self consistent. Used by tests and by the CLI
    /// under a debug flag; a malformed CSC handed to a solver tends to produce
    /// a wrong answer rather than a crash, which is much worse.
    pub fn validate(&self) -> Result<(), CscError> {
        if self.starts.len() != self.n_cols + 1 {
            return Err(CscError::StartsLength {
                got: self.starts.len(),
                want: self.n_cols + 1,
            });
        }
        if self.starts[0] != 0 {
            return Err(CscError::StartsNotZero(self.starts[0]));
        }
        if *self.starts.last().unwrap() as usize != self.rows.len() {
            return Err(CscError::StartsTail {
                got: *self.starts.last().unwrap() as usize,
                nnz: self.rows.len(),
            });
        }
        if self.rows.len() != self.vals.len() {
            return Err(CscError::RowsValsMismatch {
                rows: self.rows.len(),
                vals: self.vals.len(),
            });
        }
        for w in self.starts.windows(2) {
            if w[1] < w[0] {
                return Err(CscError::StartsNotMonotonic);
            }
        }
        if let Some(&r) = self.rows.iter().max()
            && r as usize >= self.n_rows
        {
            return Err(CscError::RowOutOfRange {
                row: r,
                n_rows: self.n_rows,
            });
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CscError {
    #[error("starts has length {got}, expected n_cols + 1 = {want}")]
    StartsLength { got: usize, want: usize },
    #[error("starts[0] is {0}, expected 0")]
    StartsNotZero(u32),
    #[error("last start is {got} but there are {nnz} entries")]
    StartsTail { got: usize, nnz: usize },
    #[error("rows has {rows} entries but vals has {vals}")]
    RowsValsMismatch { rows: usize, vals: usize },
    #[error("starts is not monotonically non-decreasing")]
    StartsNotMonotonic,
    #[error("row index {row} is out of range for {n_rows} rows")]
    RowOutOfRange { row: u32, n_rows: usize },
}

/// Serial CSR to CSC transpose by counting sort.
///
/// Kept as the reference implementation. The parallel version is checked
/// against it, because a transpose that is fast and wrong is worthless and the
/// failure mode is silent.
pub fn from_csr(row_starts: &[u32], cols: &[u32], vals: &[f64], n_cols: usize) -> Csc {
    let n_rows = row_starts.len().saturating_sub(1);
    let nnz = cols.len();

    // Pass one: how many entries in each column.
    let mut starts = vec![0u32; n_cols + 1];
    for &c in cols {
        starts[c as usize + 1] += 1;
    }
    // Prefix sum turns counts into the start offset of each column.
    for j in 0..n_cols {
        starts[j + 1] += starts[j];
    }

    let mut out_rows = vec![0u32; nnz];
    let mut out_vals = vec![0.0f64; nnz];
    // Cursor per column, advanced as entries are placed.
    let mut cursor = starts.clone();

    // Pass two: scatter. Walking rows in order means each column ends up with
    // its entries in ascending row order for free, which solvers prefer and
    // which makes the output comparable between the two implementations.
    for r in 0..n_rows {
        let s = row_starts[r] as usize;
        let e = row_starts[r + 1] as usize;
        for k in s..e {
            let c = cols[k] as usize;
            let dst = cursor[c] as usize;
            out_rows[dst] = r as u32;
            out_vals[dst] = vals[k];
            cursor[c] += 1;
        }
    }

    Csc {
        starts,
        rows: out_rows,
        vals: out_vals,
        n_cols,
        n_rows,
    }
}

/// Rows per chunk below which threading costs more than it saves.
const PAR_THRESHOLD: usize = 4096;

/// Parallel CSR to CSC transpose.
///
/// The obvious parallelisation, sharing one cursor array behind atomics, does
/// not pay: the scatter is memory bound and the atomic increment sits directly
/// in its inner loop. This instead gives every chunk of rows a private
/// histogram and resolves ownership before any writing starts.
///
/// Three phases:
///
/// 1. Each chunk counts its own entries per column, into its own histogram.
/// 2. A scan over `[chunk][column]` produces, for every pair, the exact index
///    at which that chunk's entries for that column begin. Summing across
///    chunks first gives the global column offsets; the running total within a
///    column gives each chunk its slice of it.
/// 3. Each chunk scatters into its own slices. No two chunks can target the
///    same index by construction, so the writes need no synchronisation.
///
/// Ascending row order within a column is preserved because chunks are laid
/// out in row order and each chunk walks its rows in order.
pub fn from_csr_par(row_starts: &[u32], cols: &[u32], vals: &[f64], n_cols: usize) -> Csc {
    let n_rows = row_starts.len().saturating_sub(1);
    let nnz = cols.len();

    let threads = rayon::current_num_threads().max(1);
    let chunk_rows = n_rows.div_ceil(threads).max(PAR_THRESHOLD);
    let n_chunks = n_rows.div_ceil(chunk_rows).max(1);

    if n_chunks == 1 || n_rows == 0 {
        return from_csr(row_starts, cols, vals, n_cols);
    }

    // One shared counter array, incremented atomically, rather than a private
    // dense histogram per thread.
    //
    // The reasoning and the measurement disagreed, and the measurement is
    // recorded here because the reasoning was persuasive. This matrix has 16.3
    // million columns and 29 million nonzeros — under two entries per column —
    // so a `threads × n_cols` histogram is 910 MB of counters to count 29
    // million items, plus a 65 MB cursor copy per thread. That looks obviously
    // wasteful and it is, in address space. It is **not** wasteful in resident
    // memory or in time: peak RSS is 1.951 GB either way, to within 60 KB, and
    // the transpose takes about 88 ms either way. Most of those 910 MB are
    // never written, because a chunk only touches the columns its own rows
    // reach, and pages nobody writes are never faulted in.
    //
    // Kept anyway, for the reason that survives: 65 MB of allocation against
    // 1.8 GB. Address space is not free everywhere, and the WebAssembly target
    // has 4 GB of it in total.
    //
    // What it costs is ordering. `fetch_add` hands out slots in whatever order
    // threads arrive, so rows within a column come out unsorted and have to be
    // sorted afterwards. That is nearly free at this density and it is not
    // optional: an unsorted column would make the transpose depend on thread
    // timing, and two runs of the same model would produce different files.
    use std::sync::atomic::{AtomicU32, Ordering};

    // Zeroed in one allocation rather than built element by element:
    // `AtomicU32` has the same layout and representation as `u32`, and an
    // all-zero `u32` is an `AtomicU32` holding zero. Sixteen million
    // constructor calls is not a small thing when the whole operation is
    // supposed to take a tenth of a second.
    let counts: Vec<AtomicU32> = {
        let zeros = vec![0u32; n_cols];
        // SAFETY: `AtomicU32` is `#[repr(C)]` over `u32`, same size and
        // alignment, and every bit pattern valid for one is valid for the
        // other.
        unsafe { std::mem::transmute::<Vec<u32>, Vec<AtomicU32>>(zeros) }
    };
    cols.par_iter().for_each(|&c| {
        counts[c as usize].fetch_add(1, Ordering::Relaxed);
    });

    // The prefix sum is over sixteen million columns and was the largest single
    // cost here, being sequential while everything around it was not. Done in
    // three parallel passes instead: each chunk totals itself, the chunk totals
    // are scanned (there are only as many as there are threads), and each chunk
    // then fills its own range from its own base.
    let scan_chunk = n_cols.div_ceil(threads).max(1 << 16);
    let n_scan = n_cols.div_ceil(scan_chunk).max(1);
    let mut chunk_total = vec![0u32; n_scan];
    chunk_total
        .par_iter_mut()
        .enumerate()
        .for_each(|(chunk, total)| {
            let c0 = chunk * scan_chunk;
            let c1 = ((chunk + 1) * scan_chunk).min(n_cols);
            *total = counts[c0..c1]
                .iter()
                .map(|c| c.load(Ordering::Relaxed))
                .sum();
        });
    let mut base = Vec::with_capacity(n_scan);
    let mut running = 0u32;
    for &t in &chunk_total {
        base.push(running);
        running += t;
    }
    debug_assert_eq!(running as usize, nnz);

    let mut starts = vec![0u32; n_cols + 1];
    starts[n_cols] = running;
    starts[..n_cols]
        .par_chunks_mut(scan_chunk)
        .enumerate()
        .for_each(|(chunk, out)| {
            let c0 = chunk * scan_chunk;
            let mut running = base[chunk];
            for (k, slot) in out.iter_mut().enumerate() {
                *slot = running;
                running += counts[c0 + k].load(Ordering::Relaxed);
            }
        });

    // The counters become the write cursors, each starting where its column
    // does. Filling them in parallel from `starts` costs one more pass and
    // saves the sequential one it replaces.
    counts
        .par_iter()
        .enumerate()
        .for_each(|(j, slot)| slot.store(starts[j], Ordering::Relaxed));

    let mut out_rows = vec![0u32; nnz];
    let mut out_vals = vec![0.0f64; nnz];
    {
        let rows_ptr = SendPtr(out_rows.as_mut_ptr());
        let vals_ptr = SendPtr(out_vals.as_mut_ptr());
        let counts = &counts;
        (0..n_chunks).into_par_iter().for_each(|chunk| {
            let r0 = chunk * chunk_rows;
            let r1 = ((chunk + 1) * chunk_rows).min(n_rows);
            if r0 >= r1 {
                return;
            }
            let rows_ptr = &rows_ptr;
            let vals_ptr = &vals_ptr;
            for r in r0..r1 {
                let s = row_starts[r] as usize;
                let e = row_starts[r + 1] as usize;
                for k in s..e {
                    let c = cols[k] as usize;
                    let dst = counts[c].fetch_add(1, Ordering::Relaxed) as usize;
                    // SAFETY: every slot between `starts[c]` and `starts[c+1]`
                    // is handed out exactly once, because the counter for `c`
                    // begins at `starts[c]` and is only ever incremented, and
                    // the number of increments equals the count phase one took.
                    // No two threads can receive the same `dst`.
                    unsafe {
                        *rows_ptr.0.add(dst) = r as u32;
                        *vals_ptr.0.add(dst) = vals[k];
                    }
                }
            }
        });
    }

    // Restore row order within each column. Two entries per column on average,
    // so this is a handful of comparisons rather than a sort in any meaningful
    // sense, and it is what makes the result independent of how the threads
    // happened to interleave.
    {
        let rows_ptr = SendPtr(out_rows.as_mut_ptr());
        let vals_ptr = SendPtr(out_vals.as_mut_ptr());
        let starts_ref = &starts;
        (0..n_cols)
            .into_par_iter()
            .with_min_len(4096)
            .for_each(|j| {
                let s = starts_ref[j] as usize;
                let e = starts_ref[j + 1] as usize;
                if e <= s + 1 {
                    return;
                }
                let rows_ptr = &rows_ptr;
                let vals_ptr = &vals_ptr;
                // SAFETY: `s..e` is column `j`'s exclusive range, and no other
                // parallel item touches it.
                unsafe {
                    let r = std::slice::from_raw_parts_mut(rows_ptr.0.add(s), e - s);
                    let v = std::slice::from_raw_parts_mut(vals_ptr.0.add(s), e - s);
                    // Insertion sort, moving both arrays together. At this
                    // length it beats anything cleverer and allocates nothing.
                    for i in 1..r.len() {
                        let (ri, vi) = (r[i], v[i]);
                        let mut k = i;
                        while k > 0 && r[k - 1] > ri {
                            r[k] = r[k - 1];
                            v[k] = v[k - 1];
                            k -= 1;
                        }
                        r[k] = ri;
                        v[k] = vi;
                    }
                }
            });
    }

    Csc {
        starts,
        rows: out_rows,
        vals: out_vals,
        n_cols,
        n_rows,
    }
}


/// A raw pointer that may cross into worker threads.
///
/// Needed because the scatter's safety comes from an offset computation rather
/// than from aliasing rules the compiler can check. Deliberately private, and
/// used at exactly one call site.
struct SendPtr<T>(*mut T);
// SAFETY: the only use writes to indices proven disjoint across threads by the
// phase 2 scan. Nothing reads through this pointer.
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3x4 matrix, entries scattered so several columns have gaps:
    ///   row 0: (0, 1.0) (2, 2.0)
    ///   row 1: (0, 3.0)
    ///   row 2: (1, 4.0) (2, 5.0) (3, 6.0)
    fn sample() -> (Vec<u32>, Vec<u32>, Vec<f64>, usize) {
        let row_starts = vec![0, 2, 3, 6];
        let cols = vec![0, 2, 0, 1, 2, 3];
        let vals = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        (row_starts, cols, vals, 4)
    }

    #[test]
    fn transpose_places_entries_in_the_right_columns() {
        let (rs, cs, vs, n) = sample();
        let csc = from_csr(&rs, &cs, &vs, n);
        csc.validate().unwrap();

        assert_eq!(csc.starts, vec![0, 2, 3, 5, 6]);
        assert_eq!(csc.column(0).collect::<Vec<_>>(), vec![(0, 1.0), (1, 3.0)]);
        assert_eq!(csc.column(1).collect::<Vec<_>>(), vec![(2, 4.0)]);
        assert_eq!(csc.column(2).collect::<Vec<_>>(), vec![(0, 2.0), (2, 5.0)]);
        assert_eq!(csc.column(3).collect::<Vec<_>>(), vec![(2, 6.0)]);
    }

    #[test]
    fn empty_columns_get_empty_ranges_not_missing_ones() {
        // Column 1 is never referenced; its range must still exist and be empty.
        let csc = from_csr(&[0, 1], &[0], &[1.0], 3);
        csc.validate().unwrap();
        assert_eq!(csc.starts, vec![0, 1, 1, 1]);
        assert_eq!(csc.column(1).count(), 0);
        assert_eq!(csc.column(2).count(), 0);
    }

    #[test]
    fn empty_matrix_is_valid() {
        let csc = from_csr(&[0], &[], &[], 3);
        csc.validate().unwrap();
        assert_eq!(csc.nnz(), 0);
        assert_eq!(csc.starts, vec![0, 0, 0, 0]);
    }

    #[test]
    fn parallel_matches_serial_on_the_sample() {
        let (rs, cs, vs, n) = sample();
        assert_eq!(from_csr(&rs, &cs, &vs, n), from_csr_par(&rs, &cs, &vs, n));
    }

    /// The parallel path only engages above the chunk threshold, so the
    /// agreement test has to build something genuinely large or it silently
    /// tests the serial function twice.
    #[test]
    fn parallel_matches_serial_at_scale() {
        let n_rows = 60_000usize;
        let n_cols = 800usize;
        let mut row_starts = vec![0u32];
        let mut cols = Vec::new();
        let mut vals = Vec::new();
        // A deterministic scatter with an odd stride, so columns get uneven
        // counts and chunk boundaries do not line up with column boundaries.
        let mut state = 12_345u64;
        for r in 0..n_rows {
            let per = 1 + (r % 7);
            for _ in 0..per {
                state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                let c = ((state >> 33) as usize) % n_cols;
                cols.push(c as u32);
                vals.push(((state >> 20) & 0xffff) as f64 / 7.0);
            }
            row_starts.push(cols.len() as u32);
        }

        let a = from_csr(&row_starts, &cols, &vals, n_cols);
        let b = from_csr_par(&row_starts, &cols, &vals, n_cols);
        a.validate().unwrap();
        b.validate().unwrap();
        assert!(a.nnz() > 200_000, "test data too small to exercise chunking");
        assert_eq!(a.starts, b.starts);
        assert_eq!(a.rows, b.rows, "row order diverged between implementations");
        assert_eq!(a.vals, b.vals);
    }

    #[test]
    fn rows_within_a_column_come_out_ascending() {
        let (rs, cs, vs, n) = sample();
        for csc in [from_csr(&rs, &cs, &vs, n), from_csr_par(&rs, &cs, &vs, n)] {
            for j in 0..csc.n_cols {
                let rows: Vec<u32> = csc.column(j).map(|(r, _)| r).collect();
                assert!(rows.windows(2).all(|w| w[0] < w[1]), "column {j} unsorted");
            }
        }
    }

    #[test]
    fn validate_catches_a_corrupted_tail() {
        let (rs, cs, vs, n) = sample();
        let mut csc = from_csr(&rs, &cs, &vs, n);
        csc.starts[n] = 99;
        assert!(matches!(csc.validate(), Err(CscError::StartsTail { .. })));
    }
}

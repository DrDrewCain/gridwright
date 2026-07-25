//! Sparse LU factorisation of a basis.
//!
//! The dense inverse this replaces costs `m²` memory and `O(m²)` per pivot,
//! measured at 33 ms for 216 rows and 1.2 s for 864: about `O(m^2.7)`, which is
//! the factorisation showing through rather than anything about the machine.
//! That ceiling is what stands between this solver and being able to answer
//! anything real inside a browser.
//!
//! A basis matrix drawn from a power system model is extremely sparse. Each
//! column is a variable's appearances across the constraints it touches, and a
//! generator appears in one balance row, a line in two balance rows and one
//! flow row. Holding the inverse densely throws that structure away
//! immediately: `B⁻¹` is dense even when `B` is not, which is precisely why
//! production solvers factor rather than invert.
//!
//! # What is computed
//!
//! `P B = L U`, with `L` unit lower triangular and `U` upper triangular, both
//! stored column-sparse. Only rows are permuted; columns are pre-ordered once,
//! by ascending nonzero count, which is a cheap stand-in for the fill-reducing
//! ordering a production code would compute symbolically. It is not optimal and
//! it is a great deal better than natural order on these matrices.
//!
//! Pivots are chosen by threshold partial pivoting: among the candidates within
//! a factor of the largest available, take the one that will fill in least.
//! Pure "largest wins" is the most stable and produces the most fill; pure
//! "least fill" is unstable. The threshold is the usual compromise and the
//! reason it is a parameter rather than a constant.
//!
//! # Solving
//!
//! ```text
//!   B x = b     ⟺   L U x = P b        forward then back substitution
//!   Bᵀ y = c    ⟺   Uᵀ Lᵀ (P y) = c    the same two, transposed and reversed
//! ```
//!
//! Both cost time proportional to the nonzeros in the factors rather than to
//! `m²`.

/// Threshold for accepting a pivot smaller than the column's largest entry.
///
/// A candidate qualifies at a tenth of the maximum. Tighter values force
/// larger pivots and more fill; looser ones invite the growth that makes a
/// factorisation quietly stop representing the matrix.
const PIVOT_THRESHOLD: f64 = 0.1;

/// Below this a pivot is treated as zero and the matrix as singular.
const TINY: f64 = 1e-11;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum LuError {
    #[error("basis matrix is singular: no usable pivot for column {0}")]
    Singular(usize),
    #[error("expected {want} columns, got {got}")]
    WrongSize { want: usize, got: usize },
}

/// One triangular factor, stored as sparse columns.
#[derive(Debug, Clone, Default)]
struct Tri {
    /// Where each column starts in `rows` and `vals`.
    start: Vec<usize>,
    rows: Vec<usize>,
    vals: Vec<f64>,
}

impl Tri {
    fn with_columns(n: usize) -> Self {
        Self {
            start: Vec::with_capacity(n + 1),
            rows: Vec::new(),
            vals: Vec::new(),
        }
    }

    fn open(&mut self) {
        self.start.push(self.rows.len());
    }

    fn push(&mut self, row: usize, val: f64) {
        self.rows.push(row);
        self.vals.push(val);
    }

    fn close(&mut self) {
        self.start.push(self.rows.len());
        // The trailing sentinel is rewritten by the next `open`, so it only
        // matters after the final column.
        self.start.pop();
    }

    #[inline]
    fn column(&self, j: usize) -> (&[usize], &[f64]) {
        let s = self.start[j];
        let e = self.start.get(j + 1).copied().unwrap_or(self.rows.len());
        (&self.rows[s..e], &self.vals[s..e])
    }

    fn nonzeros(&self) -> usize {
        self.rows.len()
    }
}

/// `P B = L U`, with the factors held sparsely.
#[derive(Debug, Clone)]
pub struct Lu {
    m: usize,
    /// Unit lower triangular, without its diagonal.
    l: Tri,
    /// Upper triangular, including its diagonal as the last entry of each
    /// column.
    u: Tri,
    /// `perm[k]` is the original row that became row `k`.
    perm: Vec<usize>,
    /// The inverse of the above, for scattering a right-hand side.
    inv_perm: Vec<usize>,
    /// Original column index of each factorised column, since columns are
    /// reordered before elimination.
    col_order: Vec<usize>,
    /// Where each original column ended up.
    col_rank: Vec<usize>,
}

impl Lu {
    pub fn size(&self) -> usize {
        self.m
    }

    /// Nonzeros in both factors, which is what the solve cost is proportional
    /// to and the number worth watching when fill gets out of hand.
    pub fn nonzeros(&self) -> usize {
        self.l.nonzeros() + self.u.nonzeros()
    }

    /// The identity, factorised trivially.
    pub fn identity(m: usize) -> Self {
        let mut u = Tri::with_columns(m);
        for j in 0..m {
            u.open();
            u.push(j, 1.0);
        }
        u.close();
        let mut l = Tri::with_columns(m);
        for _ in 0..m {
            l.open();
        }
        l.close();
        Self {
            m,
            l,
            u,
            perm: (0..m).collect(),
            inv_perm: (0..m).collect(),
            col_order: (0..m).collect(),
            col_rank: (0..m).collect(),
        }
    }

    /// Factor a basis given as sparse columns.
    pub fn factor(m: usize, cols: &[Vec<(usize, f64)>]) -> Result<Self, LuError> {
        if cols.len() != m {
            return Err(LuError::WrongSize {
                want: m,
                got: cols.len(),
            });
        }

        // Columns are eliminated sparsest first. A column with two entries can
        // only fill in two rows, so taking it early keeps the active submatrix
        // small for longer. This is a heuristic standing in for a symbolic
        // ordering, and it is cheap enough to be worth doing every
        // refactorisation.
        let mut col_order: Vec<usize> = (0..m).collect();
        col_order.sort_by_key(|&j| cols[j].len());

        // The active submatrix, as sparse columns that shrink as rows are
        // eliminated.
        let mut active: Vec<Vec<(usize, f64)>> =
            col_order.iter().map(|&j| cols[j].clone()).collect();
        // How many active entries each row still has, for the fill estimate.
        let mut row_count = vec![0usize; m];
        for col in &active {
            for &(r, v) in col {
                if v != 0.0 {
                    row_count[r] += 1;
                }
            }
        }

        let mut l = Tri::with_columns(m);
        let mut u = Tri::with_columns(m);
        let mut perm = Vec::with_capacity(m);
        let mut eliminated = vec![false; m];
        // Dense accumulator, reused across columns. Sparse in, dense in the
        // middle, sparse out: the classic arrangement, because random access
        // into a sparse column during elimination is what makes a naive sparse
        // LU slower than a dense one.
        let mut work = vec![0.0f64; m];
        let mut touched: Vec<usize> = Vec::with_capacity(m);
        // Where each eliminated row sits in the pivot sequence.
        let mut pivot_rank = vec![usize::MAX; m];

        for k in 0..m {
            // Scatter the column, applying the eliminations already performed.
            touched.clear();
            for &(r, v) in &active[k] {
                if v == 0.0 {
                    continue;
                }
                if work[r] == 0.0 {
                    touched.push(r);
                }
                work[r] += v;
            }

            // Apply previous pivots in order. Each one eliminates its row from
            // this column and pushes a multiple of its L column into the rest.
            for step in 0..k {
                let pr = perm[step];
                let f = work[pr];
                if f == 0.0 {
                    continue;
                }
                let (rows, vals) = l.column(step);
                for (idx, &r) in rows.iter().enumerate() {
                    if work[r] == 0.0 {
                        touched.push(r);
                    }
                    work[r] -= f * vals[idx];
                }
            }

            // Choose a pivot among the rows not yet eliminated: within a
            // threshold of the largest, the one whose row is sparsest.
            let mut largest = 0.0f64;
            for &r in &touched {
                if !eliminated[r] {
                    largest = largest.max(work[r].abs());
                }
            }
            if largest < TINY {
                return Err(LuError::Singular(col_order[k]));
            }
            let floor = largest * PIVOT_THRESHOLD;
            let mut pivot_row = usize::MAX;
            let mut best_fill = usize::MAX;
            for &r in &touched {
                if eliminated[r] || work[r].abs() < floor {
                    continue;
                }
                let fill = row_count[r];
                if fill < best_fill {
                    best_fill = fill;
                    pivot_row = r;
                }
            }
            if pivot_row == usize::MAX {
                return Err(LuError::Singular(col_order[k]));
            }

            let pivot = work[pivot_row];
            perm.push(pivot_row);
            pivot_rank[pivot_row] = k;
            eliminated[pivot_row] = true;

            // U takes the entries in already-eliminated rows plus the pivot;
            // L takes the rest, scaled by the pivot.
            u.open();
            l.open();
            // Sorted so that the triangular solves walk rows predictably and
            // so that the factorisation is reproducible.
            touched.sort_unstable();
            for &r in &touched {
                let v = work[r];
                work[r] = 0.0;
                if v == 0.0 {
                    continue;
                }
                if r == pivot_row {
                    continue;
                }
                if eliminated[r] {
                    u.push(r, v);
                } else {
                    l.push(r, v / pivot);
                    row_count[r] = row_count[r].saturating_sub(1);
                }
            }
            // The diagonal goes last, which is what lets the back substitution
            // find it without searching.
            u.push(pivot_row, pivot);
            work[pivot_row] = 0.0;
        }
        u.close();
        l.close();

        let mut inv_perm = vec![0usize; m];
        for (rank, &row) in perm.iter().enumerate() {
            inv_perm[row] = rank;
        }
        let mut col_rank = vec![0usize; m];
        for (rank, &j) in col_order.iter().enumerate() {
            col_rank[j] = rank;
        }

        Ok(Self {
            m,
            l,
            u,
            perm,
            inv_perm,
            col_order,
            col_rank,
        })
    }

    /// Solve `B x = b`.
    pub fn solve(&self, b: &[f64]) -> Vec<f64> {
        let m = self.m;
        // Forward substitution against L, in pivot order.
        let mut y = b.to_vec();
        for k in 0..m {
            let pr = self.perm[k];
            let f = y[pr];
            if f == 0.0 {
                continue;
            }
            let (rows, vals) = self.l.column(k);
            for (idx, &r) in rows.iter().enumerate() {
                y[r] -= f * vals[idx];
            }
        }

        // Back substitution against U. Column `k`'s diagonal is its last
        // entry, and the rest of the column refers to rows eliminated earlier.
        let mut x = vec![0.0; m];
        for k in (0..m).rev() {
            let pr = self.perm[k];
            let (rows, vals) = self.u.column(k);
            let diag = *vals.last().expect("every U column has a diagonal");
            let v = y[pr] / diag;
            if v != 0.0 {
                for idx in 0..rows.len() - 1 {
                    y[rows[idx]] -= v * vals[idx];
                }
            }
            // Column `k` of the factorisation is original column
            // `col_order[k]`, so the solution component belongs there.
            x[self.col_order[k]] = v;
        }
        x
    }

    /// Solve `Bᵀ y = c`.
    pub fn solve_transpose(&self, c: &[f64]) -> Vec<f64> {
        let m = self.m;
        // `Bᵀ = Uᵀ Lᵀ P`, so forward solve against Uᵀ first.
        let mut z = vec![0.0; m];
        for k in 0..m {
            let (rows, vals) = self.u.column(k);
            let diag = *vals.last().expect("every U column has a diagonal");
            let mut acc = c[self.col_order[k]];
            for idx in 0..rows.len() - 1 {
                acc -= vals[idx] * z[rows[idx]];
            }
            z[self.perm[k]] = acc / diag;
        }

        // Then Lᵀ, backwards through the pivot sequence.
        let mut y = z;
        for k in (0..m).rev() {
            let (rows, vals) = self.l.column(k);
            let mut acc = y[self.perm[k]];
            for (idx, &r) in rows.iter().enumerate() {
                acc -= vals[idx] * y[r];
            }
            y[self.perm[k]] = acc;
        }
        y
    }

    /// Where original column `j` sits in the pivot sequence.
    pub fn rank_of_column(&self, j: usize) -> usize {
        self.col_rank[j]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dense solve, for comparison. Gaussian elimination with full partial
    /// pivoting, written independently of the code under test so that a shared
    /// mistake cannot make both agree.
    fn dense_solve(m: usize, cols: &[Vec<(usize, f64)>], b: &[f64]) -> Option<Vec<f64>> {
        let mut a = vec![0.0f64; m * m];
        for (j, col) in cols.iter().enumerate() {
            for &(r, v) in col {
                a[r * m + j] += v;
            }
        }
        let mut rhs = b.to_vec();
        for c in 0..m {
            let mut best = c;
            for r in (c + 1)..m {
                if a[r * m + c].abs() > a[best * m + c].abs() {
                    best = r;
                }
            }
            if a[best * m + c].abs() < 1e-12 {
                return None;
            }
            if best != c {
                for k in 0..m {
                    a.swap(c * m + k, best * m + k);
                }
                rhs.swap(c, best);
            }
            let p = a[c * m + c];
            for r in (c + 1)..m {
                let f = a[r * m + c] / p;
                if f == 0.0 {
                    continue;
                }
                for k in c..m {
                    a[r * m + k] -= f * a[c * m + k];
                }
                rhs[r] -= f * rhs[c];
            }
        }
        let mut x = vec![0.0; m];
        for c in (0..m).rev() {
            let mut acc = rhs[c];
            for k in (c + 1)..m {
                acc -= a[c * m + k] * x[k];
            }
            x[c] = acc / a[c * m + c];
        }
        Some(x)
    }

    fn close(a: &[f64], b: &[f64], tol: f64) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < tol)
    }

    /// Deterministic pseudo-random sparse matrix, guaranteed nonsingular by a
    /// dominant diagonal.
    fn sparse(m: usize, seed: u64) -> Vec<Vec<(usize, f64)>> {
        let mut state = seed | 1;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        (0..m)
            .map(|j| {
                let mut col = vec![(j, 4.0 + (next() % 5) as f64)];
                for _ in 0..3 {
                    let r = (next() % m as u64) as usize;
                    if r != j {
                        let v = ((next() % 200) as f64 - 100.0) / 100.0;
                        if v != 0.0 {
                            col.push((r, v));
                        }
                    }
                }
                col.sort_by_key(|&(r, _)| r);
                col.dedup_by_key(|e| e.0);
                col
            })
            .collect()
    }

    #[test]
    fn the_identity_factors_to_itself() {
        let lu = Lu::identity(4);
        let b = [1.0, -2.0, 3.0, 0.5];
        assert!(close(&lu.solve(&b), &b, 1e-12));
        assert!(close(&lu.solve_transpose(&b), &b, 1e-12));
    }

    #[test]
    fn a_permutation_matrix_is_inverted_by_its_transpose() {
        // The case a permutation bug survives every symmetric test and cannot
        // survive this one.
        let cols = vec![
            vec![(2usize, 1.0)],
            vec![(0usize, 1.0)],
            vec![(1usize, 1.0)],
        ];
        let lu = Lu::factor(3, &cols).unwrap();
        let b = [7.0, 8.0, 9.0];
        let x = lu.solve(&b);
        // Column 0 puts its weight in row 2, so x[0] must answer b[2].
        assert!(close(&x, &[9.0, 7.0, 8.0], 1e-12), "{x:?}");
    }

    #[test]
    fn a_solve_agrees_with_dense_elimination() {
        for m in [1usize, 2, 3, 5, 9, 17, 40, 97] {
            let cols = sparse(m, 0x9E3779B97F4A7C15 ^ m as u64);
            let b: Vec<f64> = (0..m).map(|i| ((i * 37) % 23) as f64 - 11.0).collect();
            let want = dense_solve(m, &cols, &b).expect("the diagonal keeps it nonsingular");
            let lu = Lu::factor(m, &cols).unwrap();
            let got = lu.solve(&b);
            assert!(close(&got, &want, 1e-8), "m = {m}\n got {got:?}\nwant {want:?}");
        }
    }

    #[test]
    fn a_transposed_solve_agrees_with_dense_elimination() {
        for m in [1usize, 2, 3, 5, 9, 17, 40, 97] {
            let cols = sparse(m, 0xD1B54A32D192ED03 ^ m as u64);
            // Transposing the matrix and solving forward is the independent
            // way to compute what `solve_transpose` claims.
            let mut rows: Vec<Vec<(usize, f64)>> = vec![Vec::new(); m];
            for (j, col) in cols.iter().enumerate() {
                for &(r, v) in col {
                    rows[r].push((j, v));
                }
            }
            let c: Vec<f64> = (0..m).map(|i| ((i * 19) % 17) as f64 - 8.0).collect();
            let want = dense_solve(m, &rows, &c).unwrap();
            let lu = Lu::factor(m, &cols).unwrap();
            let got = lu.solve_transpose(&c);
            assert!(close(&got, &want, 1e-8), "m = {m}\n got {got:?}\nwant {want:?}");
        }
    }

    #[test]
    fn a_singular_matrix_is_reported_rather_than_producing_nonsense() {
        // Two identical columns have no inverse, and returning something
        // finite would be worse than failing.
        let cols = vec![
            vec![(0usize, 1.0), (1usize, 2.0)],
            vec![(0usize, 1.0), (1usize, 2.0)],
        ];
        assert!(matches!(Lu::factor(2, &cols), Err(LuError::Singular(_))));

        // An empty column is the other way to be singular.
        let cols = vec![vec![(0usize, 1.0)], Vec::new()];
        assert!(matches!(Lu::factor(2, &cols), Err(LuError::Singular(_))));
    }

    /// A banded matrix, which is what a basis drawn from a power system model
    /// actually looks like: a generator touches one balance row, a line touches
    /// the two buses it joins and its own flow row, and a storage unit touches
    /// its own level in adjacent snapshots. Connectivity is local, so the
    /// nonzeros sit near the diagonal.
    fn banded(m: usize, half_width: usize) -> Vec<Vec<(usize, f64)>> {
        (0..m)
            .map(|j| {
                let lo = j.saturating_sub(half_width);
                let hi = (j + half_width).min(m - 1);
                (lo..=hi)
                    .map(|r| {
                        let v = if r == j {
                            4.0
                        } else {
                            -1.0 / (1 + r.abs_diff(j)) as f64
                        };
                        (r, v)
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn the_factors_stay_sparse_on_a_matrix_shaped_like_a_real_basis() {
        // The entire point. A dense inverse of a 400 by 400 basis is 160,000
        // numbers whatever the matrix looks like; the factors of a banded one
        // should stay proportional to its bandwidth.
        let m = 400;
        let cols = banded(m, 2);
        let input: usize = cols.iter().map(Vec::len).sum();
        let lu = Lu::factor(m, &cols).unwrap();
        assert!(
            lu.nonzeros() < 4 * input,
            "{} nonzeros against an input of {input} and a dense {}",
            lu.nonzeros(),
            m * m
        );
        assert!(lu.nonzeros() < m * m / 40);
    }

    #[test]
    fn fill_on_an_unstructured_matrix_is_worse_and_still_beats_a_dense_inverse() {
        // Worth pinning rather than discovering. A random sparse matrix has no
        // locality for an ordering to exploit, so it fills in badly: this is a
        // property of sparse elimination, not of this implementation. It still
        // beats holding the inverse, which is the comparison that matters,
        // and a real basis looks like the banded case above rather than this
        // one.
        let m = 200;
        let cols = sparse(m, 12345);
        let lu = Lu::factor(m, &cols).unwrap();
        assert!(
            lu.nonzeros() < m * m / 3,
            "{} nonzeros against a dense {}",
            lu.nonzeros(),
            m * m
        );
    }

    #[test]
    fn a_banded_solve_agrees_with_dense_elimination() {
        for m in [4usize, 11, 50, 200] {
            let cols = banded(m, 3);
            let b: Vec<f64> = (0..m).map(|i| ((i * 13) % 29) as f64 - 14.0).collect();
            let want = dense_solve(m, &cols, &b).unwrap();
            let got = Lu::factor(m, &cols).unwrap().solve(&b);
            assert!(close(&got, &want, 1e-8), "m = {m}");
        }
    }

    #[test]
    fn a_wrong_column_count_is_refused() {
        assert!(matches!(
            Lu::factor(3, &[vec![(0usize, 1.0)]]),
            Err(LuError::WrongSize { want: 3, got: 1 })
        ));
    }
}

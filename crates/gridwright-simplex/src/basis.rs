//! The basis inverse.
//!
//! Held explicitly and densely as `m × m`. That is the decision which sets the
//! size of problem this solver suits: `m²` memory and `O(m²)` per pivot is
//! comfortable to a few thousand rows and hopeless past that. A production
//! simplex keeps a sparse LU factorisation with Forrest-Tomlin updates instead.
//!
//! This is deliberate rather than an oversight. The solver exists to run inside
//! a browser on interactive models, where a few thousand rows is a large model
//! and being obviously correct is worth more than being fast. Large problems go
//! to HiGHS through the same [`Solver`](../../gridwright_solve/trait.Solver.html)
//! trait, so the choice is a build flag rather than a rewrite.

// Row loops here index several parallel arrays at once (basis, direction,
// values, bounds). Iterating one of them and indexing the rest reads worse
// than indexing all of them by the row number they share.
#![allow(clippy::needless_range_loop)]

/// An explicit inverse of the basis matrix.
#[derive(Debug, Clone)]
pub struct Basis {
    m: usize,
    /// Row-major `m × m`, holding `B⁻¹`.
    inv: Vec<f64>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum BasisError {
    #[error("basis matrix is singular at column {0}")]
    Singular(usize),
    #[error("pivot {pivot} in row {row} is too small to use safely")]
    TinyPivot { row: usize, pivot: f64 },
    #[error("expected {want} entries, got {got}")]
    WrongSize { want: usize, got: usize },
}

impl Basis {
    /// The identity, which inverts the all-slack starting basis.
    pub fn identity(m: usize) -> Self {
        let mut inv = vec![0.0; m * m];
        for i in 0..m {
            inv[i * m + i] = 1.0;
        }
        Self { m, inv }
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.m
    }

    /// Invert a basis from its columns by Gauss-Jordan with partial pivoting.
    ///
    /// Used to refactorise. Rank-one updates accumulate rounding error, and
    /// after enough of them the matrix quietly stops being the inverse of
    /// anything; rebuilding from the original columns is the standard remedy
    /// and the reason `refactor_every` exists.
    pub fn from_columns(m: usize, cols: &[Vec<(usize, f64)>]) -> Result<Self, BasisError> {
        if cols.len() != m {
            return Err(BasisError::WrongSize {
                want: m,
                got: cols.len(),
            });
        }
        let mut a = vec![0.0; m * m];
        for (c, col) in cols.iter().enumerate() {
            for &(r, v) in col {
                if r >= m {
                    return Err(BasisError::Singular(c));
                }
                a[r * m + c] += v;
            }
        }
        let mut inv = Self::identity(m).inv;

        for c in 0..m {
            let mut best = c;
            let mut best_abs = a[c * m + c].abs();
            for r in (c + 1)..m {
                let v = a[r * m + c].abs();
                if v > best_abs {
                    best_abs = v;
                    best = r;
                }
            }
            if best_abs < 1e-12 {
                return Err(BasisError::Singular(c));
            }
            if best != c {
                for k in 0..m {
                    a.swap(c * m + k, best * m + k);
                    inv.swap(c * m + k, best * m + k);
                }
            }
            let scale = 1.0 / a[c * m + c];
            for k in 0..m {
                a[c * m + k] *= scale;
                inv[c * m + k] *= scale;
            }
            for r in 0..m {
                if r == c {
                    continue;
                }
                let f = a[r * m + c];
                if f == 0.0 {
                    continue;
                }
                for k in 0..m {
                    a[r * m + k] -= f * a[c * m + k];
                    inv[r * m + k] -= f * inv[c * m + k];
                }
            }
        }
        Ok(Self { m, inv })
    }

    /// `B⁻¹ b`, used for basic values and for the entering column's direction.
    pub fn solve(&self, b: &[f64]) -> Result<Vec<f64>, BasisError> {
        if b.len() != self.m {
            return Err(BasisError::WrongSize {
                want: self.m,
                got: b.len(),
            });
        }
        let mut out = vec![0.0; self.m];
        for r in 0..self.m {
            let row = &self.inv[r * self.m..(r + 1) * self.m];
            out[r] = row.iter().zip(b).map(|(a, x)| a * x).sum();
        }
        Ok(out)
    }

    /// `(B⁻¹)ᵀ c`, which is `y` in `Bᵀ y = c_B`: the duals.
    pub fn solve_transpose(&self, c: &[f64]) -> Result<Vec<f64>, BasisError> {
        if c.len() != self.m {
            return Err(BasisError::WrongSize {
                want: self.m,
                got: c.len(),
            });
        }
        let mut out = vec![0.0; self.m];
        for r in 0..self.m {
            let cr = c[r];
            if cr == 0.0 {
                continue;
            }
            let row = &self.inv[r * self.m..(r + 1) * self.m];
            for (k, &a) in row.iter().enumerate() {
                out[k] += a * cr;
            }
        }
        Ok(out)
    }

    /// Update after the basic variable in `row` is replaced.
    ///
    /// `direction` is `B⁻¹ a_q` for the entering column, already computed for
    /// the ratio test, so this costs one pass rather than a fresh inversion.
    pub fn update(&mut self, row: usize, direction: &[f64]) -> Result<(), BasisError> {
        if direction.len() != self.m {
            return Err(BasisError::WrongSize {
                want: self.m,
                got: direction.len(),
            });
        }
        let pivot = direction[row];
        if pivot.abs() < 1e-11 {
            return Err(BasisError::TinyPivot { row, pivot });
        }
        let scale = 1.0 / pivot;
        for k in 0..self.m {
            self.inv[row * self.m + k] *= scale;
        }
        for r in 0..self.m {
            if r == row {
                continue;
            }
            let f = direction[r];
            if f == 0.0 {
                continue;
            }
            for k in 0..self.m {
                self.inv[r * self.m + k] -= f * self.inv[row * self.m + k];
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(e: &[(usize, f64)]) -> Vec<(usize, f64)> {
        e.to_vec()
    }

    #[test]
    fn the_identity_solves_to_itself() {
        let b = Basis::identity(3);
        assert_eq!(b.solve(&[1.0, 2.0, 3.0]).unwrap(), vec![1.0, 2.0, 3.0]);
        assert_eq!(
            b.solve_transpose(&[1.0, 2.0, 3.0]).unwrap(),
            vec![1.0, 2.0, 3.0]
        );
    }

    #[test]
    fn inverts_a_diagonal_matrix() {
        let b = Basis::from_columns(2, &[col(&[(0, 2.0)]), col(&[(1, 4.0)])]).unwrap();
        let x = b.solve(&[2.0, 4.0]).unwrap();
        assert!((x[0] - 1.0).abs() < 1e-12 && (x[1] - 1.0).abs() < 1e-12);
    }

    #[test]
    fn pivots_when_the_diagonal_holds_a_zero() {
        // Without partial pivoting this looks singular and is not.
        let b = Basis::from_columns(2, &[col(&[(1, 1.0)]), col(&[(0, 1.0)])]).unwrap();
        let x = b.solve(&[3.0, 7.0]).unwrap();
        assert!((x[0] - 7.0).abs() < 1e-12, "got {x:?}");
        assert!((x[1] - 3.0).abs() < 1e-12, "got {x:?}");
    }

    #[test]
    fn a_singular_basis_is_an_error_rather_than_nonsense() {
        let e = Basis::from_columns(2, &[col(&[(0, 1.0)]), col(&[(0, 1.0)])]).unwrap_err();
        assert!(matches!(e, BasisError::Singular(_)));
    }

    #[test]
    fn transpose_solve_matches_an_explicit_transpose() {
        let b = Basis::from_columns(
            3,
            &[
                col(&[(0, 2.0), (1, 1.0)]),
                col(&[(1, 3.0), (2, 1.0)]),
                col(&[(0, 1.0), (2, 4.0)]),
            ],
        )
        .unwrap();
        let c = [1.0, -2.0, 0.5];
        let got = b.solve_transpose(&c).unwrap();
        let mut want = vec![0.0; 3];
        for k in 0..3 {
            let mut e = vec![0.0; 3];
            e[k] = 1.0;
            let colk = b.solve(&e).unwrap();
            want[k] = colk.iter().zip(&c).map(|(a, x)| a * x).sum();
        }
        for i in 0..3 {
            assert!((got[i] - want[i]).abs() < 1e-10, "got {got:?} want {want:?}");
        }
    }

    #[test]
    fn a_rank_one_update_matches_a_fresh_inversion() {
        let start = [col(&[(0, 1.0)]), col(&[(1, 1.0)]), col(&[(2, 1.0)])];
        let mut b = Basis::from_columns(3, &start).unwrap();
        let entering = vec![(0usize, 2.0), (1usize, 5.0), (2usize, 1.0)];

        let mut rhs = vec![0.0; 3];
        for &(r, v) in &entering {
            rhs[r] = v;
        }
        let direction = b.solve(&rhs).unwrap();
        b.update(1, &direction).unwrap();

        let fresh =
            Basis::from_columns(3, &[start[0].clone(), entering.clone(), start[2].clone()]).unwrap();
        let probe = [3.0, -1.0, 2.0];
        let a = b.solve(&probe).unwrap();
        let c = fresh.solve(&probe).unwrap();
        for i in 0..3 {
            assert!((a[i] - c[i]).abs() < 1e-9, "updated {a:?} vs fresh {c:?}");
        }
    }

    #[test]
    fn a_tiny_pivot_is_refused_rather_than_amplified() {
        let mut b = Basis::identity(2);
        assert!(matches!(
            b.update(0, &[1e-15, 1.0]),
            Err(BasisError::TinyPivot { .. })
        ));
    }

    #[test]
    fn a_wrong_sized_vector_is_rejected() {
        let b = Basis::identity(3);
        assert!(matches!(
            b.solve(&[1.0, 2.0]),
            Err(BasisError::WrongSize { want: 3, got: 2 })
        ));
    }
}

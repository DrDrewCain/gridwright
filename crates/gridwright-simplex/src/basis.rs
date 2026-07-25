//! The basis, factorised rather than inverted.
//!
//! # Why not the inverse
//!
//! Holding `B⁻¹` explicitly is the obvious thing and the wrong thing. `B` is
//! extremely sparse — a generator column touches one balance row, a line column
//! touches three — and its inverse is dense regardless. So an explicit inverse
//! costs `m²` memory and `O(m²)` per pivot no matter how little structure the
//! problem actually has, which is why every production simplex factors instead.
//!
//! That cost was measured before this was written: 33 ms at 216 rows, 150 ms at
//! 432, 1.2 s at 864, or roughly `O(m^2.7)`. Since a browser cannot call HiGHS,
//! that ceiling was the whole of what a page could honestly offer.
//!
//! Measured after, on the same ladder: 4.4 ms at 216 rows, 15 ms at 432, 57 ms
//! at 864. Twenty-one times faster at the top of that range, and the exponent
//! falls to about `m^1.9`. Where the dense version could not finish 2,592 rows
//! inside a ten-minute budget, the factorisation does it in 0.6 s and reaches
//! 20,736 rows in under two minutes.
//!
//! # What is held instead
//!
//! A sparse LU factorisation of the basis as it stood at the last
//! refactorisation, plus the pivots since, in product form:
//!
//! ```text
//!   B⁻¹  =  E_k · … · E_1 · B₀⁻¹
//! ```
//!
//! Each `E` is an elementary matrix differing from the identity in one column,
//! and it is stored as that column alone. Applying one costs its nonzeros
//! rather than `m²`, so a pivot no longer costs a full pass over an `m × m`
//! array.
//!
//! Product form rather than Forrest-Tomlin, which updates the factors
//! themselves. Forrest-Tomlin keeps the representation tighter over long runs
//! between refactorisations; product form is markedly simpler and the
//! refactorisation interval already bounds how far the eta list can grow. The
//! simpler one is the right trade here, and this note is the reason it was not
//! an oversight.
//!
//! # Order matters
//!
//! Forward, the etas apply after the factorisation and in the order they were
//! created. Transposed, they apply before it and in reverse. Getting that
//! backwards produces a solver that converges to the wrong answer rather than
//! one that fails, which is why both directions are checked against a fresh
//! factorisation in the tests below.

// Row loops here index several parallel arrays at once (basis, direction,
// values, bounds). Iterating one of them and indexing the rest reads worse
// than indexing all of them by the row number they share.
#![allow(clippy::needless_range_loop)]

use crate::lu::{Lu, LuError};

/// One product-form update: the elementary matrix from a single pivot.
#[derive(Debug, Clone)]
struct Eta {
    /// The row whose basic variable was replaced.
    row: usize,
    /// The entering column's direction, `B⁻¹ a_q`, at every row but the pivot.
    /// Stored sparsely because most of it is zero.
    entries: Vec<(usize, f64)>,
    /// One over the pivot, kept rather than recomputed.
    recip: f64,
}

/// The basis, as a factorisation plus its subsequent pivots.
#[derive(Debug, Clone)]
pub struct Basis {
    m: usize,
    lu: Lu,
    etas: Vec<Eta>,
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

impl From<LuError> for BasisError {
    fn from(e: LuError) -> Self {
        match e {
            LuError::Singular(c) => BasisError::Singular(c),
            LuError::WrongSize { want, got } => BasisError::WrongSize { want, got },
        }
    }
}

impl Basis {
    /// The identity, which inverts the all-slack starting basis.
    pub fn identity(m: usize) -> Self {
        Self {
            m,
            lu: Lu::identity(m),
            etas: Vec::new(),
        }
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.m
    }

    /// How many pivots have accumulated since the last factorisation.
    ///
    /// Each one lengthens every solve, so this is what a caller watches to
    /// decide when to refactorise.
    #[inline]
    pub fn updates(&self) -> usize {
        self.etas.len()
    }

    /// Nonzeros in the factors and the accumulated updates together.
    pub fn nonzeros(&self) -> usize {
        self.lu.nonzeros() + self.etas.iter().map(|e| e.entries.len()).sum::<usize>()
    }

    /// Factorise a basis from its columns.
    ///
    /// Used to refactorise. Product-form updates accumulate rounding error and
    /// lengthen every solve, so rebuilding from the original columns is both
    /// the numerical remedy and the performance one.
    pub fn from_columns(m: usize, cols: &[Vec<(usize, f64)>]) -> Result<Self, BasisError> {
        Ok(Self {
            m,
            lu: Lu::factor(m, cols)?,
            etas: Vec::new(),
        })
    }

    /// Solve `B x = b`.
    pub fn solve(&self, b: &[f64]) -> Result<Vec<f64>, BasisError> {
        if b.len() != self.m {
            return Err(BasisError::WrongSize {
                want: self.m,
                got: b.len(),
            });
        }
        let mut x = self.lu.solve(b);
        // Forward: the factorisation first, then each pivot in the order it
        // happened.
        for eta in &self.etas {
            let t = x[eta.row] * eta.recip;
            for &(r, d) in &eta.entries {
                x[r] -= d * t;
            }
            x[eta.row] = t;
        }
        Ok(x)
    }

    /// Solve `Bᵀ y = c`.
    pub fn solve_transpose(&self, c: &[f64]) -> Result<Vec<f64>, BasisError> {
        if c.len() != self.m {
            return Err(BasisError::WrongSize {
                want: self.m,
                got: c.len(),
            });
        }
        // Transposed, the whole product reverses: the last pivot applies first
        // and the factorisation last.
        let mut y = c.to_vec();
        for eta in self.etas.iter().rev() {
            let mut acc = y[eta.row];
            for &(r, d) in &eta.entries {
                acc -= d * y[r];
            }
            y[eta.row] = acc * eta.recip;
        }
        Ok(self.lu.solve_transpose(&y))
    }

    /// Record that the basic variable in `row` has been replaced.
    ///
    /// `direction` is `B⁻¹ a_q` for the entering column, already computed for
    /// the ratio test, so this costs one pass over its nonzeros rather than a
    /// fresh factorisation.
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
        let mut entries = Vec::new();
        for (r, &d) in direction.iter().enumerate() {
            if r != row && d != 0.0 {
                entries.push((r, d));
            }
        }
        self.etas.push(Eta {
            row,
            entries,
            recip: 1.0 / pivot,
        });
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

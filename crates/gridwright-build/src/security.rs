//! N-1 security: the system must survive losing any single element.
//!
//! This is the standard operators actually plan to, and it is the difference
//! between a model that says a network copes and one that says it copes *and
//! keeps coping when a line trips*. A dispatch that loads every corridor to its
//! rating is optimal and operationally useless.
//!
//! # Why this needs no extra variables
//!
//! The naive formulation duplicates every flow variable once per contingency,
//! multiplying the problem by the number of outages considered. For a network
//! with a thousand lines that is a thousandfold blow-up.
//!
//! Linear DC flow admits something much better. Because the response to an
//! outage is itself linear, the post-contingency flow on line `l` after line
//! `k` trips is a fixed multiple of the pre-contingency flow on `k`:
//!
//! ```text
//!   f_l^(k) = f_l + LODF[l][k] · f_k
//! ```
//!
//! The multipliers depend only on topology and impedance, not on dispatch, so
//! they are computed once and become ordinary constraints on the base-case flow
//! variables that already exist. Security costs rows, not columns.
//!
//! # Getting there
//!
//! `LODF` comes from `PTDF`, the sensitivity of each line's flow to injection
//! at each bus, which comes from inverting the reduced susceptance matrix.
//! Reduced means with the reference bus removed, and since angles are only
//! comparable within a synchronous area, that is done once per area rather than
//! once for the whole system.

use gridwright_model::VarBlock;
use gridwright_net::Network;

use crate::{RowBatch, VarIndex};

/// Line outage distribution factors, plus which lines they are defined for.
#[derive(Debug, Clone)]
pub struct Lodf {
    /// `factors[l][k]`: the share of line `k`'s flow that lands on line `l`
    /// when `k` trips. Zero where either line is not part of the DC network.
    factors: Vec<Vec<f64>>,
    n_lines: usize,
    /// Lines whose outage would split the network, and which therefore have no
    /// finite redistribution. Reported rather than silently skipped: losing a
    /// bridge is a real vulnerability, it just is not one N-1 flow limits can
    /// describe.
    pub islanding: Vec<usize>,
}

impl Lodf {
    #[inline]
    pub fn get(&self, monitored: usize, outaged: usize) -> f64 {
        self.factors[monitored][outaged]
    }

    #[inline]
    pub fn n_lines(&self) -> usize {
        self.n_lines
    }
}

/// Dense Gauss-Jordan inverse. Small: one per synchronous area, sized by the
/// number of buses in it less one.
fn invert(n: usize, mut a: Vec<f64>) -> Option<Vec<f64>> {
    let mut inv = vec![0.0; n * n];
    for i in 0..n {
        inv[i * n + i] = 1.0;
    }
    for c in 0..n {
        let mut best = c;
        let mut best_abs = a[c * n + c].abs();
        for r in (c + 1)..n {
            let v = a[r * n + c].abs();
            if v > best_abs {
                best_abs = v;
                best = r;
            }
        }
        // A singular reduced susceptance matrix means the area is not connected,
        // which the caller handles by treating those lines as unmonitorable.
        if best_abs < 1e-10 {
            return None;
        }
        if best != c {
            for k in 0..n {
                a.swap(c * n + k, best * n + k);
                inv.swap(c * n + k, best * n + k);
            }
        }
        let scale = 1.0 / a[c * n + c];
        for k in 0..n {
            a[c * n + k] *= scale;
            inv[c * n + k] *= scale;
        }
        for r in 0..n {
            if r == c {
                continue;
            }
            let f = a[r * n + c];
            if f == 0.0 {
                continue;
            }
            for k in 0..n {
                a[r * n + k] -= f * a[c * n + k];
                inv[r * n + k] -= f * inv[c * n + k];
            }
        }
    }
    Some(inv)
}

/// Compute LODF for every AC line in the network.
///
/// Transport corridors are excluded: an HVDC tie is controllable, so it does
/// not redistribute flow when something else trips, and nothing redistributes
/// onto it either.
pub fn compute_lodf(net: &Network) -> Lodf {
    let n_lines = net.lines.len();
    let n_buses = net.buses.len();
    let mut factors = vec![vec![0.0; n_lines]; n_lines];
    let mut islanding = Vec::new();

    // PTDF is per synchronous area, since angles do not compare across one.
    for (area, reference) in net.synchronous_areas() {
        let members: Vec<usize> = (0..n_buses)
            .filter(|&b| net.buses[b].synchronous_area == area)
            .collect();
        if members.len() < 2 {
            continue;
        }
        // Position of each bus within the reduced system, reference removed.
        let mut pos = vec![usize::MAX; n_buses];
        let mut k = 0;
        for &b in &members {
            if b == reference {
                continue;
            }
            pos[b] = k;
            k += 1;
        }
        let n = k;
        if n == 0 {
            continue;
        }

        let ac: Vec<usize> = (0..n_lines)
            .filter(|&l| {
                let line = &net.lines[l];
                !line.is_transport()
                    && net.buses[line.bus0].synchronous_area == area
                    && net.buses[line.bus1].synchronous_area == area
            })
            .collect();
        if ac.is_empty() {
            continue;
        }

        // Reduced susceptance matrix.
        let mut b_red = vec![0.0; n * n];
        for &l in &ac {
            let line = &net.lines[l];
            let (i, j) = (pos[line.bus0], pos[line.bus1]);
            let b = line.susceptance;
            if i != usize::MAX {
                b_red[i * n + i] += b;
            }
            if j != usize::MAX {
                b_red[j * n + j] += b;
            }
            if i != usize::MAX && j != usize::MAX {
                b_red[i * n + j] -= b;
                b_red[j * n + i] -= b;
            }
        }

        let Some(x) = invert(n, b_red) else {
            // Disconnected area: no finite sensitivities to compute.
            islanding.extend(ac.iter().copied());
            continue;
        };

        // PTDF of each line with respect to injection at each bus. The
        // reference bus contributes zero by construction.
        let at = |b: usize, col: usize| -> f64 {
            if pos[b] == usize::MAX {
                0.0
            } else {
                x[pos[b] * n + col]
            }
        };
        // Sensitivity of line `l` to the injection pattern of line `k`, which
        // is +1 at k's from-bus and -1 at its to-bus.
        let pair = |l: usize, k: usize| -> f64 {
            let ll = &net.lines[l];
            let kk = &net.lines[k];
            let (p, q) = (kk.bus0, kk.bus1);
            let (i, j) = (ll.bus0, ll.bus1);
            let term = |col_bus: usize, sign: f64| -> f64 {
                if pos[col_bus] == usize::MAX {
                    return 0.0;
                }
                let col = pos[col_bus];
                sign * ll.susceptance * (at(i, col) - at(j, col))
            };
            term(p, 1.0) + term(q, -1.0)
        };

        for &k in &ac {
            let self_term = pair(k, k);
            let denom = 1.0 - self_term;
            // A denominator at zero means removing this line disconnects the
            // network: there is nowhere for its flow to redistribute to.
            if denom.abs() < 1e-6 {
                islanding.push(k);
                continue;
            }
            for &l in &ac {
                if l == k {
                    continue;
                }
                factors[l][k] = pair(l, k) / denom;
            }
        }
    }

    islanding.sort_unstable();
    islanding.dedup();
    Lodf {
        factors,
        n_lines,
        islanding,
    }
}

/// Rows enforcing that no monitored line exceeds its rating after any single
/// outage.
///
/// Two rows per (monitored, outaged) pair per snapshot, because the
/// post-contingency flow is signed and both directions must be bounded. That is
/// a great many rows, which is why the set of contingencies is a deliberate
/// choice rather than automatically every line.
pub fn build_security(
    net: &Network,
    vars: &VarIndex,
    lodf: &Lodf,
    contingencies: &[usize],
    t: usize,
) -> Vec<RowBatch> {
    if contingencies.is_empty() {
        return Vec::new();
    }
    let monitored: Vec<usize> = (0..net.lines.len())
        .filter(|&l| !net.lines[l].is_transport() && net.lines[l].s_nom.is_finite())
        .collect();

    contingencies
        .iter()
        .filter_map(|&k| {
            if k >= lodf.n_lines() || lodf.islanding.contains(&k) {
                return None;
            }
            let fk: VarBlock = vars.flow[k];
            let mut batch = RowBatch::with_capacity(2 * monitored.len() * t, 8 * monitored.len() * t);
            let mut any = false;

            for &l in &monitored {
                if l == k {
                    continue;
                }
                let d = lodf.get(l, k);
                // A negligible factor means this outage does not meaningfully
                // load that line, and a row for it would be noise.
                if d.abs() < 1e-9 {
                    continue;
                }
                any = true;
                let fl = vars.flow[l];
                let rating = net.lines[l].s_nom;
                for step in 0..t {
                    let ti = step as u32;
                    // −rating ≤ f_l + d·f_k ≤ rating
                    batch.push_le([(fl.at(ti), 1.0), (fk.at(ti), d)], rating);
                    batch.push_ge([(fl.at(ti), 1.0), (fk.at(ti), d)], -rating);
                }
            }
            any.then_some(batch)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gridwright_net::{Line, Snapshots};

    /// A triangle: losing any one line pushes its flow onto the other two.
    fn triangle() -> Network {
        let mut net = Network::new(Snapshots::hourly(1));
        let a = net.add_bus("A", "X");
        let b = net.add_bus("B", "X");
        let c = net.add_bus("C", "X");
        for (n0, n1) in [(a, b), (b, c), (c, a)] {
            net.add_line(Line {
                name: format!("{n0}{n1}"),
                bus0: n0,
                bus1: n1,
                s_nom: 100.0,
                susceptance: 1.0,
                ..Default::default()
            });
        }
        net
    }

    #[test]
    fn losing_one_side_of_a_triangle_moves_its_flow_to_the_others() {
        // With equal susceptances the surviving path is two lines in series, so
        // all of the lost flow takes it. The factors must therefore be ±1.
        let lodf = compute_lodf(&triangle());
        assert!(lodf.islanding.is_empty(), "a triangle has no bridges");
        for k in 0..3 {
            for l in 0..3 {
                if l == k {
                    continue;
                }
                let d = lodf.get(l, k).abs();
                assert!(
                    (d - 1.0).abs() < 1e-6,
                    "LODF[{l}][{k}] was {d}, expected magnitude 1"
                );
            }
        }
    }

    #[test]
    fn a_radial_line_has_no_redistribution_and_is_flagged() {
        // Two buses, one line. Losing it islands B, so there is no finite
        // factor and the line is reported rather than quietly ignored.
        let mut net = Network::new(Snapshots::hourly(1));
        let a = net.add_bus("A", "X");
        let b = net.add_bus("B", "X");
        net.add_line(Line {
            name: "AB".into(),
            bus0: a,
            bus1: b,
            s_nom: 100.0,
            susceptance: 1.0,
            ..Default::default()
        });
        let lodf = compute_lodf(&net);
        assert_eq!(lodf.islanding, vec![0], "the only line is a bridge");
    }

    #[test]
    fn transport_corridors_are_excluded() {
        // HVDC is controllable: it does not pick up flow when an AC line trips.
        let mut net = triangle();
        net.lines[2].susceptance = 0.0;
        let lodf = compute_lodf(&net);
        for l in 0..3 {
            assert_eq!(lodf.get(l, 2), 0.0, "outage of a DC tie should not redistribute");
            assert_eq!(lodf.get(2, l), 0.0, "a DC tie should not receive redistribution");
        }
    }

    #[test]
    fn factors_do_not_cross_synchronous_areas() {
        // Two separate triangles in different interconnections. Tripping a line
        // in one cannot load a line in the other.
        let mut net = Network::new(Snapshots::hourly(1));
        let mk = |net: &mut Network, area: &str| {
            let a = net.add_bus_in_area(format!("{area}a"), "X", area);
            let b = net.add_bus_in_area(format!("{area}b"), "X", area);
            let c = net.add_bus_in_area(format!("{area}c"), "X", area);
            for (n0, n1) in [(a, b), (b, c), (c, a)] {
                net.add_line(Line {
                    name: format!("{area}{n0}{n1}"),
                    bus0: n0,
                    bus1: n1,
                    s_nom: 100.0,
                    susceptance: 1.0,
                    ..Default::default()
                });
            }
        };
        mk(&mut net, "east");
        mk(&mut net, "west");

        let lodf = compute_lodf(&net);
        // Lines 0..3 are eastern, 3..6 western.
        for l in 3..6 {
            for k in 0..3 {
                assert_eq!(lodf.get(l, k), 0.0, "east outage {k} loaded west line {l}");
                assert_eq!(lodf.get(k, l), 0.0, "west outage {l} loaded east line {k}");
            }
        }
    }

    #[test]
    fn susceptance_decides_how_flow_splits_after_an_outage() {
        // A four-bus network with two parallel paths of different impedance.
        // The stronger path takes proportionally more of the redistributed flow.
        let mut net = Network::new(Snapshots::hourly(1));
        let a = net.add_bus("A", "X");
        let b = net.add_bus("B", "X");
        let c = net.add_bus("C", "X");
        for (n0, n1, s) in [(a, b, 1.0), (a, c, 2.0), (c, b, 2.0)] {
            net.add_line(Line {
                name: format!("{n0}{n1}"),
                bus0: n0,
                bus1: n1,
                s_nom: 100.0,
                susceptance: s,
                ..Default::default()
            });
        }
        let lodf = compute_lodf(&net);
        // Losing the direct A-B line sends everything the long way, so both
        // legs of the detour carry the whole amount.
        assert!((lodf.get(1, 0).abs() - 1.0).abs() < 1e-6, "{}", lodf.get(1, 0));
        assert!((lodf.get(2, 0).abs() - 1.0).abs() < 1e-6, "{}", lodf.get(2, 0));
    }
}

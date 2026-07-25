//! The network: what is being modelled, before any of it becomes a matrix.
//!
//! # Memory layout
//!
//! Every time series here is stored component major, meaning component `c` at
//! snapshot `t` lives at `c * n_snapshots + t`. That choice is the reason the
//! rest of the engine is fast, and it is worth stating why, because the
//! obvious alternative looks equally reasonable.
//!
//! A dispatch variable exists for every generator at every snapshot, and the
//! model hands those out as one contiguous block per generator spanning all
//! snapshots. Availability profiles therefore have to be read in exactly that
//! order when the bounds vector is filled, and component major makes that a
//! sequential copy rather than a strided gather.
//!
//! Nodal balance appears to want the opposite: it is naturally written as a
//! loop over snapshots, and snapshot major would suit that. The resolution is
//! to parallelise balance over buses instead of over snapshots. Each bus's
//! balance rows are independent of every other bus's, exactly as each
//! snapshot's are, so either axis is legal, and choosing the bus axis means
//! every read walks memory forwards. One layout then serves both, and nothing
//! in the hot path is strided.

use std::collections::HashMap;

/// Snapshot set: the time steps the model runs over.
///
/// Weights carry the hours each snapshot represents, which is how models run
/// at reduced temporal resolution without distorting energy totals. A model
/// sampling every third hour uses weight 3.0 and its costs stay comparable to
/// an hourly run.
#[derive(Debug, Clone)]
pub struct Snapshots {
    weights: Vec<f64>,
}

impl Snapshots {
    /// `n` snapshots each representing one hour.
    pub fn hourly(n: usize) -> Self {
        Self {
            weights: vec![1.0; n],
        }
    }

    pub fn weighted(weights: Vec<f64>) -> Result<Self, NetError> {
        if weights.is_empty() {
            return Err(NetError::NoSnapshots);
        }
        if let Some(pos) = weights.iter().position(|w| !w.is_finite() || *w <= 0.0) {
            return Err(NetError::BadSnapshotWeight {
                index: pos,
                value: weights[pos],
            });
        }
        Ok(Self { weights })
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.weights.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.weights.is_empty()
    }

    #[inline]
    pub fn weight(&self, t: usize) -> f64 {
        self.weights[t]
    }

    #[inline]
    pub fn weights(&self) -> &[f64] {
        &self.weights
    }
}

/// A per-component time series, stored component major.
///
/// Empty means "no series given", which callers read as a flat default rather
/// than as zero. That distinction matters: a generator with no availability
/// profile is available at full capacity, not unavailable.
#[derive(Debug, Default, Clone)]
pub struct TimeSeries {
    data: Vec<f64>,
    n_snapshots: usize,
}

impl TimeSeries {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build from a component major buffer of `n_components * n_snapshots`.
    pub fn from_flat(
        data: Vec<f64>,
        n_components: usize,
        n_snapshots: usize,
    ) -> Result<Self, NetError> {
        let want = n_components * n_snapshots;
        if data.len() != want {
            return Err(NetError::TimeSeriesShape {
                got: data.len(),
                want,
            });
        }
        Ok(Self { data, n_snapshots })
    }

    /// Build from one row per component.
    pub fn from_rows(rows: &[Vec<f64>], n_snapshots: usize) -> Result<Self, NetError> {
        let mut data = Vec::with_capacity(rows.len() * n_snapshots);
        for (i, r) in rows.iter().enumerate() {
            if r.len() != n_snapshots {
                return Err(NetError::TimeSeriesRow {
                    component: i,
                    got: r.len(),
                    want: n_snapshots,
                });
            }
            data.extend_from_slice(r);
        }
        Ok(Self { data, n_snapshots })
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// The contiguous run of values for component `c`, or `None` if unset.
    ///
    /// Returning a slice rather than an iterator is the point of the layout:
    /// the caller copies it straight into a bounds vector.
    #[inline]
    pub fn row(&self, c: usize) -> Option<&[f64]> {
        if self.data.is_empty() {
            return None;
        }
        let s = c * self.n_snapshots;
        self.data.get(s..s + self.n_snapshots)
    }

    #[inline]
    pub fn at(&self, c: usize, t: usize) -> Option<f64> {
        if self.data.is_empty() {
            return None;
        }
        self.data.get(c * self.n_snapshots + t).copied()
    }
}

/// An electrical node. `country` is what makes the model transnational: it is
/// the axis cross-border flows are reported on.
#[derive(Debug, Clone)]
pub struct Bus {
    pub name: String,
    pub country: String,
}

/// A dispatchable or variable generator attached to one bus.
#[derive(Debug, Clone)]
pub struct Generator {
    pub name: String,
    pub bus: usize,
    /// Nameplate capacity, MW.
    pub p_nom: f64,
    /// Cost per MWh dispatched.
    pub marginal_cost: f64,
    /// Minimum output as a fraction of `p_nom`, for must-run plant.
    pub p_min_pu: f64,
}

/// A transmission link between two buses.
///
/// `susceptance` drives the DC power flow formulation. Zero or negative means
/// this link is treated as a transport corridor, free to route power up to its
/// rating without an angle relationship. Real AC lines need the angle
/// constraint; HVDC interconnectors genuinely are controllable and are
/// correctly modelled as transport.
#[derive(Debug, Clone)]
pub struct Line {
    pub name: String,
    pub bus0: usize,
    pub bus1: usize,
    /// Thermal rating, MW, applied symmetrically.
    pub s_nom: f64,
    pub susceptance: f64,
}

impl Line {
    /// Written against `is_finite` rather than as a negated comparison so the
    /// NaN case is stated rather than implied: a susceptance that is not a
    /// usable number means this link cannot carry an angle relationship, and
    /// is treated as a transport corridor.
    #[inline]
    pub fn is_transport(&self) -> bool {
        !self.susceptance.is_finite() || self.susceptance <= 0.0
    }
}

/// Inelastic demand at a bus.
#[derive(Debug, Clone)]
pub struct Load {
    pub name: String,
    pub bus: usize,
    /// Constant demand, MW, used when no profile is supplied.
    pub p_set: f64,
}

/// A store that can shift energy between snapshots.
#[derive(Debug, Clone)]
pub struct StorageUnit {
    pub name: String,
    pub bus: usize,
    /// Power rating, MW, symmetric for charge and discharge.
    pub p_nom: f64,
    /// Energy capacity expressed in hours at `p_nom`.
    pub max_hours: f64,
    pub efficiency_store: f64,
    pub efficiency_dispatch: f64,
    /// Whether the final state of charge must return to the initial one.
    ///
    /// Without this a finite horizon model simply empties the store before the
    /// end, which is free energy and makes the result meaningless.
    pub cyclic: bool,
}

/// The whole system.
#[derive(Debug, Clone)]
pub struct Network {
    pub snapshots: Snapshots,
    pub buses: Vec<Bus>,
    pub generators: Vec<Generator>,
    pub lines: Vec<Line>,
    pub loads: Vec<Load>,
    pub storage: Vec<StorageUnit>,
    /// Per-unit availability per generator per snapshot. Absent means 1.0.
    pub gen_availability: TimeSeries,
    /// Demand per load per snapshot. Absent means `Load::p_set`.
    pub load_profile: TimeSeries,
    /// Cost of shedding load, per MWh.
    ///
    /// Present so infeasibility surfaces as an expensive answer rather than as
    /// a solver status. An energy model that merely reports INFEASIBLE tells
    /// the user nothing about where or when the system failed.
    pub value_of_lost_load: f64,
}

impl Network {
    pub fn new(snapshots: Snapshots) -> Self {
        Self {
            snapshots,
            buses: Vec::new(),
            generators: Vec::new(),
            lines: Vec::new(),
            loads: Vec::new(),
            storage: Vec::new(),
            gen_availability: TimeSeries::empty(),
            load_profile: TimeSeries::empty(),
            value_of_lost_load: 10_000.0,
        }
    }

    #[inline]
    pub fn n_snapshots(&self) -> usize {
        self.snapshots.len()
    }

    pub fn add_bus(&mut self, name: impl Into<String>, country: impl Into<String>) -> usize {
        self.buses.push(Bus {
            name: name.into(),
            country: country.into(),
        });
        self.buses.len() - 1
    }

    pub fn add_generator(&mut self, g: Generator) -> usize {
        self.generators.push(g);
        self.generators.len() - 1
    }

    pub fn add_line(&mut self, l: Line) -> usize {
        self.lines.push(l);
        self.lines.len() - 1
    }

    pub fn add_load(&mut self, l: Load) -> usize {
        self.loads.push(l);
        self.loads.len() - 1
    }

    pub fn add_storage(&mut self, s: StorageUnit) -> usize {
        self.storage.push(s);
        self.storage.len() - 1
    }

    /// Generators grouped by the bus they sit on.
    ///
    /// Nodal balance needs this for every bus, and computing it once as a flat
    /// bucket list beats a hash lookup per generator per snapshot.
    pub fn generators_by_bus(&self) -> Adjacency {
        Adjacency::build(self.buses.len(), self.generators.iter().map(|g| g.bus))
    }

    pub fn loads_by_bus(&self) -> Adjacency {
        Adjacency::build(self.buses.len(), self.loads.iter().map(|l| l.bus))
    }

    pub fn storage_by_bus(&self) -> Adjacency {
        Adjacency::build(self.buses.len(), self.storage.iter().map(|s| s.bus))
    }

    /// Lines touching each bus, both directions.
    ///
    /// A line contributes to two buses, so this holds `2 * n_lines` entries
    /// and records the sign the flow enters each balance with: positive into
    /// `bus1`, negative out of `bus0`.
    pub fn lines_by_bus(&self) -> SignedAdjacency {
        let n = self.buses.len();
        let mut starts = vec![0u32; n + 1];
        for l in &self.lines {
            starts[l.bus0 + 1] += 1;
            starts[l.bus1 + 1] += 1;
        }
        for b in 0..n {
            starts[b + 1] += starts[b];
        }
        let mut cursor = starts.clone();
        let mut items = vec![(0u32, 0f64); 2 * self.lines.len()];
        for (i, l) in self.lines.iter().enumerate() {
            let a = cursor[l.bus0] as usize;
            items[a] = (i as u32, -1.0);
            cursor[l.bus0] += 1;
            let b = cursor[l.bus1] as usize;
            items[b] = (i as u32, 1.0);
            cursor[l.bus1] += 1;
        }
        SignedAdjacency { starts, items }
    }

    /// Check every index and parameter before anything is built.
    ///
    /// Failing here is cheap. Failing after assembling ten million rows, or
    /// worse, silently producing a model that solves to a wrong answer, is
    /// not.
    pub fn validate(&self) -> Result<(), NetError> {
        if self.snapshots.is_empty() {
            return Err(NetError::NoSnapshots);
        }
        if self.buses.is_empty() {
            return Err(NetError::NoBuses);
        }
        let nb = self.buses.len();
        let t = self.n_snapshots();

        for (i, g) in self.generators.iter().enumerate() {
            if g.bus >= nb {
                return Err(NetError::BadBusRef {
                    component: "generator",
                    index: i,
                    bus: g.bus,
                    n_buses: nb,
                });
            }
            if !g.p_nom.is_finite() || g.p_nom < 0.0 {
                return Err(NetError::BadParameter {
                    component: "generator",
                    index: i,
                    field: "p_nom",
                    value: g.p_nom,
                });
            }
            if !(0.0..=1.0).contains(&g.p_min_pu) {
                return Err(NetError::BadParameter {
                    component: "generator",
                    index: i,
                    field: "p_min_pu",
                    value: g.p_min_pu,
                });
            }
        }
        for (i, l) in self.lines.iter().enumerate() {
            for b in [l.bus0, l.bus1] {
                if b >= nb {
                    return Err(NetError::BadBusRef {
                        component: "line",
                        index: i,
                        bus: b,
                        n_buses: nb,
                    });
                }
            }
            if l.bus0 == l.bus1 {
                return Err(NetError::SelfLoop {
                    index: i,
                    bus: l.bus0,
                });
            }
            if !l.s_nom.is_finite() || l.s_nom < 0.0 {
                return Err(NetError::BadParameter {
                    component: "line",
                    index: i,
                    field: "s_nom",
                    value: l.s_nom,
                });
            }
        }
        for (i, l) in self.loads.iter().enumerate() {
            if l.bus >= nb {
                return Err(NetError::BadBusRef {
                    component: "load",
                    index: i,
                    bus: l.bus,
                    n_buses: nb,
                });
            }
        }
        for (i, s) in self.storage.iter().enumerate() {
            if s.bus >= nb {
                return Err(NetError::BadBusRef {
                    component: "storage",
                    index: i,
                    bus: s.bus,
                    n_buses: nb,
                });
            }
            for (field, v) in [
                ("efficiency_store", s.efficiency_store),
                ("efficiency_dispatch", s.efficiency_dispatch),
            ] {
                if !(0.0..=1.0).contains(&v) || v <= 0.0 {
                    return Err(NetError::BadParameter {
                        component: "storage",
                        index: i,
                        field,
                        value: v,
                    });
                }
            }
            if !s.max_hours.is_finite() || s.max_hours < 0.0 {
                return Err(NetError::BadParameter {
                    component: "storage",
                    index: i,
                    field: "max_hours",
                    value: s.max_hours,
                });
            }
        }

        if !self.gen_availability.is_empty() {
            let want = self.generators.len() * t;
            if self.gen_availability.len() != want {
                return Err(NetError::TimeSeriesShape {
                    got: self.gen_availability.len(),
                    want,
                });
            }
        }
        if !self.load_profile.is_empty() {
            let want = self.loads.len() * t;
            if self.load_profile.len() != want {
                return Err(NetError::TimeSeriesShape {
                    got: self.load_profile.len(),
                    want,
                });
            }
        }
        Ok(())
    }

    /// Map bus name to index, for loaders working from named data.
    pub fn bus_index(&self) -> HashMap<&str, usize> {
        self.buses
            .iter()
            .enumerate()
            .map(|(i, b)| (b.name.as_str(), i))
            .collect()
    }
}

/// CSR style buckets: which components belong to which bus.
#[derive(Debug, Clone)]
pub struct Adjacency {
    starts: Vec<u32>,
    items: Vec<u32>,
}

impl Adjacency {
    fn build(n_buckets: usize, assign: impl Iterator<Item = usize> + Clone) -> Self {
        let mut starts = vec![0u32; n_buckets + 1];
        for b in assign.clone() {
            starts[b + 1] += 1;
        }
        for i in 0..n_buckets {
            starts[i + 1] += starts[i];
        }
        let mut cursor = starts.clone();
        let mut items = vec![0u32; starts[n_buckets] as usize];
        for (i, b) in assign.enumerate() {
            items[cursor[b] as usize] = i as u32;
            cursor[b] += 1;
        }
        Self { starts, items }
    }

    #[inline]
    pub fn of(&self, bucket: usize) -> &[u32] {
        let s = self.starts[bucket] as usize;
        let e = self.starts[bucket + 1] as usize;
        &self.items[s..e]
    }
}

/// Adjacency that also carries the sign each entry contributes with.
#[derive(Debug, Clone)]
pub struct SignedAdjacency {
    starts: Vec<u32>,
    items: Vec<(u32, f64)>,
}

impl SignedAdjacency {
    #[inline]
    pub fn of(&self, bucket: usize) -> &[(u32, f64)] {
        let s = self.starts[bucket] as usize;
        let e = self.starts[bucket + 1] as usize;
        &self.items[s..e]
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum NetError {
    #[error("network has no snapshots")]
    NoSnapshots,
    #[error("network has no buses")]
    NoBuses,
    #[error("snapshot weight at index {index} is {value}, must be finite and positive")]
    BadSnapshotWeight { index: usize, value: f64 },
    #[error("{component} {index} references bus {bus}, but there are only {n_buses}")]
    BadBusRef {
        component: &'static str,
        index: usize,
        bus: usize,
        n_buses: usize,
    },
    #[error("line {index} connects bus {bus} to itself")]
    SelfLoop { index: usize, bus: usize },
    #[error("{component} {index} has invalid {field} = {value}")]
    BadParameter {
        component: &'static str,
        index: usize,
        field: &'static str,
        value: f64,
    },
    #[error("time series has {got} values, expected {want}")]
    TimeSeriesShape { got: usize, want: usize },
    #[error("time series row {component} has {got} values, expected {want}")]
    TimeSeriesRow {
        component: usize,
        got: usize,
        want: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_bus() -> Network {
        let mut n = Network::new(Snapshots::hourly(4));
        let de = n.add_bus("DE", "DE");
        let fr = n.add_bus("FR", "FR");
        n.add_generator(Generator {
            name: "de_coal".into(),
            bus: de,
            p_nom: 100.0,
            marginal_cost: 40.0,
            p_min_pu: 0.0,
        });
        n.add_generator(Generator {
            name: "fr_nuclear".into(),
            bus: fr,
            p_nom: 200.0,
            marginal_cost: 10.0,
            p_min_pu: 0.0,
        });
        n.add_line(Line {
            name: "DE-FR".into(),
            bus0: de,
            bus1: fr,
            s_nom: 50.0,
            susceptance: 0.0,
        });
        n.add_load(Load {
            name: "de_load".into(),
            bus: de,
            p_set: 80.0,
        });
        n
    }

    #[test]
    fn a_well_formed_network_validates() {
        two_bus().validate().unwrap();
    }

    #[test]
    fn dangling_bus_references_are_caught() {
        let mut n = two_bus();
        n.generators[0].bus = 7;
        assert_eq!(
            n.validate().unwrap_err(),
            NetError::BadBusRef {
                component: "generator",
                index: 0,
                bus: 7,
                n_buses: 2
            }
        );
    }

    #[test]
    fn self_loops_are_rejected() {
        let mut n = two_bus();
        n.lines[0].bus1 = n.lines[0].bus0;
        assert!(matches!(n.validate(), Err(NetError::SelfLoop { .. })));
    }

    #[test]
    fn out_of_range_p_min_pu_is_rejected() {
        let mut n = two_bus();
        n.generators[0].p_min_pu = 1.5;
        assert!(matches!(
            n.validate(),
            Err(NetError::BadParameter {
                field: "p_min_pu",
                ..
            })
        ));
    }

    #[test]
    fn zero_efficiency_storage_is_rejected() {
        let mut n = two_bus();
        n.add_storage(StorageUnit {
            name: "batt".into(),
            bus: 0,
            p_nom: 10.0,
            max_hours: 4.0,
            efficiency_store: 0.0,
            efficiency_dispatch: 0.9,
            cyclic: true,
        });
        assert!(matches!(
            n.validate(),
            Err(NetError::BadParameter {
                field: "efficiency_store",
                ..
            })
        ));
    }

    #[test]
    fn mis_shaped_availability_is_caught() {
        let mut n = two_bus();
        // Two generators, four snapshots, so eight values are required.
        n.gen_availability = TimeSeries::from_flat(vec![1.0; 6], 3, 2).unwrap();
        assert_eq!(
            n.validate().unwrap_err(),
            NetError::TimeSeriesShape { got: 6, want: 8 }
        );
    }

    #[test]
    fn adjacency_buckets_components_by_bus() {
        let n = two_bus();
        let by_bus = n.generators_by_bus();
        assert_eq!(by_bus.of(0), &[0]);
        assert_eq!(by_bus.of(1), &[1]);
        let loads = n.loads_by_bus();
        assert_eq!(loads.of(0), &[0]);
        assert!(loads.of(1).is_empty());
    }

    #[test]
    fn line_adjacency_carries_direction() {
        let n = two_bus();
        let by_bus = n.lines_by_bus();
        // Flow is defined bus0 -> bus1, so it leaves bus0 and arrives at bus1.
        assert_eq!(by_bus.of(0), &[(0, -1.0)]);
        assert_eq!(by_bus.of(1), &[(0, 1.0)]);
    }

    #[test]
    fn time_series_rows_are_contiguous() {
        let ts = TimeSeries::from_rows(&[vec![1.0, 2.0], vec![3.0, 4.0]], 2).unwrap();
        assert_eq!(ts.row(0).unwrap(), &[1.0, 2.0]);
        assert_eq!(ts.row(1).unwrap(), &[3.0, 4.0]);
        assert_eq!(ts.at(1, 0), Some(3.0));
    }

    #[test]
    fn absent_time_series_reads_as_none_not_zero() {
        let ts = TimeSeries::empty();
        assert!(ts.row(0).is_none());
        assert_eq!(ts.at(0, 0), None);
    }

    #[test]
    fn transport_lines_are_those_without_susceptance() {
        let n = two_bus();
        assert!(n.lines[0].is_transport());
        let dc = Line {
            susceptance: 10.0,
            ..n.lines[0].clone()
        };
        assert!(!dc.is_transport());
    }

    #[test]
    fn snapshot_weights_must_be_positive() {
        assert!(matches!(
            Snapshots::weighted(vec![1.0, 0.0]),
            Err(NetError::BadSnapshotWeight { index: 1, .. })
        ));
        assert!(Snapshots::weighted(vec![3.0, 3.0]).is_ok());
    }
}

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
    /// The synchronous area this node belongs to.
    ///
    /// Voltage angles are only comparable inside one. The United States has
    /// three asynchronous interconnections (Eastern, Western, ERCOT) and Japan
    /// has two grids at different frequencies, joined only by HVDC converters.
    /// Two consequences follow, and both are enforced rather than assumed: an
    /// AC line may not span two areas, and each area needs its own angle
    /// reference, because an angle in Texas means nothing relative to one in
    /// Ohio.
    pub synchronous_area: String,
    /// The energy carrier this node balances: electricity, hydrogen, heat, gas.
    ///
    /// Balance is enforced per bus, so a bus of a different carrier is simply a
    /// separate balance. Sector coupling then needs no new machinery beyond a
    /// component that moves energy between two of them at an efficiency, which
    /// is what [`Link`] is.
    pub carrier: String,
}

impl Default for Bus {
    fn default() -> Self {
        Self {
            name: String::new(),
            country: "??".into(),
            synchronous_area: "main".into(),
            carrier: "AC".into(),
        }
    }
}

/// A controllable conversion between two buses, possibly of different carriers.
///
/// An electrolyser is a link from an electricity bus to a hydrogen bus at about
/// 70% efficiency. A heat pump is a link to a heat bus with efficiency above
/// one, because it moves heat rather than creating it. A gas turbine is a link
/// the other way. One component covers all of them, because they are the same
/// equation.
#[derive(Debug, Clone)]
pub struct Link {
    pub name: String,
    /// Where energy is withdrawn.
    pub bus0: usize,
    /// Where energy arrives, multiplied by `efficiency`.
    pub bus1: usize,
    /// Rated throughput measured at `bus0`.
    pub p_nom: f64,
    /// Output per unit of input. May exceed one for heat pumps.
    pub efficiency: f64,
    /// Cost per unit of input.
    pub marginal_cost: f64,
    pub p_nom_extendable: bool,
    pub p_nom_max: f64,
    pub capital_cost: f64,
}

impl Default for Link {
    fn default() -> Self {
        Self {
            name: String::new(),
            bus0: 0,
            bus1: 0,
            p_nom: 0.0,
            efficiency: 1.0,
            marginal_cost: 0.0,
            p_nom_extendable: false,
            p_nom_max: f64::INFINITY,
            capital_cost: 0.0,
        }
    }
}

/// A dispatchable or variable generator attached to one bus.
///
/// Constructed with `..Default::default()` in practice. Most fields describe
/// capacity expansion, and a model that only dispatches existing plant should
/// not have to mention them.
#[derive(Debug, Clone)]
pub struct Generator {
    pub name: String,
    pub bus: usize,
    /// Nameplate capacity, MW. When extendable this is the starting point and
    /// the lower bound on what gets built.
    pub p_nom: f64,
    /// Cost per MWh dispatched.
    pub marginal_cost: f64,
    /// Minimum output as a fraction of capacity, for must-run plant.
    pub p_min_pu: f64,
    /// Whether the optimiser may build more of this.
    ///
    /// This is the difference between asking "how should today be run" and
    /// "what should we build", and the second question is the one energy
    /// policy actually asks.
    pub p_nom_extendable: bool,
    /// Ceiling on installed capacity, MW. Land and grid connection are finite.
    pub p_nom_max: f64,
    /// Annualised cost per MW of capacity built.
    ///
    /// Annualised rather than overnight, so it is commensurate with the
    /// marginal costs accumulated over the modelled horizon. Mixing a lifetime
    /// capital cost with one year of operation is the classic way to get an
    /// answer that is wrong by an order of magnitude.
    pub capital_cost: f64,
    /// Tonnes of CO2 per MWh generated.
    pub co2_emissions: f64,
    /// Whether this unit's on/off state is a decision rather than implied.
    ///
    /// Turning this on makes the problem a MILP. A thermal plant cannot run at
    /// 8% of rating: below its stable minimum it is off, and the choice between
    /// those two states is genuinely discrete. Modelling it continuously lets a
    /// coal unit idle at a physically impossible output, which understates both
    /// cost and emissions.
    pub committable: bool,
    /// Cost of bringing the unit from cold to synchronised, per start.
    pub start_up_cost: f64,
    /// Cost of shutting down, per stop.
    pub shut_down_cost: f64,
    /// Snapshots the unit must remain on once started.
    pub min_up_time: usize,
    /// Snapshots the unit must remain off once stopped.
    pub min_down_time: usize,
}

impl Default for Generator {
    fn default() -> Self {
        Self {
            name: String::new(),
            bus: 0,
            p_nom: 0.0,
            marginal_cost: 0.0,
            p_min_pu: 0.0,
            p_nom_extendable: false,
            p_nom_max: f64::INFINITY,
            capital_cost: 0.0,
            co2_emissions: 0.0,
            committable: false,
            start_up_cost: 0.0,
            shut_down_cost: 0.0,
            min_up_time: 0,
            min_down_time: 0,
        }
    }
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
    /// Whether the optimiser may reinforce this corridor.
    ///
    /// Only meaningful for transport links. Expanding an AC line changes its
    /// susceptance too, which makes the DC flow constraint bilinear and puts
    /// it outside a linear program; see [`Network::validate`], which refuses
    /// the combination rather than silently solving the wrong problem.
    pub s_nom_extendable: bool,
    pub s_nom_max: f64,
    /// Annualised cost per MW of transfer capacity built.
    pub capital_cost: f64,
}

impl Default for Line {
    fn default() -> Self {
        Self {
            name: String::new(),
            bus0: 0,
            bus1: 0,
            s_nom: 0.0,
            susceptance: 0.0,
            s_nom_extendable: false,
            s_nom_max: f64::INFINITY,
            capital_cost: 0.0,
        }
    }
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
#[derive(Debug, Clone, Default)]
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
    /// Whether the optimiser may build more of this.
    pub p_nom_extendable: bool,
    pub p_nom_max: f64,
    /// Annualised cost per MW of power rating built. Energy capacity follows
    /// from `max_hours`, so a battery's duration is a design input rather than
    /// a second decision variable.
    pub capital_cost: f64,
    /// Whether water may be released without generating.
    ///
    /// A reservoir taking more inflow than it can hold or turbine has to spill,
    /// and without somewhere for that energy to go the model is simply
    /// infeasible in a wet week. Batteries have no equivalent and leave this
    /// off.
    pub spillable: bool,
}

impl Default for StorageUnit {
    fn default() -> Self {
        Self {
            name: String::new(),
            bus: 0,
            p_nom: 0.0,
            max_hours: 0.0,
            efficiency_store: 1.0,
            efficiency_dispatch: 1.0,
            cyclic: true,
            p_nom_extendable: false,
            p_nom_max: f64::INFINITY,
            capital_cost: 0.0,
            spillable: false,
        }
    }
}

/// One decade, or whatever span capacity decisions are taken over.
///
/// Multi-period investment asks what to build *and when*. Capacity built in an
/// earlier period is available in every later one, so the periods are coupled
/// through a running total rather than being independent problems.
#[derive(Debug, Clone)]
pub struct InvestmentPeriod {
    pub name: String,
    /// Index of the first snapshot belonging to this period.
    pub first_snapshot: usize,
    /// Number of snapshots in this period.
    pub n_snapshots: usize,
    /// Multiplier applied to every cost incurred in this period.
    ///
    /// Money spent in 2050 is worth less than money spent today, and a model
    /// that ignores that will always defer investment to the last period.
    pub discount: f64,
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
    pub links: Vec<Link>,
    /// Investment periods, in order. Empty means a single-period model, which
    /// is the common case and costs nothing extra.
    pub investment_periods: Vec<InvestmentPeriod>,
    /// Per-unit availability per generator per snapshot. Absent means 1.0.
    pub gen_availability: TimeSeries,
    /// Demand per load per snapshot. Absent means `Load::p_set`.
    pub load_profile: TimeSeries,
    /// Natural inflow per storage unit per snapshot, in MW.
    ///
    /// Hydro's defining feature: energy arrives whether or not anyone asked for
    /// it. Absent means none, which is right for a battery.
    pub storage_inflow: TimeSeries,
    /// Cost of shedding load, per MWh.
    ///
    /// Present so infeasibility surfaces as an expensive answer rather than as
    /// a solver status. An energy model that merely reports INFEASIBLE tells
    /// the user nothing about where or when the system failed.
    pub value_of_lost_load: f64,
    /// Firm capacity required above peak demand, as a fraction, per
    /// synchronous area.
    ///
    /// An islanded system cannot import its way out of a shortfall, so planning
    /// reserve is the constraint that actually sizes its fleet. Korea, Taiwan,
    /// and every one of Indonesia's isolated grids are in this position, and so
    /// is ERCOT for practical purposes. Continental Europe leans on its
    /// neighbours instead, which is why the constraint is optional rather than
    /// assumed.
    ///
    /// Applied per area rather than system wide, because capacity on the far
    /// side of an asynchronous boundary is not firm for the area that needs it.
    pub reserve_margin: Option<f64>,
    /// System wide CO2 budget in tonnes over the modelled horizon.
    ///
    /// One constraint spanning every generator and every snapshot, which is a
    /// very different shape from everything else here: a single row millions of
    /// entries wide. It is also the constraint most decarbonisation questions
    /// are actually asked through, so it earns its place.
    pub co2_limit: Option<f64>,
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
            links: Vec::new(),
            investment_periods: Vec::new(),
            gen_availability: TimeSeries::empty(),
            load_profile: TimeSeries::empty(),
            storage_inflow: TimeSeries::empty(),
            value_of_lost_load: 10_000.0,
            reserve_margin: None,
            co2_limit: None,
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
            ..Default::default()
        });
        self.buses.len() - 1
    }

    /// A bus carrying something other than electricity.
    pub fn add_carrier_bus(
        &mut self,
        name: impl Into<String>,
        country: impl Into<String>,
        carrier: impl Into<String>,
    ) -> usize {
        self.buses.push(Bus {
            name: name.into(),
            country: country.into(),
            carrier: carrier.into(),
            ..Default::default()
        });
        self.buses.len() - 1
    }

    /// A bus in a named synchronous area, for systems with more than one.
    pub fn add_bus_in_area(
        &mut self,
        name: impl Into<String>,
        country: impl Into<String>,
        area: impl Into<String>,
    ) -> usize {
        self.buses.push(Bus {
            name: name.into(),
            country: country.into(),
            synchronous_area: area.into(),
            ..Default::default()
        });
        self.buses.len() - 1
    }

    /// Distinct synchronous areas, in order of first appearance, with the index
    /// of a bus in each to serve as that area's angle reference.
    ///
    /// Deterministic by construction: first appearance rather than hash order,
    /// so the same network always produces the same reference buses and the
    /// same solution.
    pub fn synchronous_areas(&self) -> Vec<(String, usize)> {
        let mut seen: Vec<(String, usize)> = Vec::new();
        for (i, b) in self.buses.iter().enumerate() {
            if !seen.iter().any(|(a, _)| a == &b.synchronous_area) {
                seen.push((b.synchronous_area.clone(), i));
            }
        }
        seen
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

    pub fn add_link(&mut self, l: Link) -> usize {
        self.links.push(l);
        self.links.len() - 1
    }

    /// Links attached to each bus, with the sign and coefficient they enter
    /// that bus's balance with.
    ///
    /// A link withdraws one unit at `bus0` and delivers `efficiency` units at
    /// `bus1`, so the same variable appears in two balances with different
    /// coefficients. That asymmetry is the whole of sector coupling.
    pub fn links_by_bus(&self) -> SignedAdjacency {
        let n = self.buses.len();
        let mut starts = vec![0u32; n + 1];
        for l in &self.links {
            starts[l.bus0 + 1] += 1;
            starts[l.bus1 + 1] += 1;
        }
        for b in 0..n {
            starts[b + 1] += starts[b];
        }
        let mut cursor = starts.clone();
        let mut items = vec![(0u32, 0f64); 2 * self.links.len()];
        for (i, l) in self.links.iter().enumerate() {
            let a = cursor[l.bus0] as usize;
            items[a] = (i as u32, -1.0);
            cursor[l.bus0] += 1;
            let b = cursor[l.bus1] as usize;
            items[b] = (i as u32, l.efficiency);
            cursor[l.bus1] += 1;
        }
        SignedAdjacency { starts, items }
    }

    /// The investment period each snapshot belongs to, or a single period
    /// covering everything when none were declared.
    pub fn period_of_snapshot(&self) -> Vec<usize> {
        let t = self.n_snapshots();
        if self.investment_periods.is_empty() {
            return vec![0; t];
        }
        let mut out = vec![0usize; t];
        for (p, period) in self.investment_periods.iter().enumerate() {
            let end = (period.first_snapshot + period.n_snapshots).min(t);
            for slot in out.iter_mut().take(end).skip(period.first_snapshot) {
                *slot = p;
            }
        }
        out
    }

    /// A lossy, bidirectional HVDC tie, built from a pair of links.
    ///
    /// Losses are not a rounding error at continental distance. China's UHVDC
    /// loses roughly 3% per 1,000 km, so a 3,000 km line from the western
    /// resource base to the eastern load centres arrives about 9% short, and
    /// that gap is large enough to change where it is worth building anything.
    /// Indonesia's inter-island subsea cables have the same problem for the
    /// same reason.
    ///
    /// Implemented as two one-directional links rather than as a new component,
    /// because a link already carries exactly the right equation: withdraw one
    /// unit here, deliver `efficiency` units there. A signed line variable
    /// cannot express that, since the loss must apply to whichever end is
    /// receiving, and which end that is changes hour to hour.
    ///
    /// Returns both link indices, forward then reverse.
    pub fn add_hvdc_tie(
        &mut self,
        name: impl Into<String>,
        bus_a: usize,
        bus_b: usize,
        p_nom: f64,
        efficiency: f64,
    ) -> (usize, usize) {
        let name = name.into();
        let fwd = self.add_link(Link {
            name: format!("{name}_fwd"),
            bus0: bus_a,
            bus1: bus_b,
            p_nom,
            efficiency,
            ..Default::default()
        });
        let rev = self.add_link(Link {
            name: format!("{name}_rev"),
            bus0: bus_b,
            bus1: bus_a,
            p_nom,
            efficiency,
            ..Default::default()
        });
        (fwd, rev)
    }

    /// Efficiency of a DC line of a given length, from a per-1000 km loss rate.
    ///
    /// A convenience for the common case, since the figures quoted in the
    /// literature are per distance and what the model needs is a multiplier.
    /// UHVDC is about 0.03 per 1000 km; conventional HVDC nearer 0.07.
    pub fn dc_efficiency(km: f64, loss_per_1000km: f64) -> f64 {
        (1.0 - loss_per_1000km * km / 1000.0).clamp(0.0, 1.0)
    }

    /// Number of investment periods, at least one.
    #[inline]
    pub fn n_periods(&self) -> usize {
        self.investment_periods.len().max(1)
    }

    /// Discount multiplier for a period.
    #[inline]
    pub fn discount(&self, p: usize) -> f64 {
        self.investment_periods.get(p).map_or(1.0, |x| x.discount)
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

        for (i, l) in self.lines.iter().enumerate() {
            // Expanding an AC line would change its susceptance, making the DC
            // flow constraint bilinear. Refusing is better than linearising
            // silently around an assumed value nobody chose.
            if !l.is_transport()
                && self.buses[l.bus0].synchronous_area != self.buses[l.bus1].synchronous_area
            {
                return Err(NetError::AcLineCrossesAreas {
                    index: i,
                    area0: self.buses[l.bus0].synchronous_area.clone(),
                    area1: self.buses[l.bus1].synchronous_area.clone(),
                });
            }
            if l.s_nom_extendable && !l.is_transport() {
                return Err(NetError::ExtendableAcLine { index: i });
            }
            if l.s_nom_extendable && l.s_nom_max < l.s_nom {
                return Err(NetError::CapacityCeilingBelowFloor {
                    component: "line",
                    index: i,
                    floor: l.s_nom,
                    ceiling: l.s_nom_max,
                });
            }
        }
        for (i, g) in self.generators.iter().enumerate() {
            if g.p_nom_extendable && g.p_nom_max < g.p_nom {
                return Err(NetError::CapacityCeilingBelowFloor {
                    component: "generator",
                    index: i,
                    floor: g.p_nom,
                    ceiling: g.p_nom_max,
                });
            }
            if g.co2_emissions < 0.0 || !g.co2_emissions.is_finite() {
                return Err(NetError::BadParameter {
                    component: "generator",
                    index: i,
                    field: "co2_emissions",
                    value: g.co2_emissions,
                });
            }
        }
        for (i, s) in self.storage.iter().enumerate() {
            if s.p_nom_extendable && s.p_nom_max < s.p_nom {
                return Err(NetError::CapacityCeilingBelowFloor {
                    component: "storage",
                    index: i,
                    floor: s.p_nom,
                    ceiling: s.p_nom_max,
                });
            }
        }
        if let Some(cap) = self.co2_limit
            && (cap < 0.0 || !cap.is_finite())
        {
            return Err(NetError::BadCo2Limit(cap));
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
    #[error(
        "line {index} is extendable but has a susceptance; expanding an AC line \
         changes its impedance, which a linear DC flow model cannot represent"
    )]
    ExtendableAcLine { index: usize },
    #[error("{component} {index} has capacity ceiling {ceiling} below its floor {floor}")]
    CapacityCeilingBelowFloor {
        component: &'static str,
        index: usize,
        floor: f64,
        ceiling: f64,
    },
    #[error(
        "line {index} carries a susceptance but joins synchronous areas `{area0}` and \
         `{area1}`; asynchronous grids can only be linked by a controllable HVDC tie, \
         which is modelled by leaving susceptance at zero"
    )]
    AcLineCrossesAreas {
        index: usize,
        area0: String,
        area1: String,
    },
    #[error("CO2 limit {0} must be finite and non-negative")]
    BadCo2Limit(f64),
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
            ..Default::default()
        });
        n.add_generator(Generator {
            name: "fr_nuclear".into(),
            bus: fr,
            p_nom: 200.0,
            marginal_cost: 10.0,
            p_min_pu: 0.0,
            ..Default::default()
        });
        n.add_line(Line {
            name: "DE-FR".into(),
            bus0: de,
            bus1: fr,
            s_nom: 50.0,
            susceptance: 0.0,
            ..Default::default()
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
            ..Default::default()
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

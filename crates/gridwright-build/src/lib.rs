//! Turning a [`Network`] into a linear program.
//!
//! This is the stage the whole project is a bet about. In the Python stack
//! this is where labelled intermediates get built and then converted, and it
//! is where the memory goes. Here it is two phases with a hard line between
//! them:
//!
//! 1. **Allocate.** Every variable block is handed out up front, sequentially.
//!    This is unavoidably serial because the block indices depend on each
//!    other, but it is only `resize` calls, which are memsets.
//! 2. **Assemble.** Every constraint family is generated in parallel into
//!    per-thread [`RowBatch`]es, then merged once. Nothing is shared, nothing
//!    is locked, and no thread ever reads another's output.
//!
//! The split works because after phase one every variable's index is a pure
//! function of its block and offset. A thread building the balance rows for
//! bus 400 needs no coordination to know where generator 12's dispatch at
//! snapshot 900 lives; it is `dispatch[12].at(900)`, computed locally.
//!
//! # The formulation
//!
//! Linear optimal power flow. Minimise dispatch cost subject to energy
//! balance at every bus in every snapshot, transmission limits, storage
//! dynamics, and, for lines carrying a susceptance, the DC power flow
//! relation between flow and voltage angle difference.

use rayon::prelude::*;
use gridwright_model::{Model, RowBatch, Sense, VarBlock};
use gridwright_net::{Adjacency, NetError, Network, SignedAdjacency};

/// Where every variable family lives in the model.
///
/// One [`VarBlock`] per component, each spanning all snapshots. Reading a
/// component's whole trajectory is therefore a contiguous slice of the
/// solution vector, which is what makes result extraction cheap too.
#[derive(Debug, Default, Clone)]
pub struct VarIndex {
    /// Dispatch, per generator.
    pub dispatch: Vec<VarBlock>,
    /// Signed flow along each line, positive from `bus0` toward `bus1`.
    pub flow: Vec<VarBlock>,
    /// State of charge, per storage unit.
    pub soc: Vec<VarBlock>,
    /// Charging power, per storage unit.
    pub charge: Vec<VarBlock>,
    /// Discharging power, per storage unit.
    pub discharge: Vec<VarBlock>,
    /// Unserved energy, per bus.
    pub shed: Vec<VarBlock>,
    /// Voltage angle, per bus. Empty when no line needs DC flow.
    pub angle: Vec<VarBlock>,
    /// Installed generator capacity, one variable per extendable generator.
    ///
    /// Length one rather than one per snapshot: you build a plant once, not
    /// hourly. That asymmetry is why capacity cannot simply reuse the dispatch
    /// block machinery.
    pub gen_capacity: Vec<Option<VarBlock>>,
    /// Installed transfer capacity, per extendable line.
    pub line_capacity: Vec<Option<VarBlock>>,
    /// Installed storage power rating, per extendable storage unit.
    pub storage_capacity: Vec<Option<VarBlock>>,
}

/// A built linear program plus the map back to what its variables mean.
#[derive(Debug, Clone)]
pub struct Lopf {
    pub model: Model,
    pub vars: VarIndex,
    pub n_snapshots: usize,
}

/// Rows generated for each constraint family, for reporting and for tests
/// that need to assert the model has the shape it should.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RowCounts {
    pub balance: usize,
    pub dc_flow: usize,
    pub storage: usize,
    /// Dispatch tied to built capacity, for extendable components.
    pub capacity: usize,
    /// The single system wide emissions row, if there is one.
    pub co2: usize,
}

impl RowCounts {
    pub fn total(self) -> usize {
        self.balance + self.dc_flow + self.storage + self.capacity + self.co2
    }
}

impl Lopf {
    /// How many rows each family contributes, derived from the network alone.
    pub fn row_counts(net: &Network) -> RowCounts {
        let t = net.n_snapshots();
        let dc_lines = net.lines.iter().filter(|l| !l.is_transport()).count();
        // An extendable generator needs its dispatch bounded by what was
        // built, in every snapshot: p[g,t] <= P_g * availability[g,t]. A line
        // needs two rows per snapshot because flow is signed.
        let ext_gen = net.generators.iter().filter(|g| g.p_nom_extendable).count();
        let ext_line = net.lines.iter().filter(|l| l.s_nom_extendable).count();
        let ext_store = net.storage.iter().filter(|s| s.p_nom_extendable).count();
        let capacity = (ext_gen + 2 * ext_line + 3 * ext_store) * t
            + net
                .generators
                .iter()
                .filter(|g| g.p_nom_extendable && g.p_min_pu > 0.0)
                .count()
                * t;
        RowCounts {
            balance: net.buses.len() * t,
            dc_flow: dc_lines * t,
            storage: net.storage.len() * t,
            capacity,
            co2: usize::from(net.co2_limit.is_some()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("network is not valid: {0}")]
    Network(#[from] NetError),
    #[error("model rejected a block: {0}")]
    Model(#[from] gridwright_model::ModelError),
    #[error("model would have {0} variables, which exceeds the u32 index space")]
    TooManyVariables(usize),
}

/// Build the LOPF for `net`.
pub fn build_lopf(net: &Network) -> Result<Lopf, BuildError> {
    net.validate()?;

    let t = net.n_snapshots();
    let t32 = t as u32;
    let weights = net.snapshots.weights();
    let uniform_weights = weights.iter().all(|w| *w == weights[0]);

    // Any line with a susceptance forces angle variables into existence. If
    // every line is a transport corridor the angles are pure overhead, so
    // they are never created rather than created and left free.
    let needs_angles = net.lines.iter().any(|l| !l.is_transport());

    let mut model = Model::new();
    model.sense = Sense::Minimize;

    // Capacity variables are one apiece rather than one per snapshot, so they
    // are counted outside the multiplication.
    let n_extendable = net.generators.iter().filter(|g| g.p_nom_extendable).count()
        + net.lines.iter().filter(|l| l.s_nom_extendable).count()
        + net.storage.iter().filter(|s| s.p_nom_extendable).count();
    let n_vars = (net.generators.len()
        + net.lines.len()
        + 3 * net.storage.len()
        + net.buses.len()
        + if needs_angles { net.buses.len() } else { 0 })
        * t
        + n_extendable;
    if n_vars > u32::MAX as usize {
        return Err(BuildError::TooManyVariables(n_vars));
    }
    model.reserve_cols(n_vars);

    // ---- Phase 1: allocate every variable block, sequentially. ----

    let mut vars = VarIndex::default();

    // Generator dispatch. The upper bound follows the availability profile
    // when there is one, which is the whole reason time series are stored
    // component major: `row(g)` is exactly the slice this needs.
    vars.dispatch.reserve(net.generators.len());
    let mut lo_buf: Vec<f64> = Vec::with_capacity(t);
    let mut up_buf: Vec<f64> = Vec::with_capacity(t);
    for (g, unit) in net.generators.iter().enumerate() {
        // When capacity is a decision the ceiling is no longer a constant, so
        // it cannot live in the bounds. Dispatch is left loosely bounded here
        // and tied to the capacity variable by a constraint instead. The loose
        // bound still uses p_nom_max where it is finite, because a tighter
        // bound the solver can see is strictly better than one it must derive.
        let block = if unit.p_nom_extendable {
            let ceiling = if unit.p_nom_max.is_finite() {
                unit.p_nom_max
            } else {
                f64::INFINITY
            };
            match net.gen_availability.row(g) {
                Some(avail) => {
                    up_buf.clear();
                    lo_buf.clear();
                    for &a in avail {
                        up_buf.push(ceiling * a);
                        lo_buf.push(0.0);
                    }
                    model.add_block_with(&lo_buf, &up_buf, 0.0)?
                }
                None => model.add_block(t32, 0.0, ceiling, 0.0),
            }
        } else {
            match net.gen_availability.row(g) {
                Some(avail) => {
                    lo_buf.clear();
                    up_buf.clear();
                    for &a in avail {
                        let ceiling = unit.p_nom * a;
                        up_buf.push(ceiling);
                        // A must-run floor cannot exceed a reduced ceiling. A
                        // wind farm with p_min_pu set and no wind must be
                        // allowed to produce nothing rather than render the
                        // model infeasible.
                        lo_buf.push((unit.p_nom * unit.p_min_pu).min(ceiling));
                    }
                    model.add_block_with(&lo_buf, &up_buf, 0.0)?
                }
                None => model.add_block(t32, unit.p_nom * unit.p_min_pu, unit.p_nom, 0.0),
            }
        };
        vars.dispatch.push(block);
    }

    // Capacity variables. One per extendable component, not one per snapshot:
    // a plant is built once. `p_nom` becomes the floor, since existing plant
    // does not un-build itself.
    vars.gen_capacity.reserve(net.generators.len());
    for unit in &net.generators {
        vars.gen_capacity.push(unit.p_nom_extendable.then(|| {
            model.add_block(1, unit.p_nom, unit.p_nom_max, unit.capital_cost)
        }));
    }

    // Line flows, symmetric about zero.
    vars.flow.reserve(net.lines.len());
    for l in &net.lines {
        let rating = if l.s_nom_extendable {
            l.s_nom_max
        } else {
            l.s_nom
        };
        vars.flow.push(model.add_block(t32, -rating, rating, 0.0));
    }
    vars.line_capacity.reserve(net.lines.len());
    for l in &net.lines {
        vars.line_capacity.push(
            l.s_nom_extendable
                .then(|| model.add_block(1, l.s_nom, l.s_nom_max, l.capital_cost)),
        );
    }

    // Storage: charge and discharge are separate non-negative variables rather
    // than one signed variable, because the round trip efficiencies differ and
    // a single variable cannot carry two different coefficients.
    vars.soc.reserve(net.storage.len());
    vars.charge.reserve(net.storage.len());
    vars.discharge.reserve(net.storage.len());
    for s in &net.storage {
        let rating = if s.p_nom_extendable {
            s.p_nom_max
        } else {
            s.p_nom
        };
        vars.soc
            .push(model.add_block(t32, 0.0, rating * s.max_hours, 0.0));
        vars.charge.push(model.add_block(t32, 0.0, rating, 0.0));
        vars.discharge.push(model.add_block(t32, 0.0, rating, 0.0));
    }
    vars.storage_capacity.reserve(net.storage.len());
    for s in &net.storage {
        vars.storage_capacity.push(
            s.p_nom_extendable
                .then(|| model.add_block(1, s.p_nom, s.p_nom_max, s.capital_cost)),
        );
    }

    // Load shedding, one per bus. Priced at the value of lost load so that a
    // system which physically cannot be served returns a solved model naming
    // where and when it failed, instead of the single word INFEASIBLE.
    vars.shed.reserve(net.buses.len());
    for _ in &net.buses {
        vars.shed.push(model.add_block(t32, 0.0, f64::INFINITY, 0.0));
    }

    if needs_angles {
        vars.angle.reserve(net.buses.len());
        for _ in &net.buses {
            vars.angle
                .push(model.add_block(t32, f64::NEG_INFINITY, f64::INFINITY, 0.0));
        }
        // Angles are only meaningful relative to each other, so without a
        // reference the model carries a free constant per snapshot and the
        // basis is degenerate. Pinning bus 0 removes it.
        let slack = vars.angle[0];
        let s = slack.start() as usize;
        model.columns_mut_lower()[s..s + t].fill(0.0);
        model.columns_mut_upper()[s..s + t].fill(0.0);
    }

    // ---- Objective. ----
    //
    // Cost is per MWh, so each snapshot's contribution scales by its weight.
    // The uniform case folds the weight into a scalar and takes the memset
    // path; only genuinely non-uniform weights pay for a per-element vector.
    let mut obj_buf = Vec::with_capacity(t);
    for (g, unit) in net.generators.iter().enumerate() {
        if uniform_weights {
            model.fill_obj(vars.dispatch[g], unit.marginal_cost * weights[0]);
        } else {
            obj_buf.clear();
            obj_buf.extend(weights.iter().map(|w| unit.marginal_cost * w));
            model.set_obj(vars.dispatch[g], &obj_buf)?;
        }
    }
    for b in 0..net.buses.len() {
        if uniform_weights {
            model.fill_obj(vars.shed[b], net.value_of_lost_load * weights[0]);
        } else {
            obj_buf.clear();
            obj_buf.extend(weights.iter().map(|w| net.value_of_lost_load * w));
            model.set_obj(vars.shed[b], &obj_buf)?;
        }
    }

    // ---- Phase 2: assemble constraints in parallel. ----

    let gens_at = net.generators_by_bus();
    let loads_at = net.loads_by_bus();
    let storage_at = net.storage_by_bus();
    let lines_at = net.lines_by_bus();

    let mut all = build_balance(net, &vars, &gens_at, &loads_at, &storage_at, &lines_at, t);
    if needs_angles {
        all.extend(build_dc_flow(net, &vars, t));
    }
    all.extend(build_storage(net, &vars, t));
    all.extend(build_capacity_ties(net, &vars, t));
    if let Some(batch) = build_co2(net, &vars, t) {
        all.push(batch);
    }
    model.absorb_all(&all);

    Ok(Lopf {
        model,
        vars,
        n_snapshots: t,
    })
}

/// Nodal energy balance, one row per bus per snapshot.
///
/// Parallelised over buses rather than snapshots. Both axes are independent,
/// but the bus axis means each thread reads one contiguous run of every time
/// series it touches, which the snapshot axis would turn into a stride.
fn build_balance(
    net: &Network,
    vars: &VarIndex,
    gens_at: &Adjacency,
    loads_at: &Adjacency,
    storage_at: &Adjacency,
    lines_at: &SignedAdjacency,
    t: usize,
) -> Vec<RowBatch> {
    (0..net.buses.len())
        .into_par_iter()
        .map(|b| {
            let gens = gens_at.of(b);
            let loads = loads_at.of(b);
            let stores = storage_at.of(b);
            let lines = lines_at.of(b);

            // Every row in this bucket has the same width, so the batch is
            // sized exactly rather than grown.
            let per_row = gens.len() + lines.len() + 2 * stores.len() + 1;
            let mut batch = RowBatch::with_capacity(t, t * per_row);
            let mut terms: Vec<(u32, f64)> = Vec::with_capacity(per_row);

            for step in 0..t {
                let ti = step as u32;
                terms.clear();
                for &g in gens {
                    terms.push((vars.dispatch[g as usize].at(ti), 1.0));
                }
                for &(l, sign) in lines {
                    terms.push((vars.flow[l as usize].at(ti), sign));
                }
                for &s in stores {
                    let si = s as usize;
                    terms.push((vars.discharge[si].at(ti), 1.0));
                    terms.push((vars.charge[si].at(ti), -1.0));
                }
                terms.push((vars.shed[b].at(ti), 1.0));

                // Demand moves to the right hand side.
                let mut demand = 0.0;
                for &ld in loads {
                    let li = ld as usize;
                    demand += net.load_profile.at(li, step).unwrap_or(net.loads[li].p_set);
                }
                batch.push_eq(terms.iter().copied(), demand);
            }
            batch
        })
        .collect()
}

/// DC power flow: `f - B * (theta0 - theta1) = 0`.
///
/// Only lines with a susceptance get this. Transport corridors stay free to
/// route up to their rating, which is the correct model for a controllable
/// HVDC link and the wrong one for an AC line.
fn build_dc_flow(net: &Network, vars: &VarIndex, t: usize) -> Vec<RowBatch> {
    net.lines
        .par_iter()
        .enumerate()
        .filter(|(_, l)| !l.is_transport())
        .map(|(l, line)| {
            let mut batch = RowBatch::with_capacity(t, t * 3);
            let f = vars.flow[l];
            let a0 = vars.angle[line.bus0];
            let a1 = vars.angle[line.bus1];
            for step in 0..t {
                let ti = step as u32;
                batch.push_eq(
                    [
                        (f.at(ti), 1.0),
                        (a0.at(ti), -line.susceptance),
                        (a1.at(ti), line.susceptance),
                    ],
                    0.0,
                );
            }
            batch
        })
        .collect()
}

/// Storage state of charge dynamics.
///
/// `soc[t] - soc[t-1] - eff_store * charge[t] * w + discharge[t] * w / eff_dispatch = 0`
///
/// The first snapshot wraps to the last when the unit is cyclic. Without that
/// wrap a finite horizon lets the optimiser start full and end empty, which is
/// free energy and quietly makes every result too cheap.
fn build_storage(net: &Network, vars: &VarIndex, t: usize) -> Vec<RowBatch> {
    let weights = net.snapshots.weights();
    net.storage
        .par_iter()
        .enumerate()
        .map(|(s, unit)| {
            let mut batch = RowBatch::with_capacity(t, t * 4);
            let soc = vars.soc[s];
            let ch = vars.charge[s];
            let di = vars.discharge[s];
            for (step, &w) in weights.iter().enumerate().take(t) {
                let ti = step as u32;
                let store_coeff = -unit.efficiency_store * w;
                let dispatch_coeff = w / unit.efficiency_dispatch;

                if step == 0 && !unit.cyclic {
                    // Non-cyclic units start empty, so the first row has no
                    // predecessor term: soc[0] = inflow - outflow.
                    batch.push_eq(
                        [
                            (soc.at(ti), 1.0),
                            (ch.at(ti), store_coeff),
                            (di.at(ti), dispatch_coeff),
                        ],
                        0.0,
                    );
                    continue;
                }
                let prev = if step == 0 {
                    soc.at(t as u32 - 1)
                } else {
                    soc.at(ti - 1)
                };
                batch.push_eq(
                    [
                        (soc.at(ti), 1.0),
                        (prev, -1.0),
                        (ch.at(ti), store_coeff),
                        (di.at(ti), dispatch_coeff),
                    ],
                    0.0,
                );
            }
            batch
        })
        .collect()
}

/// Ties dispatch to built capacity for every extendable component.
///
/// This is what makes capacity expansion a different problem rather than a
/// relabelled one. With fixed capacity the ceiling is a bound, which the
/// solver handles for free. Once capacity is a variable the ceiling becomes
/// `p[g,t] - availability[g,t] * P_g <= 0`, a real row, and there is one per
/// snapshot per component. It is usually the largest constraint family in an
/// expansion model.
fn build_capacity_ties(net: &Network, vars: &VarIndex, t: usize) -> Vec<RowBatch> {
    let mut batches: Vec<RowBatch> = net
        .generators
        .par_iter()
        .enumerate()
        .filter_map(|(g, unit)| {
            let cap = vars.gen_capacity[g]?;
            let cap_col = cap.at(0);
            let p = vars.dispatch[g];
            let must_run = unit.p_min_pu > 0.0;
            let rows = if must_run { 2 * t } else { t };
            let mut batch = RowBatch::with_capacity(rows, 2 * rows);
            for step in 0..t {
                let ti = step as u32;
                let a = net.gen_availability.at(g, step).unwrap_or(1.0);
                // p - a * P <= 0
                batch.push_le([(p.at(ti), 1.0), (cap_col, -a)], 0.0);
                if must_run {
                    // p - p_min_pu * a * P >= 0. Scaling the floor by
                    // availability as well is what stops a must-run wind farm
                    // from being infeasible in a calm hour.
                    batch.push_ge([(p.at(ti), 1.0), (cap_col, -unit.p_min_pu * a)], 0.0);
                }
            }
            Some(batch)
        })
        .collect();

    // Flow is signed, so an extendable line needs both sides bounded.
    batches.extend(
        net.lines
            .par_iter()
            .enumerate()
            .filter_map(|(l, _)| {
                let cap_col = vars.line_capacity[l]?.at(0);
                let f = vars.flow[l];
                let mut batch = RowBatch::with_capacity(2 * t, 4 * t);
                for step in 0..t {
                    let ti = step as u32;
                    batch.push_le([(f.at(ti), 1.0), (cap_col, -1.0)], 0.0);
                    batch.push_ge([(f.at(ti), 1.0), (cap_col, 1.0)], 0.0);
                }
                Some(batch)
            })
            .collect::<Vec<_>>(),
    );

    // Storage rating bounds charge and discharge; energy follows from the
    // rating through max_hours, so it needs a row too rather than a bound.
    batches.extend(
        net.storage
            .par_iter()
            .enumerate()
            .filter_map(|(s, unit)| {
                let cap_col = vars.storage_capacity[s]?.at(0);
                let (ch, di, soc) = (vars.charge[s], vars.discharge[s], vars.soc[s]);
                let mut batch = RowBatch::with_capacity(3 * t, 6 * t);
                for step in 0..t {
                    let ti = step as u32;
                    batch.push_le([(ch.at(ti), 1.0), (cap_col, -1.0)], 0.0);
                    batch.push_le([(di.at(ti), 1.0), (cap_col, -1.0)], 0.0);
                    batch.push_le([(soc.at(ti), 1.0), (cap_col, -unit.max_hours)], 0.0);
                }
                Some(batch)
            })
            .collect::<Vec<_>>(),
    );

    batches
}

/// The system wide emissions budget, as one row.
///
/// Shape worth noting: every other constraint here is narrow and numerous,
/// while this is a single row potentially millions of entries wide. It is
/// built serially because there is only one of it, and because a row that
/// wide is memory bound rather than compute bound anyway.
fn build_co2(net: &Network, vars: &VarIndex, t: usize) -> Option<RowBatch> {
    let limit = net.co2_limit?;
    let emitters: Vec<usize> = net
        .generators
        .iter()
        .enumerate()
        .filter(|(_, g)| g.co2_emissions > 0.0)
        .map(|(i, _)| i)
        .collect();

    // A budget with nothing to spend it on still constrains nothing, but the
    // row is emitted regardless so that row counts stay predictable and the
    // dual is available to report the (zero) carbon price.
    let weights = net.snapshots.weights();
    let mut batch = RowBatch::with_capacity(1, emitters.len() * t);
    let mut terms: Vec<(u32, f64)> = Vec::with_capacity(emitters.len() * t);
    for &g in &emitters {
        let rate = net.generators[g].co2_emissions;
        let p = vars.dispatch[g];
        for (step, &w) in weights.iter().enumerate().take(t) {
            terms.push((p.at(step as u32), rate * w));
        }
    }
    batch.push_le(terms, limit);
    Some(batch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gridwright_net::{Generator, Line, Load, Snapshots, StorageUnit, TimeSeries};

    fn two_bus(t: usize) -> Network {
        let mut n = Network::new(Snapshots::hourly(t));
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
    fn variable_count_matches_the_formulation() {
        let net = two_bus(4);
        let lopf = build_lopf(&net).unwrap();
        // 2 generators + 1 line + 2 shed, times 4 snapshots. No storage, and
        // no angles because the only line is a transport corridor.
        assert_eq!(lopf.model.num_cols(), (2 + 1 + 2) * 4);
        assert!(lopf.vars.angle.is_empty());
    }

    #[test]
    fn row_count_matches_the_formulation() {
        let net = two_bus(4);
        let lopf = build_lopf(&net).unwrap();
        let counts = Lopf::row_counts(&net);
        assert_eq!(counts.balance, 2 * 4);
        assert_eq!(counts.dc_flow, 0);
        assert_eq!(lopf.model.num_rows(), counts.total());
    }

    #[test]
    fn susceptance_creates_angles_and_dc_rows() {
        let mut net = two_bus(4);
        net.lines[0].susceptance = 10.0;
        let lopf = build_lopf(&net).unwrap();
        assert_eq!(lopf.vars.angle.len(), 2);
        let counts = Lopf::row_counts(&net);
        assert_eq!(counts.dc_flow, 4);
        assert_eq!(lopf.model.num_rows(), counts.total());
    }

    #[test]
    fn the_slack_bus_angle_is_pinned_to_zero() {
        let mut net = two_bus(3);
        net.lines[0].susceptance = 10.0;
        let lopf = build_lopf(&net).unwrap();
        let cols = lopf.model.columns();
        for i in lopf.vars.angle[0].range() {
            assert_eq!(cols.lower[i as usize], 0.0);
            assert_eq!(cols.upper[i as usize], 0.0);
        }
        // The non-slack bus must stay free, or power cannot be routed.
        let free = lopf.vars.angle[1];
        assert!(cols.lower[free.start() as usize].is_infinite());
    }

    #[test]
    fn availability_profile_becomes_the_upper_bound() {
        let mut net = two_bus(3);
        net.gen_availability =
            TimeSeries::from_rows(&[vec![0.5, 1.0, 0.0], vec![1.0, 1.0, 1.0]], 3).unwrap();
        let lopf = build_lopf(&net).unwrap();
        let g0 = lopf.vars.dispatch[0];
        let up = &lopf.model.columns().upper;
        assert_eq!(up[g0.at(0) as usize], 50.0);
        assert_eq!(up[g0.at(1) as usize], 100.0);
        assert_eq!(up[g0.at(2) as usize], 0.0);
    }

    #[test]
    fn a_must_run_floor_cannot_exceed_a_reduced_ceiling() {
        let mut net = two_bus(2);
        net.generators[0].p_min_pu = 0.5;
        // Zero availability in snapshot 1 would otherwise leave lower = 50
        // and upper = 0, which is infeasible by construction.
        net.gen_availability =
            TimeSeries::from_rows(&[vec![1.0, 0.0], vec![1.0, 1.0]], 2).unwrap();
        let lopf = build_lopf(&net).unwrap();
        let g0 = lopf.vars.dispatch[0];
        let cols = lopf.model.columns();
        let lo = cols.lower[g0.at(1) as usize];
        let up = cols.upper[g0.at(1) as usize];
        assert!(lo <= up, "lower {lo} exceeded upper {up}");
        assert_eq!(lo, 0.0);
    }

    #[test]
    fn objective_scales_with_snapshot_weight() {
        let mut net = two_bus(2);
        net.snapshots = Snapshots::weighted(vec![1.0, 3.0]).unwrap();
        let lopf = build_lopf(&net).unwrap();
        let g0 = lopf.vars.dispatch[0];
        let obj = &lopf.model.columns().obj;
        assert_eq!(obj[g0.at(0) as usize], 40.0);
        assert_eq!(obj[g0.at(1) as usize], 120.0);
    }

    #[test]
    fn storage_adds_three_blocks_and_one_row_family() {
        let mut net = two_bus(6);
        net.add_storage(StorageUnit {
            name: "batt".into(),
            bus: 0,
            p_nom: 10.0,
            max_hours: 4.0,
            efficiency_store: 0.9,
            efficiency_dispatch: 0.9,
            cyclic: true,
            ..Default::default()
        });
        let lopf = build_lopf(&net).unwrap();
        assert_eq!(lopf.vars.soc.len(), 1);
        assert_eq!(lopf.vars.charge.len(), 1);
        assert_eq!(lopf.vars.discharge.len(), 1);
        assert_eq!(Lopf::row_counts(&net).storage, 6);
        // Energy ceiling is power times duration.
        let soc = lopf.vars.soc[0];
        assert_eq!(lopf.model.columns().upper[soc.at(0) as usize], 40.0);
    }

    #[test]
    fn a_cyclic_store_links_its_first_snapshot_to_its_last() {
        let mut net = two_bus(4);
        net.add_storage(StorageUnit {
            name: "batt".into(),
            bus: 0,
            p_nom: 10.0,
            max_hours: 4.0,
            efficiency_store: 1.0,
            efficiency_dispatch: 1.0,
            cyclic: true,
            ..Default::default()
        });
        let lopf = build_lopf(&net).unwrap();
        let csc = lopf.model.to_csc();
        csc.validate().unwrap();
        // The last SOC column must appear in a row other than its own
        // snapshot's, which is the wrap term.
        let last = lopf.vars.soc[0].at(3) as usize;
        assert!(
            csc.column(last).count() >= 2,
            "cyclic wrap term is missing from the final SOC column"
        );
    }

    #[test]
    fn a_non_cyclic_store_has_no_wrap_term() {
        let mut net = two_bus(4);
        net.add_storage(StorageUnit {
            name: "batt".into(),
            bus: 0,
            p_nom: 10.0,
            max_hours: 4.0,
            efficiency_store: 1.0,
            efficiency_dispatch: 1.0,
            cyclic: false,
            ..Default::default()
        });
        let lopf = build_lopf(&net).unwrap();
        let csc = lopf.model.to_csc();
        // Final SOC appears only in its own balance row when not cyclic.
        assert_eq!(csc.column(lopf.vars.soc[0].at(3) as usize).count(), 1);
    }

    #[test]
    fn the_assembled_matrix_is_structurally_sound() {
        let mut net = two_bus(24);
        net.lines[0].susceptance = 8.0;
        net.add_storage(StorageUnit {
            name: "batt".into(),
            bus: 1,
            p_nom: 10.0,
            max_hours: 4.0,
            efficiency_store: 0.95,
            efficiency_dispatch: 0.95,
            cyclic: true,
            ..Default::default()
        });
        let lopf = build_lopf(&net).unwrap();
        let csc = lopf.model.to_csc();
        csc.validate().unwrap();
        assert_eq!(csc.n_cols, lopf.model.num_cols());
        assert_eq!(csc.n_rows, lopf.model.num_rows());
        assert_eq!(csc.nnz(), lopf.model.nnz());
    }

    #[test]
    fn an_invalid_network_is_refused_before_any_building() {
        let mut net = two_bus(4);
        net.generators[0].bus = 99;
        assert!(matches!(build_lopf(&net), Err(BuildError::Network(_))));
    }

    /// Parallel assembly must not depend on how rayon happens to schedule.
    /// Building the same network twice has to produce an identical matrix, or
    /// results become irreproducible run to run.
    #[test]
    fn assembly_is_deterministic_across_runs() {
        let mut net = two_bus(50);
        net.lines[0].susceptance = 8.0;
        let a = build_lopf(&net).unwrap().model.to_csc();
        let b = build_lopf(&net).unwrap().model.to_csc();
        assert_eq!(a, b);
    }
}

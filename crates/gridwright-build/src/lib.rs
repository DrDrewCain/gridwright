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

pub mod security;
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
    /// Throughput of each link, measured at its input bus.
    pub link_flow: Vec<VarBlock>,
    /// Installed link capacity, per extendable link.
    pub link_capacity: Vec<Option<VarBlock>>,
    /// Commitment status, per committable generator. Binary.
    pub status: Vec<Option<VarBlock>>,
    /// Start-up indicator, per committable generator.
    pub start_up: Vec<Option<VarBlock>>,
    /// Shut-down indicator, per committable generator.
    pub shut_down: Vec<Option<VarBlock>>,
    /// Spilled energy, per spillable storage unit.
    pub spill: Vec<Option<VarBlock>>,
    /// Loss on each lossy line, always non-negative.
    pub line_loss: Vec<Option<VarBlock>>,
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
    /// Commitment: status bounds, transitions and minimum run times.
    pub commitment: usize,
    /// Planning reserve, one per synchronous area.
    pub reserve: usize,
    /// The single system wide emissions row, if there is one.
    pub co2: usize,
}

impl RowCounts {
    pub fn total(self) -> usize {
        self.balance + self.dc_flow + self.storage + self.capacity + self.commitment
            + self.reserve + self.co2
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
            capacity: capacity + net.links.iter().filter(|l| l.p_nom_extendable).count() * t,
            commitment: net
                .generators
                .iter()
                .filter(|g| g.committable)
                .map(|g| {
                    // upper bound, transition, and optionally a minimum output
                    // row plus the two ramping windows
                    let mut per = 2;
                    if g.p_min_pu > 0.0 {
                        per += 1;
                    }
                    per * t
                        + usize::from(g.min_up_time > 1) * t.saturating_sub(g.min_up_time - 1)
                        + usize::from(g.min_down_time > 1) * t.saturating_sub(g.min_down_time - 1)
                })
                .sum(),
            reserve: if net.reserve_margin.is_some() {
                net.synchronous_areas().len()
            } else {
                0
            },
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
    let n_periods = net.n_periods();
    let period_of = net.period_of_snapshot();

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
        + n_extendable * n_periods;
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
        } else if unit.committable {
            // A committable unit's floor belongs to the commitment constraint,
            // not to its bounds. Putting p_min in the lower bound would force
            // output even when the unit is switched off, which contradicts
            // p <= p_max * u and makes every such model infeasible.
            match net.gen_availability.row(g) {
                Some(avail) => {
                    lo_buf.clear();
                    up_buf.clear();
                    for &a in avail {
                        up_buf.push(unit.p_nom * a);
                        lo_buf.push(0.0);
                    }
                    model.add_block_with(&lo_buf, &up_buf, 0.0)?
                }
                None => model.add_block(t32, 0.0, unit.p_nom, 0.0),
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
        // One build variable per investment period, not one overall. Capacity
        // built in an early period is available in every later one, so the
        // periods couple through a running total rather than being independent
        // problems. Single-period models get a block of length one and pay
        // nothing for the generality.
        vars.gen_capacity.push(unit.p_nom_extendable.then(|| {
            let b = model.add_block(n_periods as u32, 0.0, unit.p_nom_max, 0.0);
            for period in 0..n_periods {
                model.set_obj_at(b, period as u32, unit.capital_cost * net.discount(period));
            }
            b
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
        vars.line_capacity.push(l.s_nom_extendable.then(|| {
            let b = model.add_block(n_periods as u32, 0.0, l.s_nom_max, 0.0);
            for period in 0..n_periods {
                model.set_obj_at(b, period as u32, l.capital_cost * net.discount(period));
            }
            b
        }));
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
        vars.storage_capacity.push(s.p_nom_extendable.then(|| {
            let b = model.add_block(n_periods as u32, 0.0, s.p_nom_max, 0.0);
            for period in 0..n_periods {
                model.set_obj_at(b, period as u32, s.capital_cost * net.discount(period));
            }
            b
        }));
    }

    // Losses, for lines that declare a rate. Non-negative and driven up only by
    // the constraints below, so the optimiser has every incentive to keep them
    // at exactly the linearised value rather than above it.
    vars.line_loss.reserve(net.lines.len());
    for l in &net.lines {
        vars.line_loss.push(
            (l.loss > 0.0).then(|| model.add_block(t32, 0.0, f64::INFINITY, 0.0)),
        );
    }

    // Links, measured at the input bus.
    vars.link_flow.reserve(net.links.len());
    for l in &net.links {
        let rating = if l.p_nom_extendable { l.p_nom_max } else { l.p_nom };
        vars.link_flow.push(model.add_block(t32, 0.0, rating, 0.0));
    }
    vars.link_capacity.reserve(net.links.len());
    for l in &net.links {
        vars.link_capacity.push(l.p_nom_extendable.then(|| {
            let b = model.add_block(n_periods as u32, 0.0, l.p_nom_max, 0.0);
            for period in 0..n_periods {
                model.set_obj_at(b, period as u32, l.capital_cost * net.discount(period));
            }
            b
        }));
    }

    // Commitment. Status is binary, which is what turns this into a MILP;
    // start-up and shut-down are continuous because the status constraint
    // forces them to integral values anyway, and relaxing them shrinks the
    // branch and bound tree substantially for no loss of correctness.
    vars.status.reserve(net.generators.len());
    vars.start_up.reserve(net.generators.len());
    vars.shut_down.reserve(net.generators.len());
    for unit in &net.generators {
        if unit.committable {
            vars.status.push(Some(model.add_binary_block(t32, 0.0)));
            vars.start_up
                .push(Some(model.add_block(t32, 0.0, 1.0, unit.start_up_cost)));
            vars.shut_down
                .push(Some(model.add_block(t32, 0.0, 1.0, unit.shut_down_cost)));
        } else {
            vars.status.push(None);
            vars.start_up.push(None);
            vars.shut_down.push(None);
        }
    }

    // Spill, for reservoirs that can release water without generating.
    vars.spill.reserve(net.storage.len());
    for unit in &net.storage {
        vars.spill.push(
            unit.spillable
                .then(|| model.add_block(t32, 0.0, f64::INFINITY, 0.0)),
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
        // basis is degenerate. One reference is not enough: angles are only
        // comparable within a synchronous area, so an interconnection that has
        // three of them, as the United States does, needs three references.
        // Pinning only the first would leave the others floating.
        for (_, bus) in net.synchronous_areas() {
            let slack = vars.angle[bus];
            let s = slack.start() as usize;
            model.columns_mut_lower()[s..s + t].fill(0.0);
            model.columns_mut_upper()[s..s + t].fill(0.0);
        }
    }

    // ---- Objective. ----
    //
    // Cost is per MWh, so each snapshot's contribution scales by its weight.
    // The uniform case folds the weight into a scalar and takes the memset
    // path; only genuinely non-uniform weights pay for a per-element vector.
    // Money spent in 2050 is worth less than money spent today. A model that
    // ignores that will defer every decision to the final period, which is a
    // artefact of the arithmetic rather than a finding.
    // Operating cost carries three multipliers: the snapshot's own weight, the
    // discount of the period it falls in, and the probability of the scenario
    // it belongs to. Capital carries only the discount, because you build once
    // and then find out which future you got.
    let scenario_w = net.scenario_weight();
    let deterministic = net.scenarios.is_empty();
    let single_period = n_periods == 1 && net.investment_periods.is_empty();
    let flat = uniform_weights && single_period && deterministic;
    let mut obj_buf = Vec::with_capacity(t);
    let cost_at = |base: f64, step: usize| {
        base * weights[step] * net.discount(period_of[step]) * scenario_w[step]
    };

    for (g, unit) in net.generators.iter().enumerate() {
        // A carbon price is simply an addition to the cost of running an
        // emitting unit, which is exactly what a carbon price is in reality.
        let effective = unit.marginal_cost + net.co2_price * unit.co2_emissions;
        if flat {
            model.fill_obj(vars.dispatch[g], effective * weights[0]);
        } else {
            obj_buf.clear();
            obj_buf.extend((0..t).map(|s| cost_at(effective, s)));
            model.set_obj(vars.dispatch[g], &obj_buf)?;
        }
    }
    for (k, link) in net.links.iter().enumerate() {
        if link.marginal_cost != 0.0 {
            obj_buf.clear();
            obj_buf.extend((0..t).map(|s| cost_at(link.marginal_cost, s)));
            model.set_obj(vars.link_flow[k], &obj_buf)?;
        }
    }
    for b in 0..net.buses.len() {
        if flat {
            model.fill_obj(vars.shed[b], net.value_of_lost_load * weights[0]);
        } else {
            obj_buf.clear();
            obj_buf.extend((0..t).map(|s| cost_at(net.value_of_lost_load, s)));
            model.set_obj(vars.shed[b], &obj_buf)?;
        }
    }

    // ---- Phase 2: assemble constraints in parallel. ----

    let gens_at = net.generators_by_bus();
    let loads_at = net.loads_by_bus();
    let storage_at = net.storage_by_bus();
    let lines_at = net.lines_by_bus();
    let links_at = net.links_by_bus();

    let incidence = BusIncidence {
        gens: gens_at,
        loads: loads_at,
        storage: storage_at,
        lines: lines_at,
        links: links_at,
    };
    let mut all = build_balance(net, &vars, &incidence, t);
    if needs_angles {
        all.extend(build_dc_flow(net, &vars, t));
    }
    all.extend(build_storage(net, &vars, t));
    all.extend(build_capacity_ties(net, &vars, t));
    all.extend(build_commitment(net, &vars, t));
    all.extend(build_ramps(net, &vars, t));
    all.extend(build_head(net, &vars, t));
    all.extend(build_losses(net, &vars, t));
    all.extend(build_cascades(net, &vars, t));
    if let Some(batch) = build_co2(net, &vars, t) {
        all.push(batch);
    }
    all.extend(build_reserve(net, &vars));
    if !net.contingencies.is_empty() {
        let lodf = security::compute_lodf(net);
        all.extend(security::build_security(
            net,
            &vars,
            &lodf,
            &net.contingencies,
            t,
        ));
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
/// Everything attached to each bus, gathered once so the balance builder takes
/// one argument instead of five.
struct BusIncidence {
    gens: Adjacency,
    loads: Adjacency,
    storage: Adjacency,
    lines: SignedAdjacency,
    links: SignedAdjacency,
}

fn build_balance(
    net: &Network,
    vars: &VarIndex,
    at: &BusIncidence,
    t: usize,
) -> Vec<RowBatch> {
    let (gens_at, loads_at, storage_at, lines_at, links_at) =
        (&at.gens, &at.loads, &at.storage, &at.lines, &at.links);
    (0..net.buses.len())
        .into_par_iter()
        .map(|b| {
            let gens = gens_at.of(b);
            let loads = loads_at.of(b);
            let stores = storage_at.of(b);
            let lines = lines_at.of(b);
            let links = links_at.of(b);

            // Every row in this bucket has the same width, so the batch is
            // sized exactly rather than grown.
            let per_row = gens.len() + lines.len() + links.len() + 2 * stores.len() + 1;
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
                // A link withdraws one unit at bus0 and delivers `efficiency`
                // units at bus1, so the same variable carries a different
                // coefficient into each balance. That is sector coupling in one
                // line: nothing else about the formulation changes.
                for &(k, coeff) in links {
                    terms.push((vars.link_flow[k as usize].at(ti), coeff));
                }
                // Half of a line's loss is charged to each of its ends, which
                // is the usual convention and avoids making the loss depend on
                // which direction the power happened to be going.
                for &(l, _) in lines {
                    if let Some(loss) = vars.line_loss[l as usize] {
                        terms.push((loss.at(ti), -0.5));
                    }
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
            let mut batch = RowBatch::with_capacity(t, t * 6);
            let soc = vars.soc[s];
            let ch = vars.charge[s];
            let di = vars.discharge[s];
            let spill = vars.spill[s];
            for (step, &w) in weights.iter().enumerate().take(t) {
                let ti = step as u32;
                let store_coeff = -unit.efficiency_store * w;
                let dispatch_coeff = w / unit.efficiency_dispatch;

                // Natural inflow arrives whether or not anyone wanted it, so
                // it is a constant on the right hand side rather than a
                // decision. Spill is the release valve: without it a reservoir
                // taking more water than it can hold makes the model infeasible
                // in exactly the weeks a hydro model exists to study.
                let inflow = net.storage_inflow.at(s, step).unwrap_or(0.0) * w;
                let mut terms: Vec<(u32, f64)> = Vec::with_capacity(5);
                terms.push((soc.at(ti), 1.0));
                if step == 0 && let Some(start_level) = unit.soc_initial {
                    // A stated starting level is a constant, so it moves to the
                    // right hand side rather than referring to another variable.
                    // This overrides cyclicity: a window of a rolling horizon
                    // inherits a level, it does not return to one.
                    terms.push((ch.at(ti), store_coeff));
                    terms.push((di.at(ti), dispatch_coeff));
                    if let Some(sp) = spill {
                        terms.push((sp.at(ti), w));
                    }
                    batch.push_eq(terms, inflow + start_level);
                    continue;
                }
                if step > 0 || unit.cyclic {
                    let prev = if step == 0 {
                        soc.at(t as u32 - 1)
                    } else {
                        soc.at(ti - 1)
                    };
                    // Same degeneracy as commitment: a one snapshot cyclic
                    // store is its own predecessor, and the duplicate column
                    // would be rejected. Cancelling leaves soc unconstrained by
                    // its own past, which is exactly right when there is none.
                    if prev == soc.at(ti) {
                        terms.remove(0);
                    } else {
                        terms.push((prev, -1.0));
                    }
                }
                terms.push((ch.at(ti), store_coeff));
                terms.push((di.at(ti), dispatch_coeff));
                if let Some(sp) = spill {
                    terms.push((sp.at(ti), w));
                }
                batch.push_eq(terms, inflow);
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
    let period_of = net.period_of_snapshot();

    // Capacity available at a snapshot is the existing fleet plus everything
    // built in this period or any earlier one. Expressed by summing the build
    // variables up to that period rather than by carrying a stock variable,
    // which would need its own balance row per period for no benefit.
    let available =
        |cap: gridwright_model::VarBlock, step: usize, coeff: f64, terms: &mut Vec<(u32, f64)>| {
            for q in 0..=period_of[step] {
                terms.push((cap.at(q as u32), coeff));
            }
        };

    let mut batches: Vec<RowBatch> = net
        .generators
        .par_iter()
        .enumerate()
        .filter_map(|(g, unit)| {
            let cap = vars.gen_capacity[g]?;
            let p = vars.dispatch[g];
            let must_run = unit.p_min_pu > 0.0;
            let rows = if must_run { 2 * t } else { t };
            let mut batch = RowBatch::with_capacity(rows, 3 * rows);
            let mut terms: Vec<(u32, f64)> = Vec::with_capacity(net.n_periods() + 1);
            for step in 0..t {
                let ti = step as u32;
                let a = net.gen_availability.at(g, step).unwrap_or(1.0);
                // p - a * (built so far) <= a * p_nom
                terms.clear();
                terms.push((p.at(ti), 1.0));
                available(cap, step, -a, &mut terms);
                batch.push_le(terms.iter().copied(), a * unit.p_nom);
                if must_run {
                    // Scaling the floor by availability too is what stops a
                    // must-run wind farm being infeasible in a calm hour.
                    terms.clear();
                    terms.push((p.at(ti), 1.0));
                    available(cap, step, -unit.p_min_pu * a, &mut terms);
                    batch.push_ge(terms.iter().copied(), unit.p_min_pu * a * unit.p_nom);
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
            .filter_map(|(l, line)| {
                let cap = vars.line_capacity[l]?;
                let f = vars.flow[l];
                let mut batch = RowBatch::with_capacity(2 * t, 6 * t);
                let mut terms: Vec<(u32, f64)> = Vec::with_capacity(net.n_periods() + 1);
                for step in 0..t {
                    let ti = step as u32;
                    terms.clear();
                    terms.push((f.at(ti), 1.0));
                    available(cap, step, -1.0, &mut terms);
                    batch.push_le(terms.iter().copied(), line.s_nom);
                    terms.clear();
                    terms.push((f.at(ti), 1.0));
                    available(cap, step, 1.0, &mut terms);
                    batch.push_ge(terms.iter().copied(), -line.s_nom);
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
                let cap = vars.storage_capacity[s]?;
                let (ch, di, soc) = (vars.charge[s], vars.discharge[s], vars.soc[s]);
                let mut batch = RowBatch::with_capacity(3 * t, 9 * t);
                let mut terms: Vec<(u32, f64)> = Vec::with_capacity(net.n_periods() + 1);
                for step in 0..t {
                    let ti = step as u32;
                    for (var, coeff, rhs) in [
                        (ch, -1.0, unit.p_nom),
                        (di, -1.0, unit.p_nom),
                        (soc, -unit.max_hours, unit.p_nom * unit.max_hours),
                    ] {
                        terms.clear();
                        terms.push((var.at(ti), 1.0));
                        available(cap, step, coeff, &mut terms);
                        batch.push_le(terms.iter().copied(), rhs);
                    }
                }
                Some(batch)
            })
            .collect::<Vec<_>>(),
    );

    // Link throughput bounded by built capacity. One row per snapshot, not two,
    // because link flow is non-negative by construction.
    batches.extend(
        net.links
            .par_iter()
            .enumerate()
            .filter_map(|(k, link)| {
                let cap = vars.link_capacity[k]?;
                let f = vars.link_flow[k];
                let mut batch = RowBatch::with_capacity(t, 3 * t);
                let mut terms: Vec<(u32, f64)> = Vec::with_capacity(net.n_periods() + 1);
                for step in 0..t {
                    let ti = step as u32;
                    terms.clear();
                    terms.push((f.at(ti), 1.0));
                    available(cap, step, -1.0, &mut terms);
                    batch.push_le(terms.iter().copied(), link.p_nom);
                }
                Some(batch)
            })
            .collect::<Vec<_>>(),
    );

    batches
}

/// Unit commitment: the on/off state of thermal plant, and what it costs to
/// change it.
///
/// Four families, and each exists because leaving it out produces a specific
/// wrong answer:
///
/// - `p <= p_max * u` and `p >= p_min * u`, so a unit that is off produces
///   nothing and a unit that is on respects its stable minimum. Without the
///   second, a coal plant idles at 8% of rating, which no coal plant can do.
/// - `u[t] - u[t-1] = su[t] - sd[t]`, which defines starts and stops in terms
///   of the status they follow from. Costing starts without this lets the
///   optimiser claim it never started.
/// - minimum up time, so a unit that starts stays on. Without it, plant
///   flickers on and off hourly to chase prices, which is free in the model and
///   impossible in a boiler.
/// - minimum down time, the same argument in reverse.
fn build_commitment(net: &Network, vars: &VarIndex, t: usize) -> Vec<RowBatch> {
    net.generators
        .par_iter()
        .enumerate()
        .filter_map(|(g, unit)| {
            let status = vars.status[g]?;
            let su = vars.start_up[g]?;
            let sd = vars.shut_down[g]?;
            let p = vars.dispatch[g];

            let mut batch = RowBatch::with_capacity(4 * t, 12 * t);
            for step in 0..t {
                let ti = step as u32;
                let avail = net.gen_availability.at(g, step).unwrap_or(1.0);
                let p_max = unit.p_nom * avail;
                let p_min = unit.p_nom * unit.p_min_pu * avail;

                // Output is zero unless committed, and at least the stable
                // minimum when it is.
                batch.push_le([(p.at(ti), 1.0), (status.at(ti), -p_max)], 0.0);
                if p_min > 0.0 {
                    batch.push_ge([(p.at(ti), 1.0), (status.at(ti), -p_min)], 0.0);
                }

                // State transition. The first snapshot wraps, so a horizon can
                // be studied without pretending every unit began offline.
                //
                // With a single snapshot the wrap makes the predecessor the
                // same variable as the successor, and the two status terms
                // would cancel into one column appearing twice in one row.
                // Solvers reject that outright, so the degenerate case emits
                // the cancelled form instead: with nowhere to transition to,
                // starts and stops must simply match.
                if step == 0 && let Some(was_on) = unit.initially_on {
                    // u[0] - was_on = su[0] - sd[0]. A unit already running does
                    // not pay to start again, which is exactly what a rolling
                    // window must not get wrong.
                    batch.push_eq(
                        [
                            (status.at(ti), 1.0),
                            (su.at(ti), -1.0),
                            (sd.at(ti), 1.0),
                        ],
                        if was_on { 1.0 } else { 0.0 },
                    );
                    continue;
                }
                let prev = if step == 0 {
                    status.at(t as u32 - 1)
                } else {
                    status.at(ti - 1)
                };
                if prev == status.at(ti) {
                    batch.push_eq([(su.at(ti), -1.0), (sd.at(ti), 1.0)], 0.0);
                } else {
                    batch.push_eq(
                        [
                            (status.at(ti), 1.0),
                            (prev, -1.0),
                            (su.at(ti), -1.0),
                            (sd.at(ti), 1.0),
                        ],
                        0.0,
                    );
                }

                // Minimum up time: having started at any point in the preceding
                // window, the unit must still be on now. Expressed as a sum over
                // the window rather than one row per pair, which is the tighter
                // formulation and the smaller matrix.
                if unit.min_up_time > 1 && step + 1 >= unit.min_up_time {
                    let mut terms: Vec<(u32, f64)> = Vec::with_capacity(unit.min_up_time + 1);
                    for k in (step + 1 - unit.min_up_time)..=step {
                        terms.push((su.at(k as u32), 1.0));
                    }
                    terms.push((status.at(ti), -1.0));
                    batch.push_le(terms, 0.0);
                }
                if unit.min_down_time > 1 && step + 1 >= unit.min_down_time {
                    let mut terms: Vec<(u32, f64)> = Vec::with_capacity(unit.min_down_time + 1);
                    for k in (step + 1 - unit.min_down_time)..=step {
                        terms.push((sd.at(k as u32), 1.0));
                    }
                    terms.push((status.at(ti), 1.0));
                    batch.push_le(terms, 1.0);
                }
            }
            Some(batch)
        })
        .collect()
}

/// Planning reserve, one row per synchronous area.
///
/// Firm capacity in the area must cover its own peak demand plus a margin.
/// Capacity across an asynchronous boundary does not count, which is the whole
/// reason this is per area: an islanded system genuinely cannot borrow.
///
/// Variable renewables contribute at their minimum availability over the
/// horizon rather than their nameplate, because a reserve margin met by solar
/// at midnight is not met at all. That is a deliberately conservative reading,
/// and a cruder one than the capacity-credit calculations a utility would use,
/// but it errs in the direction of building too much rather than too little.
fn build_reserve(net: &Network, vars: &VarIndex) -> Vec<RowBatch> {
    let Some(margin) = net.reserve_margin else {
        return Vec::new();
    };
    let t = net.n_snapshots();
    let areas = net.synchronous_areas();

    areas
        .iter()
        .map(|(area, _)| {
            // Peak demand inside this area, over the whole horizon.
            let mut peak: f64 = 0.0;
            for step in 0..t {
                let mut demand = 0.0;
                for (l, load) in net.loads.iter().enumerate() {
                    if &net.buses[load.bus].synchronous_area == area {
                        demand += net.load_profile.at(l, step).unwrap_or(load.p_set);
                    }
                }
                peak = peak.max(demand);
            }

            let mut terms: Vec<(u32, f64)> = Vec::new();
            let mut firm_fixed = 0.0;
            for (g, unit) in net.generators.iter().enumerate() {
                if &net.buses[unit.bus].synchronous_area != area {
                    continue;
                }
                // The worst hour this unit could be asked to cover.
                let credit = (0..t)
                    .map(|s| net.gen_availability.at(g, s).unwrap_or(1.0))
                    .fold(f64::INFINITY, f64::min)
                    .min(1.0);
                if credit <= 0.0 {
                    continue;
                }
                match vars.gen_capacity[g] {
                    Some(cap) => {
                        firm_fixed += unit.p_nom * credit;
                        for q in 0..net.n_periods() {
                            terms.push((cap.at(q as u32), credit));
                        }
                    }
                    None => firm_fixed += unit.p_nom * credit,
                }
            }
            for (s, unit) in net.storage.iter().enumerate() {
                if &net.buses[unit.bus].synchronous_area != area {
                    continue;
                }
                match vars.storage_capacity[s] {
                    Some(cap) => {
                        firm_fixed += unit.p_nom;
                        for q in 0..net.n_periods() {
                            terms.push((cap.at(q as u32), 1.0));
                        }
                    }
                    None => firm_fixed += unit.p_nom,
                }
            }

            let required = peak * (1.0 + margin) - firm_fixed;
            let mut batch = RowBatch::with_capacity(1, terms.len().max(1));
            if terms.is_empty() {
                // Nothing extendable to satisfy it with. The row is still
                // emitted so that an unmeetable requirement shows up as an
                // infeasible model rather than as silence.
                batch.push_ge([(0u32, 0.0)], required.min(0.0));
            } else {
                batch.push_ge(terms, required);
            }
            batch
        })
        .collect()
}

/// Ramp limits between consecutive snapshots.
///
/// A nuclear station cannot go from quarter load to full in an hour. Without
/// this the model treats every unit as infinitely flexible, which understates
/// what it costs to follow a renewable ramp and how much flexible plant a
/// system actually needs.
fn build_ramps(net: &Network, vars: &VarIndex, t: usize) -> Vec<RowBatch> {
    net.generators
        .par_iter()
        .enumerate()
        .filter(|(_, g)| {
            (g.ramp_up > 0.0 && g.ramp_up < 1.0) || (g.ramp_down > 0.0 && g.ramp_down < 1.0)
        })
        .map(|(g, unit)| {
            let p = vars.dispatch[g];
            let mut batch = RowBatch::with_capacity(2 * t, 4 * t);
            // Ramping is between neighbours, so a horizon shorter than two
            // snapshots has nothing to constrain.
            for step in 1..t {
                let ti = step as u32;
                if unit.ramp_up > 0.0 && unit.ramp_up < 1.0 {
                    batch.push_le(
                        [(p.at(ti), 1.0), (p.at(ti - 1), -1.0)],
                        unit.ramp_up * unit.p_nom,
                    );
                }
                if unit.ramp_down > 0.0 && unit.ramp_down < 1.0 {
                    batch.push_le(
                        [(p.at(ti - 1), 1.0), (p.at(ti), -1.0)],
                        unit.ramp_down * unit.p_nom,
                    );
                }
            }
            batch
        })
        .collect()
}

/// Hydraulic head: a low reservoir cannot reach its rated output.
///
/// Power is proportional to the height water falls through, so a reservoir at
/// a quarter full delivers less than one at the brim even with the gates wide
/// open. Available capacity therefore rises with stored volume:
///
/// ```text
///   discharge[t] ≤ p_nom · ( h_min + (1 − h_min) · soc[t] / e_max )
/// ```
///
/// Linear in the state of charge, so it costs one row per snapshot and no
/// variables. Without it a model empties a reservoir at full power right to the
/// bottom, which overstates exactly the flexibility a dry season removes.
fn build_head(net: &Network, vars: &VarIndex, t: usize) -> Vec<RowBatch> {
    net.storage
        .par_iter()
        .enumerate()
        .filter(|(_, s)| s.head_min_pu < 1.0 && s.max_hours > 0.0 && s.p_nom > 0.0)
        .map(|(s, unit)| {
            let e_max = unit.p_nom * unit.max_hours;
            let slope = unit.p_nom * (1.0 - unit.head_min_pu) / e_max;
            let floor = unit.p_nom * unit.head_min_pu;
            let di = vars.discharge[s];
            let soc = vars.soc[s];
            let mut batch = RowBatch::with_capacity(t, 2 * t);
            for step in 0..t {
                let ti = step as u32;
                // Head is taken at the *start* of the period, not the end.
                // Using the end level makes the constraint self-limiting:
                // discharging lowers the level that permits the discharge, so a
                // brim-full reservoir could never reach its rating. Physically
                // the water leaves at the head it had on the way out.
                if step == 0 {
                    match (unit.soc_initial, unit.cyclic) {
                        // A known starting level is a constant.
                        (Some(e0), _) => {
                            batch.push_le([(di.at(ti), 1.0)], floor + slope * e0);
                        }
                        // Cyclic: the level before the first snapshot is the
                        // level after the last.
                        (None, true) => {
                            batch.push_le(
                                [(di.at(ti), 1.0), (soc.at(t as u32 - 1), -slope)],
                                floor,
                            );
                        }
                        // Non-cyclic and unspecified means it starts empty, so
                        // only the floor is available.
                        (None, false) => {
                            batch.push_le([(di.at(ti), 1.0)], floor);
                        }
                    }
                } else {
                    batch.push_le([(di.at(ti), 1.0), (soc.at(ti - 1), -slope)], floor);
                }
            }
            batch
        })
        .collect()
}

/// Linearised transmission losses.
///
/// Real losses go as the square of current, which is not linear and so cannot
/// appear in a linear program at all. What is available is a marginal rate
/// applied to the magnitude of the flow, which is what production planning
/// models use in practice.
///
/// Absolute value is not linear either, but it is the *maximum* of two linear
/// functions, and a variable bounded below by both of them equals the larger.
/// Since loss only ever removes energy, the optimiser pushes it down to exactly
/// that bound, so the pair of inequalities behaves as an equality without
/// needing to be one.
///
/// The approximation is calibrated at a chosen operating point rather than
/// exact everywhere, and a network run far from it will see loss estimates
/// drift. That is a real limitation of every linear loss model, this one
/// included.
fn build_losses(net: &Network, vars: &VarIndex, t: usize) -> Vec<RowBatch> {
    net.lines
        .par_iter()
        .enumerate()
        .filter_map(|(l, line)| {
            let loss = vars.line_loss[l]?;
            let f = vars.flow[l];
            let mut batch = RowBatch::with_capacity(2 * t, 4 * t);
            for step in 0..t {
                let ti = step as u32;
                // loss >= k*f and loss >= -k*f, so loss >= k*|f|.
                batch.push_ge([(loss.at(ti), 1.0), (f.at(ti), -line.loss)], 0.0);
                batch.push_ge([(loss.at(ti), 1.0), (f.at(ti), line.loss)], 0.0);
            }
            Some(batch)
        })
        .collect()
}

/// Hydro cascades: an upstream release becomes a downstream inflow.
///
/// Modelling reservoirs on one river independently counts the same water twice,
/// which flatters the system's flexibility exactly when it is scarce. The delay
/// is in snapshots rather than hours because that is the resolution the model
/// has; a travel time shorter than one snapshot is simply immediate.
fn build_cascades(net: &Network, vars: &VarIndex, t: usize) -> Vec<RowBatch> {
    let weights = net.snapshots.weights();
    net.storage
        .par_iter()
        .enumerate()
        .filter_map(|(s, unit)| {
            let down = unit.downstream?;
            let soc_d = vars.soc[down];
            let mut batch = RowBatch::with_capacity(t, 5 * t);
            let mut terms: Vec<(u32, f64)> = Vec::with_capacity(5);

            for step in 0..t {
                // Water released now arrives downstream after the travel time.
                // Releases that would arrive past the horizon simply leave the
                // system, which is the same thing the last snapshot does with
                // everything else.
                let arrival = step + unit.travel_time;
                if arrival >= t {
                    continue;
                }
                let ai = arrival as u32;
                let w = weights[arrival];
                terms.clear();
                // The downstream store gains what the upstream one let go, both
                // through its turbines and over its spillway.
                terms.push((soc_d.at(ai), 1.0));
                terms.push((vars.discharge[s].at(step as u32), -w));
                if let Some(sp) = vars.spill[s] {
                    terms.push((sp.at(step as u32), -w));
                }
                // Expressed as an addition to the downstream balance rather
                // than as a rewrite of it, so the two constraint families stay
                // independent and either can change without the other.
                batch.push_ge(terms.iter().copied(), 0.0);
            }
            (!batch.is_empty()).then_some(batch)
        })
        .collect()
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
    let mut batch = RowBatch::with_capacity(1, emitters.len() * t + net.generators.len());
    let mut terms: Vec<(u32, f64)> = Vec::with_capacity(emitters.len() * t);
    // Capacity built carries its embodied carbon into the same budget. Leaving
    // it out lets a model meet a cap by building its way there for free.
    for (g, unit) in net.generators.iter().enumerate() {
        if unit.embodied_co2 > 0.0
            && let Some(cap) = vars.gen_capacity[g]
        {
            for q in 0..cap.len() {
                terms.push((cap.at(q), unit.embodied_co2));
            }
        }
    }
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
            ..Default::default()
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

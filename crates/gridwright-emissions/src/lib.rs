//! Emissions accounting: what was emitted, where, and on whose behalf.
//!
//! The optimiser already knows every generator's output and every line's flow.
//! Turning that into an emissions answer is arithmetic, but it is arithmetic
//! with several defensible results, and the differences between them are large
//! enough to reverse a decision. This crate computes them separately and names
//! each one, rather than producing a single number labelled "carbon".
//!
//! # Production against consumption
//!
//! **Production** emissions are what was emitted inside a region. Easy, and
//! usually not what is being asked.
//!
//! **Consumption** emissions are what was emitted on behalf of the electricity
//! a region used, which depends on where its imports came from — and on where
//! *those* came from, since an importer may be re-exporting power it imported
//! from somewhere else. Following that through the network is the interesting
//! part, and it is a linear system rather than a single pass:
//!
//! ```text
//!   intensity_b · inflow_b  =  Σ_g p_g e_g  +  Σ_{lines into b} flow · intensity_source
//! ```
//!
//! Every bus's intensity depends on its neighbours' and theirs on it, so the
//! whole thing is solved at once. This is the proportional-sharing convention:
//! power arriving at a bus mixes, and everything leaving carries the mixture.
//! It is a convention rather than a physical fact — electrons are not labelled
//! — but it is the standard one, and it is at least self-consistent, which
//! naive attribution schemes are not.
//!
//! # Average against marginal
//!
//! **Average** intensity is total emissions over total generation. It answers
//! "what was the carbon content of what I used".
//!
//! **Marginal** intensity is the emissions of the plant that would respond to
//! one more megawatt-hour. It answers "what happens if I use more", which is
//! the question anyone deciding whether to shift a load is actually asking.
//!
//! They routinely differ by a factor of two, in either direction, and quoting
//! one when the other was meant is a common and consequential error. Both are
//! returned, separately named.

use gridwright_net::Network;

/// Emissions in every form this crate can compute.
#[derive(Debug, Clone)]
pub struct Emissions {
    /// Tonnes emitted, per generator.
    pub by_generator: Vec<f64>,
    /// Tonnes emitted inside each country, by the plant standing there.
    pub production_by_country: Vec<(String, f64)>,
    /// Tonnes attributable to what each country consumed, after tracing
    /// imports back through the network.
    pub consumption_by_country: Vec<(String, f64)>,
    /// Tonnes emitted per fuel, alongside the MWh that fuel generated.
    ///
    /// Both numbers, because the emissions alone cannot distinguish a small
    /// filthy fleet from a large clean one, and the ratio is what a fuel-mix
    /// chart is actually showing.
    pub by_carrier: Vec<CarrierTotals>,
    /// Average carbon intensity of the electricity consumed at each bus, in
    /// tonnes per MWh, per snapshot.
    pub intensity: Vec<Vec<f64>>,
    /// Marginal intensity at each bus per snapshot: the emissions of one more
    /// megawatt-hour consumed there.
    pub marginal_intensity: Vec<Vec<f64>>,
    /// Total tonnes over the horizon.
    pub total: f64,
    /// System average intensity, tonnes per MWh.
    pub average_intensity: f64,
    /// Emissions embodied in capacity built, as opposed to emitted running it.
    pub embodied: f64,
    /// Tonnes emitted to cover what the network lost in transmission.
    ///
    /// Not an addition to [`Emissions::total`] but a slice of it: this carbon
    /// was emitted and is already counted, and the question is what it was for.
    /// Consumption accounting spreads it silently over whoever drew power
    /// through the lines that lost it, which is defensible and hides one of the
    /// few numbers a transmission planner can actually act on. Reported
    /// separately so it can be seen.
    ///
    /// Zero when losses were not modelled, which is not the same as their being
    /// zero: a DC model without loss terms simply does not know.
    pub losses: f64,
    /// The same, per line, for finding which corridors are expensive.
    pub losses_by_line: Vec<f64>,
    /// Buses whose intensity could not be traced, because nothing reached them.
    ///
    /// Reported rather than filled with zero: an untraceable bus is one nothing
    /// flowed to, and calling that "zero carbon electricity" would be a lie of
    /// exactly the flattering kind.
    pub untraced: Vec<usize>,
}

/// Everything the accounting needs from a solved model.
///
/// Taken as plain slices rather than a solver type so this crate depends on
/// neither backend, and so a caller can feed it results from anywhere.
#[derive(Debug, Clone, Copy)]
pub struct SolvedFlows<'a> {
    /// `[generator][snapshot]`, MW.
    pub dispatch: &'a [Vec<f64>],
    /// `[line][snapshot]`, MW, positive from `bus0` toward `bus1`.
    pub flows: &'a [Vec<f64>],
    /// `[bus][snapshot]`, MW, unserved.
    pub shed: &'a [Vec<f64>],
    /// Capacity built per generator, MW. Empty when nothing was expandable.
    pub built: &'a [f64],
    /// `[line][snapshot]`, MW lost in transmission. Empty when losses were not
    /// modelled, which is not the same as their being zero.
    pub losses: &'a [Vec<f64>],
}

/// What one fuel emitted and how much it generated.
#[derive(Debug, Clone, PartialEq)]
pub struct CarrierTotals {
    pub carrier: String,
    /// Tonnes of CO₂.
    pub emissions: f64,
    /// MWh generated.
    pub generation: f64,
}

impl CarrierTotals {
    /// Tonnes per MWh for this fuel as it was actually run.
    ///
    /// Not the same as the input `co2_emissions` figure when several units
    /// share a carrier name and differ, which is the normal case for a fleet.
    /// Returns `None` for a fuel that generated nothing, since an intensity
    /// with no energy behind it is not a number anyone should plot.
    pub fn intensity(&self) -> Option<f64> {
        (self.generation > 1e-9).then(|| self.emissions / self.generation)
    }
}

/// Owning counterpart to [`SolvedFlows`].
///
/// [`SolvedFlows`] borrows so that a caller who already holds the results in
/// some other shape pays nothing to pass them. A caller reading results out of
/// a solver has to put them somewhere first, and this is that somewhere.
#[derive(Debug, Clone, Default)]
pub struct Flows {
    pub dispatch: Vec<Vec<f64>>,
    pub flows: Vec<Vec<f64>>,
    pub shed: Vec<Vec<f64>>,
    pub built: Vec<f64>,
    pub losses: Vec<Vec<f64>>,
}

impl Flows {
    pub fn as_slices(&self) -> SolvedFlows<'_> {
        SolvedFlows {
            dispatch: &self.dispatch,
            flows: &self.flows,
            shed: &self.shed,
            built: &self.built,
            losses: &self.losses,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EmissionsError {
    #[error("dispatch has {got} generators but the network has {want}")]
    DispatchShape { got: usize, want: usize },
    #[error("flows have {got} lines but the network has {want}")]
    FlowShape { got: usize, want: usize },
}

/// Solve a small dense system by Gaussian elimination with partial pivoting.
///
/// The intensity system is one per snapshot and sized by the bus count, so it
/// is solved many times on a long horizon. Dense is fine for the hundreds of
/// buses these models cluster to; a national model at full resolution would
/// want a sparse solve, and that is a change to this function alone.
fn solve_dense(n: usize, mut a: Vec<f64>, mut b: Vec<f64>) -> Option<Vec<f64>> {
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
        if best_abs < 1e-12 {
            return None;
        }
        if best != c {
            for k in 0..n {
                a.swap(c * n + k, best * n + k);
            }
            b.swap(c, best);
        }
        let pivot = a[c * n + c];
        for r in (c + 1)..n {
            let f = a[r * n + c] / pivot;
            if f == 0.0 {
                continue;
            }
            for k in c..n {
                a[r * n + k] -= f * a[c * n + k];
            }
            b[r] -= f * b[c];
        }
    }
    let mut x = vec![0.0; n];
    for c in (0..n).rev() {
        let mut acc = b[c];
        for k in (c + 1)..n {
            acc -= a[c * n + k] * x[k];
        }
        x[c] = acc / a[c * n + c];
    }
    Some(x)
}

/// Compute every emissions figure for a solved model.
pub fn account(net: &Network, sol: SolvedFlows<'_>) -> Result<Emissions, EmissionsError> {
    if sol.dispatch.len() != net.generators.len() {
        return Err(EmissionsError::DispatchShape {
            got: sol.dispatch.len(),
            want: net.generators.len(),
        });
    }
    if sol.flows.len() != net.lines.len() {
        return Err(EmissionsError::FlowShape {
            got: sol.flows.len(),
            want: net.lines.len(),
        });
    }

    let t = net.n_snapshots();
    let weights = net.snapshots.weights();
    let n_bus = net.buses.len();

    // --- Production side, which needs no tracing. ---
    let mut by_generator = vec![0.0; net.generators.len()];
    for (g, unit) in net.generators.iter().enumerate() {
        by_generator[g] = (0..t)
            .map(|s| sol.dispatch[g][s] * weights[s])
            .sum::<f64>()
            * unit.co2_emissions;
    }
    let total: f64 = by_generator.iter().sum();

    let mut production_by_country: Vec<(String, f64)> = Vec::new();
    for (g, unit) in net.generators.iter().enumerate() {
        let country = &net.buses[unit.bus].country;
        match production_by_country.iter_mut().find(|(c, _)| c == country) {
            Some(entry) => entry.1 += by_generator[g],
            None => production_by_country.push((country.clone(), by_generator[g])),
        }
    }

    let mut by_carrier: Vec<CarrierTotals> = Vec::new();
    for (g, unit) in net.generators.iter().enumerate() {
        let mwh: f64 = (0..t).map(|s| sol.dispatch[g][s] * weights[s]).sum();
        match by_carrier.iter_mut().find(|c| c.carrier == unit.carrier) {
            Some(entry) => {
                entry.emissions += by_generator[g];
                entry.generation += mwh;
            }
            None => by_carrier.push(CarrierTotals {
                carrier: unit.carrier.clone(),
                emissions: by_generator[g],
                generation: mwh,
            }),
        }
    }

    // --- Consumption side: trace the mixture through the network. ---
    let mut intensity = vec![vec![0.0; t]; n_bus];
    let mut untraced = Vec::new();

    for s in 0..t {
        // For each bus: intensity_b * inflow_b - sum(import * intensity_src) =
        // emissions generated at b. Written as a dense system in the bus
        // intensities.
        let mut a = vec![0.0; n_bus * n_bus];
        let mut rhs = vec![0.0; n_bus];
        let mut inflow = vec![0.0; n_bus];

        for (g, unit) in net.generators.iter().enumerate() {
            let p = sol.dispatch[g][s];
            if p > 0.0 {
                inflow[unit.bus] += p;
                rhs[unit.bus] += p * unit.co2_emissions;
            }
        }
        for (l, line) in net.lines.iter().enumerate() {
            let f = sol.flows[l][s];
            // Power arriving at a bus counts toward its inflow and carries the
            // exporting bus's mixture with it.
            let (from, to, mag) = if f > 0.0 {
                (line.bus0, line.bus1, f)
            } else if f < 0.0 {
                (line.bus1, line.bus0, -f)
            } else {
                continue;
            };
            inflow[to] += mag;
            a[to * n_bus + from] -= mag;
        }
        for b in 0..n_bus {
            a[b * n_bus + b] += inflow[b];
        }

        // A bus nothing reached has an all-zero row, which makes the system
        // singular. Pin it to zero intensity so the rest still solves, and
        // record it rather than pretending the answer means something.
        for b in 0..n_bus {
            if inflow[b] <= 1e-9 {
                for k in 0..n_bus {
                    a[b * n_bus + k] = 0.0;
                }
                a[b * n_bus + b] = 1.0;
                rhs[b] = 0.0;
                if !untraced.contains(&b) {
                    untraced.push(b);
                }
            }
        }

        if let Some(x) = solve_dense(n_bus, a, rhs) {
            for (b, row) in intensity.iter_mut().enumerate() {
                row[s] = x[b].max(0.0);
            }
        } else {
            for b in 0..n_bus {
                if !untraced.contains(&b) {
                    untraced.push(b);
                }
            }
        }
    }

    // Consumption emissions: each bus's demand times the mixture it drew on.
    let mut consumption_by_country: Vec<(String, f64)> = Vec::new();
    for (l, load) in net.loads.iter().enumerate() {
        let country = net.buses[load.bus].country.clone();
        let mut tonnes = 0.0;
        for s in 0..t {
            let demand = net.load_profile.at(l, s).unwrap_or(load.p_set);
            let served = (demand - sol.shed.get(load.bus).map_or(0.0, |v| v[s])).max(0.0);
            tonnes += served * weights[s] * intensity[load.bus][s];
        }
        match consumption_by_country.iter_mut().find(|(c, _)| c == &country) {
            Some(entry) => entry.1 += tonnes,
            None => consumption_by_country.push((country, tonnes)),
        }
    }

    // --- Marginal intensity. ---
    //
    // The emissions of whichever unit would answer one more megawatt-hour. Read
    // off the merit order: among units at the bus with headroom, the dearest
    // running one is the one that moves. Falls back to the cheapest idle unit
    // when everything running is already at its ceiling.
    let mut marginal_intensity = vec![vec![0.0; t]; n_bus];
    let gens_at = net.generators_by_bus();
    for (b, row) in marginal_intensity.iter_mut().enumerate() {
        for (s, cell) in row.iter_mut().enumerate() {
            let mut marginal: Option<(f64, f64)> = None; // (cost, emissions)
            for &g in gens_at.of(b) {
                let gi = g as usize;
                let unit = &net.generators[gi];
                let cap = unit.p_nom * net.gen_availability.at(gi, s).unwrap_or(1.0);
                let out = sol.dispatch[gi][s];
                let running = out > 1e-9;
                let headroom = cap - out > 1e-9;
                if running && headroom {
                    // A part-loaded unit is the marginal one by definition.
                    marginal = Some((unit.marginal_cost, unit.co2_emissions));
                    break;
                }
                if !running
                    && headroom
                    && marginal.is_none_or(|(c, _)| unit.marginal_cost < c)
                {
                    marginal = Some((unit.marginal_cost, unit.co2_emissions));
                }
            }
            *cell = marginal.map_or(0.0, |(_, e)| e);
        }
    }

    // --- Carbon spent on losses. ---
    //
    // Losses are charged half to each end of a line, so the carbon behind them
    // is the energy lost against the mixture at those two buses. This is a
    // reading of `total` rather than an addition to it: the emissions happened
    // and are already counted, and the point is to say what they bought.
    let mut losses_by_line = vec![0.0; net.lines.len()];
    for (l, line) in net.lines.iter().enumerate() {
        let Some(series) = sol.losses.get(l) else {
            continue;
        };
        let mut tonnes = 0.0;
        for (s, &lost) in series.iter().enumerate().take(t) {
            if lost <= 0.0 {
                continue;
            }
            let mix = 0.5 * (intensity[line.bus0][s] + intensity[line.bus1][s]);
            tonnes += lost * weights[s] * mix;
        }
        losses_by_line[l] = tonnes;
    }
    let losses: f64 = losses_by_line.iter().sum();

    // --- Embodied emissions. ---
    let embodied = net
        .generators
        .iter()
        .enumerate()
        .map(|(g, unit)| {
            let built = sol.built.get(g).copied().unwrap_or(0.0);
            built * unit.embodied_co2
        })
        .sum();

    let generated: f64 = (0..net.generators.len())
        .map(|g| (0..t).map(|s| sol.dispatch[g][s] * weights[s]).sum::<f64>())
        .sum();
    let average_intensity = if generated > 1e-9 {
        total / generated
    } else {
        0.0
    };

    untraced.sort_unstable();
    Ok(Emissions {
        by_generator,
        production_by_country,
        consumption_by_country,
        by_carrier,
        intensity,
        marginal_intensity,
        total,
        losses,
        losses_by_line,
        average_intensity,
        embodied,
        untraced,
    })
}

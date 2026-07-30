//! Cutting one region out of a network, so a continental model can be solved.
//!
//! The European extract is 7,893 buses and builds a program of 18,737 rows, past
//! what a browser tab will solve without freezing for several seconds. One country
//! out of it is a few hundred buses and solves in milliseconds — so a reader who has
//! switched the rest of the continent off can have an answer for what is left.
//!
//! # What a cut network is, and is not
//!
//! **A country lifted out of a synchronous grid is an island, and the answer is an
//! island's answer.** Every interconnector to the countries that were switched off
//! is severed, so the region has to serve its own demand from its own plant. For a
//! net importer that means shedding load it would never shed in reality; for a net
//! exporter it means plant sitting idle that would otherwise be selling.
//!
//! This is not a shortcut, it is the only self-consistent choice available. The
//! alternatives are worse:
//!
//! - Fixing the border flows to what they were in a full solve needs the full solve
//!   first, which is the thing that does not fit.
//! - Leaving the border buses free to inject whatever they like removes all scarcity
//!   from the problem: nothing is ever short, and every nodal price collapses to the
//!   cheapest generator's cost.
//!
//! So the region is solved as an island and the interface says so. A number that is
//! honest about what it assumed is worth more than one that is quietly wrong.
//!
//! # Mapping back
//!
//! A solve of the cut network is indexed by the cut network. The canvas draws the
//! whole file. So the maps below are not an optimisation — without them every price
//! and every loading would land on the wrong component, and the picture would look
//! entirely plausible.

use gridwright_net::{Network, TimeSeries};

/// A network cut down to some of its buses, and the way back.
pub struct Submodel {
    pub net: Network,
    /// For each bus in `net`, its index in the network this came from.
    pub bus_of: Vec<usize>,
    /// For each line in `net`, its index in the network this came from.
    pub line_of: Vec<usize>,
    /// Interconnectors that were cut, and therefore the reason this is an island.
    ///
    /// Counted rather than listed: the number is what a reader needs in order to
    /// know how much to distrust the answer, and on a continental model the list is
    /// hundreds long.
    pub severed: usize,
}

/// Cut `net` down to the buses `keep` accepts.
///
/// A line survives only when **both** of its ends do. One end inside and one
/// outside is an interconnector, and there is nothing sound to attach the far end
/// to; see the module docs.
///
/// Generators, loads and storage move with their bus. Their time series move with
/// them too, which is easy to forget and silent when forgotten: the rows are
/// indexed by component position, so dropping a generator without dropping its row
/// shifts every profile after it onto the wrong machine.
pub fn cut(net: &Network, keep: impl Fn(usize) -> bool) -> Submodel {
    let n_snap = net.snapshots.len();

    // Old index to new, and the reverse. `usize::MAX` for a bus left behind, so a
    // lookup that should never happen is a panic rather than a silent zero.
    let mut new_of = vec![usize::MAX; net.buses.len()];
    let mut bus_of = Vec::new();
    let mut buses = Vec::new();
    for (b, bus) in net.buses.iter().enumerate() {
        if !keep(b) {
            continue;
        }
        new_of[b] = buses.len();
        bus_of.push(b);
        buses.push(bus.clone());
    }

    let mut out = Network::new(net.snapshots.clone());
    out.buses = buses;
    out.base_mva = net.base_mva;
    out.value_of_lost_load = net.value_of_lost_load;
    out.co2_price = net.co2_price;
    // Global limits are deliberately dropped rather than carried.
    //
    // A carbon cap or a land budget written for a continent is not a cap for one
    // country of it, and scaling one by any measure available here -- buses, demand,
    // capacity -- would be inventing a policy. An absent limit is visibly absent;
    // a wrongly scaled one looks like a result.
    out.reserve_margin = net.reserve_margin;

    let mut line_of = Vec::new();
    let mut severed = 0usize;
    for (e, line) in net.lines.iter().enumerate() {
        let (a, b) = (new_of[line.bus0], new_of[line.bus1]);
        match (a == usize::MAX, b == usize::MAX) {
            (false, false) => {
                let mut kept = line.clone();
                kept.bus0 = a;
                kept.bus1 = b;
                line_of.push(e);
                out.lines.push(kept);
            }
            // Exactly one end inside: an interconnector, and the reason the result
            // is an island's.
            (true, false) | (false, true) => severed += 1,
            (true, true) => {}
        }
    }

    // Links are transport, and a link with one end outside is severed for the same
    // reason a line is.
    for link in &net.links {
        let (a, b) = (new_of[link.bus0], new_of[link.bus1]);
        if a != usize::MAX && b != usize::MAX {
            let mut kept = link.clone();
            kept.bus0 = a;
            kept.bus1 = b;
            out.links.push(kept);
        } else if a != usize::MAX || b != usize::MAX {
            severed += 1;
        }
    }

    // Each of these carries a time series indexed by component position, so the
    // rows are rebuilt alongside the components rather than after them.
    let (gens, gen_rows) = take(&net.generators, &net.gen_availability, n_snap, |g| {
        new_of[g.bus]
    });
    out.generators = gens
        .into_iter()
        .map(|(mut g, bus)| {
            g.bus = bus;
            g
        })
        .collect();
    out.gen_availability = rows(gen_rows, n_snap);

    let (loads, load_rows) = take(&net.loads, &net.load_profile, n_snap, |l| new_of[l.bus]);
    out.loads = loads
        .into_iter()
        .map(|(mut l, bus)| {
            l.bus = bus;
            l
        })
        .collect();
    out.load_profile = rows(load_rows, n_snap);

    let (units, inflow_rows) = take(&net.storage, &net.storage_inflow, n_snap, |s| new_of[s.bus]);
    out.storage = units
        .into_iter()
        .map(|(mut s, bus)| {
            s.bus = bus;
            s
        })
        .collect();
    out.storage_inflow = rows(inflow_rows, n_snap);

    Submodel {
        net: out,
        bus_of,
        line_of,
        severed,
    }
}

/// Keep the components whose bus survived, with their series rows.
///
/// Returns each surviving component beside its new bus index, and the rows in the
/// same order — which is the whole point, because a row list that has drifted from
/// its component list is a profile applied to the wrong machine and nothing about
/// the result says so.
fn take<T: Clone>(
    all: &[T],
    series: &TimeSeries,
    n_snap: usize,
    bus_of: impl Fn(&T) -> usize,
) -> (Vec<(T, usize)>, Vec<Vec<f64>>) {
    let mut kept = Vec::new();
    let mut rows = Vec::new();
    for (i, item) in all.iter().enumerate() {
        let bus = bus_of(item);
        if bus == usize::MAX {
            continue;
        }
        kept.push((item.clone(), bus));
        if !series.is_empty() {
            rows.push(
                series
                    .row(i)
                    .map(<[f64]>::to_vec)
                    .unwrap_or_else(|| vec![1.0; n_snap]),
            );
        }
    }
    (kept, rows)
}

/// Rebuild a series, or leave it empty when there was none.
fn rows(rows: Vec<Vec<f64>>, n_snap: usize) -> TimeSeries {
    if rows.is_empty() {
        return TimeSeries::empty();
    }
    TimeSeries::from_rows(&rows, n_snap).unwrap_or_else(|_| TimeSeries::empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gridwright_net::{Bus, Generator, Line, Load, Snapshots};

    /// Three buses in a line, one generator and one load at each end.
    fn chain(countries: &[&str]) -> Network {
        let mut net = Network::new(Snapshots::hourly(2));
        for (i, c) in countries.iter().enumerate() {
            net.buses.push(Bus {
                name: format!("b{i}"),
                country: (*c).to_string(),
                v_nom: 380.0,
                ..Bus::default()
            });
        }
        for i in 0..countries.len().saturating_sub(1) {
            net.lines.push(Line {
                name: format!("l{i}"),
                bus0: i,
                bus1: i + 1,
                s_nom: 1000.0,
                susceptance: 50.0,
                ..Line::default()
            });
        }
        net
    }

    #[test]
    fn a_cut_keeps_only_the_buses_asked_for() {
        let net = chain(&["A", "B", "C"]);
        let sub = cut(&net, |b| net.buses[b].country != "B");
        assert_eq!(sub.net.buses.len(), 2);
        assert_eq!(sub.bus_of, vec![0, 2]);
        assert_eq!(
            sub.net.buses.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["b0", "b2"],
        );
    }

    #[test]
    fn a_line_with_one_end_outside_is_severed_and_counted() {
        // Both of this chain's lines cross into B, so nothing survives and the
        // count is what tells a reader the answer is an island's.
        let net = chain(&["A", "B", "C"]);
        let sub = cut(&net, |b| net.buses[b].country != "B");
        assert!(sub.net.lines.is_empty());
        assert_eq!(sub.severed, 2);
        assert!(sub.line_of.is_empty());
    }

    #[test]
    fn a_line_inside_the_cut_survives_with_remapped_ends() {
        // **The index remap is the part that fails silently.** A kept line still
        // pointing at its old bus numbers would attach to whichever buses now sit
        // at those positions, and the network would validate.
        let net = chain(&["X", "A", "A", "X"]);
        let sub = cut(&net, |b| net.buses[b].country == "A");
        assert_eq!(sub.net.buses.len(), 2);
        assert_eq!(sub.net.lines.len(), 1, "the A-A line should survive");
        assert_eq!((sub.net.lines[0].bus0, sub.net.lines[0].bus1), (0, 1));
        assert_eq!(sub.line_of, vec![1], "and map back to line 1 of the original");
        assert_eq!(sub.severed, 2);
    }

    #[test]
    fn generators_and_loads_move_with_their_bus() {
        let mut net = chain(&["A", "B"]);
        net.generators.push(Generator {
            name: "ga".into(),
            bus: 0,
            p_nom: 100.0,
            ..Generator::default()
        });
        net.generators.push(Generator {
            name: "gb".into(),
            bus: 1,
            p_nom: 200.0,
            ..Generator::default()
        });
        net.loads.push(Load {
            name: "lb".into(),
            bus: 1,
            p_set: 50.0,
            ..Load::default()
        });

        let sub = cut(&net, |b| net.buses[b].country == "B");
        assert_eq!(sub.net.generators.len(), 1);
        assert_eq!(sub.net.generators[0].name, "gb");
        assert_eq!(sub.net.generators[0].bus, 0, "remapped onto the only bus left");
        assert_eq!(sub.net.loads.len(), 1);
        assert_eq!(sub.net.loads[0].bus, 0);
    }

    #[test]
    fn a_series_row_stays_with_the_component_it_belongs_to() {
        // **Easy to forget and silent when forgotten.** Rows are indexed by
        // component position, so dropping a generator without dropping its row
        // shifts every later profile onto the wrong machine -- and the result is a
        // solve that runs, validates, and is wrong.
        let mut net = chain(&["A", "B"]);
        for (name, bus) in [("g0", 0), ("g1", 1), ("g2", 1)] {
            net.generators.push(Generator {
                name: name.into(),
                bus,
                p_nom: 100.0,
                ..Generator::default()
            });
        }
        net.gen_availability = TimeSeries::from_rows(
            &[vec![0.1, 0.1], vec![0.2, 0.2], vec![0.3, 0.3]],
            2,
        )
        .unwrap();

        let sub = cut(&net, |b| net.buses[b].country == "B");
        assert_eq!(sub.net.generators.len(), 2);
        assert_eq!(sub.net.generators[0].name, "g1");
        assert_eq!(sub.net.generators[1].name, "g2");
        assert_eq!(sub.net.gen_availability.row(0), Some(&[0.2, 0.2][..]));
        assert_eq!(sub.net.gen_availability.row(1), Some(&[0.3, 0.3][..]));
    }

    #[test]
    fn a_network_with_no_series_stays_without_one() {
        let net = chain(&["A", "A"]);
        let sub = cut(&net, |_| true);
        assert!(sub.net.gen_availability.is_empty());
        assert!(sub.net.load_profile.is_empty());
    }

    #[test]
    fn keeping_everything_reproduces_the_network() {
        let mut net = chain(&["A", "A", "A"]);
        net.generators.push(Generator {
            name: "g".into(),
            bus: 1,
            p_nom: 10.0,
            ..Generator::default()
        });
        let sub = cut(&net, |_| true);
        assert_eq!(sub.net.buses.len(), net.buses.len());
        assert_eq!(sub.net.lines.len(), net.lines.len());
        assert_eq!(sub.net.generators.len(), net.generators.len());
        assert_eq!(sub.severed, 0);
        assert_eq!(sub.bus_of, vec![0, 1, 2]);
        assert_eq!(sub.line_of, vec![0, 1]);
    }

    #[test]
    fn a_cut_network_validates() {
        // The point of the remapping. An index left pointing outside the new bus
        // list is the failure this guards, and the engine is the judge of it.
        let mut net = chain(&["A", "B", "A", "A"]);
        net.generators.push(Generator {
            name: "g".into(),
            bus: 2,
            p_nom: 500.0,
            ..Generator::default()
        });
        net.loads.push(Load {
            name: "l".into(),
            bus: 3,
            p_set: 100.0,
            ..Load::default()
        });
        let sub = cut(&net, |b| net.buses[b].country == "A");
        assert!(
            sub.net.validate().is_ok(),
            "{:?}",
            sub.net.validate().err(),
        );
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn a_country_cut_out_of_the_european_network_is_a_solvable_model() {
        // **Asked of HiGHS, not of the solver the studio ships.**
        //
        // Cutting France out and solving it in the browser came back `Unbounded`,
        // and the first thing to establish is whose fault that is. This model cannot
        // be unbounded: every cost is positive, nothing is extendable, no load is
        // negative, and the objective is a sum of non-negative terms. So either the
        // cut produces a broken program or the solver cannot handle it.
        //
        // HiGHS solves all three of these to a finite optimum, which settles it --
        // the cut is sound, and the pure-Rust simplex that reaches wasm is what
        // cannot get an answer out of a network this fragmented. A country lifted
        // out of the extract is hundreds of small islands, and an island with no
        // reference bus leaves free angle variables that a simplex reads as an
        // unbounded direction.
        //
        // The studio therefore does not claim a region solve is trustworthy in a
        // browser. This test is what will notice when the simplex can do it.
        use gridwright_solve::{HighsSolver, Solver};

        const EU: &[u8] = include_bytes!("../../../examples/eu-grid.json");
        let net = gridwright_worker::load(Some("eu-grid.json"), EU).unwrap().network;

        for code in ["FR", "ES", "DE"] {
            let sub = cut(&net, |b| net.buses[b].country == code);
            assert!(!sub.net.buses.is_empty(), "{code} has no buses");
            assert!(
                sub.net.validate().is_ok(),
                "{code}: {:?}",
                sub.net.validate().err(),
            );

            let lopf = gridwright_build::build_lopf(&sub.net)
                .unwrap_or_else(|e| panic!("{code} does not build: {e}"));
            let sol = HighsSolver::default()
                .solve(&lopf)
                .unwrap_or_else(|e| panic!("{code}: {e}"));
            assert!(
                sol.objective.is_finite(),
                "{code} cut out of Europe has no finite optimum",
            );
        }
    }

    #[test]
    fn cutting_nothing_out_leaves_an_empty_network_rather_than_a_broken_one() {
        // A reader can switch every country off. That has to be an empty network,
        // not a network with dangling indices in it.
        let net = chain(&["A", "B"]);
        let sub = cut(&net, |_| false);
        assert!(sub.net.buses.is_empty());
        assert!(sub.net.lines.is_empty());
        assert!(sub.bus_of.is_empty());
        // Nothing severed, because there is no boundary to sever at. A line with
        // both ends outside the cut is not an interconnector that was broken, it is
        // simply not in the picture -- and counting it would tell a reader their
        // empty selection had cut something.
        assert_eq!(sub.severed, 0);
    }
}

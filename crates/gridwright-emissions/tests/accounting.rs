//! Emissions accounting, against figures worked out by hand.

use gridwright_emissions::{SolvedFlows, account};
use gridwright_net::{Generator, Line, Load, Network, Snapshots};

fn close(a: f64, b: f64) -> bool { (a - b).abs() < 1e-6 }

/// A exports to B. All the plant, and all the emissions, stand in A.
fn exporter() -> Network {
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("A", "AA");
    let b = net.add_bus("B", "BB");
    net.add_generator(Generator {
        name: "coal".into(), bus: a, p_nom: 200.0, marginal_cost: 10.0,
        co2_emissions: 1.0, ..Default::default()
    });
    net.add_load(Load { name: "la".into(), bus: a, p_set: 40.0, ..Default::default() });
    net.add_load(Load { name: "lb".into(), bus: b, p_set: 60.0, ..Default::default() });
    net.add_line(Line {
        name: "AB".into(), bus0: a, bus1: b, s_nom: 500.0, susceptance: 1.0,
        ..Default::default()
    });
    net
}

#[test]
fn production_and_consumption_differ_and_that_is_the_point() {
    // A burns everything, so all 100 t are produced in A. But 60 MWh of it was
    // for B, so B consumed 60 t. A country exporting power exports its carbon
    // with it, and a production-only account hides that entirely.
    let net = exporter();
    let e = account(&net, SolvedFlows {
        dispatch: &[vec![100.0]],
        flows: &[vec![60.0]],
        shed: &[vec![0.0], vec![0.0]],
        built: &[],
    }).unwrap();

    assert!(close(e.total, 100.0), "total {}", e.total);

    let prod = |c: &str| e.production_by_country.iter().find(|(k, _)| k == c).map_or(0.0, |(_, v)| *v);
    assert!(close(prod("AA"), 100.0), "AA produced {}", prod("AA"));
    assert!(close(prod("BB"), 0.0), "BB produced {}", prod("BB"));

    let cons = |c: &str| e.consumption_by_country.iter().find(|(k, _)| k == c).map_or(0.0, |(_, v)| *v);
    assert!(close(cons("AA"), 40.0), "AA consumed {}", cons("AA"));
    assert!(close(cons("BB"), 60.0), "BB consumed {}", cons("BB"));
}

#[test]
fn imported_power_carries_the_exporters_intensity() {
    let net = exporter();
    let e = account(&net, SolvedFlows {
        dispatch: &[vec![100.0]],
        flows: &[vec![60.0]],
        shed: &[vec![0.0], vec![0.0]],
        built: &[],
    }).unwrap();
    // B generates nothing, so its intensity is entirely inherited.
    assert!(close(e.intensity[1][0], 1.0), "B intensity {}", e.intensity[1][0]);
    assert!(e.untraced.is_empty(), "nothing should be untraceable here");
}

#[test]
fn power_from_two_sources_mixes_in_proportion() {
    // 100 MW at 1.0 and 100 MW at 0.0 both feeding one bus gives 0.5 there.
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("A", "AA");
    let b = net.add_bus("B", "BB");
    let c = net.add_bus("C", "CC");
    net.add_generator(Generator {
        name: "coal".into(), bus: a, p_nom: 200.0, marginal_cost: 10.0,
        co2_emissions: 1.0, ..Default::default()
    });
    net.add_generator(Generator {
        name: "wind".into(), bus: c, p_nom: 200.0, marginal_cost: 0.0,
        ..Default::default()
    });
    net.add_load(Load { name: "lb".into(), bus: b, p_set: 200.0, ..Default::default() });
    net.add_line(Line { name: "AB".into(), bus0: a, bus1: b, s_nom: 500.0,
        susceptance: 1.0, ..Default::default() });
    net.add_line(Line { name: "CB".into(), bus0: c, bus1: b, s_nom: 500.0,
        susceptance: 1.0, ..Default::default() });

    let e = account(&net, SolvedFlows {
        dispatch: &[vec![100.0], vec![100.0]],
        flows: &[vec![100.0], vec![100.0]],
        shed: &[vec![0.0], vec![0.0], vec![0.0]],
        built: &[],
    }).unwrap();
    assert!(close(e.intensity[1][0], 0.5), "mixed intensity {}", e.intensity[1][0]);
}

#[test]
fn average_and_marginal_intensity_are_different_numbers() {
    // 80 MW of clean and 20 MW of dirty. Average is 0.2; the marginal unit is
    // the dirty one at 1.0. Reporting either as "the" carbon intensity would be
    // wrong for half of the questions people ask.
    let mut net = Network::new(Snapshots::hourly(1));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "clean".into(), bus: b, p_nom: 80.0, marginal_cost: 1.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "dirty".into(), bus: b, p_nom: 40.0, marginal_cost: 50.0,
        co2_emissions: 1.0, ..Default::default()
    });
    net.add_load(Load { name: "l".into(), bus: b, p_set: 100.0, ..Default::default() });

    let e = account(&net, SolvedFlows {
        dispatch: &[vec![80.0], vec![20.0]],
        flows: &[],
        shed: &[vec![0.0]],
        built: &[],
    }).unwrap();

    assert!(close(e.average_intensity, 0.2), "average {}", e.average_intensity);
    assert!(close(e.intensity[0][0], 0.2), "bus average {}", e.intensity[0][0]);
    // The dirty unit is part-loaded, so it is the one that would respond.
    assert!(close(e.marginal_intensity[0][0], 1.0),
            "marginal {}", e.marginal_intensity[0][0]);
    assert!(e.marginal_intensity[0][0] > e.average_intensity * 4.0,
            "the two should differ substantially here");
}

#[test]
fn embodied_emissions_are_counted_against_what_was_built() {
    let mut net = Network::new(Snapshots::hourly(1));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "wind".into(), bus: b, p_nom: 0.0, marginal_cost: 0.0,
        p_nom_extendable: true, p_nom_max: 500.0, capital_cost: 1.0,
        embodied_co2: 3.0, ..Default::default()
    });
    net.add_load(Load { name: "l".into(), bus: b, p_set: 50.0, ..Default::default() });

    let e = account(&net, SolvedFlows {
        dispatch: &[vec![50.0]],
        flows: &[],
        shed: &[vec![0.0]],
        built: &[50.0],
    }).unwrap();
    // Nothing emitted running it, 150 t emitted making it.
    assert!(close(e.total, 0.0), "operational {}", e.total);
    assert!(close(e.embodied, 150.0), "embodied {}", e.embodied);
}

#[test]
fn a_bus_nothing_reached_is_reported_rather_than_called_clean() {
    // An isolated bus with no generation and no imports has no defined
    // intensity. Filling it with zero would read as carbon-free electricity,
    // which is the most flattering possible lie.
    let mut net = Network::new(Snapshots::hourly(1));
    let a = net.add_bus("A", "AA");
    let _b = net.add_bus("B", "BB");
    net.add_generator(Generator {
        name: "g".into(), bus: a, p_nom: 100.0, marginal_cost: 1.0,
        co2_emissions: 0.5, ..Default::default()
    });
    net.add_load(Load { name: "l".into(), bus: a, p_set: 50.0, ..Default::default() });

    let e = account(&net, SolvedFlows {
        dispatch: &[vec![50.0]],
        flows: &[],
        shed: &[vec![0.0], vec![0.0]],
        built: &[],
    }).unwrap();
    assert!(e.untraced.contains(&1), "the isolated bus should be flagged, got {:?}", e.untraced);
}

#[test]
fn mismatched_input_shapes_are_refused() {
    let net = exporter();
    assert!(account(&net, SolvedFlows {
        dispatch: &[], flows: &[vec![0.0]], shed: &[vec![0.0], vec![0.0]], built: &[],
    }).is_err());
}

#[test]
fn emissions_group_by_fuel_and_carry_their_generation_with_them() {
    // Two coal units of different vintages plus wind. Grouping by fuel has to
    // add both coal units together, and the fleet intensity that comes out is
    // neither unit's own figure.
    let mut net = Network::new(Snapshots::hourly(1));
    let b = net.add_bus("B", "XX");
    for (name, cost, co2, carrier) in [
        ("old_coal", 30.0, 1.1, "coal"),
        ("new_coal", 25.0, 0.8, "coal"),
        ("wind", 0.0, 0.0, "wind"),
    ] {
        net.add_generator(Generator {
            name: name.into(), bus: b, p_nom: 200.0, marginal_cost: cost,
            co2_emissions: co2, carrier: carrier.into(), ..Default::default()
        });
    }
    net.add_load(Load { name: "l".into(), bus: b, p_set: 250.0, ..Default::default() });

    let e = account(&net, SolvedFlows {
        dispatch: &[vec![100.0], vec![50.0], vec![100.0]],
        flows: &[],
        shed: &[vec![0.0]],
        built: &[],
    }).unwrap();

    let coal = e.by_carrier.iter().find(|c| c.carrier == "coal").expect("coal missing");
    // 100 * 1.1 + 50 * 0.8 = 150 t over 150 MWh.
    assert!(close(coal.emissions, 150.0), "coal emissions {}", coal.emissions);
    assert!(close(coal.generation, 150.0), "coal generation {}", coal.generation);
    assert!(close(coal.intensity().unwrap(), 1.0), "fleet intensity {:?}", coal.intensity());

    let wind = e.by_carrier.iter().find(|c| c.carrier == "wind").expect("wind missing");
    assert!(close(wind.emissions, 0.0));
    assert!(close(wind.generation, 100.0));

    assert_eq!(e.by_carrier.len(), 2, "got {:?}", e.by_carrier);
}

#[test]
fn a_fuel_that_did_not_run_reports_no_intensity_rather_than_zero() {
    // Zero over zero is not zero. A fuel sitting idle has no intensity, and
    // printing 0.0 t/MWh next to an unused coal plant would be worse than
    // printing nothing.
    let mut net = Network::new(Snapshots::hourly(1));
    let b = net.add_bus("B", "XX");
    net.add_generator(Generator {
        name: "coal".into(), bus: b, p_nom: 100.0, marginal_cost: 90.0,
        co2_emissions: 1.0, carrier: "coal".into(), ..Default::default()
    });
    net.add_load(Load { name: "l".into(), bus: b, p_set: 0.0, ..Default::default() });

    let e = account(&net, SolvedFlows {
        dispatch: &[vec![0.0]], flows: &[], shed: &[vec![0.0]], built: &[],
    }).unwrap();
    assert!(e.by_carrier[0].intensity().is_none());
}

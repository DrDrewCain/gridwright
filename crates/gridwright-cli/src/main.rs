//! `gw` — build and solve transnational energy system models.
//!
//! The `bench` subcommand exists because the project's whole premise is a
//! performance claim, and a claim nobody can reproduce is marketing. It
//! generates a network of a stated shape, times construction and solve
//! separately, and prints the numbers whether or not they flatter us.

use std::time::Instant;

use gridwright_build::{Lopf, build_lopf};
use gridwright_io::{Results, load_network};
use gridwright_net::{Generator, Line, Load, Network, Snapshots, StorageUnit, TimeSeries};
use gridwright_solve::{HighsSolver, Solver};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or("help") {
        "bench" => {
            let buses = arg(&args, "--buses").unwrap_or(50.0) as usize;
            let hours = arg(&args, "--hours").unwrap_or(24.0) as usize;
            let solve = args.iter().any(|a| a == "--solve");
            bench(buses, hours, solve);
        }
        "demo" => demo(),
        "case" => {
            let Some(path) = args.get(1) else {
                eprintln!("usage: gw case <file>");
                std::process::exit(2);
            };
            run_case(path);
        }
        "formats" => list_formats(),
        "identify" => {
            let Some(path) = args.get(1) else {
                eprintln!("usage: gw identify <file-or-directory>");
                std::process::exit(2);
            };
            match gridwright_io::sniff(path) {
                Ok(f) => {
                    println!("{path}: {}", f.label());
                    if !f.available() {
                        println!("  this build cannot read it");
                    }
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        "run" => {
            let Some(dir) = args.get(1) else {
                eprintln!("usage: gw run <network-dir> [--out <dir>]");
                std::process::exit(2);
            };
            let out = args
                .iter()
                .position(|a| a == "--out")
                .and_then(|i| args.get(i + 1))
                .cloned();
            run(dir, out.as_deref());
        }
        _ => {
            eprintln!(
                "gw — gridwright: cross-border energy system modelling\n\
                 \n  gw demo                              two-country dispatch example\
                 \n  gw run <dir> [--out <dir>]           solve a network of CSV files\
                 \n  gw case <file>                       solve a network in any format\
                 \n  gw identify <file>                   say what a file is\
                 \n  gw formats                           list every format this reads\
                 \n  gw bench [--buses N] [--hours H] [--solve]\n\
                 \n\
                 bench reports construction time separately from solve time,\n\
                 because construction is what this engine claims to be fast at\n\
                 and the solve is HiGHS doing its own work."
            );
        }
    }
}

fn arg(args: &[String], name: &str) -> Option<f64> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1)?.parse().ok()
}

/// A synthetic network shaped like a continental transmission system.
///
/// Not real data, and not claimed to be. It generates problems of a given size
/// whose structure resembles the real thing, so timings describe the shape of
/// the workload rather than one specific dataset. A ring plus chords gives
/// every bus degree three or more without the dense graph random pairing would
/// produce.
fn synthetic(n_buses: usize, n_hours: usize) -> Network {
    let mut net = Network::new(Snapshots::hourly(n_hours));

    const COUNTRIES: [&str; 15] = [
        "DE", "FR", "ES", "IT", "PL", "NL", "BE", "AT", "SE", "DK", "CZ", "PT", "FI", "NO", "CH",
    ];
    for b in 0..n_buses {
        net.add_bus(format!("bus{b}"), COUNTRIES[b % COUNTRIES.len()]);
    }

    // Ring, so the graph is connected, plus chords for meshing.
    for b in 0..n_buses {
        net.add_line(Line {
            name: format!("ring{b}"),
            bus0: b,
            bus1: (b + 1) % n_buses,
            s_nom: 3000.0,
            susceptance: 10.0,
            ..Default::default()
        });
    }
    if n_buses > 8 {
        for b in (0..n_buses).step_by(2) {
            let far = (b + n_buses / 3) % n_buses;
            if far != b {
                net.add_line(Line {
                    name: format!("chord{b}"),
                    bus0: b,
                    bus1: far,
                    s_nom: 1500.0,
                    susceptance: 6.0,
                    ..Default::default()
                });
            }
        }
    }

    // Three generators per bus: baseload, peaking, and a variable renewable
    // whose availability profile is what makes the time series matter.
    let mut avail_rows: Vec<Vec<f64>> = Vec::with_capacity(n_buses * 3);
    for b in 0..n_buses {
        net.add_generator(Generator {
            name: format!("base{b}"),
            bus: b,
            p_nom: 800.0,
            marginal_cost: 12.0 + (b % 5) as f64,
            p_min_pu: 0.0,
            ..Default::default()
        });
        avail_rows.push(vec![1.0; n_hours]);

        net.add_generator(Generator {
            name: format!("peak{b}"),
            bus: b,
            p_nom: 400.0,
            marginal_cost: 85.0 + (b % 11) as f64,
            p_min_pu: 0.0,
            ..Default::default()
        });
        avail_rows.push(vec![1.0; n_hours]);

        net.add_generator(Generator {
            name: format!("wind{b}"),
            bus: b,
            p_nom: 600.0,
            marginal_cost: 0.0,
            p_min_pu: 0.0,
            ..Default::default()
        });
        // A daily cycle offset per bus, so profiles differ across the system
        // the way weather does, without pulling in a real weather dataset.
        avail_rows.push(
            (0..n_hours)
                .map(|t| {
                    let phase = (t + b * 7) as f64 * std::f64::consts::TAU / 24.0;
                    (0.45 + 0.45 * phase.sin()).clamp(0.0, 1.0)
                })
                .collect(),
        );
    }
    net.gen_availability = TimeSeries::from_rows(&avail_rows, n_hours).unwrap();

    // Demand with a daily shape, scaled per bus.
    let mut load_rows: Vec<Vec<f64>> = Vec::with_capacity(n_buses);
    for b in 0..n_buses {
        net.add_load(Load {
            name: format!("load{b}"),
            bus: b,
            p_set: 700.0,
            ..Default::default()
        });
        load_rows.push(
            (0..n_hours)
                .map(|t| {
                    let phase = t as f64 * std::f64::consts::TAU / 24.0;
                    700.0 * (1.0 + 0.25 * phase.sin()) + (b % 13) as f64 * 10.0
                })
                .collect(),
        );
    }
    net.load_profile = TimeSeries::from_rows(&load_rows, n_hours).unwrap();

    // Storage on every fourth bus.
    for b in (0..n_buses).step_by(4) {
        net.add_storage(StorageUnit {
            name: format!("batt{b}"),
            bus: b,
            p_nom: 200.0,
            max_hours: 6.0,
            efficiency_store: 0.92,
            efficiency_dispatch: 0.92,
            cyclic: true,
            ..Default::default()
        });
    }

    net
}

fn bench(n_buses: usize, n_hours: usize, do_solve: bool) {
    println!("synthetic network: {n_buses} buses x {n_hours} snapshots");

    let t0 = Instant::now();
    let net = synthetic(n_buses, n_hours);
    let gen_time = t0.elapsed();

    println!(
        "  network:      {} buses, {} lines, {} generators, {} loads, {} storage",
        net.buses.len(),
        net.lines.len(),
        net.generators.len(),
        net.loads.len(),
        net.storage.len()
    );
    println!("  data setup:   {:>10.3} ms", gen_time.as_secs_f64() * 1e3);

    let t1 = Instant::now();
    let lopf = match build_lopf(&net) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("build failed: {e}");
            std::process::exit(1);
        }
    };
    let build_time = t1.elapsed();

    let counts = Lopf::row_counts(&net);
    println!(
        "  problem:      {} cols, {} rows, {} nonzeros",
        lopf.model.num_cols(),
        lopf.model.num_rows(),
        lopf.model.nnz()
    );
    println!(
        "                balance {}, dc flow {}, storage {}",
        counts.balance, counts.dc_flow, counts.storage
    );
    println!(
        "  CONSTRUCTION: {:>10.3} ms  <- the claim",
        build_time.as_secs_f64() * 1e3
    );

    // The transpose used to be timed here, after construction. It is now inside
    // it: rows are scattered into column major form as they are absorbed, so
    // the figure above already includes it and there is no separate step left
    // to charge for. The line stays to say so, because it was reported for long
    // enough that its disappearance would look like the work went missing.
    println!(
        "  csr->csc:            (in construction, {} nonzeros)",
        lopf.model.matrix().nnz()
    );

    // Throughput is what compares across machines and problem sizes; absolute
    // milliseconds mean nothing without both.
    println!(
        "  throughput:   {:>10.2} M nonzeros/s",
        lopf.model.nnz() as f64 / build_time.as_secs_f64() / 1e6
    );

    if do_solve {
        let t3 = Instant::now();
        match HighsSolver::default().solve(&lopf) {
            Ok(sol) => {
                let solve_time = t3.elapsed();
                println!(
                    "  solve:        {:>10.3} ms  (HiGHS, {:?})",
                    solve_time.as_secs_f64() * 1e3,
                    sol.status
                );
                println!("  objective:    {:>10.0}", sol.objective);
                let shed = sol.total_shed(&lopf.vars);
                if shed > 1e-6 {
                    println!("  unserved:     {shed:>10.1} MWh");
                }
                println!(
                    "  build share:  {:>9.1}% of build + solve",
                    100.0 * build_time.as_secs_f64()
                        / (build_time.as_secs_f64() + solve_time.as_secs_f64())
                );
            }
            Err(e) => eprintln!("solve failed: {e}"),
        }
    }
}

fn demo() {
    let mut net = Network::new(Snapshots::hourly(4));
    let de = net.add_bus("DE", "DE");
    let fr = net.add_bus("FR", "FR");
    net.add_generator(Generator {
        name: "de_coal".into(),
        bus: de,
        p_nom: 100.0,
        marginal_cost: 40.0,
        p_min_pu: 0.0,
        ..Default::default()
    });
    net.add_generator(Generator {
        name: "fr_nuclear".into(),
        bus: fr,
        p_nom: 200.0,
        marginal_cost: 10.0,
        p_min_pu: 0.0,
        ..Default::default()
    });
    net.add_line(Line {
        name: "DE-FR".into(),
        bus0: de,
        bus1: fr,
        s_nom: 50.0,
        susceptance: 0.0,
        ..Default::default()
    });
    net.add_load(Load {
        name: "de_load".into(),
        bus: de,
        p_set: 80.0,
        ..Default::default()
    });

    let lopf = build_lopf(&net).expect("demo network should build");
    let sol = HighsSolver::default()
        .solve(&lopf)
        .expect("demo network should solve");

    println!("two countries, one 50 MW interconnector, 80 MW of German demand\n");
    println!("  status:     {:?}", sol.status);
    println!("  total cost: {:.0}", sol.objective);
    println!();
    for (i, g) in net.generators.iter().enumerate() {
        println!(
            "  {:<12} {:>6.1} MW  at {:>5.1}/MWh",
            g.name,
            sol.dispatch(&lopf.vars, i)[0],
            g.marginal_cost
        );
    }
    println!(
        "\n  flow DE->FR: {:>6.1} MW  (negative means power arriving in DE)",
        sol.flow(&lopf.vars, 0)[0]
    );
    println!("\n  marginal price by country:");
    for (b, bus) in net.buses.iter().enumerate() {
        println!("    {:<4} {:>6.1}/MWh", bus.country, sol.price(b, 4)[0].abs());
    }
    println!(
        "\n  the prices differ because the interconnector is full: once it\n  \
         saturates, the two countries stop being one market."
    );
}

/// Solve a network read from disk and optionally write the results back.
fn run(dir: &str, out: Option<&str>) {
    let net = match load_network(dir) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("could not load {dir}: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "loaded {} buses, {} lines, {} generators, {} loads, {} storage, {} snapshots",
        net.buses.len(),
        net.lines.len(),
        net.generators.len(),
        net.loads.len(),
        net.storage.len(),
        net.n_snapshots()
    );

    let t0 = Instant::now();
    let lopf = match build_lopf(&net) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("build failed: {e}");
            std::process::exit(1);
        }
    };
    let build_ms = t0.elapsed().as_secs_f64() * 1e3;

    let t1 = Instant::now();
    let sol = match HighsSolver::default().solve(&lopf) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("solve failed: {e}");
            std::process::exit(1);
        }
    };
    let solve_ms = t1.elapsed().as_secs_f64() * 1e3;

    println!(
        "  {} cols, {} rows, {} nonzeros",
        lopf.model.num_cols(),
        lopf.model.num_rows(),
        lopf.model.nnz()
    );
    println!("  build {build_ms:.3} ms, solve {solve_ms:.3} ms");
    println!("  status {:?}, objective {:.2}", sol.status, sol.objective);

    let shed = sol.total_shed(&lopf.vars);
    if shed > 1e-6 {
        println!("  UNSERVED ENERGY: {shed:.1} MWh");
    }

    // Capacity decisions are the headline of an expansion run, so they are
    // printed rather than left in a file the user has to go and open.
    let mut built = Vec::new();
    for (g, unit) in net.generators.iter().enumerate() {
        if let Some(cap) = lopf.vars.gen_capacity[g] {
            built.push((unit.name.clone(), sol.total_capacity(Some(cap), unit.p_nom)));
        }
    }
    for (l, line) in net.lines.iter().enumerate() {
        if let Some(cap) = lopf.vars.line_capacity[l] {
            built.push((line.name.clone(), sol.total_capacity(Some(cap), line.s_nom)));
        }
    }
    for (s, unit) in net.storage.iter().enumerate() {
        if let Some(cap) = lopf.vars.storage_capacity[s] {
            built.push((unit.name.clone(), sol.total_capacity(Some(cap), unit.p_nom)));
        }
    }
    if !built.is_empty() {
        println!("\n  capacity built:");
        for (name, mw) in &built {
            println!("    {name:<20} {mw:>10.2} MW");
        }
    }

    if let Some(out) = out {
        let n = net.n_snapshots();
        let results = Results {
            network: &net,
            dispatch: (0..net.generators.len())
                .map(|g| sol.dispatch(&lopf.vars, g))
                .collect(),
            flows: (0..net.lines.len())
                .map(|l| sol.flow(&lopf.vars, l))
                .collect(),
            prices: (0..net.buses.len()).map(|b| sol.price(b, n)).collect(),
            shed: (0..net.buses.len())
                .map(|b| sol.shed(&lopf.vars, b))
                .collect(),
            built,
        };
        match results.write(out) {
            Ok(()) => println!("\n  results written to {out}/"),
            Err(e) => eprintln!("could not write results: {e}"),
        }
    }
}

/// What this build can read.
fn list_formats() {
    use gridwright_io::Format::*;
    println!("gw reads, and identifies from the file itself:\n");
    for f in [
        CsvDirectory,
        ParquetDirectory,
        Matpower,
        Psse,
        PowerModels,
        Rawx,
        NativeJson,
        Netcdf,
        Excel,
        Cgmes,
    ] {
        println!(
            "  {:<20} {}",
            f.label(),
            if f.available() {
                "yes"
            } else {
                "not built into this binary"
            }
        );
    }
    println!(
        "\nEvery reader also reports what it had to drop, since each format\n\
         carries more than a linear model can hold and each carries a\n\
         different more."
    );
}

/// Solve a network in whatever format it arrived in.
///
/// The format is worked out from the file rather than asked for, because
/// someone who has been handed a grid model has a file and not necessarily any
/// idea which of a dozen conventions produced it.
fn run_case(path: &str) {
    let case = match gridwright_io::load_any(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("could not load {path}: {e}");
            std::process::exit(1);
        }
    };
    let net = &case.network;
    println!("{}", case.name);
    println!(
        "  {} buses, {} branches, {} generators, {} loads, {} synchronous areas",
        net.buses.len(),
        net.lines.len(),
        net.generators.len(),
        net.loads.len(),
        net.synchronous_areas().len()
    );
    for n in &case.notes {
        println!("  note: {n}");
    }

    let t0 = Instant::now();
    let lopf = match build_lopf(net) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("build failed: {e}");
            std::process::exit(1);
        }
    };
    let build_ms = t0.elapsed().as_secs_f64() * 1e3;

    let t1 = Instant::now();
    let sol = match HighsSolver::default().solve(&lopf) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("solve failed: {e}");
            std::process::exit(1);
        }
    };
    let solve_ms = t1.elapsed().as_secs_f64() * 1e3;

    let demand: f64 = (0..net.loads.len())
        .map(|l| net.load_profile.at(l, 0).unwrap_or(net.loads[l].p_set))
        .sum();
    let served: f64 = (0..net.generators.len())
        .map(|g| sol.dispatch(&lopf.vars, g)[0])
        .sum();

    println!(
        "  {} cols, {} rows, {} nonzeros",
        lopf.model.num_cols(),
        lopf.model.num_rows(),
        lopf.model.nnz()
    );
    println!("  build {build_ms:.3} ms, solve {solve_ms:.3} ms");
    println!("  status {:?}", sol.status);
    println!("  DC-OPF cost {:.2}", sol.objective);
    println!("  demand {demand:.1} MW, generation {served:.1} MW");
    let shed = sol.total_shed(&lopf.vars);
    if shed > 1e-4 {
        println!("  unserved {shed:.2} MW");
    }
}

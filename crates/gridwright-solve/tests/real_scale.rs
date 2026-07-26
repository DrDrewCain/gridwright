//! Scaling measured on a real network carrying a real year of hourly data.
//!
//! Every scaling number this project has published was measured on the
//! synthetic ring in `scale.rs`: a regular topology, every bus identical, every
//! line the same rating, and a demand profile that is a sine wave. Two separate
//! things in that sentence could be flattering the solve, and until now nobody
//! had separated them.
//!
//! A ring is the friendliest topology there is. Its incidence matrix is banded,
//! every bus has degree two, and there is exactly one loop, so the DC flow
//! equations have almost no structure for a factorisation to struggle with.
//! Real transmission networks are nothing like that: degree varies from one to
//! twenty, radial spurs hang off a meshed core, and the sparsity pattern has no
//! bandwidth to speak of. Separately, a sine wave repeats, and an optimiser
//! that has already priced one Tuesday has effectively priced the rest of them.
//! Real demand does not repeat, real wind does not repeat, and the hard hours
//! of a year are hard precisely because they are unlike the others.
//!
//! So this file measures the same quantities as `scale.rs` on real published
//! networks carrying real measured time series, and then measures the ring
//! again in the same process at matched problem size, so the two numbers differ
//! only in what is being modelled and not in what machine was free that
//! afternoon. The comparison is the point. A real topology being *harder* is a
//! finding, and a real topology being *easier* is equally a finding and would
//! mean the ring's regularity was never the thing making it fast.
//!
//! # What is real here and what is not
//!
//! Real: the topology, the ratings, the reactances, the generator capacities
//! and costs, and the spatial distribution of demand, all from PGLib-OPF
//! v23.07 under CC-BY 4.0, unmodified. Real: the hourly shape of demand and of
//! wind and solar output, from measurements published by the four German
//! transmission control zones through the ENTSO-E Transparency Platform and
//! redistributed by Open Power System Data.
//!
//! Not real, and labelled as such everywhere it appears: the pairing between
//! the two. IEEE 118 is a model of an American system and the demand shape
//! hung on it was metered in Germany. The storage fleet is invented outright,
//! for a reason set out at `add_storage_fleet`. Which generators are called
//! wind and which solar is our choice, since MATPOWER records no carrier.
//!
//! What that costs is bounded and worth stating plainly: this fixture is not a
//! study of any real power system and no dispatch, price or cost it produces
//! means anything about Germany or about the networks the cases model. It is a
//! linear program of realistic *structure*, which is the only property a
//! scaling measurement depends on.
//!
//! # Getting the data
//!
//! The time series is not committed. Run
//!
//! ```text
//! python3 benchmarks/fetch_opsd_time_series.py
//! ```
//!
//! once, which caches into the gitignored `benchmarks/.cache/`. Every test here
//! prints an explanation and returns rather than failing when it is absent, so
//! a fresh clone stays green.
//!
//! # Running the measurements
//!
//! ```text
//! cargo test -p gridwright-solve --test real_scale --release -- --ignored --nocapture
//! ```
//!
//! Ignored by default because the ladder runs for hours and exists to produce
//! numbers for a human rather than to guard a behaviour. Two cheap tests here
//! are *not* ignored, because they guard the fixture itself: a fixture that
//! assembles without complaint but does not carry the variation its comments
//! claim would produce a perfectly plausible table describing a different model.
//!
//! For peak memory, run `benchmarks/real_scale_memory.sh`, which puts one rung
//! in one process under `/usr/bin/time -l`. The memory column printed by the
//! comparison below is a process high-water mark and so inherits whatever the
//! earlier rungs reached, which is close enough to read a trend from and not
//! close enough to quote.
//!
//! Every timing carries the one-minute load average it was taken under. This
//! project has had to withdraw three published numbers that were measured on a
//! busy machine, most recently its entire scaling table, and in each case
//! nothing in the recorded output said the machine was busy. Now it does.

#![cfg(feature = "highs")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gridwright_build::build_lopf;
use gridwright_io::matpower::load_case;
use gridwright_net::{Generator, Line, Load, Network, Snapshots, StorageUnit, TimeSeries};
use gridwright_solve::rolling::{Horizon, solve_rolling};
use gridwright_solve::{HighsSolver, Solver};

/// How many times each measurement is repeated, with the best kept.
///
/// Two, matching the standard the rest of this project settled on after a
/// loaded machine put a wrong number into the README three separate times. Best
/// rather than mean, because load can only ever make a timing worse: the
/// fastest run is the one least contaminated by whatever else the machine was
/// doing. The spread between the two runs is printed alongside, because a wide
/// spread means the machine was not idle and the number should not be quoted.
const RUNS: usize = 2;

/// Hours in the modelled year. 2019 was not a leap year.
const HOURS: usize = 8760;

/// The four German transmission control zones, in the order the distilled CSV
/// writes them.
const N_ZONES: usize = 4;

// ---------------------------------------------------------------------------
// The measured time series
// ---------------------------------------------------------------------------

/// One calendar year of hourly measurements for four control zones.
///
/// Load is in megawatts as metered. Solar and wind are per unit of their own
/// annual maximum, so the shape is measured and the level is a normalisation;
/// see `benchmarks/fetch_opsd_time_series.py` for why that choice was forced
/// and what it costs.
struct ControlZones {
    /// Demand as a fraction of the zone's own annual peak, which is the form
    /// every caller wants and so is the form it is stored in.
    ///
    /// Scaling by the peak rather than by the mean is deliberate. A MATPOWER
    /// case is a single published operating point and its generation fleet is
    /// sized for that point, so treating it as the *annual peak* keeps every
    /// hour of the year inside the envelope the case was published as feasible
    /// in. Treating it as the annual mean instead would put roughly a third of
    /// the year above the case's own peak and turn the measurement into a study
    /// of how quickly the solver can shed load, which is a different program
    /// with a different shape.
    demand_pu: Vec<Vec<f64>>,
    solar: Vec<Vec<f64>>,
    wind: Vec<Vec<f64>>,
}

fn opsd_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../benchmarks/.cache/opsd_de_control_zones_2019_hourly.csv")
}

/// Read the distilled year, or `None` if it has not been fetched.
///
/// The header is checked column by column rather than trusted. A future version
/// of the fetch script that reorders or renames columns would otherwise be read
/// without complaint, and wind would silently be measured as solar: the file
/// would still parse, the model would still solve, and the numbers would be
/// quietly wrong in a way no assertion here would catch.
fn load_control_zones() -> Option<ControlZones> {
    let path = opsd_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        println!(
            "\n  skipping: {} is absent.\n  Run `python3 benchmarks/fetch_opsd_time_series.py` \
             once to fetch it. It is not committed;\n  see the script's docstring for why.",
            path.display()
        );
        return None;
    };

    let mut lines = text.lines();
    let header = lines.next().expect("a written CSV always has a header");
    let zones = ["50hertz", "amprion", "tennet", "transnetbw"];
    let mut expected = vec!["utc_timestamp".to_string()];
    for zone in zones {
        expected.push(format!("{zone}_load_mw"));
        expected.push(format!("{zone}_solar_pu"));
        expected.push(format!("{zone}_wind_pu"));
    }
    let found: Vec<&str> = header.split(',').collect();
    assert_eq!(
        found, expected,
        "the distilled time series has a header this test does not recognise, so the \
         columns it would read are not the ones it thinks it is reading. Regenerate it \
         with benchmarks/fetch_opsd_time_series.py, or update both together."
    );

    let year = || -> Vec<Vec<f64>> { (0..N_ZONES).map(|_| Vec::with_capacity(HOURS)).collect() };
    let (mut load, mut solar, mut wind) = (year(), year(), year());
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split(',').skip(1);
        for z in 0..N_ZONES {
            for series in [&mut load, &mut solar, &mut wind] {
                let raw = fields.next().expect("row shorter than the checked header");
                series[z].push(raw.parse().expect("a numeric field in the distilled CSV"));
            }
        }
    }
    assert_eq!(
        load[0].len(),
        HOURS,
        "the distilled series is not a whole year, so anything measured on it would be \
         labelled 8,760 hours and would not be"
    );

    // Normalise once, here, rather than on every lookup. The fixture asks for
    // this value once per load per hour, which on the largest case is thirteen
    // million times, and a maximum recomputed inside that loop turns fixture
    // assembly from milliseconds into minutes without changing a single number.
    let demand_pu = load
        .iter()
        .map(|zone| {
            let peak = zone.iter().copied().fold(f64::MIN, f64::max);
            assert!(
                peak > 0.0,
                "a control zone with no positive load cannot be normalised, and a zone of \
                 zeros would silently give every bus in it no demand at all"
            );
            zone.iter().map(|mw| mw / peak).collect()
        })
        .collect();

    Some(ControlZones {
        demand_pu,
        solar,
        wind,
    })
}

// ---------------------------------------------------------------------------
// Assembling the fixture
// ---------------------------------------------------------------------------

/// Which control zone a bus draws its profiles from.
///
/// Contiguous blocks of bus index, four of them, near enough equal in size.
///
/// This is the mapping assumption and it is the weakest thing in the file, so
/// it is stated in full. What it needs to be right about is only that buses
/// near each other in the numbering are near each other on the ground, which is
/// broadly how these cases are written but is nowhere guaranteed. What it buys
/// is that demand across the network is *not* perfectly correlated: four real
/// regional series rise and fall at different times, so power actually has to
/// move and the transmission constraints do work. A single national profile
/// applied everywhere would scale every bus by the same number in every hour,
/// which is the one arrangement guaranteed to leave the flows almost unchanged
/// hour to hour and would flatter the solve in exactly the way this file exists
/// to detect.
///
/// The residual cost: within a block, demand is still perfectly correlated. Four
/// zones is far coarser than the real spatial diversity of a transmission
/// system, so this fixture still understates how much the network has to do.
/// The error therefore runs in the known direction, towards *easier*, which
/// means any excess difficulty found here is real rather than an artefact.
fn zone_of_bus(bus: usize, n_buses: usize) -> usize {
    (bus * N_ZONES / n_buses).min(N_ZONES - 1)
}

/// Which carrier a generator is treated as having.
///
/// MATPOWER records no carrier at all, so this is our designation and not data.
/// Every fourth unit becomes wind and every fourth solar, leaving half the
/// fleet thermal, which matches the ring fixture's split of half its generators
/// onto a profile and keeps the two comparable.
///
/// A unit with a must-run floor is never designated renewable. A floor says the
/// unit cannot go below some output, and a weather-driven unit at night can and
/// must. The builder does defend itself against that combination by clamping
/// the floor to what is available, so this is not about avoiding a crash; it is
/// that a must-run wind farm is not a thing, and a fixture that contained one
/// would be measuring a formulation nobody would write.
fn carrier_of(unit: &Generator, index: usize) -> Option<Carrier> {
    if unit.p_min_pu > 0.0 {
        return None;
    }
    match index % 4 {
        1 => Some(Carrier::Wind),
        3 => Some(Carrier::Solar),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum Carrier {
    Wind,
    Solar,
}

/// Add storage, which the MATPOWER cases do not have and which the measurement
/// cannot do without.
///
/// This is invented and it is the largest invented thing in the fixture, so the
/// reason has to be good. Without any inter-temporal coupling, a year of DC
/// optimal power flow is not one program at all: it is 8,760 completely
/// independent single-hour programs sharing a variable numbering, block
/// diagonal with nothing off the diagonal. HiGHS is very fast on that, and the
/// number it produced would say nothing about how a year-long model scales,
/// only about how a one-hour model scales multiplied by 8,760. Every
/// interesting thing about a long horizon comes from constraints that reach
/// across hours.
///
/// So storage is added on the same rule the ring uses, one unit at every fourth
/// load, six hours of energy at its rated power, cyclic over the whole year.
/// Cyclic is what makes the coupling global rather than local: the state of
/// charge in the first hour is tied to the last, so the constraint graph is
/// connected end to end and the solve cannot be decomposed by inspection.
///
/// Rating it at 30% of the host load's own peak demand keeps the fleet in
/// proportion to the network rather than at some absolute number that would
/// mean one thing on a 14-bus case and another on a 300-bus one.
fn add_storage_fleet(net: &mut Network) {
    let sites: Vec<(usize, f64)> = net
        .loads
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 4 == 0)
        .map(|(_, load)| (load.bus, load.p_set))
        .filter(|(_, p_set)| *p_set > 0.0)
        .collect();
    for (n, (bus, p_set)) in sites.into_iter().enumerate() {
        net.add_storage(StorageUnit {
            name: format!("store{n}"),
            bus,
            p_nom: 0.3 * p_set,
            max_hours: 6.0,
            efficiency_store: 0.94,
            efficiency_dispatch: 0.94,
            cyclic: true,
            ..Default::default()
        });
    }
}

/// A named real case carrying the measured year.
struct RealYear {
    name: &'static str,
    net: Network,
    /// How many generators ended up on a measured weather profile, which is
    /// worth printing because the must-run exclusion above can make it much
    /// less than half on cases whose units nearly all have a floor.
    on_weather: usize,
}

fn case_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/pglib")
        .join(format!("{name}.m"))
}

/// Build the fixture: a real PGLib network, a real year, and the two invented
/// pieces above.
///
/// Note what is *not* touched. Line ratings, reactances, generator capacities
/// and marginal costs are exactly as published. The spatial distribution of
/// demand is exactly as published; only its variation through the year is
/// added, and it is added as one multiplier per zone per hour, so the ratios
/// between buses inside a zone stay at the published values in every hour.
fn real_year(name: &'static str, zones: &ControlZones, hours: usize) -> RealYear {
    let mut case = load_case(case_path(name)).expect("PGLib case reads");
    let net = &mut case.network;
    net.snapshots = Snapshots::hourly(hours);

    let n_buses = net.buses.len();

    // Demand: one real regional shape per zone, applied to the published bus
    // demands. `load_profile` is absolute megawatts per load per snapshot
    // rather than a per-unit factor, so the published `p_set` is multiplied in
    // here.
    let profile: Vec<Vec<f64>> = net
        .loads
        .iter()
        .map(|load| {
            let zone = zone_of_bus(load.bus, n_buses);
            (0..hours)
                .map(|t| load.p_set * zones.demand_pu[zone][t])
                .collect()
        })
        .collect();
    net.load_profile = TimeSeries::from_rows(&profile, hours).expect("one row per load");

    // Availability: measured wind and solar for the designated units, a flat
    // one for everything else. A flat row is written rather than left absent
    // because `TimeSeries` is a dense rectangle and a partial one would be
    // misread as belonging to the wrong generators.
    let mut on_weather = 0;
    let availability: Vec<Vec<f64>> = net
        .generators
        .iter()
        .enumerate()
        .map(|(i, unit)| {
            let zone = zone_of_bus(unit.bus, n_buses);
            match carrier_of(unit, i) {
                Some(Carrier::Wind) => {
                    on_weather += 1;
                    zones.wind[zone][..hours].to_vec()
                }
                Some(Carrier::Solar) => {
                    on_weather += 1;
                    zones.solar[zone][..hours].to_vec()
                }
                None => vec![1.0; hours],
            }
        })
        .collect();
    net.gen_availability =
        TimeSeries::from_rows(&availability, hours).expect("one row per generator");

    add_storage_fleet(net);
    net.validate()
        .expect("the assembled fixture is a valid network");

    RealYear {
        name,
        net: case.network,
        on_weather,
    }
}

// ---------------------------------------------------------------------------
// The synthetic ring, for comparison
// ---------------------------------------------------------------------------

/// The `scale.rs` ring fixture, reproduced here rather than shared.
///
/// Duplicated on purpose. The whole value of the comparison below is that the
/// ring numbers it quotes are the ones the README published, so the fixture has
/// to stay bit-for-bit the shape it was when those were measured. Sharing it
/// through a helper crate would mean a future edit made for one file silently
/// moved the other's baseline, and the resulting discrepancy would look like a
/// finding about topology when it was an edit. If the two ever diverge, this
/// comment is the record of which one is the copy.
fn ring(buses: usize, hours: usize) -> Network {
    let mut net = Network::new(Snapshots::hourly(hours));
    for i in 0..buses {
        net.add_bus(format!("b{i}"), format!("c{}", i % 8));
    }
    for i in 0..buses {
        net.add_generator(Generator {
            name: format!("base{i}"),
            bus: i,
            p_nom: 400.0,
            marginal_cost: 20.0 + (i % 5) as f64,
            ..Default::default()
        });
        net.add_generator(Generator {
            name: format!("peak{i}"),
            bus: i,
            p_nom: 200.0,
            marginal_cost: 120.0,
            ..Default::default()
        });
        net.add_load(Load {
            name: format!("d{i}"),
            bus: i,
            p_set: 300.0,
            ..Default::default()
        });
        net.add_line(Line {
            name: format!("l{i}"),
            bus0: i,
            bus1: (i + 1) % buses,
            s_nom: 500.0,
            susceptance: 10.0,
            ..Default::default()
        });
        if i % 4 == 0 {
            net.add_storage(StorageUnit {
                name: format!("s{i}"),
                bus: i,
                p_nom: 100.0,
                max_hours: 6.0,
                efficiency_store: 0.94,
                efficiency_dispatch: 0.94,
                cyclic: true,
                ..Default::default()
            });
        }
    }
    let rows: Vec<Vec<f64>> = (0..net.generators.len())
        .map(|g| {
            (0..hours)
                .map(|t| {
                    if g % 2 == 0 {
                        0.6 + 0.4 * ((t as f64 / 12.0).sin())
                    } else {
                        1.0
                    }
                })
                .collect()
        })
        .collect();
    net.gen_availability = TimeSeries::from_rows(&rows, hours).unwrap();
    net
}

/// The ring bus count whose column count comes closest to `target`.
///
/// Matching on columns rather than on buses is the whole methodology. A real
/// network and a ring with the same number of buses are not the same size of
/// problem at all: PEGASE 1354 has one and a half branches per bus where the
/// ring has one, so comparing at equal bus counts would compare a large program
/// against a small one and report the difference as a property of topology.
/// Matching the column count instead means any remaining gap in solve time is
/// about the *shape* of the matrix rather than its size, which is the question.
///
/// The search builds each candidate over a single day and scales, because
/// columns are exactly linear in the number of snapshots and building four
/// hundred year-long rings to pick one would cost more than the measurement.
fn ring_matching_columns(target: usize, hours: usize) -> usize {
    const PROBE_HOURS: usize = 24;
    let mut best = (1usize, usize::MAX);
    for buses in 3..=512 {
        let probe = build_lopf(&ring(buses, PROBE_HOURS)).expect("the ring always builds");
        let scaled = probe.model.num_cols() / PROBE_HOURS * hours;
        let error = scaled.abs_diff(target);
        if error < best.1 {
            best = (buses, error);
        } else if buses > best.0 + 8 {
            // Columns rise monotonically in the bus count, so once the error
            // has been growing for a while the minimum is behind us.
            break;
        }
    }
    best.0
}

// ---------------------------------------------------------------------------
// Measuring
// ---------------------------------------------------------------------------

/// Peak resident set size of this process, in bytes.
///
/// `getrusage` is declared here rather than reached through the `libc` crate,
/// because a benchmark ought not to add a dependency to the crate it is
/// measuring. `ru_maxrss` sits after the two `timeval`s in `struct rusage` on
/// both platforms this builds for, and the two structs agree on layout up to
/// that point; what they disagree on is units, bytes on macOS and kilobytes on
/// Linux. Getting that wrong would misreport memory by a factor of 1,024, which
/// is precisely the class of error this project has already published once, so
/// the conversion is explicit rather than assumed.
///
/// Note the limitation this cannot fix: a high-water mark belongs to the
/// process, not to the measurement, so within one process every rung after the
/// first reports the largest peak reached so far. Rungs run in increasing size
/// and each model is dropped before the next is built, which makes that
/// approximately the rung's own peak, but only approximately. For a figure that
/// does not need that excuse, run one rung per process with
/// `benchmarks/real_scale_memory.sh`.
fn peak_rss_bytes() -> u64 {
    #[repr(C)]
    struct RUsage {
        /// `ru_utime` and `ru_stime`: two `timeval`s, sixteen bytes each.
        times: [i64; 4],
        ru_maxrss: i64,
        /// Fourteen further `long` fields, none of them read here.
        rest: [i64; 14],
    }

    unsafe extern "C" {
        fn getrusage(who: i32, usage: *mut RUsage) -> i32;
    }

    let mut usage = RUsage {
        times: [0; 4],
        ru_maxrss: 0,
        rest: [0; 14],
    };
    // RUSAGE_SELF is 0 on every platform that defines it.
    let rc = unsafe { getrusage(0, &raw mut usage) };
    if rc != 0 {
        return 0;
    }
    let raw = usage.ru_maxrss.max(0) as u64;
    if cfg!(target_os = "macos") {
        raw
    } else {
        raw * 1024
    }
}

/// One-minute load average.
///
/// Printed beside every timing, and it is not decoration. Three numbers in this
/// project have had to be retracted because they were measured on a machine
/// that was busy, and in each case nothing in the recorded output said so, which
/// is why the retraction came months later instead of the same afternoon. A row
/// carrying its own load average can be judged by whoever reads it: on a
/// fourteen-core machine, one or two is quiet and ten is another job competing
/// for the same cores and the same memory bandwidth.
fn load_average() -> f64 {
    unsafe extern "C" {
        fn getloadavg(loadavg: *mut f64, nelem: i32) -> i32;
    }
    let mut averages = [0.0f64; 3];
    let n = unsafe { getloadavg(averages.as_mut_ptr(), 3) };
    if n < 1 { f64::NAN } else { averages[0] }
}

/// Sizes and timings for one rung.
struct Measured {
    rows: usize,
    cols: usize,
    nnz: usize,
    build: Duration,
    build_spread: f64,
    solve: Duration,
    solve_spread: f64,
    peak_rss: u64,
    /// Highest one-minute load average seen while this rung was running.
    load: f64,
}

fn spread(times: &[Duration]) -> f64 {
    let lo = times.iter().min().expect("at least one run").as_secs_f64();
    let hi = times.iter().max().expect("at least one run").as_secs_f64();
    if lo > 0.0 { (hi - lo) / lo } else { 0.0 }
}

/// Build and solve `net`, best of `RUNS`.
///
/// The model is rebuilt on every run rather than built once and re-solved. A
/// re-solve reuses warm caches and a matrix already resident, and would measure
/// something the first solve of a fresh model never gets.
fn measure(net: &Network) -> Measured {
    let mut builds = Vec::with_capacity(RUNS);
    let mut solves = Vec::with_capacity(RUNS);
    let (mut rows, mut cols, mut nnz) = (0, 0, 0);
    let mut load = load_average();
    for _ in 0..RUNS {
        load = load.max(load_average());
        let t0 = Instant::now();
        let lopf = build_lopf(net).expect("the fixture builds");
        builds.push(t0.elapsed());
        rows = lopf.model.num_rows();
        cols = lopf.model.num_cols();
        nnz = lopf.model.nnz();

        let t1 = Instant::now();
        let solved = HighsSolver::default()
            .solve(&lopf)
            .expect("HiGHS returns a status");
        solves.push(t1.elapsed());
        assert!(
            matches!(solved.status, gridwright_solve::Status::Optimal),
            "a rung that did not solve to optimality has nothing to say about how long \
             solving takes; got {:?}",
            solved.status
        );
    }
    Measured {
        rows,
        cols,
        nnz,
        build: *builds.iter().min().expect("RUNS is not zero"),
        build_spread: spread(&builds),
        solve: *solves.iter().min().expect("RUNS is not zero"),
        solve_spread: spread(&solves),
        peak_rss: peak_rss_bytes(),
        load: load.max(load_average()),
    }
}

/// One rolling-horizon run: how long, and over how many windows.
struct Rolled {
    elapsed: Duration,
    windows: usize,
}

/// Solve `net` a year at a time through overlapping windows, best of `RUNS`.
fn measure_rolling(net: &Network) -> Rolled {
    let horizon = Horizon {
        window: 96,
        keep: 72,
    };
    let mut times = Vec::with_capacity(RUNS);
    let mut windows = 0;
    for _ in 0..RUNS {
        let t0 = Instant::now();
        let solved = solve_rolling(net, horizon, &HighsSolver::default())
            .expect("the rolling solve completes");
        times.push(t0.elapsed());
        windows = solved.windows;
    }
    Rolled {
        elapsed: *times.iter().min().expect("RUNS is not zero"),
        windows,
    }
}

/// Everything measured about one rung, real side and ring side.
struct Comparison {
    name: &'static str,
    ring_buses: usize,
    real: Measured,
    synthetic: Measured,
    real_rolled: Rolled,
    ring_rolled: Rolled,
}

/// The ladder of real cases whose year is solved whole, smallest first.
///
/// It stops at IEEE 118, and where it stops was decided by a calibration run
/// rather than by taste. IEEE 57 is 2.05 million columns and takes HiGHS about
/// five minutes, where the ring at the same column count takes three quarters
/// of one; the real network is not a little harder at matched size but several
/// times harder, and that changes what is affordable. IEEE 300 comes to 10.7
/// million columns, two thirds again as large as the biggest ring ever measured
/// here, and extrapolating from IEEE 118 puts its solve in the hours. It is
/// measured for construction only, in `build_only_on_larger_real_networks`,
/// alongside two PEGASE cases.
///
/// That is a real limit and worth stating as one: the largest *real* network
/// this project has solved for a whole year has 118 buses. The ring table goes
/// to 128 buses but at a quarter the columns per bus, so the two are less
/// comparable than the bus counts suggest, which is itself part of the finding.
const LADDER: [&str; 3] = ["case14_ieee", "case57_ieee", "case118_ieee"];

/// Which rungs to run, from `GRIDWRIGHT_REAL_CASE` if it is set.
///
/// Present so `benchmarks/real_scale_memory.sh` can put one rung in one process
/// and get a peak memory figure that means what it says.
fn selected_ladder() -> Vec<&'static str> {
    match std::env::var("GRIDWRIGHT_REAL_CASE") {
        Ok(want) => LADDER
            .into_iter()
            .filter(|name| *name == want)
            .collect::<Vec<_>>(),
        Err(_) => LADDER.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// The measurements
// ---------------------------------------------------------------------------

/// The measurement this file exists for.
///
/// One test rather than two, and the reason is cost rather than tidiness. The
/// real side of the comparison is by far the most expensive thing here, and a
/// separate "how big and how slow is the real case" test would measure exactly
/// the same models a second time for the same hour of wall clock. So each rung
/// is measured once and printed into three tables: what the real case is and
/// what it costs, what the size-matched ring costs, and the ratio between them.
///
/// Rows are printed as each rung finishes rather than accumulated, because the
/// ladder runs for hours and a run that is interrupted at rung three should
/// still have published rungs one and two.
#[test]
#[ignore = "hours; run explicitly for numbers"]
fn whether_the_synthetic_ring_has_been_flattering_the_solve() {
    let Some(zones) = load_control_zones() else {
        return;
    };
    println!(
        "\n  Real PGLib topologies carrying 2019 hourly demand and weather from the four\n  \
         German control zones, each against the synthetic ring sized to the same column\n  \
         count and measured in the same process minutes apart, so the two differ in what\n  \
         is modelled and not in what the machine was doing.\n\n  \
         Best of {RUNS}. `spread` is the gap between the two runs and `load` the highest\n  \
         one-minute load average seen: on a fourteen-core machine anything much above two\n  \
         means another job was competing, and the row should be treated as an upper bound\n  \
         rather than a measurement.\n"
    );
    println!(
        "  case            buses  gens  wx  rows        cols        nnz          build     \
         spread   solve       spread   peak RSS  load"
    );
    let mut comparisons = Vec::new();
    for name in selected_ladder() {
        let fixture = real_year(name, &zones, HOURS);
        let net = &fixture.net;
        let real = measure(net);
        println!(
            "  {:<14}  {:5}  {:4}  {:2}  {:<10}  {:<10}  {:<11}  {:>7.1?}  {:>5.0}%   {:>8.1?}  \
             {:>5.0}%   {:>6.2} GB  {:>4.1}",
            fixture.name,
            net.buses.len(),
            net.generators.len(),
            fixture.on_weather,
            real.rows,
            real.cols,
            real.nnz,
            real.build,
            real.build_spread * 100.0,
            real.solve,
            real.solve_spread * 100.0,
            real.peak_rss as f64 / 1e9,
            real.load,
        );

        let buses = ring_matching_columns(real.cols, HOURS);
        let synthetic = measure(&ring(buses, HOURS));
        println!(
            "    ring {:<9}  {:5}  {:4}  {:2}  {:<10}  {:<10}  {:<11}  {:>7.1?}  {:>5.0}%   \
             {:>8.1?}  {:>5.0}%   {:>6.2} GB  {:>4.1}",
            buses,
            buses,
            buses * 2,
            buses,
            synthetic.rows,
            synthetic.cols,
            synthetic.nnz,
            synthetic.build,
            synthetic.build_spread * 100.0,
            synthetic.solve,
            synthetic.solve_spread * 100.0,
            synthetic.peak_rss as f64 / 1e9,
            synthetic.load,
        );
        // The rolling horizon, on both topologies, while both models are still
        // to hand. The README now leans on a trend rather than on a single
        // ratio: rolling is 2.6x faster than the whole year at 16 ring buses
        // and 7.4x at 128, and the argument that a continental model is
        // tractable is an extrapolation of that slope. The mechanism it rests
        // on is that the whole-horizon solve grows superlinearly while the
        // windowed one grows nearly linearly in the number of windows, and
        // that is a claim about how the solve scales, which is exactly the
        // thing a topology can change. Measuring it here rather than in a test
        // of its own costs nothing, because the expensive half, the
        // whole-horizon solve it is compared against, has just been done.
        let real_rolled = measure_rolling(&fixture.net);
        let ring_rolled = measure_rolling(&ring(buses, HOURS));
        comparisons.push(Comparison {
            name: fixture.name,
            ring_buses: buses,
            real,
            synthetic,
            real_rolled,
            ring_rolled,
        });
    }

    println!(
        "\n  The comparison. `size gap` is how far the ring missed the real case's column\n  \
         count, and is the part of the ratio that is not about topology. `nnz/col` is the\n  \
         average column density, the one structural difference visible without\n  \
         factorising anything.\n"
    );
    println!(
        "  case            cols        ring buses  size gap  nnz/col real  nnz/col ring  \
         real solve  ring solve  ratio"
    );
    for c in &comparisons {
        let gap = (c.synthetic.cols as f64 - c.real.cols as f64) / c.real.cols as f64;
        println!(
            "  {:<14}  {:<10}  {:<10}  {:>7.1}%  {:>12.2}  {:>12.2}  {:>10.1?}  {:>10.1?}  \
             {:>5.2}x",
            c.name,
            c.real.cols,
            c.ring_buses,
            gap * 100.0,
            c.real.nnz as f64 / c.real.cols as f64,
            c.synthetic.nnz as f64 / c.synthetic.cols as f64,
            c.real.solve,
            c.synthetic.solve,
            c.real.solve.as_secs_f64() / c.synthetic.solve.as_secs_f64(),
        );
    }

    println!(
        "\n  The rolling horizon, 96-hour windows keeping 72, against the same year solved\n  \
         whole, on both topologies. What matters is not the two advantages but whether\n  \
         they move the same way as the problem grows, because that slope is what the\n  \
         README extrapolates from.\n"
    );
    println!(
        "  case            cols        windows  real rolling  real whole  advantage  \
         ring rolling  ring whole  advantage"
    );
    for c in &comparisons {
        println!(
            "  {:<14}  {:<10}  {:7}  {:>12.1?}  {:>10.1?}  {:>8.2}x  {:>12.1?}  {:>10.1?}  \
             {:>8.2}x",
            c.name,
            c.real.cols,
            c.real_rolled.windows,
            c.real_rolled.elapsed,
            c.real.solve,
            c.real.solve.as_secs_f64() / c.real_rolled.elapsed.as_secs_f64(),
            c.ring_rolled.elapsed,
            c.synthetic.solve,
            c.synthetic.solve.as_secs_f64() / c.ring_rolled.elapsed.as_secs_f64(),
        );
    }
}

/// One rung, once, so a process high-water mark means one rung's memory.
///
/// The peak memory column in the comparison above is a whole-process figure and
/// therefore inherits whatever the rungs before it reached. That is close enough
/// to read a trend from and not close enough to quote, and memory is the one
/// number this project cannot afford to quote loosely, because its founding
/// claim is a memory claim. So this test does exactly one build and one solve of
/// exactly one case, selected by `GRIDWRIGHT_REAL_CASE`, and
/// `benchmarks/real_scale_memory.sh` wraps it in `/usr/bin/time -l` to take the
/// figure from the kernel rather than from the program.
///
/// One run rather than best of `RUNS`, because peak memory does not vary between
/// runs the way a timing does: it is set by how much the model and the solver's
/// factorisation need, not by what else the machine was doing.
#[test]
#[ignore = "minutes to an hour depending on the rung; driven by benchmarks/real_scale_memory.sh"]
fn one_rung_in_its_own_process_for_an_honest_memory_figure() {
    let Some(zones) = load_control_zones() else {
        return;
    };
    let ladder = selected_ladder();
    assert_eq!(
        ladder.len(),
        1,
        "this test measures one rung in one process, which is the whole point of it. \
         Set GRIDWRIGHT_REAL_CASE to one of {LADDER:?}."
    );
    let fixture = real_year(ladder[0], &zones, HOURS);
    let lopf = build_lopf(&fixture.net).expect("the fixture builds");
    println!(
        "\n  {}: {} rows, {} columns, {} nonzeros",
        fixture.name,
        lopf.model.num_rows(),
        lopf.model.num_cols(),
        lopf.model.nnz()
    );
    let solved = HighsSolver::default()
        .solve(&lopf)
        .expect("HiGHS returns a status");
    println!(
        "  status {:?}, in-process peak {:.2} GB",
        solved.status,
        peak_rss_bytes() as f64 / 1e9
    );
}

#[test]
#[ignore = "a few minutes; run explicitly for numbers"]
fn build_only_on_larger_real_networks() {
    let Some(zones) = load_control_zones() else {
        return;
    };
    // Construction alone, on the cases whose year-long solve is out of reach.
    // Worth having because the founding claim of this project is about
    // construction, and because it puts a number on where a year of a real
    // continental network actually sits: the columns and the nonzeros are the
    // honest measure of the size of the thing nobody here has solved.
    println!(
        "\n  Construction only, on real networks whose whole-year solve was not attempted.\n  \
         Best of {RUNS}.\n"
    );
    println!("  case             buses  rows        cols        nnz          build     peak RSS");
    for name in ["case300_ieee", "case1354_pegase", "case2869_pegase"] {
        let fixture = real_year(name, &zones, HOURS);
        let mut builds = Vec::with_capacity(RUNS);
        let (mut rows, mut cols, mut nnz) = (0, 0, 0);
        for _ in 0..RUNS {
            let t0 = Instant::now();
            let lopf = build_lopf(&fixture.net).expect("the fixture builds");
            builds.push(t0.elapsed());
            rows = lopf.model.num_rows();
            cols = lopf.model.num_cols();
            nnz = lopf.model.nnz();
        }
        println!(
            "  {:<15}  {:5}  {:<10}  {:<10}  {:<11}  {:>7.1?}  {:>6.2} GB",
            fixture.name,
            fixture.net.buses.len(),
            rows,
            cols,
            nnz,
            builds.iter().min().expect("RUNS is not zero"),
            peak_rss_bytes() as f64 / 1e9,
        );
    }
}

// ---------------------------------------------------------------------------
// Guarding the fixture
// ---------------------------------------------------------------------------

/// Cheap enough to run in the ordinary suite, and it earns that by catching the
/// failure mode the expensive tests cannot: a fixture that assembles without
/// error but does not mean what its comments say. A demand profile silently
/// left flat, or every generator quietly on the same zone's wind, would not
/// stop anything above from producing a table; it would just make the table
/// describe a different model.
#[test]
fn the_real_year_fixture_carries_the_variation_it_claims_to() {
    let Some(zones) = load_control_zones() else {
        return;
    };
    const PROBE_HOURS: usize = 168;
    let fixture = real_year("case118_ieee", &zones, PROBE_HOURS);
    let net = &fixture.net;

    assert!(
        fixture.on_weather > 0,
        "no generator ended up on a measured weather profile, so the year has no \
         renewable variation in it at all"
    );
    assert!(
        !net.storage.is_empty(),
        "no storage was added, so the year decomposes into independent hours and \
         measuring it says nothing about a long horizon"
    );
    assert!(
        net.storage.iter().all(|s| s.cyclic),
        "storage that is not cyclic lets the model empty every reservoir before the \
         final hour, which is free energy and removes the end-to-end coupling"
    );

    // The four zones must actually differ. If the mapping collapsed to one
    // zone, or every zone got the same column, demand would be perfectly
    // correlated across the network and the comparison against the ring would
    // be measuring the thing it exists to avoid.
    let zones_used: std::collections::BTreeSet<usize> = net
        .loads
        .iter()
        .map(|load| zone_of_bus(load.bus, net.buses.len()))
        .collect();
    assert_eq!(
        zones_used.len(),
        N_ZONES,
        "demand was drawn from {} control zones rather than {N_ZONES}, so the fixture \
         has less spatial diversity than it claims",
        zones_used.len()
    );

    // Demand varies through the week, and never above the published operating
    // point, which is what makes the case's own fleet adequate for every hour.
    for (i, load) in net.loads.iter().enumerate() {
        if load.p_set <= 0.0 {
            continue;
        }
        let series: Vec<f64> = (0..PROBE_HOURS)
            .map(|t| net.load_profile.at(i, t).expect("a dense profile"))
            .collect();
        let lo = series.iter().copied().fold(f64::MAX, f64::min);
        let hi = series.iter().copied().fold(f64::MIN, f64::max);
        assert!(
            hi > lo * 1.05,
            "load {i} varies by less than 5% across a week, which is not what a metered \
             demand series looks like"
        );
        assert!(
            hi <= load.p_set * 1.000_001,
            "load {i} exceeds its published operating point, so the case's fleet is no \
             longer known to be adequate and the measurement becomes a study of load \
             shedding"
        );
    }

    // Weather profiles are per unit and are not flat.
    let mut varying = 0;
    for g in 0..net.generators.len() {
        let series: Vec<f64> = (0..PROBE_HOURS)
            .map(|t| net.gen_availability.at(g, t).expect("a dense profile"))
            .collect();
        assert!(
            series.iter().all(|a| (0.0..=1.0).contains(a)),
            "generator {g} has an availability outside [0, 1], which is not an \
             availability"
        );
        if series.iter().copied().fold(f64::MIN, f64::max)
            > series.iter().copied().fold(f64::MAX, f64::min) + 1e-9
        {
            varying += 1;
        }
    }
    assert_eq!(
        varying, fixture.on_weather,
        "the number of generators with a varying availability does not match the number \
         designated as wind or solar, so the profiles went to the wrong units"
    );
}

/// The ring must be matched on columns, not on buses, or the comparison in
/// `whether_the_synthetic_ring_has_been_flattering_the_solve` is meaningless.
/// This pins the matcher at a size cheap enough to check.
#[test]
fn the_ring_is_matched_to_the_real_case_by_column_count() {
    let hours = 24;
    let mut case = load_case(case_path("case118_ieee")).expect("PGLib case reads");
    case.network.snapshots = Snapshots::hourly(hours);
    let target = build_lopf(&case.network)
        .expect("the case builds")
        .model
        .num_cols();

    let buses = ring_matching_columns(target, hours);
    let matched = build_lopf(&ring(buses, hours))
        .expect("the ring builds")
        .model
        .num_cols();

    let error = (matched as f64 - target as f64).abs() / target as f64;
    assert!(
        error < 0.02,
        "the closest ring to IEEE 118 is {buses} buses at {matched} columns against \
         {target}, which is {:.1}% out. Anything above a couple of percent and the two \
         solve times are not comparable, because size rather than shape would be doing \
         the work.",
        error * 100.0
    );
}

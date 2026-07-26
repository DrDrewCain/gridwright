//! Reading networks from disk and writing results back.
//!
//! The layout is one CSV per component type in a directory, which is what
//! PyPSA writes and therefore what most existing data already looks like.
//! Column names are matched case-insensitively and every optional column may
//! simply be absent, so a minimal network is four short files and a model with
//! capacity expansion and emissions is the same four files with more columns.
//!
//! ```text
//! network/
//!   buses.csv           name, country
//!   generators.csv      name, bus, p_nom, marginal_cost, carrier, ...
//!   lines.csv           name, bus0, bus1, s_nom, susceptance, ...
//!   loads.csv           name, bus, p_set
//!   storage_units.csv   name, bus, p_nom, max_hours, ...      (optional)
//!   snapshots.csv       weight                                 (optional)
//!   gen_availability.csv   wide: one column per generator      (optional)
//!   load_profile.csv       wide: one column per load           (optional)
//! ```
//!
//! Time series files are wide, one column per component and one row per
//! snapshot, because that is how they are produced and stored everywhere. They
//! are transposed on load into the component major layout the engine wants,
//! which is a real cost paid once at the edge rather than a layout compromise
//! carried through the hot path.

use std::path::Path;

use gridwright_net::{
    Generator, Line, Load, NetError, Network, Snapshots, StorageUnit, TimeSeries,
};

pub mod csv;
pub mod matpower;
pub mod psse;
pub mod detect;
pub mod write;
pub mod memory;

pub use detect::{DetectError, Format, load_any, sniff};
pub use memory::{Files, load_bytes, load_files, sniff_bytes};
pub use write::{Written, to_matpower, to_psse, write_matpower, write_psse};

#[cfg(feature = "json")]
pub mod json;
#[cfg(feature = "parquet")]
pub mod parquet;
#[cfg(feature = "excel")]
pub mod excel;
#[cfg(feature = "netcdf")]
pub mod netcdf;
#[cfg(feature = "cgmes")]
pub mod cgmes;

/// A network read from a file, plus what had to be discarded to make it fit.
///
/// Every format carries more than a linear optimisation model can hold, and
/// they each carry a different more. `notes` is where that goes: a caller can
/// print it and tell a user exactly what was dropped, instead of the reader
/// deciding quietly on their behalf.
#[derive(Debug)]
pub struct Case {
    pub name: String,
    pub network: Network,
    /// Things dropped or approximated, so a caller can report them honestly.
    pub notes: Vec<String>,
}

use csv::{CsvError, Table};

#[derive(Debug, thiserror::Error)]
pub enum IoError {
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("in {file}: {source}")]
    Csv {
        file: String,
        #[source]
        source: CsvError,
    },
    #[error("in {file}, row {row}: unknown bus `{bus}`")]
    UnknownBus {
        file: String,
        row: usize,
        bus: String,
    },
    #[error("{file} has a column `{column}` that matches no {kind}")]
    UnknownComponentColumn {
        file: String,
        column: String,
        kind: &'static str,
    },
    #[error("{file} has {got} rows but there are {want} snapshots")]
    TimeSeriesRows {
        file: String,
        got: usize,
        want: usize,
    },
    #[error("network is not valid: {0}")]
    Invalid(#[from] NetError),
    #[error("{0}")]
    Detect(#[from] detect::DetectError),
    #[error("reading MATPOWER case: {0}")]
    Matpower(#[from] matpower::MatpowerError),
    #[error("reading PSS/E RAW case: {0}")]
    Psse(#[from] psse::PsseError),
    #[cfg(feature = "json")]
    #[error("reading JSON case: {0}")]
    Json(#[from] json::JsonError),
    #[cfg(feature = "parquet")]
    #[error("reading Parquet: {0}")]
    Parquet(#[from] parquet::ParquetError),
    #[cfg(feature = "excel")]
    #[error("reading spreadsheet: {0}")]
    Excel(#[from] excel::ExcelError),
    #[cfg(feature = "netcdf")]
    #[error("reading netCDF: {0}")]
    Netcdf(#[from] netcdf::NetcdfError),
    #[cfg(feature = "cgmes")]
    #[error("reading CIM/CGMES: {0}")]
    Cgmes(#[from] cgmes::CgmesError),
}

/// Where the tables come from.
///
/// The column semantics of a network description are the same whether the
/// rows arrive as CSV, as Parquet or out of a spreadsheet, so there is one
/// assembler and several sources rather than one reader per format. A bug
/// fixed in how `p_nom_extendable` is interpreted is then fixed everywhere at
/// once, which three parallel readers would not give.
pub trait TableSource {
    /// The table with this stem, or `None` if this source has no such table.
    fn table(&self, stem: &str) -> Result<Option<Table>, IoError>;
    /// A whole file as text, for the handful of single-value settings.
    fn text(&self, name: &str) -> Result<Option<String>, IoError>;
    /// What to call this table in an error message.
    fn label(&self, stem: &str) -> String;
}

/// A directory of CSV files.
pub struct CsvDir<'a>(pub &'a Path);

impl TableSource for CsvDir<'_> {
    fn table(&self, stem: &str) -> Result<Option<Table>, IoError> {
        let name = self.label(stem);
        let Some(text) = self.text(&name)? else {
            return Ok(None);
        };
        Table::parse(&text).map(Some).map_err(|source| IoError::Csv {
            file: name,
            source,
        })
    }

    fn text(&self, name: &str) -> Result<Option<String>, IoError> {
        let path = self.0.join(name);
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(s)),
            // A missing optional file is not an error; a missing required one
            // is reported by the caller with better context than "no such
            // file".
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(IoError::Read {
                path: path.display().to_string(),
                source: e,
            }),
        }
    }

    fn label(&self, stem: &str) -> String {
        format!("{stem}.csv")
    }
}

fn required(src: &dyn TableSource, stem: &str) -> Result<Table, IoError> {
    src.table(stem)?.ok_or_else(|| IoError::Read {
        path: src.label(stem),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "required file is missing"),
    })
}

/// Load a network from a directory of CSV files.
pub fn load_network(dir: impl AsRef<Path>) -> Result<Network, IoError> {
    assemble(&CsvDir(dir.as_ref()))
}

/// Assemble a network from whatever source supplies the tables.
pub fn assemble(src: &dyn TableSource) -> Result<Network, IoError> {
    let field = |source, file: &str| IoError::Csv {
        file: file.to_string(),
        source,
    };

    // Snapshots first, since every time series is validated against them.
    let snapshots = match src.table("snapshots")? {
        Some(t) if !t.rows.is_empty() => {
            let mut weights = Vec::with_capacity(t.rows.len());
            for r in 0..t.rows.len() {
                weights.push(t.number(r, "weight", 1.0).map_err(|e| field(e, &src.label("snapshots")))?);
            }
            Snapshots::weighted(weights)?
        }
        _ => Snapshots::hourly(1),
    };
    let n_snap = snapshots.len();
    let mut net = Network::new(snapshots);

    // Buses.
    let buses = required(src, "buses")?;
    for r in 0..buses.rows.len() {
        let name = buses.text(r, "name").map_err(|e| field(e, &src.label("buses")))?;
        let country = buses
            .text(r, "country")
            .unwrap_or_else(|_| "??".to_string());
        let idx = net.add_bus(name, country);
        let bf = |c: &str, d: f64| {
            buses
                .number(r, c, d)
                .map_err(|e| field(e, &src.label("buses")))
        };
        net.buses[idx].v_nom = bf("v_nom", 0.0)?;
        net.buses[idx].g_shunt = bf("g_shunt", 0.0)?;
        net.buses[idx].b_shunt = bf("b_shunt", 0.0)?;
        net.buses[idx].v_min = bf("v_min", 0.9)?;
        net.buses[idx].v_max = bf("v_max", 1.1)?;
        net.buses[idx].carrier = buses.text_or(r, "carrier", "AC");
        net.buses[idx].synchronous_area = buses.text_or(r, "synchronous_area", "main");
    }
    let bus_of: std::collections::HashMap<String, usize> = net
        .buses
        .iter()
        .enumerate()
        .map(|(i, b)| (b.name.clone(), i))
        .collect();
    let lookup = |t: &Table, r: usize, col: &str, file: &str| -> Result<usize, IoError> {
        let raw = t.text(r, col).map_err(|e| field(e, file))?;
        bus_of.get(&raw).copied().ok_or(IoError::UnknownBus {
            file: file.to_string(),
            row: r + 2,
            bus: raw,
        })
    };

    // Generators.
    let mut gen_names = Vec::new();
    if let Some(t) = src.table("generators")? {
        for r in 0..t.rows.len() {
            let name = t.text(r, "name").map_err(|e| field(e, &src.label("generators")))?;
            gen_names.push(name.clone());
            let f = |c: &str, d: f64| t.number(r, c, d).map_err(|e| field(e, &src.label("generators")));
            net.add_generator(Generator {
                name,
                bus: lookup(&t, r, "bus", &src.label("generators"))?,
                p_nom: f("p_nom", 0.0)?,
                marginal_cost: f("marginal_cost", 0.0)?,
                carrier: t.text_or(r, "carrier", "unknown"),
                p_min_pu: f("p_min_pu", 0.0)?,
                p_nom_extendable: t
                    .boolean(r, "p_nom_extendable", false)
                    .map_err(|e| field(e, &src.label("generators")))?,
                p_nom_max: f("p_nom_max", f64::INFINITY)?,
                capital_cost: f("capital_cost", 0.0)?,
                co2_emissions: f("co2_emissions", 0.0)?,
                embodied_co2: f("embodied_co2", 0.0)?,
                water_use: f("water_use", 0.0)?,
                land_use: f("land_use", 0.0)?,
                committable: t
                    .boolean(r, "committable", false)
                    .map_err(|e| field(e, &src.label("generators")))?,
                start_up_cost: f("start_up_cost", 0.0)?,
                shut_down_cost: f("shut_down_cost", 0.0)?,
                min_up_time: f("min_up_time", 0.0)? as usize,
                min_down_time: f("min_down_time", 0.0)? as usize,
                ramp_up: f("ramp_up", 0.0)?,
                ramp_down: f("ramp_down", 0.0)?,
                initially_on: t
                    .boolean(r, "initially_on", false)
                    .ok()
                    .filter(|_| t.column("initially_on").is_some()),
                q_min: f("q_min", f64::NEG_INFINITY)?,
                q_max: f("q_max", f64::INFINITY)?,
            });
        }
    }

    // Lines.
    if let Some(t) = src.table("lines")? {
        for r in 0..t.rows.len() {
            let f = |c: &str, d: f64| t.number(r, c, d).map_err(|e| field(e, &src.label("lines")));
            net.add_line(Line {
                name: t.text(r, "name").map_err(|e| field(e, &src.label("lines")))?,
                bus0: lookup(&t, r, "bus0", &src.label("lines"))?,
                bus1: lookup(&t, r, "bus1", &src.label("lines"))?,
                s_nom: f("s_nom", 0.0)?,
                susceptance: f("susceptance", 0.0)?,
                s_nom_extendable: t
                    .boolean(r, "s_nom_extendable", false)
                    .map_err(|e| field(e, &src.label("lines")))?,
                s_nom_max: f("s_nom_max", f64::INFINITY)?,
                capital_cost: f("capital_cost", 0.0)?,
                loss: f("loss", 0.0)?,
                phase_shift: f("phase_shift", 0.0)?,
                resistance: f("resistance", 0.0)?,
                reactance: f("reactance", 0.0)?,
                shunt_susceptance: f("shunt_susceptance", 0.0)?,
                tap_ratio: {
                    let v = f("tap_ratio", 1.0)?;
                    if v > 0.0 { v } else { 1.0 }
                },
            });
        }
    }

    // Loads.
    let mut load_names = Vec::new();
    if let Some(t) = src.table("loads")? {
        for r in 0..t.rows.len() {
            let name = t.text(r, "name").map_err(|e| field(e, &src.label("loads")))?;
            load_names.push(name.clone());
            net.add_load(Load {
                name,
                bus: lookup(&t, r, "bus", &src.label("loads"))?,
                p_set: t.number(r, "p_set", 0.0).map_err(|e| field(e, &src.label("loads")))?,
                q_set: t.number(r, "q_set", 0.0).map_err(|e| field(e, &src.label("loads")))?,
                shiftable_pu: t.number(r, "shiftable_pu", 0.0).map_err(|e| field(e, &src.label("loads")))?,
                shift_window: t.number(r, "shift_window", 0.0).map_err(|e| field(e, &src.label("loads")))? as usize,
                shift_cost: t.number(r, "shift_cost", 0.0).map_err(|e| field(e, &src.label("loads")))?,
            });
        }
    }

    // Storage.
    if let Some(t) = src.table("storage_units")? {
        for r in 0..t.rows.len() {
            let f = |c: &str, d: f64| t.number(r, c, d).map_err(|e| field(e, &src.label("storage_units")));
            net.add_storage(StorageUnit {
                name: t.text(r, "name").map_err(|e| field(e, &src.label("storage_units")))?,
                bus: lookup(&t, r, "bus", &src.label("storage_units"))?,
                p_nom: f("p_nom", 0.0)?,
                max_hours: f("max_hours", 0.0)?,
                efficiency_store: f("efficiency_store", 1.0)?,
                efficiency_dispatch: f("efficiency_dispatch", 1.0)?,
                cyclic: t
                    .boolean(r, "cyclic", true)
                    .map_err(|e| field(e, &src.label("storage_units")))?,
                p_nom_extendable: t
                    .boolean(r, "p_nom_extendable", false)
                    .map_err(|e| field(e, &src.label("storage_units")))?,
                p_nom_max: f("p_nom_max", f64::INFINITY)?,
                capital_cost: f("capital_cost", 0.0)?,
                spillable: t
                    .boolean(r, "spillable", false)
                    .map_err(|e| field(e, &src.label("storage_units")))?,
                // Resolved by name after every unit exists, since a cascade
                // may be declared in any order.
                downstream: None,
                travel_time: f("travel_time", 0.0)? as usize,
                head_min_pu: f("head_min_pu", 1.0)?,
                head_bands: f("head_bands", 0.0)? as usize,
                soc_initial: t
                    .column("soc_initial")
                    .and(f("soc_initial", f64::NAN).ok())
                    .filter(|v| v.is_finite()),
            });
        }
    }

    // Cascade links, resolved once every reservoir exists so the file may list
    // them in any order.
    if let Some(t2) = src.table("storage_units")? {
        let index_of: std::collections::HashMap<String, usize> = net
            .storage
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name.clone(), i))
            .collect();
        for r in 0..t2.rows.len() {
            if let Ok(name) = t2.text(r, "downstream") {
                let Some(&d) = index_of.get(&name) else {
                    return Err(IoError::UnknownComponentColumn {
                        file: src.label("storage_units"),
                        column: name,
                        kind: "storage unit",
                    });
                };
                net.storage[r].downstream = Some(d);
            }
        }
    }

    // Wide time series, transposed on the way in.
    if let Some(t) = src.table("gen_availability")? {
        net.gen_availability =
            wide_series(&t, &gen_names, n_snap, 1.0, &src.label("gen_availability"), "generator")?;
    }
    if let Some(t) = src.table("load_profile")? {
        let defaults: Vec<f64> = net.loads.iter().map(|l| l.p_set).collect();
        net.load_profile = wide_series_with(
            &t,
            &load_names,
            n_snap,
            &defaults,
            &src.label("load_profile"),
            "load",
        )?;
    }

    if let Some(text) = src.text("co2_price.txt")?
        && let Ok(v) = text.trim().parse::<f64>()
        && v.is_finite()
    {
        net.co2_price = v;
    }
    for (file, slot) in [
        ("water_limit.txt", 0usize),
        ("land_limit.txt", 1),
    ] {
        if let Some(text) = src.text(file)?
            && let Ok(v) = text.trim().parse::<f64>()
            && v.is_finite()
        {
            if slot == 0 {
                net.water_limit = Some(v);
            } else {
                net.land_limit = Some(v);
            }
        }
    }
    if let Some(text) = src.text("co2_limit.txt")? {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            net.co2_limit = trimmed.parse().ok();
        }
    }

    net.validate()?;
    Ok(net)
}

/// Transpose a wide table into component major order, with one shared default.
fn wide_series(
    t: &Table,
    names: &[String],
    n_snap: usize,
    default: f64,
    file: &str,
    kind: &'static str,
) -> Result<TimeSeries, IoError> {
    let defaults = vec![default; names.len()];
    wide_series_with(t, names, n_snap, &defaults, file, kind)
}

/// As above, but each component may have its own fallback value.
fn wide_series_with(
    t: &Table,
    names: &[String],
    n_snap: usize,
    defaults: &[f64],
    file: &str,
    kind: &'static str,
) -> Result<TimeSeries, IoError> {
    if t.rows.len() != n_snap {
        return Err(IoError::TimeSeriesRows {
            file: file.to_string(),
            got: t.rows.len(),
            want: n_snap,
        });
    }
    // A column naming a component that does not exist is a mistake worth
    // reporting: it is almost always a rename that was applied to one file and
    // not the other, and silently ignoring it loses data without saying so.
    for h in &t.header {
        let h = h.trim();
        if !h.is_empty() && !names.iter().any(|n| n.eq_ignore_ascii_case(h)) {
            return Err(IoError::UnknownComponentColumn {
                file: file.to_string(),
                column: h.to_string(),
                kind,
            });
        }
    }

    let mut data = Vec::with_capacity(names.len() * n_snap);
    for (c, name) in names.iter().enumerate() {
        let fallback = defaults.get(c).copied().unwrap_or(0.0);
        match t.column(name) {
            Some(_) => {
                for r in 0..n_snap {
                    data.push(t.number(r, name, fallback).map_err(|source| IoError::Csv {
                        file: file.to_string(),
                        source,
                    })?);
                }
            }
            // A component with no column keeps its scalar value at every step.
            None => data.extend(std::iter::repeat_n(fallback, n_snap)),
        }
    }
    TimeSeries::from_flat(data, names.len(), n_snap).map_err(IoError::Invalid)
}

/// Write one CSV, creating the directory if needed.
fn write_csv(dir: &Path, name: &str, contents: &str) -> Result<(), IoError> {
    std::fs::create_dir_all(dir).map_err(|source| IoError::Read {
        path: dir.display().to_string(),
        source,
    })?;
    let path = dir.join(name);
    std::fs::write(&path, contents).map_err(|source| IoError::Read {
        path: path.display().to_string(),
        source,
    })
}


/// Write a network as a directory of CSV files.
///
/// The inverse of [`load_network`], and the reason conversion between any two
/// formats here is a read followed by a write rather than a matrix of
/// converters. Every column the reader understands is written, so a round trip
/// through CSV loses only what the type itself does not carry.
pub fn write_network(net: &Network, dir: impl AsRef<Path>) -> Result<(), IoError> {
    let dir = dir.as_ref();
    std::fs::create_dir_all(dir).map_err(|source| IoError::Read {
        path: dir.display().to_string(),
        source,
    })?;

    // Values are rendered with `{:?}`, which for a float is the shortest text
    // that reads back to the same bits. `{}` would round and quietly lose the
    // last places of an impedance.
    fn f(v: f64) -> String {
        if v.is_infinite() {
            if v > 0.0 { "inf".into() } else { "-inf".into() }
        } else {
            format!("{v:?}")
        }
    }
    fn q(s: &str) -> String {
        if s.contains([',', '"', '\n']) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    }

    let bus = |i: usize| q(&net.buses[i].name);

    let mut out = String::from("name,country,synchronous_area,carrier,v_nom,v_min,v_max,g_shunt,b_shunt\n");
    for b in &net.buses {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{}\n",
            q(&b.name), q(&b.country), q(&b.synchronous_area), q(&b.carrier),
            f(b.v_nom), f(b.v_min), f(b.v_max), f(b.g_shunt), f(b.b_shunt)
        ));
    }
    write_csv(dir, "buses.csv", &out)?;

    let mut out = String::from(
        "name,bus,carrier,p_nom,p_nom_extendable,p_nom_max,p_min_pu,marginal_cost,\
capital_cost,co2_emissions,embodied_co2,water_use,land_use,committable,start_up_cost,shut_down_cost,\
min_up_time,min_down_time,ramp_up,ramp_down,q_min,q_max\n",
    );
    for g in &net.generators {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            q(&g.name), bus(g.bus), q(&g.carrier), f(g.p_nom), g.p_nom_extendable,
            f(g.p_nom_max), f(g.p_min_pu), f(g.marginal_cost), f(g.capital_cost),
            f(g.co2_emissions), f(g.embodied_co2), f(g.water_use), f(g.land_use),
            g.committable, f(g.start_up_cost),
            f(g.shut_down_cost), g.min_up_time, g.min_down_time, f(g.ramp_up),
            f(g.ramp_down), f(g.q_min), f(g.q_max)
        ));
    }
    write_csv(dir, "generators.csv", &out)?;

    let mut out = String::from(
        "name,bus0,bus1,s_nom,susceptance,resistance,reactance,shunt_susceptance,\
tap_ratio,phase_shift,loss,s_nom_extendable,s_nom_max,capital_cost\n",
    );
    for l in &net.lines {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            q(&l.name), bus(l.bus0), bus(l.bus1), f(l.s_nom), f(l.susceptance),
            f(l.resistance), f(l.reactance), f(l.shunt_susceptance), f(l.tap_ratio),
            f(l.phase_shift), f(l.loss), l.s_nom_extendable, f(l.s_nom_max),
            f(l.capital_cost)
        ));
    }
    write_csv(dir, "lines.csv", &out)?;

    let mut out = String::from("name,bus,p_set,q_set,shiftable_pu,shift_window,shift_cost\n");
    for l in &net.loads {
        out.push_str(&format!(
            "{},{},{},{},{},{},{}\n",
            q(&l.name), bus(l.bus), f(l.p_set), f(l.q_set), f(l.shiftable_pu),
            l.shift_window, f(l.shift_cost)
        ));
    }
    write_csv(dir, "loads.csv", &out)?;

    if !net.storage.is_empty() {
        let mut out = String::from(
            "name,bus,p_nom,max_hours,efficiency_store,efficiency_dispatch,cyclic,\
p_nom_extendable,p_nom_max,capital_cost,head_min_pu,head_bands,travel_time,spillable\n",
        );
        for s in &net.storage {
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
                q(&s.name), bus(s.bus), f(s.p_nom), f(s.max_hours),
                f(s.efficiency_store), f(s.efficiency_dispatch), s.cyclic,
                s.p_nom_extendable, f(s.p_nom_max), f(s.capital_cost),
                f(s.head_min_pu), s.head_bands, s.travel_time, s.spillable
            ));
        }
        write_csv(dir, "storage_units.csv", &out)?;
    }

    let mut out = String::from("weight\n");
    for w in net.snapshots.weights() {
        out.push_str(&format!("{}\n", f(*w)));
    }
    write_csv(dir, "snapshots.csv", &out)?;

    // Wide series: one column per component, one row per snapshot, which is
    // the shape the reader transposes back on the way in.
    let n = net.n_snapshots();
    let wide = |names: Vec<String>, ts: &TimeSeries| -> Option<String> {
        if ts.is_empty() {
            return None;
        }
        let mut out = names.iter().map(|s| q(s)).collect::<Vec<_>>().join(",");
        out.push('\n');
        for t in 0..n {
            let row: Vec<String> = (0..names.len())
                .map(|c| f(ts.at(c, t).unwrap_or(0.0)))
                .collect();
            out.push_str(&row.join(","));
            out.push('\n');
        }
        Some(out)
    };
    if let Some(text) = wide(
        net.generators.iter().map(|g| g.name.clone()).collect(),
        &net.gen_availability,
    ) {
        write_csv(dir, "gen_availability.csv", &text)?;
    }
    if let Some(text) = wide(
        net.loads.iter().map(|l| l.name.clone()).collect(),
        &net.load_profile,
    ) {
        write_csv(dir, "load_profile.csv", &text)?;
    }

    if net.co2_price != 0.0 {
        write_csv(dir, "co2_price.txt", &f(net.co2_price))?;
    }
    if let Some(limit) = net.co2_limit {
        write_csv(dir, "co2_limit.txt", &f(limit))?;
    }
    if let Some(limit) = net.water_limit {
        write_csv(dir, "water_limit.txt", &f(limit))?;
    }
    if let Some(limit) = net.land_limit {
        write_csv(dir, "land_limit.txt", &f(limit))?;
    }
    Ok(())
}

/// Everything a solved model has to say, as wide CSVs matching the input shape.
pub struct Results<'a> {
    pub network: &'a Network,
    pub dispatch: Vec<&'a [f64]>,
    pub flows: Vec<&'a [f64]>,
    pub prices: Vec<&'a [f64]>,
    pub shed: Vec<&'a [f64]>,
    pub built: Vec<(String, f64)>,
}

impl Results<'_> {
    pub fn write(&self, dir: impl AsRef<Path>) -> Result<(), IoError> {
        let dir = dir.as_ref();
        let n = self.network.n_snapshots();

        let wide = |names: Vec<&str>, cols: &[&[f64]]| -> String {
            let mut s = String::from("snapshot");
            for name in &names {
                s.push(',');
                s.push_str(&csv::escape(name));
            }
            s.push('\n');
            for t in 0..n {
                s.push_str(&t.to_string());
                for c in cols {
                    s.push(',');
                    s.push_str(&format!("{:.6}", c[t]));
                }
                s.push('\n');
            }
            s
        };

        write_csv(
            dir,
            "dispatch.csv",
            &wide(
                self.network.generators.iter().map(|g| g.name.as_str()).collect(),
                &self.dispatch,
            ),
        )?;
        write_csv(
            dir,
            "flows.csv",
            &wide(
                self.network.lines.iter().map(|l| l.name.as_str()).collect(),
                &self.flows,
            ),
        )?;
        write_csv(
            dir,
            "prices.csv",
            &wide(
                self.network.buses.iter().map(|b| b.name.as_str()).collect(),
                &self.prices,
            ),
        )?;
        write_csv(
            dir,
            "shed.csv",
            &wide(
                self.network.buses.iter().map(|b| b.name.as_str()).collect(),
                &self.shed,
            ),
        )?;

        if !self.built.is_empty() {
            let mut s = String::from("component,capacity\n");
            for (name, mw) in &self.built {
                s.push_str(&format!("{},{mw:.6}\n", csv::escape(name)));
            }
            write_csv(dir, "capacity.csv", &s)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempdir::Dir {
        let d = tempdir::Dir::new();
        d.write("buses.csv", "name,country\nDE,DE\nFR,FR\n");
        d.write(
            "generators.csv",
            "name,bus,p_nom,marginal_cost\ncoal,DE,100,40\nnuke,FR,200,10\n",
        );
        d.write(
            "lines.csv",
            "name,bus0,bus1,s_nom,susceptance\nDE-FR,DE,FR,50,0\n",
        );
        d.write("loads.csv", "name,bus,p_set\nl,DE,80\n");
        d
    }

    #[test]
    fn loads_a_minimal_network() {
        let d = fixture();
        let net = load_network(d.path()).unwrap();
        assert_eq!(net.buses.len(), 2);
        assert_eq!(net.generators.len(), 2);
        assert_eq!(net.generators[0].marginal_cost, 40.0);
        assert_eq!(net.lines[0].bus0, 0);
        assert_eq!(net.lines[0].bus1, 1);
        assert_eq!(net.loads[0].p_set, 80.0);
        // Absent optional files leave their components empty rather than
        // failing, so a dispatch-only model needs four files.
        assert!(net.storage.is_empty());
    }

    #[test]
    fn bus_names_are_resolved_to_indices_and_typos_are_caught() {
        let d = fixture();
        d.write("loads.csv", "name,bus,p_set\nl,ATLANTIS,80\n");
        assert!(matches!(
            load_network(d.path()),
            Err(IoError::UnknownBus { .. })
        ));
    }

    #[test]
    fn optional_expansion_columns_default_when_absent() {
        let d = fixture();
        let net = load_network(d.path()).unwrap();
        assert!(!net.generators[0].p_nom_extendable);
        assert!(net.generators[0].p_nom_max.is_infinite());
        assert_eq!(net.generators[0].capital_cost, 0.0);
    }

    #[test]
    fn expansion_columns_are_read_when_present() {
        let d = fixture();
        d.write(
            "generators.csv",
            "name,bus,p_nom,marginal_cost,p_nom_extendable,p_nom_max,capital_cost,co2_emissions\n\
             coal,DE,100,40,false,inf,0,0.9\n\
             solar,FR,0,0,true,500,120,0\n",
        );
        let net = load_network(d.path()).unwrap();
        assert!(!net.generators[0].p_nom_extendable);
        assert_eq!(net.generators[0].co2_emissions, 0.9);
        assert!(net.generators[1].p_nom_extendable);
        assert_eq!(net.generators[1].p_nom_max, 500.0);
        assert_eq!(net.generators[1].capital_cost, 120.0);
    }

    #[test]
    fn wide_time_series_are_transposed_into_component_major_order() {
        let d = fixture();
        d.write("snapshots.csv", "weight\n1\n1\n1\n");
        d.write("gen_availability.csv", "coal,nuke\n1.0,1.0\n0.5,1.0\n0.0,0.9\n");
        let net = load_network(d.path()).unwrap();
        // Component major: generator 0's whole run comes first.
        assert_eq!(net.gen_availability.row(0).unwrap(), &[1.0, 0.5, 0.0]);
        assert_eq!(net.gen_availability.row(1).unwrap(), &[1.0, 1.0, 0.9]);
    }

    #[test]
    fn a_component_missing_from_a_wide_file_keeps_its_default() {
        let d = fixture();
        d.write("snapshots.csv", "weight\n1\n1\n");
        // Only coal has a profile; nuke should stay fully available.
        d.write("gen_availability.csv", "coal\n0.4\n0.6\n");
        let net = load_network(d.path()).unwrap();
        assert_eq!(net.gen_availability.row(0).unwrap(), &[0.4, 0.6]);
        assert_eq!(net.gen_availability.row(1).unwrap(), &[1.0, 1.0]);
    }

    #[test]
    fn a_load_without_a_profile_holds_its_scalar_value() {
        let d = fixture();
        d.write("loads.csv", "name,bus,p_set\nl,DE,80\nm,FR,25\n");
        d.write("snapshots.csv", "weight\n1\n1\n");
        d.write("load_profile.csv", "l\n70\n90\n");
        let net = load_network(d.path()).unwrap();
        assert_eq!(net.load_profile.row(0).unwrap(), &[70.0, 90.0]);
        // The second load had no column, so its p_set carries across.
        assert_eq!(net.load_profile.row(1).unwrap(), &[25.0, 25.0]);
    }

    #[test]
    fn a_time_series_column_naming_nothing_is_an_error() {
        let d = fixture();
        d.write("snapshots.csv", "weight\n1\n");
        d.write("gen_availability.csv", "coal,ghost\n1.0,1.0\n");
        assert!(matches!(
            load_network(d.path()),
            Err(IoError::UnknownComponentColumn { .. })
        ));
    }

    #[test]
    fn a_time_series_with_the_wrong_row_count_is_an_error() {
        let d = fixture();
        d.write("snapshots.csv", "weight\n1\n1\n1\n");
        d.write("gen_availability.csv", "coal\n1.0\n");
        assert!(matches!(
            load_network(d.path()),
            Err(IoError::TimeSeriesRows { got: 1, want: 3, .. })
        ));
    }

    #[test]
    fn snapshot_weights_are_honoured() {
        let d = fixture();
        d.write("snapshots.csv", "weight\n3\n3\n");
        let net = load_network(d.path()).unwrap();
        assert_eq!(net.n_snapshots(), 2);
        assert_eq!(net.snapshots.weight(0), 3.0);
    }

    #[test]
    fn a_missing_required_file_is_reported() {
        let d = tempdir::Dir::new();
        assert!(matches!(load_network(d.path()), Err(IoError::Read { .. })));
    }

    #[test]
    fn an_invalid_network_fails_at_load_rather_than_at_build() {
        let d = fixture();
        d.write("lines.csv", "name,bus0,bus1,s_nom,susceptance\nx,DE,DE,50,0\n");
        assert!(matches!(load_network(d.path()), Err(IoError::Invalid(_))));
    }

    /// A tiny scratch directory helper, so the tests do not need a dependency
    /// for something this small.
    mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicUsize, Ordering};

        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new() -> Self {
                let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                let p = std::env::temp_dir().join(format!(
                    "gridwright-io-test-{}-{n}",
                    std::process::id()
                ));
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
            pub fn write(&self, name: &str, contents: &str) {
                std::fs::write(self.0.join(name), contents).unwrap();
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}

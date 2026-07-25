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
    #[error("reading MATPOWER case: {0}")]
    Matpower(#[from] matpower::MatpowerError),
}

fn read(dir: &Path, name: &str) -> Result<Option<String>, IoError> {
    let path = dir.join(name);
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s)),
        // A missing optional file is not an error; a missing required one is
        // reported by the caller with better context than "no such file".
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(IoError::Read {
            path: path.display().to_string(),
            source: e,
        }),
    }
}

fn table(dir: &Path, name: &str) -> Result<Option<Table>, IoError> {
    let Some(text) = read(dir, name)? else {
        return Ok(None);
    };
    Table::parse(&text)
        .map(Some)
        .map_err(|source| IoError::Csv {
            file: name.to_string(),
            source,
        })
}

fn required(dir: &Path, name: &str) -> Result<Table, IoError> {
    table(dir, name)?.ok_or_else(|| IoError::Read {
        path: dir.join(name).display().to_string(),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "required file is missing"),
    })
}

/// Load a network from a directory of CSV files.
pub fn load_network(dir: impl AsRef<Path>) -> Result<Network, IoError> {
    let dir = dir.as_ref();
    let field = |source, file: &str| IoError::Csv {
        file: file.to_string(),
        source,
    };

    // Snapshots first, since every time series is validated against them.
    let snapshots = match table(dir, "snapshots.csv")? {
        Some(t) if !t.rows.is_empty() => {
            let mut weights = Vec::with_capacity(t.rows.len());
            for r in 0..t.rows.len() {
                weights.push(t.number(r, "weight", 1.0).map_err(|e| field(e, "snapshots.csv"))?);
            }
            Snapshots::weighted(weights)?
        }
        _ => Snapshots::hourly(1),
    };
    let n_snap = snapshots.len();
    let mut net = Network::new(snapshots);

    // Buses.
    let buses = required(dir, "buses.csv")?;
    for r in 0..buses.rows.len() {
        let name = buses.text(r, "name").map_err(|e| field(e, "buses.csv"))?;
        let country = buses
            .text(r, "country")
            .unwrap_or_else(|_| "??".to_string());
        net.add_bus(name, country);
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
    if let Some(t) = table(dir, "generators.csv")? {
        for r in 0..t.rows.len() {
            let name = t.text(r, "name").map_err(|e| field(e, "generators.csv"))?;
            gen_names.push(name.clone());
            let f = |c: &str, d: f64| t.number(r, c, d).map_err(|e| field(e, "generators.csv"));
            net.add_generator(Generator {
                name,
                bus: lookup(&t, r, "bus", "generators.csv")?,
                p_nom: f("p_nom", 0.0)?,
                marginal_cost: f("marginal_cost", 0.0)?,
                carrier: t.text_or(r, "carrier", "unknown"),
                p_min_pu: f("p_min_pu", 0.0)?,
                p_nom_extendable: t
                    .boolean(r, "p_nom_extendable", false)
                    .map_err(|e| field(e, "generators.csv"))?,
                p_nom_max: f("p_nom_max", f64::INFINITY)?,
                capital_cost: f("capital_cost", 0.0)?,
                co2_emissions: f("co2_emissions", 0.0)?,
                embodied_co2: f("embodied_co2", 0.0)?,
                committable: t
                    .boolean(r, "committable", false)
                    .map_err(|e| field(e, "generators.csv"))?,
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
    if let Some(t) = table(dir, "lines.csv")? {
        for r in 0..t.rows.len() {
            let f = |c: &str, d: f64| t.number(r, c, d).map_err(|e| field(e, "lines.csv"));
            net.add_line(Line {
                name: t.text(r, "name").map_err(|e| field(e, "lines.csv"))?,
                bus0: lookup(&t, r, "bus0", "lines.csv")?,
                bus1: lookup(&t, r, "bus1", "lines.csv")?,
                s_nom: f("s_nom", 0.0)?,
                susceptance: f("susceptance", 0.0)?,
                s_nom_extendable: t
                    .boolean(r, "s_nom_extendable", false)
                    .map_err(|e| field(e, "lines.csv"))?,
                s_nom_max: f("s_nom_max", f64::INFINITY)?,
                capital_cost: f("capital_cost", 0.0)?,
                loss: f("loss", 0.0)?,
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
    if let Some(t) = table(dir, "loads.csv")? {
        for r in 0..t.rows.len() {
            let name = t.text(r, "name").map_err(|e| field(e, "loads.csv"))?;
            load_names.push(name.clone());
            net.add_load(Load {
                name,
                bus: lookup(&t, r, "bus", "loads.csv")?,
                p_set: t.number(r, "p_set", 0.0).map_err(|e| field(e, "loads.csv"))?,
                q_set: t.number(r, "q_set", 0.0).map_err(|e| field(e, "loads.csv"))?,
            });
        }
    }

    // Storage.
    if let Some(t) = table(dir, "storage_units.csv")? {
        for r in 0..t.rows.len() {
            let f = |c: &str, d: f64| t.number(r, c, d).map_err(|e| field(e, "storage_units.csv"));
            net.add_storage(StorageUnit {
                name: t.text(r, "name").map_err(|e| field(e, "storage_units.csv"))?,
                bus: lookup(&t, r, "bus", "storage_units.csv")?,
                p_nom: f("p_nom", 0.0)?,
                max_hours: f("max_hours", 0.0)?,
                efficiency_store: f("efficiency_store", 1.0)?,
                efficiency_dispatch: f("efficiency_dispatch", 1.0)?,
                cyclic: t
                    .boolean(r, "cyclic", true)
                    .map_err(|e| field(e, "storage_units.csv"))?,
                p_nom_extendable: t
                    .boolean(r, "p_nom_extendable", false)
                    .map_err(|e| field(e, "storage_units.csv"))?,
                p_nom_max: f("p_nom_max", f64::INFINITY)?,
                capital_cost: f("capital_cost", 0.0)?,
                spillable: t
                    .boolean(r, "spillable", false)
                    .map_err(|e| field(e, "storage_units.csv"))?,
                // Resolved by name after every unit exists, since a cascade
                // may be declared in any order.
                downstream: None,
                travel_time: f("travel_time", 0.0)? as usize,
                head_min_pu: f("head_min_pu", 1.0)?,
                soc_initial: t
                    .column("soc_initial")
                    .and(f("soc_initial", f64::NAN).ok())
                    .filter(|v| v.is_finite()),
            });
        }
    }

    // Cascade links, resolved once every reservoir exists so the file may list
    // them in any order.
    if let Some(t2) = table(dir, "storage_units.csv")? {
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
                        file: "storage_units.csv".into(),
                        column: name,
                        kind: "storage unit",
                    });
                };
                net.storage[r].downstream = Some(d);
            }
        }
    }

    // Wide time series, transposed on the way in.
    if let Some(t) = table(dir, "gen_availability.csv")? {
        net.gen_availability =
            wide_series(&t, &gen_names, n_snap, 1.0, "gen_availability.csv", "generator")?;
    }
    if let Some(t) = table(dir, "load_profile.csv")? {
        let defaults: Vec<f64> = net.loads.iter().map(|l| l.p_set).collect();
        net.load_profile = wide_series_with(
            &t,
            &load_names,
            n_snap,
            &defaults,
            "load_profile.csv",
            "load",
        )?;
    }

    if let Some(text) = read(dir, "co2_price.txt")?
        && let Ok(v) = text.trim().parse::<f64>()
        && v.is_finite()
    {
        net.co2_price = v;
    }
    if let Some(text) = read(dir, "co2_limit.txt")? {
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

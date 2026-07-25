//! PyPSA networks, read from the netCDF they are published as.
//!
//! PyPSA is the largest open energy modelling ecosystem there is. PyPSA-Eur,
//! PyPSA-Earth and every study built on them ship their networks as `.nc`, so
//! this is the single format that opens the most existing data.
//!
//! netCDF4 is HDF5 underneath, and the reader here is pure Rust. That matters
//! beyond tidiness: the C netCDF library would put a toolchain between this
//! crate and its WebAssembly target, and the interface this is all heading
//! towards runs in a browser.
//!
//! # Layout
//!
//! PyPSA flattens its data frames into variables named `<component>_<attr>`,
//! with the component index in `<component>_i`:
//!
//! ```text
//!   buses_i, buses_v_nom, buses_country
//!   generators_i, generators_bus, generators_p_nom, generators_marginal_cost
//!   lines_i, lines_bus0, lines_bus1, lines_x, lines_s_nom
//!   snapshots, snapshot_weightings_objective
//!   generators_t_p_max_pu   with its own axis generators_t_p_max_pu_i
//! ```
//!
//! Time-varying attributes carry their own component axis, because PyPSA only
//! stores series for the components that have one. A file with profiles for
//! two generators out of five is normal, and the other three are static.
//!
//! # The unit that catches people
//!
//! **PyPSA states line impedance in ohms.** The optimisation works in per
//! unit, and the conversion needs the nominal voltage:
//!
//! ```text
//!   x_pu = x_ohm · S_base / v_nom²
//! ```
//!
//! Reading `lines_x` as though it were already per unit gives a 132 kV line a
//! susceptance roughly 170 times too small, which does not fail — it produces
//! a network where power will not flow and the optimiser sheds load to explain
//! it. Where no nominal voltage can be found the values are taken as per unit
//! already and that assumption is reported.

use std::collections::HashMap;
use std::path::Path;

use gridwright_net::{Generator, Line, Load, Network, Snapshots, StorageUnit, TimeSeries};

use crate::Case;

#[derive(Debug, thiserror::Error)]
pub enum NetcdfError {
    #[error("{file}: {message}")]
    Open { file: String, message: String },
    #[error("{file} has no `buses_i`; this does not look like a PyPSA network")]
    NotPypsa { file: String },
    #[error("{variable}: expected {want} values, found {got}")]
    Length {
        variable: String,
        got: usize,
        want: usize,
    },
    #[error("{variable} names bus `{bus}`, which no bus is called")]
    UnknownBus { variable: String, bus: String },
    #[error("network is not valid: {0}")]
    Invalid(#[from] gridwright_net::NetError),
}

/// A PyPSA netCDF file, opened.
struct Nc {
    file: hdf5_pure::File,
    present: std::collections::HashSet<String>,
}

impl Nc {
    fn open(path: &Path) -> Result<Self, NetcdfError> {
        let bytes = std::fs::read(path).map_err(|e| NetcdfError::Open {
            file: path.display().to_string(),
            message: e.to_string(),
        })?;
        Self::from_bytes(bytes, &path.display().to_string())
    }

    /// The same, from memory.
    ///
    /// The reader is pure Rust and takes a byte vector, so a network arriving
    /// from a file picker or over a socket needs no temporary file and no
    /// filesystem at all — which is the whole reason a WebAssembly build can
    /// open one.
    fn from_bytes(bytes: Vec<u8>, label: &str) -> Result<Self, NetcdfError> {
        let label = label.to_string();
        let file = hdf5_pure::File::from_bytes(bytes).map_err(|e| NetcdfError::Open {
            file: label.clone(),
            message: e.to_string(),
        })?;
        let present = file
            .root()
            .datasets()
            .map_err(|e| NetcdfError::Open {
                file: label,
                message: e.to_string(),
            })?
            .into_iter()
            .collect();
        Ok(Self { file, present })
    }

    fn has(&self, name: &str) -> bool {
        self.present.contains(name)
    }

    /// A numeric variable, or `None` when the file does not carry it.
    ///
    /// An absent optional attribute is the normal case — PyPSA writes only
    /// what differs from its defaults — so this is not an error.
    fn reals(&self, name: &str) -> Option<Vec<f64>> {
        if !self.has(name) {
            return None;
        }
        self.file.dataset(name).ok()?.read_f64().ok()
    }

    fn strings(&self, name: &str) -> Option<Vec<String>> {
        if !self.has(name) {
            return None;
        }
        self.file.dataset(name).ok()?.read_string().ok()
    }

    fn shape(&self, name: &str) -> Option<Vec<u64>> {
        self.file.dataset(name).ok()?.shape().ok()
    }
}

/// A column of numbers for a component set, defaulting where absent.
fn column(nc: &Nc, name: &str, n: usize, default: f64) -> Result<Vec<f64>, NetcdfError> {
    match nc.reals(name) {
        None => Ok(vec![default; n]),
        Some(v) if v.len() == n => Ok(v),
        // A scalar written for a uniform attribute broadcasts, which is what
        // PyPSA does when every component shares a value.
        Some(v) if v.len() == 1 => Ok(vec![v[0]; n]),
        Some(v) => Err(NetcdfError::Length {
            variable: name.into(),
            got: v.len(),
            want: n,
        }),
    }
}

fn labels(nc: &Nc, name: &str, n: usize, default: &str) -> Vec<String> {
    match nc.strings(name) {
        Some(v) if v.len() == n => v,
        Some(v) if v.len() == 1 => vec![v[0].clone(); n],
        _ => vec![default.to_string(); n],
    }
}

/// Read a time-varying attribute onto the components that have one.
///
/// The stored array is `(snapshots, components)` — snapshot major, the
/// opposite of what the engine wants — and only covers the components listed
/// on its own axis. Both are handled here rather than by the caller, since
/// getting either wrong produces a plausible profile attached to the wrong
/// plant.
fn series(
    nc: &Nc,
    name: &str,
    all_names: &[String],
    n_snapshots: usize,
    defaults: &[f64],
) -> Option<TimeSeries> {
    let values = nc.reals(name)?;
    let axis = format!("{name}_i");
    let covered = nc
        .strings(&axis)
        .unwrap_or_else(|| all_names.to_vec());
    let width = covered.len();
    if width == 0 || values.len() != n_snapshots * width {
        // A shape that does not match is worse than no series: it would be
        // silently misaligned. Reported by the caller through the notes.
        return None;
    }

    let mut data = Vec::with_capacity(all_names.len() * n_snapshots);
    for (c, _) in all_names.iter().enumerate() {
        data.extend(std::iter::repeat_n(
            defaults.get(c).copied().unwrap_or(1.0),
            n_snapshots,
        ));
    }
    for (col, who) in covered.iter().enumerate() {
        let Some(component) = all_names.iter().position(|n| n == who) else {
            continue;
        };
        for t in 0..n_snapshots {
            data[component * n_snapshots + t] = values[t * width + col];
        }
    }
    TimeSeries::from_flat(data, all_names.len(), n_snapshots).ok()
}

/// Read a PyPSA network from a netCDF file.
pub fn load_network(path: impl AsRef<Path>) -> Result<Case, crate::IoError> {
    let path = path.as_ref();
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "network".into());
    read(Nc::open(path).map_err(crate::IoError::Netcdf)?, &name)
        .map_err(crate::IoError::Netcdf)
}

/// Read a PyPSA network from bytes already in memory.
pub fn parse_network(bytes: Vec<u8>, name: &str) -> Result<Case, NetcdfError> {
    let nc = Nc::from_bytes(bytes, name)?;
    read(nc, name)
}

fn read(nc: Nc, name: &str) -> Result<Case, NetcdfError> {
    let Some(bus_names) = nc.strings("buses_i") else {
        return Err(NetcdfError::NotPypsa {
            file: name.to_string(),
        });
    };
    let n_bus = bus_names.len();
    let mut notes = Vec::new();

    // Snapshots, with their weights. PyPSA renamed this variable at 0.20; both
    // spellings are still in circulation and both are accepted.
    let n_snap = nc
        .shape("snapshots")
        .and_then(|s| s.first().copied())
        .unwrap_or(1)
        .max(1) as usize;
    let weights = nc
        .reals("snapshot_weightings_objective")
        .or_else(|| nc.reals("snapshot_weightings"))
        .filter(|w| w.len() == n_snap)
        .unwrap_or_else(|| vec![1.0; n_snap]);
    let mut net = Network::new(Snapshots::weighted(weights)?);

    let countries = labels(&nc, "buses_country", n_bus, "??");
    let carriers = labels(&nc, "buses_carrier", n_bus, "AC");
    let v_nom = column(&nc, "buses_v_nom", n_bus, 0.0)?;
    let v_min = column(&nc, "buses_v_mag_pu_min", n_bus, 0.9)?;
    let v_max = column(&nc, "buses_v_mag_pu_max", n_bus, 1.1)?;
    let mut index_of: HashMap<String, usize> = HashMap::new();
    for (i, name) in bus_names.iter().enumerate() {
        let idx = net.add_bus(name.clone(), countries[i].clone());
        net.buses[idx].v_nom = v_nom[i];
        net.buses[idx].carrier = carriers[i].clone();
        if v_max[i] > v_min[i] && v_min[i] > 0.0 {
            net.buses[idx].v_min = v_min[i];
            net.buses[idx].v_max = v_max[i];
        }
        index_of.insert(name.clone(), idx);
    }

    let bus_of = |variable: &str, name: &str| -> Result<usize, NetcdfError> {
        index_of
            .get(name)
            .copied()
            .ok_or_else(|| NetcdfError::UnknownBus {
                variable: variable.into(),
                bus: name.into(),
            })
    };

    // Generators.
    let gen_names = nc.strings("generators_i").unwrap_or_default();
    let n_gen = gen_names.len();
    let at = labels(&nc, "generators_bus", n_gen, "");
    let p_nom = column(&nc, "generators_p_nom", n_gen, 0.0)?;
    let p_min_pu = column(&nc, "generators_p_min_pu", n_gen, 0.0)?;
    let cost = column(&nc, "generators_marginal_cost", n_gen, 0.0)?;
    let capital = column(&nc, "generators_capital_cost", n_gen, 0.0)?;
    let p_nom_max = column(&nc, "generators_p_nom_max", n_gen, f64::INFINITY)?;
    let extendable = column(&nc, "generators_p_nom_extendable", n_gen, 0.0)?;
    let gen_carrier = labels(&nc, "generators_carrier", n_gen, "unknown");
    let ramp_up = column(&nc, "generators_ramp_limit_up", n_gen, 0.0)?;
    let ramp_down = column(&nc, "generators_ramp_limit_down", n_gen, 0.0)?;
    for (i, name) in gen_names.iter().enumerate() {
        net.add_generator(Generator {
            name: name.clone(),
            bus: bus_of("generators_bus", &at[i])?,
            p_nom: p_nom[i],
            marginal_cost: cost[i],
            capital_cost: capital[i],
            carrier: gen_carrier[i].clone(),
            p_min_pu: p_min_pu[i].clamp(0.0, 1.0),
            p_nom_extendable: extendable[i] != 0.0,
            p_nom_max: p_nom_max[i],
            ramp_up: ramp_up[i],
            ramp_down: ramp_down[i],
            ..Default::default()
        });
    }

    // Loads.
    let load_names = nc.strings("loads_i").unwrap_or_default();
    let n_load = load_names.len();
    let at = labels(&nc, "loads_bus", n_load, "");
    let p_set = column(&nc, "loads_p_set", n_load, 0.0)?;
    let q_set = column(&nc, "loads_q_set", n_load, 0.0)?;
    for (i, name) in load_names.iter().enumerate() {
        net.add_load(Load {
            name: name.clone(),
            bus: bus_of("loads_bus", &at[i])?,
            p_set: p_set[i],
            q_set: q_set[i],
        });
    }

    // Lines. This is where the ohms live.
    let line_names = nc.strings("lines_i").unwrap_or_default();
    let n_line = line_names.len();
    let b0 = labels(&nc, "lines_bus0", n_line, "");
    let b1 = labels(&nc, "lines_bus1", n_line, "");
    let s_nom = column(&nc, "lines_s_nom", n_line, 0.0)?;
    let x_ohm = column(&nc, "lines_x", n_line, 0.0)?;
    let r_ohm = column(&nc, "lines_r", n_line, 0.0)?;
    let tap = column(&nc, "lines_tap_ratio", n_line, 1.0)?;
    let base = net.base_mva;
    let mut assumed_pu = 0;
    let mut zero_reactance = 0;
    for (i, name) in line_names.iter().enumerate() {
        let bus0 = bus_of("lines_bus0", &b0[i])?;
        let bus1 = bus_of("lines_bus1", &b1[i])?;
        // The line's own nominal voltage if the file states it, otherwise the
        // bus it leaves.
        let kv = nc
            .reals("lines_v_nom")
            .and_then(|v| v.get(i).copied())
            .filter(|v| *v > 0.0)
            .unwrap_or(net.buses[bus0].v_nom);
        let (x, r) = if kv > 0.0 {
            let z_base = kv * kv / base;
            (x_ohm[i] / z_base, r_ohm[i] / z_base)
        } else {
            assumed_pu += 1;
            (x_ohm[i], r_ohm[i])
        };
        let susceptance = if x.abs() > 1e-12 {
            1.0 / x
        } else {
            zero_reactance += 1;
            0.0
        };
        net.add_line(Line {
            name: name.clone(),
            bus0,
            bus1,
            s_nom: if s_nom[i] > 0.0 { s_nom[i] } else { 1e6 },
            susceptance,
            resistance: r,
            reactance: x,
            tap_ratio: if tap[i] > 0.0 { tap[i] } else { 1.0 },
            ..Default::default()
        });
    }

    // Links: PyPSA's controllable connections, which is what its HVDC and all
    // its sector coupling are. A link between two electricity buses is a
    // transport corridor; one between different carriers is a conversion, and
    // the efficiency is what tells them apart.
    let link_names = nc.strings("links_i").unwrap_or_default();
    let n_link = link_names.len();
    if n_link > 0 {
        let b0 = labels(&nc, "links_bus0", n_link, "");
        let b1 = labels(&nc, "links_bus1", n_link, "");
        let p_nom = column(&nc, "links_p_nom", n_link, 0.0)?;
        let eff = column(&nc, "links_efficiency", n_link, 1.0)?;
        let cost = column(&nc, "links_marginal_cost", n_link, 0.0)?;
        for (i, name) in link_names.iter().enumerate() {
            net.add_link(gridwright_net::Link {
                name: name.clone(),
                bus0: bus_of("links_bus0", &b0[i])?,
                bus1: bus_of("links_bus1", &b1[i])?,
                p_nom: p_nom[i],
                efficiency: eff[i],
                marginal_cost: cost[i],
                ..Default::default()
            });
        }
    }

    // Storage.
    let store_names = nc.strings("storage_units_i").unwrap_or_default();
    let n_store = store_names.len();
    if n_store > 0 {
        let at = labels(&nc, "storage_units_bus", n_store, "");
        let p_nom = column(&nc, "storage_units_p_nom", n_store, 0.0)?;
        let hours = column(&nc, "storage_units_max_hours", n_store, 1.0)?;
        let store = column(&nc, "storage_units_efficiency_store", n_store, 1.0)?;
        let dispatch = column(&nc, "storage_units_efficiency_dispatch", n_store, 1.0)?;
        let cyclic = column(&nc, "storage_units_cyclic_state_of_charge", n_store, 0.0)?;
        for (i, name) in store_names.iter().enumerate() {
            net.add_storage(StorageUnit {
                name: name.clone(),
                bus: bus_of("storage_units_bus", &at[i])?,
                p_nom: p_nom[i],
                max_hours: hours[i],
                efficiency_store: store[i],
                efficiency_dispatch: dispatch[i],
                cyclic: cyclic[i] != 0.0,
                ..Default::default()
            });
        }
    }

    // Time series.
    if let Some(ts) = series(
        &nc,
        "generators_t_p_max_pu",
        &gen_names,
        n_snap,
        &vec![1.0; n_gen],
    ) {
        net.gen_availability = ts;
    } else if nc.has("generators_t_p_max_pu") {
        notes.push(
            "generators_t_p_max_pu has a shape that does not match the snapshots \
             and was not applied"
                .into(),
        );
    }
    let defaults: Vec<f64> = net.loads.iter().map(|l| l.p_set).collect();
    if let Some(ts) = series(&nc, "loads_t_p_set", &load_names, n_snap, &defaults) {
        net.load_profile = ts;
    }

    notes.push(format!(
        "PyPSA netCDF: {n_bus} buses, {n_gen} generators, {n_line} lines, \
         {n_link} links, {n_store} storage units, {n_snap} snapshots"
    ));
    if assumed_pu > 0 {
        notes.push(format!(
            "{assumed_pu} lines had no nominal voltage, so their impedance was \
             taken as already per unit rather than in ohms"
        ));
    } else if n_line > 0 {
        notes.push(format!(
            "line impedances converted from ohms to per unit on baseMVA {base}"
        ));
    }
    if zero_reactance > 0 {
        notes.push(format!(
            "{zero_reactance} zero-reactance lines treated as transport links"
        ));
    }

    net.validate()?;
    Ok(Case {
        name: name.to_string(),
        network: net,
        notes,
    })
}

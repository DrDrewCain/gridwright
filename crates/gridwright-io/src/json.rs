//! JSON, in two dialects.
//!
//! **Native.** A [`Network`] serialised field for field. Lossless in both
//! directions, which none of the other formats are, so it is what to reach for
//! when a network has to survive a round trip: between a solver run and a
//! browser, between a job and its cache, or into a test fixture. It is also
//! the format the planned interface speaks, since a WebAssembly build cannot
//! open a directory of CSV files.
//!
//! **PowerModels.** The dialect PowerModels.jl and the wider Julia power
//! systems ecosystem publish, and the format most recent optimisation papers
//! ship their cases in. Reading it is what makes those cases usable here.
//!
//! # The per-unit trap
//!
//! PowerModels files usually carry `"per_unit": true`, meaning every power
//! quantity is divided by `baseMVA`. A 47.8 MW load is written `0.478`.
//! Loading such a file without the conversion gives a network whose demand is
//! a hundredth of the truth, which solves perfectly happily and answers a
//! question nobody asked. The flag is honoured, and reported.

use std::collections::BTreeMap;

use gridwright_net::{Generator, Line, Load, Network, Snapshots};

use crate::Case;

#[derive(Debug, thiserror::Error)]
pub enum JsonError {
    #[error("not valid JSON: {0}")]
    Syntax(#[from] serde_json::Error),
    #[error("no `{0}` object; this does not look like a PowerModels case")]
    MissingSection(&'static str),
    #[error("{section} `{key}` has no `{field}`")]
    MissingField {
        section: &'static str,
        key: String,
        field: &'static str,
    },
    #[error("{section} `{key}` references bus {bus}, which no bus object defines")]
    UnknownBus {
        section: &'static str,
        key: String,
        bus: i64,
    },
    #[error("network is not valid: {0}")]
    Invalid(#[from] gridwright_net::NetError),
}

/// Serialise a network losslessly.
pub fn to_string(net: &Network) -> Result<String, JsonError> {
    Ok(serde_json::to_string_pretty(net)?)
}

/// Read a network written by [`to_string`].
pub fn from_str(text: &str) -> Result<Network, JsonError> {
    let net: Network = serde_json::from_str(text)?;
    net.validate()?;
    Ok(net)
}

/// Write a network to a `.json` file.
pub fn write_network(net: &Network, path: impl AsRef<std::path::Path>) -> Result<(), crate::IoError> {
    let path = path.as_ref();
    let text = to_string(net).map_err(crate::IoError::Json)?;
    std::fs::write(path, text).map_err(|source| crate::IoError::Read {
        path: path.display().to_string(),
        source,
    })
}

/// Read a native network file.
pub fn load_network(path: impl AsRef<std::path::Path>) -> Result<Network, crate::IoError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|source| crate::IoError::Read {
        path: path.display().to_string(),
        source,
    })?;
    from_str(&text).map_err(crate::IoError::Json)
}

// --- PowerModels ---

type Obj = serde_json::Map<String, serde_json::Value>;

fn num(o: &Obj, key: &str) -> Option<f64> {
    o.get(key)?.as_f64()
}

fn section<'a>(
    root: &'a Obj,
    name: &'static str,
) -> Result<BTreeMap<String, &'a Obj>, JsonError> {
    let Some(v) = root.get(name) else {
        return Ok(BTreeMap::new());
    };
    let Some(map) = v.as_object() else {
        return Ok(BTreeMap::new());
    };
    // Keys are numeric strings. Sorting them as numbers rather than as text
    // keeps component order stable and matches the file's own indexing, which
    // matters when a caller correlates our indices back to the source.
    let mut out = BTreeMap::new();
    for (k, v) in map {
        if let Some(o) = v.as_object() {
            let sortable = k
                .parse::<i64>()
                .map(|n| format!("{n:020}"))
                .unwrap_or_else(|_| k.clone());
            out.insert(sortable, o);
        }
    }
    Ok(out)
}

/// Parse a PowerModels JSON case.
pub fn parse_powermodels(text: &str, name: impl Into<String>) -> Result<Case, JsonError> {
    let root: serde_json::Value = serde_json::from_str(text)?;
    let root = root
        .as_object()
        .ok_or(JsonError::MissingSection("bus"))?;

    let base_mva = num(root, "baseMVA").filter(|v| *v > 0.0).unwrap_or(100.0);
    // Absent means false in the PowerModels reader, but every case they
    // publish sets it true, so an absent flag on a file with tiny loads is
    // worth reporting rather than guessing at.
    let per_unit = root
        .get("per_unit")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let scale = if per_unit { base_mva } else { 1.0 };

    let buses = section(root, "bus")?;
    if buses.is_empty() {
        return Err(JsonError::MissingSection("bus"));
    }

    let mut net = Network::new(Snapshots::hourly(1));
    net.base_mva = base_mva;
    let mut notes = Vec::new();
    let mut index_of = std::collections::HashMap::new();

    for (key, b) in &buses {
        let id = num(b, "bus_i")
            .or_else(|| num(b, "index"))
            .ok_or_else(|| JsonError::MissingField {
                section: "bus",
                key: key.clone(),
                field: "bus_i",
            })? as i64;
        // Type 4 is out of service.
        if num(b, "bus_type").unwrap_or(1.0) as i64 == 4 {
            continue;
        }
        let area = num(b, "area").map(|a| format!("area{}", a as i64));
        let idx = net.add_bus(
            b.get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| format!("bus{id}")),
            area.unwrap_or_else(|| "??".to_string()),
        );
        if let (Some(hi), Some(lo)) = (num(b, "vmax"), num(b, "vmin"))
            && hi > lo
            && lo > 0.0
        {
            net.buses[idx].v_max = hi;
            net.buses[idx].v_min = lo;
        }
        index_of.insert(id, idx);
    }

    let bus_of = |section: &'static str, key: &String, o: &Obj, field: &'static str| {
        let id = num(o, field).ok_or_else(|| JsonError::MissingField {
            section,
            key: key.clone(),
            field,
        })? as i64;
        index_of
            .get(&id)
            .copied()
            .ok_or_else(|| JsonError::UnknownBus {
                section,
                key: key.clone(),
                bus: id,
            })
    };

    let mut skipped = 0;
    for (key, l) in &section(root, "load")? {
        if num(l, "status").unwrap_or(1.0) as i64 == 0 {
            skipped += 1;
            continue;
        }
        let Ok(bus) = bus_of("load", key, l, "load_bus") else {
            skipped += 1;
            continue;
        };
        net.add_load(Load {
            name: format!("load{}", key.trim_start_matches('0')),
            bus,
            p_set: num(l, "pd").unwrap_or(0.0) * scale,
            q_set: num(l, "qd").unwrap_or(0.0) * scale,
        });
    }

    let mut quadratic = 0;
    for (key, g) in &section(root, "gen")? {
        if num(g, "gen_status").unwrap_or(1.0) as i64 == 0 {
            skipped += 1;
            continue;
        }
        let Ok(bus) = bus_of("gen", key, g, "gen_bus") else {
            skipped += 1;
            continue;
        };
        let pmax = num(g, "pmax").unwrap_or(0.0) * scale;
        let pmin = num(g, "pmin").unwrap_or(0.0) * scale;

        // Cost is a polynomial, highest order first, so the linear term is the
        // second from last. A quadratic term has nowhere to go in a linear
        // program and is reported rather than dropped in silence.
        let mut marginal = 0.0;
        if let Some(c) = g.get("cost").and_then(|v| v.as_array()) {
            let coeffs: Vec<f64> = c.iter().filter_map(|v| v.as_f64()).collect();
            if coeffs.len() >= 2 {
                if coeffs.len() > 2 {
                    quadratic += 1;
                }
                marginal = coeffs[coeffs.len() - 2];
                // Costs are per MWh of the per-unit quantity, so a per-unit
                // file states them per unit of power too.
                if per_unit {
                    marginal /= scale;
                }
            }
        }

        net.add_generator(Generator {
            name: format!("gen{}", key.trim_start_matches('0')),
            bus,
            p_nom: pmax,
            marginal_cost: marginal,
            p_min_pu: if pmax > 0.0 {
                (pmin / pmax).clamp(0.0, 1.0)
            } else {
                0.0
            },
            q_min: num(g, "qmin").unwrap_or(f64::NEG_INFINITY) * scale,
            q_max: num(g, "qmax").unwrap_or(f64::INFINITY) * scale,
            ..Default::default()
        });
    }

    let mut zero_reactance = 0;
    for (key, b) in &section(root, "branch")? {
        if num(b, "br_status").unwrap_or(1.0) as i64 == 0 {
            skipped += 1;
            continue;
        }
        let (Ok(bus0), Ok(bus1)) = (
            bus_of("branch", key, b, "f_bus"),
            bus_of("branch", key, b, "t_bus"),
        ) else {
            skipped += 1;
            continue;
        };
        if bus0 == bus1 {
            skipped += 1;
            continue;
        }
        let x = num(b, "br_x").unwrap_or(0.0);
        let susceptance = if x.abs() > 1e-9 {
            1.0 / x
        } else {
            zero_reactance += 1;
            0.0
        };
        // Charging susceptance is split across the two ends in PowerModels,
        // where MATPOWER keeps one total. Adding them recovers the total.
        let b_total = num(b, "b_fr").unwrap_or(0.0) + num(b, "b_to").unwrap_or(0.0);
        let rate = num(b, "rate_a").unwrap_or(0.0) * scale;
        let tap = num(b, "tap").filter(|t| *t > 0.0).unwrap_or(1.0);
        net.add_line(Line {
            name: format!("branch{}", key.trim_start_matches('0')),
            bus0,
            bus1,
            s_nom: if rate > 0.0 { rate } else { 1e6 },
            susceptance,
            resistance: num(b, "br_r").unwrap_or(0.0),
            reactance: x,
            shunt_susceptance: b_total,
            tap_ratio: tap,
            ..Default::default()
        });
    }

    // DC lines are corridors: no angle relationship, a rating in each
    // direction. PowerModels states those separately, and the tighter of the
    // two is what the corridor can actually carry symmetrically.
    for (key, d) in &section(root, "dcline")? {
        if num(d, "br_status").unwrap_or(1.0) as i64 == 0 {
            continue;
        }
        let (Ok(bus0), Ok(bus1)) = (
            bus_of("dcline", key, d, "f_bus"),
            bus_of("dcline", key, d, "t_bus"),
        ) else {
            skipped += 1;
            continue;
        };
        let pmax = num(d, "pmaxf")
            .unwrap_or(0.0)
            .abs()
            .max(num(d, "pmaxt").unwrap_or(0.0).abs())
            * scale;
        net.add_line(Line {
            name: format!("dcline{}", key.trim_start_matches('0')),
            bus0,
            bus1,
            s_nom: if pmax > 0.0 { pmax } else { 1e6 },
            susceptance: 0.0,
            ..Default::default()
        });
    }

    if per_unit {
        notes.push(format!(
            "per-unit case, every power quantity multiplied by baseMVA {base_mva}"
        ));
    } else {
        notes.push(format!(
            "case declares no per-unit flag, values taken as MW; baseMVA {base_mva}"
        ));
    }
    if skipped > 0 {
        notes.push(format!("{skipped} out-of-service or dangling components skipped"));
    }
    if zero_reactance > 0 {
        notes.push(format!(
            "{zero_reactance} zero-reactance branches treated as transport links"
        ));
    }
    if quadratic > 0 {
        notes.push(format!(
            "{quadratic} generators had quadratic costs, approximated by their linear term"
        ));
    }

    net.validate()?;
    Ok(Case {
        name: name.into(),
        network: net,
        notes,
    })
}

/// Read a PowerModels JSON case from a path.
pub fn load_powermodels(path: impl AsRef<std::path::Path>) -> Result<Case, crate::IoError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|source| crate::IoError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "case".into());
    parse_powermodels(&text, name).map_err(crate::IoError::Json)
}

/// Whether a JSON document looks like PowerModels rather than a native network.
///
/// A native file has a `buses` array; a PowerModels file has a `bus` object
/// keyed by number. Checking both directions rather than one means a file that
/// is neither is reported as neither.
pub fn looks_like_powermodels(text: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    let Some(o) = v.as_object() else {
        return false;
    };
    o.get("bus").and_then(|b| b.as_object()).is_some() && o.get("buses").is_none()
}

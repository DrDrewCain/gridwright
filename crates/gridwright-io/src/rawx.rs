//! PSS/E RAWX, the JSON reformulation introduced with version 35.
//!
//! Same data model as [`crate::psse`], entirely different encoding. Where RAW
//! is positional — a record's meaning depends on which column a value sits in,
//! and on which revision wrote it — RAWX names every field. That removes the
//! whole class of problem the RAW reader spends most of its length on: there
//! are no version-dependent column offsets here because there are no column
//! offsets.
//!
//! # Shape
//!
//! Each section is an object with `fields`, naming the columns, and `data`, an
//! array of rows in that order:
//!
//! ```json
//! {"network": {
//!   "bus": {"fields": ["ibus", "name", "baskv", "ide"],
//!           "data": [[1, "NORTH", 400.0, 3]]},
//!   "load": {"fields": ["ibus", "loadid", "stat", "pl", "ql"],
//!            "data": [[1, "1", 1, 120.0, 40.0]]}
//! }}
//! ```
//!
//! So a reader looks up each field by name and tolerates any order, any
//! omission, and any additional column a later revision adds. That is the point
//! of the format and it is why this file is a fraction of the length of the one
//! that reads RAW.

use std::collections::HashMap;

use gridwright_net::{Generator, Line, Load, Network, Snapshots};

use crate::Case;

#[derive(Debug, thiserror::Error)]
pub enum RawxError {
    #[error("not valid JSON: {0}")]
    Syntax(#[from] serde_json::Error),
    #[error("no `network` object; this does not look like a RAWX case")]
    NotRawx,
    #[error("section `{section}` has no bus data")]
    Empty { section: &'static str },
    #[error("{section} references bus {bus}, which no bus record defines")]
    UnknownBus { section: &'static str, bus: i64 },
    #[error("network is not valid: {0}")]
    Invalid(#[from] gridwright_net::NetError),
}

/// One section: its column names, and its rows.
struct Section<'a> {
    index: HashMap<String, usize>,
    rows: Vec<&'a Vec<serde_json::Value>>,
}

impl<'a> Section<'a> {
    fn read(root: &'a serde_json::Map<String, serde_json::Value>, name: &str) -> Option<Self> {
        let obj = root.get(name)?.as_object()?;
        let index = obj
            .get("fields")?
            .as_array()?
            .iter()
            .enumerate()
            .filter_map(|(i, v)| Some((v.as_str()?.to_ascii_lowercase(), i)))
            .collect();
        let rows = obj
            .get("data")?
            .as_array()?
            .iter()
            .filter_map(|r| r.as_array())
            .collect();
        Some(Self { index, rows })
    }

    fn num(&self, row: &[serde_json::Value], field: &str) -> Option<f64> {
        let v = row.get(*self.index.get(field)?)?;
        // A numeric field may arrive as a JSON number or, from some exporters,
        // as a string holding one.
        v.as_f64().or_else(|| v.as_str()?.trim().parse().ok())
    }

    fn text(&self, row: &[serde_json::Value], field: &str) -> Option<String> {
        let v = row.get(*self.index.get(field)?)?;
        v.as_str()
            .map(|s| s.trim().to_string())
            .or_else(|| v.as_f64().map(|n| n.to_string()))
    }
}

/// Parse a RAWX case.
pub fn parse_rawx(text: &str, name: impl Into<String>) -> Result<Case, RawxError> {
    let doc: serde_json::Value = serde_json::from_str(text)?;
    let root = doc
        .get("network")
        .and_then(|v| v.as_object())
        .ok_or(RawxError::NotRawx)?;

    let mut notes = Vec::new();
    let base = Section::read(root, "caseid")
        .and_then(|s| s.rows.first().and_then(|r| s.num(r, "sbase")))
        .filter(|v| *v > 0.0)
        .unwrap_or(100.0);

    let buses = Section::read(root, "bus").ok_or(RawxError::Empty { section: "bus" })?;
    if buses.rows.is_empty() {
        return Err(RawxError::Empty { section: "bus" });
    }

    let mut net = Network::new(Snapshots::hourly(1));
    net.base_mva = base;
    let mut index_of: HashMap<i64, usize> = HashMap::new();
    let mut skipped = 0;

    for row in &buses.rows {
        let id = buses.num(row, "ibus").unwrap_or(0.0) as i64;
        // Type 4 is disconnected, exactly as in RAW.
        if buses.num(row, "ide").unwrap_or(1.0) as i64 == 4 {
            skipped += 1;
            continue;
        }
        let name = buses
            .text(row, "name")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("bus{id}"));
        let area = buses.num(row, "area").unwrap_or(1.0) as i64;
        let idx = net.add_bus(name, format!("area{area}"));
        net.buses[idx].v_nom = buses.num(row, "baskv").unwrap_or(0.0);
        let (hi, lo) = (buses.num(row, "nvhi"), buses.num(row, "nvlo"));
        if let (Some(hi), Some(lo)) = (hi, lo)
            && hi > lo
            && lo > 0.0
        {
            net.buses[idx].v_max = hi;
            net.buses[idx].v_min = lo;
        }
        index_of.insert(id, idx);
    }

    // Loads, summed per bus as in RAW: several records may sit on one node.
    let mut demand: HashMap<usize, (f64, f64)> = HashMap::new();
    if let Some(loads) = Section::read(root, "load") {
        for row in &loads.rows {
            if loads.num(row, "stat").unwrap_or(1.0) as i64 == 0 {
                skipped += 1;
                continue;
            }
            let id = loads.num(row, "ibus").unwrap_or(0.0) as i64;
            let Some(&bus) = index_of.get(&id) else {
                skipped += 1;
                continue;
            };
            let e = demand.entry(bus).or_insert((0.0, 0.0));
            e.0 += loads.num(row, "pl").unwrap_or(0.0);
            e.1 += loads.num(row, "ql").unwrap_or(0.0);
        }
    }
    let mut at: Vec<(usize, (f64, f64))> = demand.into_iter().collect();
    at.sort_by_key(|(b, _)| *b);
    for (bus, (p, q)) in at {
        if p.abs() < 1e-12 && q.abs() < 1e-12 {
            continue;
        }
        net.add_load(Load {
            name: format!("load_{}", net.buses[bus].name),
            bus,
            p_set: p,
            q_set: q,
            ..Default::default()
        });
    }

    if let Some(shunts) = Section::read(root, "fixshunt") {
        for row in &shunts.rows {
            let id = shunts.num(row, "ibus").unwrap_or(0.0) as i64;
            if let Some(&bus) = index_of.get(&id) {
                net.buses[bus].g_shunt += shunts.num(row, "gl").unwrap_or(0.0) / base;
                net.buses[bus].b_shunt += shunts.num(row, "bl").unwrap_or(0.0) / base;
            }
        }
    }

    if let Some(gens) = Section::read(root, "generator") {
        for row in &gens.rows {
            if gens.num(row, "stat").unwrap_or(1.0) as i64 == 0 {
                skipped += 1;
                continue;
            }
            let id = gens.num(row, "ibus").unwrap_or(0.0) as i64;
            let Some(&bus) = index_of.get(&id) else {
                skipped += 1;
                continue;
            };
            let pt = gens.num(row, "pt").unwrap_or(0.0);
            let pb = gens.num(row, "pb").unwrap_or(0.0);
            let tag = gens.text(row, "machid").unwrap_or_default();
            net.add_generator(Generator {
                name: if tag.is_empty() {
                    format!("gen{id}")
                } else {
                    format!("gen{id}_{tag}")
                },
                bus,
                p_nom: pt,
                p_min_pu: if pt > 0.0 { (pb / pt).clamp(0.0, 1.0) } else { 0.0 },
                q_min: gens.num(row, "qb").unwrap_or(f64::NEG_INFINITY),
                q_max: gens.num(row, "qt").unwrap_or(f64::INFINITY),
                ..Default::default()
            });
        }
    }

    let mut zero_reactance = 0;
    let mut push = |net: &mut Network, name: String, bus0, bus1, r: f64, x: f64, b: f64,
                    rate: f64, tap: f64, shift: f64| {
        let susceptance = if x.abs() > 1e-9 {
            1.0 / x
        } else {
            zero_reactance += 1;
            0.0
        };
        net.add_line(Line {
            name,
            bus0,
            bus1,
            s_nom: if rate > 0.0 { rate } else { 1e6 },
            susceptance,
            resistance: r,
            reactance: x,
            shunt_susceptance: b,
            tap_ratio: if tap > 0.0 { tap } else { 1.0 },
            phase_shift: shift.to_radians(),
            ..Default::default()
        });
    };

    if let Some(branches) = Section::read(root, "acline") {
        for row in &branches.rows {
            if branches.num(row, "stat").unwrap_or(1.0) as i64 == 0 {
                skipped += 1;
                continue;
            }
            let (i, j) = (
                branches.num(row, "ibus").unwrap_or(0.0).abs() as i64,
                branches.num(row, "jbus").unwrap_or(0.0).abs() as i64,
            );
            let (Some(&bus0), Some(&bus1)) = (index_of.get(&i), index_of.get(&j)) else {
                skipped += 1;
                continue;
            };
            if bus0 == bus1 {
                skipped += 1;
                continue;
            }
            let ckt = branches.text(row, "ckt").unwrap_or_default();
            push(
                &mut net,
                format!("{i}-{j}-{ckt}"),
                bus0,
                bus1,
                branches.num(row, "rpu").unwrap_or(0.0),
                branches.num(row, "xpu").unwrap_or(0.0),
                branches.num(row, "bpu").unwrap_or(0.0),
                branches.num(row, "rate1").unwrap_or(0.0),
                1.0,
                0.0,
            );
        }
    }

    // Two-winding transformers. RAWX flattens the four-line RAW record into one
    // row, which is the single largest simplification the format brings.
    if let Some(xf) = Section::read(root, "transformer") {
        for row in &xf.rows {
            if xf.num(row, "stat").unwrap_or(1.0) as i64 == 0 {
                skipped += 1;
                continue;
            }
            if xf.num(row, "kbus").unwrap_or(0.0) as i64 != 0 {
                // Three-winding, which needs a star point. Reported rather than
                // mangled into a two-terminal branch.
                notes.push(
                    "a three-winding transformer was skipped; RAWX support covers \
                     two-winding units"
                        .into(),
                );
                continue;
            }
            let (i, j) = (
                xf.num(row, "ibus").unwrap_or(0.0).abs() as i64,
                xf.num(row, "jbus").unwrap_or(0.0).abs() as i64,
            );
            let (Some(&bus0), Some(&bus1)) = (index_of.get(&i), index_of.get(&j)) else {
                skipped += 1;
                continue;
            };
            let (r, x) = (
                xf.num(row, "r1_2").unwrap_or(0.0),
                xf.num(row, "x1_2").unwrap_or(0.0),
            );
            // Impedance may be given on the winding's own base, exactly as in
            // RAW, and the code saying so is the same one.
            let (r, x) = if xf.num(row, "cz").unwrap_or(1.0) as i64 == 2 {
                let sbase = xf.num(row, "sbase1_2").unwrap_or(base);
                if sbase > 0.0 {
                    (r * base / sbase, x * base / sbase)
                } else {
                    (r, x)
                }
            } else {
                (r, x)
            };
            let w1 = xf.num(row, "windv1").unwrap_or(1.0);
            let w2 = xf.num(row, "windv2").unwrap_or(1.0);
            let tap = if xf.num(row, "cw").unwrap_or(1.0) as i64 == 2 {
                let (a, b) = (net.buses[bus0].v_nom, net.buses[bus1].v_nom);
                if a > 0.0 && b > 0.0 {
                    (w1 / a) / (w2 / b)
                } else {
                    1.0
                }
            } else {
                w1 / w2.max(1e-12)
            };
            let name = xf
                .text(row, "name")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("xfmr{i}-{j}"));
            push(
                &mut net,
                name,
                bus0,
                bus1,
                r,
                x,
                0.0,
                xf.num(row, "rate1_1").unwrap_or(0.0),
                tap,
                xf.num(row, "ang1").unwrap_or(0.0),
            );
        }
    }

    notes.push(format!("PSS/E RAWX, baseMVA {base}"));
    if skipped > 0 {
        notes.push(format!(
            "{skipped} out-of-service or dangling components skipped"
        ));
    }
    if zero_reactance > 0 {
        notes.push(format!(
            "{zero_reactance} zero-reactance branches treated as transport links"
        ));
    }
    notes.push(
        "RAWX carries no generator costs; every marginal cost is zero until one is supplied"
            .into(),
    );

    net.validate()?;
    Ok(Case {
        name: name.into(),
        network: net,
        notes,
    })
}

/// Whether a JSON document is RAWX rather than one of the other dialects.
pub fn looks_like_rawx(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| Some(v.get("network")?.get("bus").is_some()))
        .unwrap_or(false)
}

/// Read a RAWX case from a path.
pub fn load_rawx(path: impl AsRef<std::path::Path>) -> Result<Case, crate::IoError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|source| crate::IoError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "case".into());
    parse_rawx(&text, name).map_err(crate::IoError::Rawx)
}

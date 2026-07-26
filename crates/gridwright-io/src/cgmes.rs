//! CGMES and CIM, the format European TSOs actually exchange grids in.
//!
//! ENTSO-E's Common Grid Model Exchange Standard is how the European
//! transmission system is described between the people who operate it. Every
//! TSO publishes its model this way, and the pan-European model is assembled
//! from them. It is RDF/XML rather than a table, and it is the least
//! convenient format here by a wide margin, which is precisely why a tool that
//! reads it can be pointed at data no spreadsheet reader will ever see.
//!
//! # Why it is shaped like this
//!
//! CIM is an object model serialised as RDF, so nothing is where a table
//! would put it. Equipment does not name the node it sits on. Instead:
//!
//! ```text
//!   ACLineSegment  <--  Terminal  -->  ConnectivityNode  (or TopologicalNode)
//! ```
//!
//! A line has two `Terminal` objects, each pointing at the equipment and at a
//! node, and the line's endpoints are recovered by finding both. A transformer
//! has one terminal per winding and its impedance lives on separate
//! `PowerTransformerEnd` objects. Nothing can be read in one pass, so the
//! whole document is indexed first and assembled afterwards.
//!
//! # Profiles
//!
//! A CGMES model is several files: equipment (EQ), topology (TP), steady state
//! hypothesis (SSH), state variables (SV). They cross-reference by identifier,
//! and no single one of them is a network. Point this at a directory and every
//! `.xml` in it is merged before anything is assembled, which is what
//! unzipping a published model and pointing at the folder gives you.
//!
//! # Units
//!
//! CIM states impedance in ohms and voltage in kilovolts, like PyPSA and
//! unlike MATPOWER. The conversion needs the base voltage, which arrives
//! through `BaseVoltage` on the equipment or on its voltage level, and where
//! it cannot be found the line is reported rather than silently taken as per
//! unit.

use std::collections::HashMap;
use std::path::Path;

use gridwright_net::{Generator, Line, Load, Network, Snapshots};
use quick_xml::events::Event;

use crate::Case;

#[derive(Debug, thiserror::Error)]
pub enum CgmesError {
    #[error("{file}: {message}")]
    Xml { file: String, message: String },
    #[error("no connectivity or topological nodes found in {file}; this does not look like a CIM model")]
    NoNodes { file: String },
    #[error("network is not valid: {0}")]
    Invalid(#[from] gridwright_net::NetError),
}

/// One CIM object: its class, and every property read off it.
#[derive(Debug, Default, Clone)]
struct Object {
    class: String,
    /// Literal properties, by local name (`ACLineSegment.x` becomes `x`).
    values: HashMap<String, String>,
    /// Reference properties, pointing at another object's identifier.
    refs: HashMap<String, String>,
}

impl Object {
    fn num(&self, key: &str) -> Option<f64> {
        self.values.get(key)?.trim().parse().ok()
    }
    fn text(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
}

/// Strip a namespace prefix and the class part of a CIM property name.
///
/// `cim:ACLineSegment.x` is the property `x`. Keeping the class prefix would
/// mean spelling out every inheritance path, since the same property arrives
/// as `Conductor.length` on one class and `ACLineSegment.length` on another.
fn local(name: &str) -> &str {
    let after_ns = name.rsplit(':').next().unwrap_or(name);
    after_ns.rsplit('.').next().unwrap_or(after_ns)
}

/// Whether the steady-state hypothesis left this equipment in service.
///
/// The SSH profile is where a published model says what is actually running,
/// and it says so by adding `Equipment.inService` to objects the equipment
/// profile already defined. A reader that ignores it builds the network as
/// designed rather than as operated, which is a different network and usually a
/// more capable one.
///
/// Absent means in service, since a model with no SSH profile has not switched
/// anything off.
fn in_service(obj: &Object) -> bool {
    match obj.text("inService").or_else(|| obj.text("connected")) {
        Some(v) => !v.trim().eq_ignore_ascii_case("false"),
        None => true,
    }
}

fn class_of(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// An identifier as written in an `rdf:resource`, with the leading marker gone.
///
/// References are written `#_abc123`, sometimes with a full URL in front of
/// the fragment, and identifiers as `_abc123`. Normalising both to the same
/// string is what lets them be looked up against each other.
fn id(raw: &str) -> String {
    let s = raw.rsplit('#').next().unwrap_or(raw);
    s.trim().to_string()
}

/// Read every object out of one RDF/XML document.
fn parse(text: &str, file: &str, into: &mut HashMap<String, Object>) -> Result<(), CgmesError> {
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().trim_text(true);

    // The object currently open, and the property inside it if any.
    let mut current: Option<(String, Object)> = None;
    let mut property: Option<String> = None;
    let mut buf = String::new();

    loop {
        match reader.read_event() {
            Err(e) => {
                return Err(CgmesError::Xml {
                    file: file.into(),
                    message: e.to_string(),
                });
            }
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let raw = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let mut about: Option<String> = None;
                let mut resource: Option<String> = None;
                for attr in e.attributes().flatten() {
                    let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                    let value = String::from_utf8_lossy(&attr.value).to_string();
                    match local(&key) {
                        "ID" | "about" => about = Some(id(&value)),
                        "resource" => resource = Some(id(&value)),
                        _ => {}
                    }
                }

                if current.is_none() {
                    // A top-level element with an identifier opens an object.
                    if let Some(key) = about {
                        // `rdf:about` on a later profile updates an object the
                        // equipment profile already defined, so an existing
                        // entry is extended rather than replaced.
                        let existing = into.remove(&key).unwrap_or_default();
                        let mut obj = existing;
                        if obj.class.is_empty() {
                            obj.class = class_of(&raw).to_string();
                        }
                        current = Some((key, obj));
                    }
                    continue;
                }

                // Inside an object: either a reference or a literal.
                if let Some(target) = resource {
                    if let Some((_, obj)) = current.as_mut() {
                        obj.refs.insert(local(&raw).to_string(), target);
                    }
                } else {
                    property = Some(local(&raw).to_string());
                    buf.clear();
                }
            }
            Ok(Event::Text(t)) => {
                if property.is_some() {
                    buf.push_str(&t.decode().unwrap_or_default());
                }
            }
            Ok(Event::End(e)) => {
                let raw = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if let Some(p) = property.take() {
                    if p == local(&raw) {
                        if let Some((_, obj)) = current.as_mut()
                            && !buf.trim().is_empty()
                        {
                            obj.values.insert(p, buf.trim().to_string());
                        }
                        buf.clear();
                        continue;
                    }
                    // Not the property's own close tag, so it closed the object.
                    property = None;
                }
                if let Some((key, obj)) = current.take() {
                    if class_of(&raw) == obj.class {
                        into.insert(key, obj);
                    } else {
                        current = Some((key, obj));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Everything indexed, ready to assemble.
struct Model {
    objects: HashMap<String, Object>,
    /// Equipment identifier to the nodes its terminals reach, in terminal order.
    nodes_of: HashMap<String, Vec<String>>,
    /// Terminal identifier to the node it reaches, for the components that
    /// name their terminals directly rather than relying on order.
    terminal_node: HashMap<String, String>,
}

impl Model {
    fn build(objects: HashMap<String, Object>) -> Self {
        // A terminal is the only thing that knows which node a piece of
        // equipment sits on, and it points at both. Topological nodes are
        // preferred where a topology profile supplied them, since that is the
        // bus-branch view the optimisation wants; connectivity nodes are the
        // node-breaker view and are used when nothing better is present.
        //
        // Ordered by the terminal's sequence number, falling back to its
        // identifier. Not a tidiness measure: which end of a line is `bus0`
        // decides the sign of every flow reported on it, and taking it from
        // hash iteration order would give a different answer on each run of
        // the same file.
        let mut collected: HashMap<String, Vec<(i64, String, String)>> = HashMap::new();
        for (key, t) in objects.iter().filter(|(_, o)| o.class == "Terminal") {
            let Some(equipment) = t.refs.get("ConductingEquipment") else {
                continue;
            };
            let node = t
                .refs
                .get("TopologicalNode")
                .or_else(|| t.refs.get("ConnectivityNode"));
            if let Some(node) = node {
                let seq = t.num("sequenceNumber").unwrap_or(f64::MAX) as i64;
                collected
                    .entry(equipment.clone())
                    .or_default()
                    .push((seq, key.clone(), node.clone()));
            }
        }
        let mut nodes_of: HashMap<String, Vec<String>> = HashMap::new();
        let mut terminal_node: HashMap<String, String> = HashMap::new();
        for (equipment, mut list) in collected {
            list.sort();
            for (_, terminal, node) in &list {
                terminal_node.insert(terminal.clone(), node.clone());
            }
            nodes_of.insert(equipment, list.into_iter().map(|(_, _, n)| n).collect());
        }
        Self {
            objects,
            nodes_of,
            terminal_node,
        }
    }

    fn get(&self, key: &str) -> Option<&Object> {
        self.objects.get(key)
    }

    /// Nominal voltage in kV for a piece of equipment.
    ///
    /// Reached either directly through `BaseVoltage` or through the container
    /// it sits in, since CIM allows both and published models use both.
    fn kv(&self, obj: &Object) -> Option<f64> {
        if let Some(bv) = obj.refs.get("BaseVoltage")
            && let Some(v) = self.get(bv).and_then(|o| o.num("nominalVoltage"))
        {
            return Some(v);
        }
        let container = obj.refs.get("EquipmentContainer")?;
        let level = self.get(container)?;
        let bv = level.refs.get("BaseVoltage")?;
        self.get(bv)?.num("nominalVoltage")
    }

    fn name_of(&self, key: &str, obj: &Object, fallback: &str) -> String {
        obj.text("name")
            .map(str::to_string)
            .unwrap_or_else(|| format!("{fallback}{key}"))
    }
}

/// Parse a CIM model from one or more RDF/XML documents.
pub fn parse_model(
    documents: &[(String, String)],
    name: impl Into<String>,
) -> Result<Case, CgmesError> {
    let mut objects = HashMap::new();
    for (file, text) in documents {
        parse(text, file, &mut objects)?;
    }
    let label = documents
        .first()
        .map(|(f, _)| f.clone())
        .unwrap_or_default();
    let model = Model::build(objects);
    let mut notes = Vec::new();

    // Nodes. A topology profile gives topological nodes, which are buses; with
    // only an equipment profile the connectivity nodes stand in for them.
    let mut node_index: HashMap<String, usize> = HashMap::new();
    let mut net = Network::new(Snapshots::hourly(1));
    let topological: Vec<(&String, &Object)> = model
        .objects
        .iter()
        .filter(|(_, o)| o.class == "TopologicalNode")
        .collect();
    let use_topological = !topological.is_empty();
    let mut nodes: Vec<(String, &Object)> = if use_topological {
        topological
            .into_iter()
            .map(|(k, o)| (k.clone(), o))
            .collect()
    } else {
        model
            .objects
            .iter()
            .filter(|(_, o)| o.class == "ConnectivityNode")
            .map(|(k, o)| (k.clone(), o))
            .collect()
    };
    if nodes.is_empty() {
        return Err(CgmesError::NoNodes { file: label });
    }
    // Identifier order, so a model reads the same way twice.
    nodes.sort_by(|a, b| a.0.cmp(&b.0));

    for (key, obj) in &nodes {
        // The subregion a node's container belongs to is the closest thing CIM
        // has to a country, and in a pan-European model it is what makes the
        // cross-border flows visible.
        let country = obj
            .refs
            .get("ConnectivityNodeContainer")
            .or_else(|| obj.refs.get("EquipmentContainer"))
            .and_then(|c| model.get(c))
            .and_then(|c| {
                c.refs
                    .get("Substation")
                    .or_else(|| c.refs.get("Region"))
                    .or_else(|| c.refs.get("SubGeographicalRegion"))
            })
            .and_then(|r| model.get(r))
            .and_then(|r| r.text("name"))
            .unwrap_or("??")
            .to_string();
        let idx = net.add_bus(model.name_of(key, obj, "node"), country);
        if let Some(v) = model.kv(obj) {
            net.buses[idx].v_nom = v;
        }
        node_index.insert(key.clone(), idx);
    }

    let ends = |equipment: &String| -> Option<(usize, usize)> {
        let list = model.nodes_of.get(equipment)?;
        let mut seen: Vec<usize> = Vec::new();
        for n in list {
            if let Some(i) = node_index.get(n)
                && !seen.contains(i)
            {
                seen.push(*i);
            }
        }
        if seen.len() >= 2 {
            Some((seen[0], seen[1]))
        } else {
            None
        }
    };

    let base = net.base_mva;
    let mut ordered: Vec<(&String, &Object)> = model.objects.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));

    // Ratings live on operational limits, one level of indirection away from
    // the equipment. A current limit needs the voltage to become a power.
    let mut limit_of: HashMap<String, f64> = HashMap::new();
    for (_, o) in &ordered {
        if o.class != "CurrentLimit" && o.class != "ActivePowerLimit" {
            continue;
        }
        let Some(value) = o.num("value") else { continue };
        let Some(set) = o.refs.get("OperationalLimitSet") else {
            continue;
        };
        let Some(terminal) = model.get(set).and_then(|s| s.refs.get("Terminal")) else {
            continue;
        };
        let Some(equipment) = model.get(terminal).and_then(|t| t.refs.get("ConductingEquipment"))
        else {
            continue;
        };
        let mva = if o.class == "ActivePowerLimit" {
            value
        } else {
            // Three-phase apparent power from a per-phase current limit.
            let kv = model.get(equipment).and_then(|e| model.kv(e)).unwrap_or(0.0);
            if kv <= 0.0 {
                continue;
            }
            3f64.sqrt() * kv * value / 1000.0
        };
        // The tightest limit is the one that binds.
        limit_of
            .entry(equipment.clone())
            .and_modify(|v| *v = v.min(mva))
            .or_insert(mva);
    }

    let mut no_voltage = 0;
    let mut dangling = 0;
    let mut out_of_service = 0;
    let mut zero_reactance = 0;

    for (key, obj) in &ordered {
        match obj.class.as_str() {
            "ACLineSegment" => {
                if !in_service(obj) {
                    out_of_service += 1;
                    continue;
                }
                let Some((bus0, bus1)) = ends(key) else {
                    dangling += 1;
                    continue;
                };
                if bus0 == bus1 {
                    dangling += 1;
                    continue;
                }
                let (r_ohm, x_ohm) = (obj.num("r").unwrap_or(0.0), obj.num("x").unwrap_or(0.0));
                let kv = model
                    .kv(obj)
                    .or_else(|| Some(net.buses[bus0].v_nom).filter(|v| *v > 0.0));
                let (r, x) = match kv {
                    Some(v) if v > 0.0 => {
                        let z = v * v / base;
                        (r_ohm / z, x_ohm / z)
                    }
                    _ => {
                        no_voltage += 1;
                        (r_ohm, x_ohm)
                    }
                };
                let susceptance = if x.abs() > 1e-12 {
                    1.0 / x
                } else {
                    zero_reactance += 1;
                    0.0
                };
                net.add_line(Line {
                    name: model.name_of(key, obj, "line"),
                    bus0,
                    bus1,
                    s_nom: limit_of.get(*key).copied().unwrap_or(1e6),
                    susceptance,
                    resistance: r,
                    reactance: x,
                    ..Default::default()
                });
            }
            "PowerTransformer" => {
                let Some((bus0, bus1)) = ends(key) else {
                    dangling += 1;
                    continue;
                };
                if bus0 == bus1 {
                    dangling += 1;
                    continue;
                }
                // Impedance sits on the ends, not on the transformer, and is
                // stated on whichever end's base the modeller chose. Summing
                // the ends after rebasing each is the general answer; almost
                // every published model puts it all on one end and zero on the
                // other, which this handles as the same arithmetic.
                // Ends are ordered by their end number, which is what pairs a
                // winding with the voltage level it faces. Where an end names
                // its own terminal — as a published model does — the bus is
                // taken from that rather than from position, so the join does
                // not depend on the order anything was listed in.
                let mut windings: Vec<(i64, &Object)> = ordered
                    .iter()
                    .filter(|(_, o)| {
                        o.class == "PowerTransformerEnd"
                            && o.refs.get("PowerTransformer") == Some(*key)
                    })
                    .map(|(_, o)| (o.num("endNumber").unwrap_or(f64::MAX) as i64, *o))
                    .collect();
                windings.sort_by_key(|(n, _)| *n);
                if windings.len() < 2 {
                    dangling += 1;
                    continue;
                }
                let mut r = 0.0;
                let mut x = 0.0;
                let mut rated_s: f64 = 0.0;
                let mut rated: Vec<f64> = Vec::new();
                let mut at: Vec<Option<usize>> = Vec::new();
                for (_, end) in &windings {
                    let kv = end.num("ratedU").unwrap_or(0.0);
                    rated.push(kv);
                    rated_s = rated_s.max(end.num("ratedS").unwrap_or(0.0));
                    if kv > 0.0 {
                        let z = kv * kv / base;
                        r += end.num("r").unwrap_or(0.0) / z;
                        x += end.num("x").unwrap_or(0.0) / z;
                    }
                    at.push(
                        end.refs
                            .get("Terminal")
                            .and_then(|t| model.terminal_node.get(t))
                            .and_then(|n| node_index.get(n))
                            .copied(),
                    );
                }
                let (bus0, bus1) = match (at[0], at[1]) {
                    (Some(a), Some(b)) if a != b => (a, b),
                    _ => (bus0, bus1),
                };
                // Tap ratio from each winding's rated voltage against the
                // nominal voltage of the bus it actually faces.
                let tap = {
                    let (a, b) = (net.buses[bus0].v_nom, net.buses[bus1].v_nom);
                    if a > 0.0 && b > 0.0 && rated[0] > 0.0 && rated[1] > 0.0 {
                        (rated[0] / a) / (rated[1] / b)
                    } else {
                        1.0
                    }
                };
                let susceptance = if x.abs() > 1e-12 {
                    1.0 / x
                } else {
                    zero_reactance += 1;
                    0.0
                };
                let rating = limit_of
                    .get(*key)
                    .copied()
                    .unwrap_or(if rated_s > 0.0 { rated_s } else { 1e6 });
                net.add_line(Line {
                    name: model.name_of(key, obj, "transformer"),
                    bus0,
                    bus1,
                    s_nom: rating,
                    susceptance,
                    resistance: r,
                    reactance: x,
                    tap_ratio: if tap.is_finite() && tap > 0.0 { tap } else { 1.0 },
                    ..Default::default()
                });
            }
            "SynchronousMachine" | "GeneratingUnit" | "ThermalGeneratingUnit"
            | "HydroGeneratingUnit" | "WindGeneratingUnit" | "NuclearGeneratingUnit"
            | "SolarGeneratingUnit" => {
                // Only the machine sits on a terminal; the generating unit
                // carries the operating limits. Skip the units and reach them
                // from the machine.
                if obj.class != "SynchronousMachine" {
                    continue;
                }
                if !in_service(obj) {
                    out_of_service += 1;
                    continue;
                }
                let Some(bus) = model
                    .nodes_of
                    .get(*key)
                    .and_then(|n| n.first())
                    .and_then(|n| node_index.get(n))
                    .copied()
                else {
                    dangling += 1;
                    continue;
                };
                let unit = obj.refs.get("GeneratingUnit").and_then(|u| model.get(u));
                let p_max = unit
                    .and_then(|u| u.num("maxOperatingP"))
                    .or_else(|| obj.num("ratedS"))
                    .unwrap_or(0.0);
                let p_min = unit.and_then(|u| u.num("minOperatingP")).unwrap_or(0.0);
                let carrier = unit
                    .map(|u| match u.class.as_str() {
                        "ThermalGeneratingUnit" => "thermal",
                        "HydroGeneratingUnit" => "hydro",
                        "WindGeneratingUnit" => "wind",
                        "NuclearGeneratingUnit" => "nuclear",
                        "SolarGeneratingUnit" => "solar",
                        _ => "unknown",
                    })
                    .unwrap_or("unknown");
                net.add_generator(Generator {
                    name: model.name_of(key, obj, "gen"),
                    bus,
                    p_nom: p_max,
                    carrier: carrier.into(),
                    p_min_pu: if p_max > 0.0 {
                        (p_min / p_max).clamp(0.0, 1.0)
                    } else {
                        0.0
                    },
                    q_min: obj.num("minQ").unwrap_or(f64::NEG_INFINITY),
                    q_max: obj.num("maxQ").unwrap_or(f64::INFINITY),
                    ..Default::default()
                });
            }
            "EnergyConsumer" | "ConformLoad" | "NonConformLoad" => {
                if !in_service(obj) {
                    out_of_service += 1;
                    continue;
                }
                let Some(bus) = model
                    .nodes_of
                    .get(*key)
                    .and_then(|n| n.first())
                    .and_then(|n| node_index.get(n))
                    .copied()
                else {
                    dangling += 1;
                    continue;
                };
                let p = obj.num("p").unwrap_or(0.0);
                let q = obj.num("q").unwrap_or(0.0);
                if p.abs() < 1e-12 && q.abs() < 1e-12 {
                    continue;
                }
                net.add_load(Load {
                    name: model.name_of(key, obj, "load"),
                    bus,
                    p_set: p,
                    q_set: q,
            ..Default::default()
                });
            }
            _ => {}
        }
    }

    notes.push(format!(
        "CIM/CGMES: {} nodes ({}), {} lines and transformers, {} generators, {} loads",
        net.buses.len(),
        if use_topological {
            "topological, bus-branch"
        } else {
            "connectivity, node-breaker"
        },
        net.lines.len(),
        net.generators.len(),
        net.loads.len()
    ));
    if no_voltage > 0 {
        notes.push(format!(
            "{no_voltage} lines had no base voltage, so their ohms were taken as \
             already per unit"
        ));
    }
    if out_of_service > 0 {
        notes.push(format!(
            "{out_of_service} pieces of equipment were switched off by the steady \
             state hypothesis and left out"
        ));
    }
    if dangling > 0 {
        notes.push(format!(
            "{dangling} pieces of equipment had too few terminals reaching a node \
             and were skipped"
        ));
    }
    if zero_reactance > 0 {
        notes.push(format!(
            "{zero_reactance} zero-reactance branches treated as transport links"
        ));
    }
    notes.push(
        "CIM carries no generation costs; every marginal cost is zero until one is supplied"
            .into(),
    );

    net.validate()?;
    Ok(Case {
        name: name.into(),
        network: net,
        notes,
    })
}

/// Read every XML document out of a zip archive.
///
/// The form a CGMES model is actually published in. ENTSO-E distributes each
/// profile as its own file inside one archive, and often nests an archive per
/// operator inside another, so this recurses one level: an archive holding
/// archives is exactly what a pan-European model looks like.
#[cfg(feature = "cgmes")]
pub fn documents_from_zip(bytes: Vec<u8>, label: &str) -> Result<Vec<(String, String)>, CgmesError> {
    fn read(bytes: Vec<u8>, label: &str, depth: usize, into: &mut Vec<(String, String)>) {
        use std::io::Read;
        let Ok(mut archive) = zip::ZipArchive::new(std::io::Cursor::new(bytes)) else {
            return;
        };
        // Sorted, so a model assembled from an archive is assembled the same
        // way twice. Zip ordering is whatever the writer chose.
        let mut names: Vec<String> = (0..archive.len())
            .filter_map(|i| archive.by_index(i).ok().map(|f| f.name().to_string()))
            .collect();
        names.sort();
        for name in names {
            let Ok(mut entry) = archive.by_name(&name) else {
                continue;
            };
            let lower = name.to_ascii_lowercase();
            if lower.ends_with(".xml") {
                let mut text = String::new();
                if entry.read_to_string(&mut text).is_ok() {
                    into.push((format!("{label}!{name}"), text));
                }
            } else if lower.ends_with(".zip") && depth < 2 {
                let mut inner = Vec::new();
                if entry.read_to_end(&mut inner).is_ok() {
                    read(inner, &format!("{label}!{name}"), depth + 1, into);
                }
            }
        }
    }

    let mut out = Vec::new();
    read(bytes, label, 0, &mut out);
    if out.is_empty() {
        return Err(CgmesError::NoNodes {
            file: label.to_string(),
        });
    }
    Ok(out)
}

/// Read a CIM model from a file, a zip archive, or a directory of profiles.
///
/// A published CGMES model is several XML documents that cross-reference each
/// other, usually inside one archive. Pointing at the archive or at the
/// unpacked directory both work; pointing at a single file only works when that
/// file is self-contained.
pub fn load_model(path: impl AsRef<Path>) -> Result<Case, crate::IoError> {
    let path = path.as_ref();
    let mut documents = Vec::new();

    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) {
        let bytes = std::fs::read(path).map_err(|source| crate::IoError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "model".into());
        let docs = documents_from_zip(bytes, &name).map_err(crate::IoError::Cgmes)?;
        return parse_model(&docs, name).map_err(crate::IoError::Cgmes);
    }

    let read_one = |p: &Path| -> Result<(String, String), crate::IoError> {
        let text = std::fs::read_to_string(p).map_err(|source| crate::IoError::Read {
            path: p.display().to_string(),
            source,
        })?;
        Ok((p.display().to_string(), text))
    };

    if path.is_dir() {
        let entries = std::fs::read_dir(path).map_err(|source| crate::IoError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let mut paths: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("xml"))
            })
            .collect();
        // Deterministic order, so a model assembled from several profiles is
        // assembled the same way every time.
        paths.sort();
        for p in paths {
            documents.push(read_one(&p)?);
        }
    } else {
        documents.push(read_one(path)?);
    }

    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "model".into());
    parse_model(&documents, name).map_err(crate::IoError::Cgmes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_property_name_loses_its_namespace_and_its_class() {
        assert_eq!(local("cim:ACLineSegment.x"), "x");
        assert_eq!(local("cim:IdentifiedObject.name"), "name");
        assert_eq!(local("rdf:resource"), "resource");
        assert_eq!(local("value"), "value");
    }

    #[test]
    fn identifiers_and_references_normalise_to_the_same_string() {
        // An object is `rdf:ID="_abc"` and a reference to it is
        // `rdf:resource="#_abc"`, sometimes with a whole URL in front. They
        // have to end up equal or nothing joins up.
        assert_eq!(id("#_abc"), "_abc");
        assert_eq!(id("_abc"), "_abc");
        assert_eq!(id("http://example.com/model#_abc"), "_abc");
    }
}

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
//!
//! # The solved state
//!
//! The state variables profile is the odd one out: it is not needed to build a
//! network and it is the most valuable thing in the archive. `SvVoltage`,
//! `SvPowerFlow`, `SvTapStep` and `SvShuntCompensatorSections` together are the
//! answer the operator's own tools produced for this model, published so that
//! whoever receives it can reproduce it.
//!
//! It is therefore deliberately kept out of the [`Network`]. Folding published
//! voltages and flows into the model would destroy the only thing they are good
//! for: an independent answer stops being independent the moment the solver is
//! handed it. It also would not fit — a `Network` states what a system *can*
//! do, and a solved state states what one *did*, which is a result and not an
//! input. So [`load_model_with_state`] returns a [`SolvedState`] beside the
//! `Case`, indexed to line up with the network component by component, and
//! [`load_model`] stays exactly as it was for every caller who only wants the
//! model.

use std::collections::HashMap;
use std::path::Path;

use gridwright_net::{Generator, Line, Load, Network, Snapshots};
use quick_xml::events::Event;

use crate::Case;

#[derive(Debug, thiserror::Error)]
pub enum CgmesError {
    #[error("{file}: {message}")]
    Xml { file: String, message: String },
    #[error(
        "no connectivity or topological nodes found in {file}; this does not look like a CIM model"
    )]
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
                collected.entry(equipment.clone()).or_default().push((
                    seq,
                    key.clone(),
                    node.clone(),
                ));
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

/// The power crossing one terminal, as the state variables profile states it.
///
/// CIM measures at the terminal and signs the flow *into* the equipment, away
/// from the node. A line whose sending end reads `+610` and whose receiving end
/// reads `-603` is carrying 610 MW away from one bus and delivering 603 MW to
/// the other, and the seven megawatts between them are the losses. The same
/// convention makes a machine that is generating read negative, because it is
/// the network's view of the terminal and not the machine's.
///
/// That sign is kept rather than flipped to match the engine's generator
/// convention. A validation input that has been quietly rearranged is no longer
/// the published answer, and the one property worth having here — that every
/// terminal at a node sums to zero — only holds in CIM's own signs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerminalFlow {
    /// Active power into the equipment, MW.
    pub p: f64,
    /// Reactive power into the equipment, MVAr.
    pub q: f64,
}

/// The voltage a solved state reports at one bus.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BusVoltage {
    /// Magnitude in kilovolts, exactly as published.
    pub v_kv: f64,
    /// Magnitude per unit on the bus's nominal voltage.
    ///
    /// Absent where the model never said what the bus's nominal voltage is,
    /// which is the same gap that stops ohms becoming per unit. Reporting the
    /// kilovolts as though they were per unit would give a bus sitting at 400
    /// per unit, so it is left unanswered instead.
    pub v_pu: Option<f64>,
    /// Angle in radians.
    ///
    /// CIM publishes degrees. Everything downstream of here — the DC flow
    /// constraint, the phase shift a MATPOWER branch carries, the angle
    /// variables the solver allocates — is in radians, so the conversion
    /// happens once, at the edge. Leaving it in degrees would not fail
    /// anywhere; it would make every angle difference wrong by a factor of 57
    /// and produce flows that look plausible.
    pub angle: f64,
}

/// The flows a solved state reports on one branch, one per end.
///
/// Each end is matched to the bus it was measured at rather than to the order
/// the profile happened to list its terminals in, so `end0` is always the flow
/// at the branch's `bus0`. Getting this from position would make the sign of
/// every reported flow depend on document order, which is the same failure the
/// transformer ends already have to avoid.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BranchFlow {
    /// At the `bus0` end of the line.
    pub end0: Option<TerminalFlow>,
    /// At the `bus1` end of the line.
    pub end1: Option<TerminalFlow>,
}

/// Where a tap changer was left standing.
#[derive(Debug, Clone, PartialEq)]
pub struct TapPosition {
    pub name: String,
    /// The branch in the network it drives, when it could be reached.
    ///
    /// The path is `SvTapStep` to `TapChanger` to `TransformerEnd` to
    /// `PowerTransformer`, and any of those links may be missing in a partial
    /// model, so the position is still reported when the branch is not.
    pub branch: Option<usize>,
    pub position: f64,
}

/// How much of a shunt compensator was switched in.
#[derive(Debug, Clone, PartialEq)]
pub struct ShuntSections {
    pub name: String,
    /// The bus it sits on, when its terminal reaches one.
    pub bus: Option<usize>,
    pub sections: f64,
}

/// A solved state, as published in a model's state variables profile.
///
/// Indexed to line up with the [`Case`] it was read beside: `voltages[i]`
/// belongs to `network.buses[i]`, `branches[i]` to `network.lines[i]`, and so
/// on. That is what makes it usable as a check — a caller can walk its own
/// solution and the operator's side by side without matching names.
///
/// An entry is `None` where the profile said nothing about that component. A
/// published SV profile is frequently partial, and a zero would be a claim the
/// model never made.
///
/// Flows on equipment this reader does not model — shunt compensators,
/// switches, converters — are not indexed here, because there is nothing in the
/// network to compare them against. A node's reactive balance may therefore
/// fail to close on what is recorded, and the missing term is real equipment
/// rather than a lost number.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SolvedState {
    /// One entry per bus in the network, in the same order.
    pub voltages: Vec<Option<BusVoltage>>,
    /// One entry per line in the network, in the same order.
    pub branches: Vec<BranchFlow>,
    /// One entry per generator in the network, in the same order.
    pub generators: Vec<Option<TerminalFlow>>,
    /// One entry per load in the network, in the same order.
    pub loads: Vec<Option<TerminalFlow>>,
    /// Every tap position the profile stated, in identifier order.
    pub taps: Vec<TapPosition>,
    /// Every shunt compensator position the profile stated, in identifier order.
    pub shunts: Vec<ShuntSections>,
}

impl SolvedState {
    /// How many buses the profile gave a voltage for.
    pub fn buses_covered(&self) -> usize {
        self.voltages.iter().filter(|v| v.is_some()).count()
    }

    /// How many branches the profile gave a flow for, at either end.
    pub fn branches_covered(&self) -> usize {
        self.branches
            .iter()
            .filter(|b| b.end0.is_some() || b.end1.is_some())
            .count()
    }
}

/// Where each CIM object ended up in the assembled network.
///
/// The state variables profile talks about equipment by identifier and about
/// buses through terminals, so reading it needs the same joins the assembly
/// already made. Keeping them rather than rebuilding them is also what stops
/// the two passes from disagreeing about which end of a branch is which.
#[derive(Default)]
struct Placed {
    node: HashMap<String, usize>,
    line: HashMap<String, usize>,
    generator: HashMap<String, usize>,
    load: HashMap<String, usize>,
}

/// Read the state variables profile out of an already-assembled model.
///
/// `None` when the merged documents carry no SV profile at all, which is the
/// ordinary case: EQ and TP alone describe a network nobody has solved yet.
fn solved_state(
    model: &Model,
    ordered: &[(&String, &Object)],
    net: &Network,
    placed: &Placed,
) -> Option<SolvedState> {
    if !ordered.iter().any(|(_, o)| o.class.starts_with("Sv")) {
        return None;
    }

    let mut state = SolvedState {
        voltages: vec![None; net.buses.len()],
        branches: vec![BranchFlow::default(); net.lines.len()],
        generators: vec![None; net.generators.len()],
        loads: vec![None; net.loads.len()],
        taps: Vec::new(),
        shunts: Vec::new(),
    };

    for (_, o) in ordered {
        match o.class.as_str() {
            "SvVoltage" => {
                let Some(bus) = o
                    .refs
                    .get("TopologicalNode")
                    .and_then(|n| placed.node.get(n))
                    .copied()
                else {
                    continue;
                };
                let Some(v_kv) = o.num("v") else { continue };
                let v_nom = net.buses[bus].v_nom;
                state.voltages[bus] = Some(BusVoltage {
                    v_kv,
                    v_pu: (v_nom > 0.0).then(|| v_kv / v_nom),
                    angle: o.num("angle").unwrap_or(0.0).to_radians(),
                });
            }
            "SvPowerFlow" => {
                let Some(terminal_id) = o.refs.get("Terminal") else {
                    continue;
                };
                let Some(equipment) = model
                    .get(terminal_id)
                    .and_then(|t| t.refs.get("ConductingEquipment"))
                else {
                    continue;
                };
                let flow = TerminalFlow {
                    p: o.num("p").unwrap_or(0.0),
                    q: o.num("q").unwrap_or(0.0),
                };
                if let Some(i) = placed.line.get(equipment).copied() {
                    // The end is decided by the node this terminal itself
                    // reaches, not by the order the profile listed its flows
                    // in. A branch whose ends were swapped reports its losses
                    // as negative and its direction backwards.
                    let bus = model
                        .terminal_node
                        .get(terminal_id)
                        .and_then(|n| placed.node.get(n))
                        .copied();
                    let line = &net.lines[i];
                    if bus == Some(line.bus0) {
                        state.branches[i].end0 = Some(flow);
                    } else if bus == Some(line.bus1) {
                        state.branches[i].end1 = Some(flow);
                    }
                } else if let Some(i) = placed.generator.get(equipment).copied() {
                    state.generators[i] = Some(flow);
                } else if let Some(i) = placed.load.get(equipment).copied() {
                    state.loads[i] = Some(flow);
                }
            }
            "SvTapStep" => {
                let Some(changer_id) = o.refs.get("TapChanger") else {
                    continue;
                };
                // A continuous position is what a model that treats the tap as
                // a real variable publishes; a discrete one is the step the
                // changer is actually standing on.
                let Some(position) = o.num("position").or_else(|| o.num("continuousPosition"))
                else {
                    continue;
                };
                let changer = model.get(changer_id);
                let branch = changer
                    .and_then(|c| c.refs.get("TransformerEnd"))
                    .and_then(|e| model.get(e))
                    .and_then(|e| e.refs.get("PowerTransformer"))
                    .and_then(|t| placed.line.get(t))
                    .copied();
                state.taps.push(TapPosition {
                    name: changer
                        .map(|c| model.name_of(changer_id, c, "tap"))
                        .unwrap_or_else(|| format!("tap{changer_id}")),
                    branch,
                    position,
                });
            }
            "SvShuntCompensatorSections" => {
                let Some(shunt_id) = o.refs.get("ShuntCompensator") else {
                    continue;
                };
                let Some(sections) = o.num("sections").or_else(|| o.num("continuousSections"))
                else {
                    continue;
                };
                let shunt = model.get(shunt_id);
                let bus = model
                    .nodes_of
                    .get(shunt_id)
                    .and_then(|n| n.first())
                    .and_then(|n| placed.node.get(n))
                    .copied();
                state.shunts.push(ShuntSections {
                    name: shunt
                        .map(|s| model.name_of(shunt_id, s, "shunt"))
                        .unwrap_or_else(|| format!("shunt{shunt_id}")),
                    bus,
                    sections,
                });
            }
            _ => {}
        }
    }
    Some(state)
}

/// Parse a CIM model from one or more RDF/XML documents.
pub fn parse_model(
    documents: &[(String, String)],
    name: impl Into<String>,
) -> Result<Case, CgmesError> {
    parse_model_with_state(documents, name).map(|(case, _)| case)
}

/// Parse a CIM model and whatever solved state was published with it.
///
/// The `Case` is byte for byte what [`parse_model`] returns; the state is
/// additional, and `None` for the ordinary EQ-and-TP model that nobody has
/// solved yet. See [`SolvedState`] for why it is returned beside the network
/// rather than written into it.
pub fn parse_model_with_state(
    documents: &[(String, String)],
    name: impl Into<String>,
) -> Result<(Case, Option<SolvedState>), CgmesError> {
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
        let Some(value) = o.num("value") else {
            continue;
        };
        let Some(set) = o.refs.get("OperationalLimitSet") else {
            continue;
        };
        let Some(terminal) = model.get(set).and_then(|s| s.refs.get("Terminal")) else {
            continue;
        };
        let Some(equipment) = model
            .get(terminal)
            .and_then(|t| t.refs.get("ConductingEquipment"))
        else {
            continue;
        };
        let mva = if o.class == "ActivePowerLimit" {
            value
        } else {
            // Three-phase apparent power from a per-phase current limit.
            let kv = model
                .get(equipment)
                .and_then(|e| model.kv(e))
                .unwrap_or(0.0);
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

    // Where each CIM identifier ends up, recorded as the components are added
    // rather than reconstructed afterwards. A second pass that re-derived which
    // line a `PowerTransformer` became could disagree with the first about
    // which end is `bus0`, and a solved state attached to the wrong end is
    // worse than none.
    let mut line_of: HashMap<String, usize> = HashMap::new();
    let mut generator_of: HashMap<String, usize> = HashMap::new();
    let mut load_of: HashMap<String, usize> = HashMap::new();

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
                let at = net.add_line(Line {
                    name: model.name_of(key, obj, "line"),
                    bus0,
                    bus1,
                    s_nom: limit_of.get(*key).copied().unwrap_or(1e6),
                    susceptance,
                    resistance: r,
                    reactance: x,
                    ..Default::default()
                });
                line_of.insert((*key).clone(), at);
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
                let rating = limit_of.get(*key).copied().unwrap_or(if rated_s > 0.0 {
                    rated_s
                } else {
                    1e6
                });
                let placed_at = net.add_line(Line {
                    name: model.name_of(key, obj, "transformer"),
                    bus0,
                    bus1,
                    s_nom: rating,
                    susceptance,
                    resistance: r,
                    reactance: x,
                    tap_ratio: if tap.is_finite() && tap > 0.0 {
                        tap
                    } else {
                        1.0
                    },
                    ..Default::default()
                });
                line_of.insert((*key).clone(), placed_at);
            }
            "SynchronousMachine"
            | "GeneratingUnit"
            | "ThermalGeneratingUnit"
            | "HydroGeneratingUnit"
            | "WindGeneratingUnit"
            | "NuclearGeneratingUnit"
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
                let at = net.add_generator(Generator {
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
                generator_of.insert((*key).clone(), at);
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
                let at = net.add_load(Load {
                    name: model.name_of(key, obj, "load"),
                    bus,
                    p_set: p,
                    q_set: q,
                    ..Default::default()
                });
                load_of.insert((*key).clone(), at);
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
    let placed = Placed {
        node: node_index,
        line: line_of,
        generator: generator_of,
        load: load_of,
    };
    let state = solved_state(&model, &ordered, &net, &placed);
    if let Some(s) = &state {
        // Said out loud, because a caller who is handed only the `Case` would
        // otherwise never learn that the archive contained the operator's own
        // answer and that this reader deliberately did not apply it.
        notes.push(format!(
            "state variables: a published solution covers {} of {} buses and {} of {} \
             branches; it is returned beside the network rather than folded into it, so \
             a solver's own answer can be checked against it",
            s.buses_covered(),
            net.buses.len(),
            s.branches_covered(),
            net.lines.len()
        ));
        if !s.taps.is_empty() || !s.shunts.is_empty() {
            notes.push(format!(
                "the solved state also fixes {} tap positions and {} shunt compensator \
                 settings, which describe the controls rather than the network and are \
                 reported rather than applied",
                s.taps.len(),
                s.shunts.len()
            ));
        }
    }
    notes.push(
        "CIM carries no generation costs; every marginal cost is zero until one is supplied".into(),
    );

    net.validate()?;
    Ok((
        Case {
            name: name.into(),
            network: net,
            notes,
        },
        state,
    ))
}

/// Read every XML document out of a zip archive.
///
/// The form a CGMES model is actually published in. ENTSO-E distributes each
/// profile as its own file inside one archive, and often nests an archive per
/// operator inside another, so this recurses one level: an archive holding
/// archives is exactly what a pan-European model looks like.
#[cfg(feature = "cgmes")]
pub fn documents_from_zip(
    bytes: Vec<u8>,
    label: &str,
) -> Result<Vec<(String, String)>, CgmesError> {
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
    load_model_with_state(path).map(|(case, _)| case)
}

/// Read a CIM model together with the solved state published alongside it.
///
/// Identical to [`load_model`] in every other respect, including reading an
/// archive or a directory, because the state variables profile is just another
/// file in the same model and has to survive both routes in.
pub fn load_model_with_state(
    path: impl AsRef<Path>,
) -> Result<(Case, Option<SolvedState>), crate::IoError> {
    let path = path.as_ref();
    let (documents, name) = documents_of(path)?;
    parse_model_with_state(&documents, name).map_err(crate::IoError::Cgmes)
}

/// Collect every profile document a path stands for, and the model's name.
fn documents_of(path: &Path) -> Result<(Vec<(String, String)>, String), crate::IoError> {
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "model".into());
    let mut documents = Vec::new();

    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
    {
        let bytes = std::fs::read(path).map_err(|source| crate::IoError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let docs = documents_from_zip(bytes, &name).map_err(crate::IoError::Cgmes)?;
        return Ok((docs, name));
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
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("xml")))
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

    Ok((documents, name))
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

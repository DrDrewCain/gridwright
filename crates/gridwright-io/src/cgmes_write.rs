//! Writing a network back out as CIM/CGMES RDF/XML.
//!
//! This is the last of the formats to gain a writer and it was left until last
//! deliberately, because a CGMES file that is not conformant is worse than no
//! file at all: someone will hand it to a tool that trusts it. The stance taken
//! here is therefore conformance over coverage. A small subset of CIM is
//! emitted and everything that could only be written by guessing is left out
//! and named in the notes, in the same way every reader in this crate names
//! what it dropped.
//!
//! # What is emitted
//!
//! Separate documents, one per profile, which is how a CGMES model is
//! published:
//!
//! - **EQ**, the equipment profile: the containment hierarchy, base voltages,
//!   `ACLineSegment`, `PowerTransformer` with its two `PowerTransformerEnd`s,
//!   `SynchronousMachine` with its generating unit, `EnergyConsumer`,
//!   `LinearShuntCompensator`, every `Terminal`, and the operational limits
//!   that carry a branch rating.
//! - **TP**, the topology profile: one `TopologicalNode` per bus, and the
//!   association from each terminal to the node it reaches.
//! - **SSH**, the steady state hypothesis: what the loads are drawing, which
//!   terminals are connected, how much of each shunt is switched in, and the
//!   machines' operating points where a solved state supplied them.
//! - **SV**, the state variables profile, and only when a caller hands over a
//!   [`crate::cgmes::SolvedState`]. See below.
//!
//! The split is not cosmetic. `EnergyConsumer.p` lives in SSH and not in EQ,
//! so an equipment profile on its own describes a network with plant in it and
//! no demand. That is a property of the standard rather than of this writer,
//! and it is why the SSH document is always produced rather than only when
//! something is switched off.
//!
//! # The two things a network does not know
//!
//! A `Network` says what a system is and can do. Two kinds of fact a CGMES file
//! needs are not in it, and both are handled the same way, through
//! [`ModelOptions`]: they are taken from the caller and never invented.
//!
//! The first is **when**. `Model.created` and `Model.scenarioTime` are required
//! and neither is derivable, and reading the clock inside the writer would mean
//! the same network wrote a different file every run. So the caller states them,
//! which keeps the writer a function of its arguments; [`write_cgmes`] stamps
//! the current time, because a file being written to disk really is happening
//! now and that is exactly what `Model.created` records.
//!
//! The second is **what it was doing**. A state variables profile is the
//! operator's own answer, published so the receiver can reproduce it, and it is
//! the single most dangerous thing in this module to invent: somebody checks
//! their own solution against it. So an SV profile is written only from a
//! `SolvedState`, which is the type the reader produces from a real one, and
//! never from a network on its own. The same holds for the machine set points
//! in SSH.
//!
//! # Terminals, which are the whole difficulty
//!
//! CIM equipment does not name the bus it sits on. A `Terminal` points at the
//! equipment and at a `TopologicalNode`, and the association runs that way
//! round only. A writer that emitted beautifully specified components and got
//! the terminals wrong would produce a file full of plant connected to nothing,
//! which parses perfectly. Every piece of equipment here is emitted together
//! with its terminals, and the terminals are collected in one list that is
//! written into EQ (equipment and sequence number), into TP (the node) and into
//! SSH (connected), so the three documents cannot disagree about connectivity.
//!
//! # Units, each one the inverse of what the reader does
//!
//! CIM states impedance in ohms and voltage in kilovolts. The reader divides by
//! the impedance base `v_nom² / S_base` to reach per unit, so this multiplies by
//! it. Susceptance goes the other way, because the admittance base is the
//! reciprocal of the impedance base. A current limit becomes an apparent power
//! through `√3 · V · I`, so a rating becomes a current by dividing. Nothing
//! here converts an angle, because nothing here writes one: see the phase shift
//! note below.
//!
//! A round trip that loses the base is the characteristic failure of this
//! format, and it is silent, because per-unit numbers written into a field read
//! as ohms describe a network of near short circuits that solves and means
//! nothing.
//!
//! # Identifiers
//!
//! Every CIM object needs an mRID, which is a UUID. Random ones would make two
//! runs over the same network produce two different files, so these are derived
//! from the object's content: a 64-bit hash of a canonical string naming the
//! class and the properties that distinguish the object supplies the low bits,
//! and the version and variant nibbles are set so the result is a well-formed
//! UUID (version 8, the one RFC 9562 reserves for exactly this, a
//! vendor-defined derivation).
//!
//! The leading 32 bits are not hash. They are the component's index in the
//! network. That is deliberate and it is the one piece of cleverness here worth
//! justifying. The reader sorts nodes, and then equipment, by identifier before
//! assembling anything, because hash iteration order would otherwise decide
//! which end of a line is `bus0`. Putting the ordinal at the front of the
//! identifier makes the reader's sort reproduce the writer's order, so a
//! network that goes out and comes back has its buses, branches, generators and
//! loads in the same positions rather than merely the same contents. Branches
//! share one ordinal sequence across `ACLineSegment` and `PowerTransformer`,
//! since the reader interleaves them into a single list of lines.
//!
//! The cost of that choice is that inserting a bus at the front of a network
//! renumbers every identifier after it, so these mRIDs are not stable across
//! edits the way a real registry's would be. For a conversion, where the file
//! is produced from the model each time, that costs nothing. For a model
//! exchanged repeatedly with a counterparty who tracks objects by mRID, it
//! would, and such a caller should carry their own identifiers.

use std::collections::{BTreeMap, BTreeSet};

use gridwright_net::Network;

/// The CIM 16 schema, which is the version CGMES 2.4.15 is built on.
const CIM: &str = "http://iec.ch/TC57/2013/CIM-schema-cim16#";
const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
/// The model description namespace, which is where the file header lives. It is
/// IEC 61970-552 and not part of the CIM schema itself.
const MD: &str = "http://iec.ch/TC57/61970-552/ModelDescription/1#";

const EQ_PROFILE: &str = "http://entsoe.eu/CIM/EquipmentCore/3/1";
const EQ_OPERATION_PROFILE: &str = "http://entsoe.eu/CIM/EquipmentOperation/3/1";
const TP_PROFILE: &str = "http://entsoe.eu/CIM/Topology/4/1";
const SSH_PROFILE: &str = "http://entsoe.eu/CIM/SteadyStateHypothesis/1/1";
const SV_PROFILE: &str = "http://entsoe.eu/CIM/StateVariables/4/1";

/// Who made the model. CGMES requires a modelling authority set and it is a
/// URI rather than a name, so this identifies the tool rather than claiming to
/// be a transmission operator.
const AUTHORITY: &str = "urn:gridwright:modelling-authority";

/// A rating at or above this is the convention this crate uses for unlimited,
/// and CIM says unlimited by carrying no limit at all.
const UNLIMITED: f64 = 1e6;

/// A written CGMES model: one document per profile, and what could not be said.
///
/// The same shape as [`crate::Written`] except that a CGMES model is several
/// files rather than one, so the text is a list. The pairs are exactly what
/// [`crate::cgmes::parse_model`] takes, so a caller can read back what was
/// written without going through a directory.
#[derive(Debug, Clone)]
pub struct WrittenModel {
    /// File name and content, in the order equipment, topology, hypothesis,
    /// and state variables where a solved state was supplied.
    pub documents: Vec<(String, String)>,
    /// What CIM could not hold, or could only hold by guessing, stated rather
    /// than discovered.
    pub notes: Vec<String>,
}

/// The facts a CGMES file needs that a [`Network`] does not contain.
///
/// Three things are in this position, and the reason they are parameters rather
/// than defaults is the same for all three: they are true or false about the
/// world rather than derivable from the model, so a writer that filled them in
/// would be making them up.
///
/// Leaving them out has a cost, which is that the header is then missing
/// attributes CGMES requires. Supplying them is therefore the ordinary case and
/// [`write_cgmes`] does it, since a file being written to disk is already
/// happening at a particular moment and that moment is exactly what
/// `Model.created` records.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelOptions<'a> {
    /// When the file was made, in seconds since the Unix epoch.
    ///
    /// Not read from the clock inside the writer. That would make the same
    /// network produce a different file on every run, and the writer being a
    /// function of its arguments is what makes its output comparable at all.
    /// A caller who wants the current time passes it, which keeps the impurity
    /// where it belongs.
    pub created: Option<i64>,
    /// The moment the operating point describes, in seconds since the epoch.
    ///
    /// Distinct from `created`: a model built today may describe next winter's
    /// peak. A [`Network`]'s snapshots carry weights and no calendar, so there
    /// is nothing here to derive it from.
    pub scenario_time: Option<i64>,
    /// A solved state to publish beside the model, as a state variables
    /// profile.
    ///
    /// The only honest source of one. A network states what a system can do and
    /// an SV profile states what it did, so writing one without this would mean
    /// inventing a load flow, and somebody would then check their own answer
    /// against it.
    pub state: Option<&'a crate::cgmes::SolvedState>,
}

/// Render a Unix timestamp as the `xsd:dateTime` a CIM header carries.
///
/// Written out rather than taken from a date library because this crate has no
/// date dependency and adding one for eleven lines of arithmetic would be a
/// poor trade. The civil-from-days conversion is Howard Hinnant's, which shifts
/// the epoch to the first of March so that the leap day lands at the end of the
/// year and the month lengths become a linear formula.
fn xsd_datetime(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    // Days since 0000-03-01, which is 719468 days before the Unix epoch.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + i64::from(m <= 2);
    let (h, min, s) = (time / 3600, (time % 3600) / 60, time % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
}

/// A 64-bit hash of a string, salted so one string yields several.
///
/// FNV-1a with a final avalanche step. Deliberately not `DefaultHasher`, whose
/// algorithm is explicitly unspecified across releases of the standard library:
/// identifiers that changed when the compiler was upgraded would break the
/// promise that the same network writes the same file.
fn hash64(salt: u64, s: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325_u64 ^ salt.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // FNV alone avalanches poorly in the high bits, and the high bits are the
    // ones that become the identifier.
    let mut z = h;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Assemble a UUID from a leading word and a hash of the content.
///
/// The leading underscore is not decoration: `rdf:ID` is an XML `NCName`, which
/// may not begin with a digit, and a UUID usually does. Every CIM file in
/// existence solves this the same way.
fn uuid(leading: u32, content: &str) -> String {
    let a = hash64(1, content);
    let b = hash64(2, content);
    let time_mid = (a >> 48) as u16;
    // Version 8: a UUID whose bits are vendor-defined, which is what a
    // content-derived identifier is.
    let time_hi = 0x8000_u16 | ((a >> 32) as u16 & 0x0fff);
    // Variant 10, the RFC 4122 layout.
    let clock_seq = 0x8000_u16 | ((a >> 16) as u16 & 0x3fff);
    let node = b & 0xffff_ffff_ffff;
    format!("_{leading:08x}-{time_mid:04x}-{time_hi:04x}-{clock_seq:04x}-{node:012x}")
}

/// An identifier for an object whose position in the file does not matter.
fn mrid(content: &str) -> String {
    uuid(hash64(3, content) as u32, content)
}

/// An identifier whose sort order is the component's position in the network.
///
/// See the module documentation: the reader sorts by identifier before it
/// assembles anything, so this is what makes a round trip land each component
/// back in the index it started at.
fn ordered_mrid(position: usize, content: &str) -> String {
    uuid(position as u32, content)
}

/// The `urn:uuid:` form, which is how a model header names itself.
fn urn(m: &str) -> String {
    format!("urn:uuid:{}", m.trim_start_matches('_'))
}

/// Render a number the way an RDF/XML document should carry it.
///
/// Rust's `Display` for a float never uses exponential notation and prints the
/// shortest string that reads back as the same value, which are exactly the two
/// properties needed. `1e-07` is legal XML Schema and is rejected by more than
/// one CIM tool, and a fixed number of decimal places, which is what the
/// MATPOWER and PSS/E writers use, would round an impedance stated against a
/// one-kilovolt base down to four significant figures and lose the round trip.
fn cim_num(v: f64) -> String {
    if !v.is_finite() {
        // No caller reaches this: every one either checks for a finite value
        // first or has computed one. Zero rather than a panic, since a library
        // has no business unwrapping, and unlike the fixed-width formats CIM
        // has no conventional large number meaning unlimited.
        return "0".to_string();
    }
    let s = format!("{v}");
    if s == "-0" { "0".to_string() } else { s }
}

/// XML text escaping. A bus called `A&B` would otherwise produce a document no
/// parser will open.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Open an object this profile is the authority for.
fn defined(out: &mut String, class: &str, id: &str) {
    out.push_str(&format!("  <cim:{class} rdf:ID=\"{id}\">\n"));
}

/// Open an object another profile defined, to add properties to it.
///
/// The `rdf:ID` against `rdf:about` distinction is the discipline that makes
/// profiles composable: a topology file that redefined its terminals with
/// `rdf:ID` would be claiming to own equipment it only annotates.
fn updated(out: &mut String, class: &str, id: &str) {
    out.push_str(&format!("  <cim:{class} rdf:about=\"#{id}\">\n"));
}

fn close(out: &mut String, class: &str) {
    out.push_str(&format!("  </cim:{class}>\n"));
}

fn text_prop(out: &mut String, prop: &str, value: &str) {
    let value = escape(value);
    out.push_str(&format!("    <cim:{prop}>{value}</cim:{prop}>\n"));
}

fn num_prop(out: &mut String, prop: &str, value: f64) {
    let value = cim_num(value);
    out.push_str(&format!("    <cim:{prop}>{value}</cim:{prop}>\n"));
}

fn bool_prop(out: &mut String, prop: &str, value: bool) {
    out.push_str(&format!("    <cim:{prop}>{value}</cim:{prop}>\n"));
}

fn link(out: &mut String, prop: &str, target: &str) {
    out.push_str(&format!("    <cim:{prop} rdf:resource=\"#{target}\"/>\n"));
}

/// A reference to a value of a CIM enumeration, which is a schema URI and not a
/// fragment of this document.
fn enum_link(out: &mut String, prop: &str, value: &str) {
    out.push_str(&format!(
        "    <cim:{prop} rdf:resource=\"{CIM}{value}\"/>\n"
    ));
}

/// One equipment end: which terminal it is, what it belongs to, where it goes.
///
/// Collected once and written into all three profiles, so the equipment file's
/// idea of how many terminals a transformer has cannot drift from the topology
/// file's idea of where they land.
struct TerminalRecord {
    mrid: String,
    equipment: String,
    sequence: u32,
    bus: usize,
    name: String,
}

/// Write the model header every CGMES document carries.
///
/// `Model.created` and `Model.scenarioTime` appear only when the caller stated
/// them, since neither is derivable from a network and both are required. See
/// [`ModelOptions`] for why that is a parameter rather than a call to the clock.
fn header(
    out: &mut String,
    model: &str,
    description: &str,
    profiles: &[&str],
    depends_on: &[&str],
    options: &ModelOptions<'_>,
) {
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<rdf:RDF xmlns:rdf=\"{RDF}\" xmlns:cim=\"{CIM}\" xmlns:md=\"{MD}\">\n"
    ));
    out.push_str(&format!("  <md:FullModel rdf:about=\"{}\">\n", urn(model)));
    let description = escape(description);
    out.push_str(&format!(
        "    <md:Model.description>{description}</md:Model.description>\n"
    ));
    if let Some(t) = options.created {
        out.push_str(&format!(
            "    <md:Model.created>{}</md:Model.created>\n",
            xsd_datetime(t)
        ));
    }
    if let Some(t) = options.scenario_time {
        out.push_str(&format!(
            "    <md:Model.scenarioTime>{}</md:Model.scenarioTime>\n",
            xsd_datetime(t)
        ));
    }
    out.push_str("    <md:Model.version>1</md:Model.version>\n");
    out.push_str(&format!(
        "    <md:Model.modelingAuthoritySet>{AUTHORITY}</md:Model.modelingAuthoritySet>\n"
    ));
    for p in profiles {
        out.push_str(&format!("    <md:Model.profile>{p}</md:Model.profile>\n"));
    }
    for d in depends_on {
        out.push_str(&format!(
            "    <md:Model.DependentOn rdf:resource=\"{}\"/>\n",
            urn(d)
        ));
    }
    out.push_str("  </md:FullModel>\n");
}

/// Write a network as a CGMES model: an equipment, a topology and a steady
/// state hypothesis profile.
///
/// The header will have no `Model.created` and no `Model.scenarioTime`, because
/// neither can be derived from a network and this entry point takes nothing to
/// derive them from. Use [`to_cgmes_with`] to state them, or [`write_cgmes`],
/// which stamps the moment it writes.
///
/// See the module documentation for what is emitted and what is not. The
/// returned notes are not decoration; several of them describe information that
/// left the model at this point and cannot be recovered from the file.
pub fn to_cgmes(net: &Network, name: &str) -> WrittenModel {
    to_cgmes_with(net, name, &ModelOptions::default())
}

/// Write a network as a CGMES model, stating the things a network cannot.
///
/// The same output as [`to_cgmes`] plus whatever [`ModelOptions`] supplies: a
/// complete model header, and a state variables profile when a solved state is
/// given.
pub fn to_cgmes_with(net: &Network, name: &str, options: &ModelOptions<'_>) -> WrittenModel {
    let base = if net.base_mva > 0.0 {
        net.base_mva
    } else {
        100.0
    };
    let mut notes = Vec::new();
    let mut terminals: Vec<TerminalRecord> = Vec::new();

    // Identifiers first, because everything refers to everything else and a
    // second derivation that disagreed with the first would produce a file of
    // dangling references.
    let bus_mrid: Vec<String> = net
        .buses
        .iter()
        .enumerate()
        .map(|(i, b)| {
            ordered_mrid(
                i,
                &format!(
                    "TopologicalNode:{}:{}:{}",
                    b.name,
                    b.country,
                    cim_num(b.v_nom)
                ),
            )
        })
        .collect();

    // A branch is a transformer when it changes voltage or carries an
    // off-nominal ratio, and only when both ends have a nominal voltage to
    // state the ratio against. An ACLineSegment whose two terminals sit at
    // different base voltages is not a thing CIM allows, so the voltage step is
    // as much a reason as the tap.
    let ratio_of = |i: usize| net.lines[i].tap_ratio;
    let is_transformer = |i: usize| {
        let l = &net.lines[i];
        let (a, b) = (net.buses[l.bus0].v_nom, net.buses[l.bus1].v_nom);
        let off_nominal = (l.tap_ratio - 1.0).abs() > 1e-12;
        let steps_voltage = (a - b).abs() > 1e-9;
        (off_nominal || steps_voltage) && a > 0.0 && b > 0.0
    };
    let branch_mrid: Vec<String> = net
        .lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let class = if is_transformer(i) {
                "PowerTransformer"
            } else {
                "ACLineSegment"
            };
            ordered_mrid(i, &format!("{class}:{}:{}:{}", l.name, l.bus0, l.bus1))
        })
        .collect();
    let gen_mrid: Vec<String> = net
        .generators
        .iter()
        .enumerate()
        .map(|(i, g)| ordered_mrid(i, &format!("SynchronousMachine:{}:{}", g.name, g.bus)))
        .collect();
    let load_mrid: Vec<String> = net
        .loads
        .iter()
        .enumerate()
        .map(|(i, l)| ordered_mrid(i, &format!("EnergyConsumer:{}:{}", l.name, l.bus)))
        .collect();

    // The containment hierarchy. A Network has a country per bus and nothing
    // else geographic, so the regions and substations are synthesised from it:
    // one sub-region and one substation per country, one voltage level per
    // country and nominal voltage. The substation is named for the country
    // because that is where the reader looks for it, and because a substation
    // name invented from a bus name would come back as a country that the
    // network never stated.
    let region_mrid = mrid(&format!("GeographicalRegion:{name}"));
    let countries: BTreeSet<&str> = net.buses.iter().map(|b| b.country.as_str()).collect();
    let sub_region_mrid = |c: &str| mrid(&format!("SubGeographicalRegion:{c}"));
    let substation_mrid = |c: &str| mrid(&format!("Substation:{c}"));
    let voltage_level_mrid = |c: &str, v: f64| mrid(&format!("VoltageLevel:{c}:{}", cim_num(v)));
    let base_voltage_mrid = |v: f64| mrid(&format!("BaseVoltage:{}", cim_num(v)));

    let mut levels: BTreeSet<(&str, String)> = BTreeSet::new();
    let mut base_voltages: BTreeMap<String, f64> = BTreeMap::new();
    for b in &net.buses {
        levels.insert((b.country.as_str(), cim_num(b.v_nom)));
        if b.v_nom > 0.0 {
            base_voltages.insert(cim_num(b.v_nom), b.v_nom);
        }
    }
    let level_of = |bus: usize| {
        let b = &net.buses[bus];
        voltage_level_mrid(&b.country, b.v_nom)
    };
    let substation_of = |bus: usize| substation_mrid(&net.buses[bus].country);

    // Counters for what the format cannot take.
    let mut no_voltage = 0usize;
    let mut lost_tap = 0usize;
    let mut lost_shift = 0usize;
    let mut lost_charging = 0usize;
    let mut lost_susceptance = 0usize;
    let mut unrated = 0usize;
    let mut unrateable = 0usize;
    let mut shunts = 0usize;
    let mut unplaceable_shunts = 0usize;
    let mut empty_loads = 0usize;
    let mut invented_carriers = 0usize;
    let mut unbounded_machines = 0usize;

    // ---------------------------------------------------------------- EQ ----

    let eq_model = mrid(&format!("FullModel:EQ:{name}"));
    let tp_model = mrid(&format!("FullModel:TP:{name}"));
    let ssh_model = mrid(&format!("FullModel:SSH:{name}"));
    let sv_model = mrid(&format!("FullModel:SV:{name}"));

    let mut eq = String::new();
    let mut limits = String::new();

    defined(&mut eq, "GeographicalRegion", &region_mrid);
    text_prop(&mut eq, "IdentifiedObject.name", name);
    close(&mut eq, "GeographicalRegion");
    for c in &countries {
        let label = if c.is_empty() { "??" } else { c };
        defined(&mut eq, "SubGeographicalRegion", &sub_region_mrid(c));
        text_prop(&mut eq, "IdentifiedObject.name", label);
        link(&mut eq, "SubGeographicalRegion.Region", &region_mrid);
        close(&mut eq, "SubGeographicalRegion");

        defined(&mut eq, "Substation", &substation_mrid(c));
        text_prop(&mut eq, "IdentifiedObject.name", label);
        link(&mut eq, "Substation.Region", &sub_region_mrid(c));
        close(&mut eq, "Substation");
    }
    for (key, v) in &base_voltages {
        defined(&mut eq, "BaseVoltage", &base_voltage_mrid(*v));
        text_prop(&mut eq, "IdentifiedObject.name", &format!("{key}kV"));
        num_prop(&mut eq, "BaseVoltage.nominalVoltage", *v);
        close(&mut eq, "BaseVoltage");
    }
    for (country, key) in &levels {
        let v: f64 = base_voltages.get(key).copied().unwrap_or(0.0);
        defined(&mut eq, "VoltageLevel", &voltage_level_mrid(country, v));
        let label = if country.is_empty() { "??" } else { country };
        text_prop(
            &mut eq,
            "IdentifiedObject.name",
            &format!("{label} {key}kV"),
        );
        link(
            &mut eq,
            "VoltageLevel.Substation",
            &substation_mrid(country),
        );
        if v > 0.0 {
            link(&mut eq, "VoltageLevel.BaseVoltage", &base_voltage_mrid(v));
        }
        close(&mut eq, "VoltageLevel");
    }

    // One shared limit type, since every rating written here is the same kind
    // of limit: the one a branch may carry indefinitely.
    let limit_type_mrid = mrid("OperationalLimitType:PATL");
    let mut any_limit = false;

    for (i, l) in net.lines.iter().enumerate() {
        let id = &branch_mrid[i];
        let kv0 = net.buses[l.bus0].v_nom;
        let kv1 = net.buses[l.bus1].v_nom;
        let terminal = |end: u32| mrid(&format!("Terminal:{id}:{end}"));
        for (end, bus) in [(1u32, l.bus0), (2, l.bus1)] {
            terminals.push(TerminalRecord {
                mrid: terminal(end),
                equipment: id.clone(),
                sequence: end,
                bus,
                name: format!("{} T{end}", l.name),
            });
        }
        if l.shunt_susceptance.abs() > 1e-12 {
            lost_charging += 1;
        }
        if l.phase_shift.abs() > 1e-12 {
            lost_shift += 1;
        }

        if is_transformer(i) {
            // The ratio is carried by the rated voltages of the two windings
            // rather than by a tap changer, because a `Line` states a fixed
            // ratio and not a changer with a range: a `RatioTapChanger` with a
            // single step would be inventing a control that does not exist.
            // The second winding is rated at the voltage it faces and the first
            // takes the whole ratio, which is the reading the reader inverts.
            let rated0 = ratio_of(i) * kv0;
            let rated1 = kv1;
            let z = rated0 * rated0 / base;
            defined(&mut eq, "PowerTransformer", id);
            text_prop(&mut eq, "IdentifiedObject.name", &l.name);
            link(
                &mut eq,
                "Equipment.EquipmentContainer",
                &substation_of(l.bus0),
            );
            close(&mut eq, "PowerTransformer");

            for (end, rated_u, bus) in [(1u32, rated0, l.bus0), (2, rated1, l.bus1)] {
                let end_mrid = mrid(&format!("PowerTransformerEnd:{id}:{end}"));
                defined(&mut eq, "PowerTransformerEnd", &end_mrid);
                text_prop(
                    &mut eq,
                    "IdentifiedObject.name",
                    &format!("{} W{end}", l.name),
                );
                link(&mut eq, "PowerTransformerEnd.PowerTransformer", id);
                link(&mut eq, "TransformerEnd.Terminal", &terminal(end));
                num_prop(&mut eq, "TransformerEnd.endNumber", f64::from(end));
                if net.buses[bus].v_nom > 0.0 {
                    link(
                        &mut eq,
                        "TransformerEnd.BaseVoltage",
                        &base_voltage_mrid(net.buses[bus].v_nom),
                    );
                }
                num_prop(&mut eq, "PowerTransformerEnd.ratedU", rated_u);
                if l.s_nom < UNLIMITED {
                    num_prop(&mut eq, "PowerTransformerEnd.ratedS", l.s_nom);
                }
                // The whole impedance sits on the first winding and the second
                // is ideal. Splitting it would need a division the network
                // never stated, and the reader sums the ends after rebasing
                // each, so one end carrying all of it is the same arithmetic.
                let (r, x) = if end == 1 {
                    (l.resistance * z, l.reactance * z)
                } else {
                    (0.0, 0.0)
                };
                num_prop(&mut eq, "PowerTransformerEnd.r", r);
                num_prop(&mut eq, "PowerTransformerEnd.x", x);
                num_prop(&mut eq, "PowerTransformerEnd.g", 0.0);
                num_prop(&mut eq, "PowerTransformerEnd.b", 0.0);
                // The winding connection is not a free choice being made here.
                // A phase displacement is exactly what a vector group encodes,
                // and this branch has already said its displacement is zero, so
                // a star on both ends with a clock of zero is the CIM spelling
                // of what the network states rather than a guess about the
                // hardware. Where the branch does carry a shift, the network
                // determines nothing about which windings produce it, so both
                // attributes are left out and the note about the lost shift
                // covers it.
                if l.phase_shift.abs() <= 1e-12 {
                    enum_link(
                        &mut eq,
                        "PowerTransformerEnd.connectionKind",
                        "WindingConnection.Y",
                    );
                    num_prop(&mut eq, "PowerTransformerEnd.phaseAngleClock", 0.0);
                }
                close(&mut eq, "PowerTransformerEnd");
            }
            if l.s_nom >= UNLIMITED {
                unrated += 1;
            }
            continue;
        }

        if (l.tap_ratio - 1.0).abs() > 1e-12 {
            // Reached only when a bus has no nominal voltage, since otherwise
            // the branch would have been a transformer. There is nowhere in CIM
            // to put a ratio without the voltages it is a ratio between.
            lost_tap += 1;
        }
        // Impedance against the base voltage of the end the reader will use,
        // which is the segment's own. Where there is none, the per-unit numbers
        // go out unchanged: the reader will read them as ohms and divide by
        // nothing, which is the same identity from the other side, and both
        // ends say so in their notes.
        let z = if kv0 > 0.0 {
            kv0 * kv0 / base
        } else {
            no_voltage += 1;
            1.0
        };
        if (l.reactance.abs() > 1e-12
            && (l.susceptance - 1.0 / l.reactance).abs() > 1e-9 * l.susceptance.abs().max(1.0))
            || (l.reactance.abs() <= 1e-12 && l.susceptance.abs() > 1e-12)
        {
            lost_susceptance += 1;
        }

        let container = mrid(&format!("Line:{id}"));
        defined(&mut eq, "Line", &container);
        text_prop(&mut eq, "IdentifiedObject.name", &l.name);
        link(
            &mut eq,
            "Line.Region",
            &sub_region_mrid(&net.buses[l.bus0].country),
        );
        close(&mut eq, "Line");

        defined(&mut eq, "ACLineSegment", id);
        text_prop(&mut eq, "IdentifiedObject.name", &l.name);
        link(&mut eq, "Equipment.EquipmentContainer", &container);
        if kv0 > 0.0 {
            link(
                &mut eq,
                "ConductingEquipment.BaseVoltage",
                &base_voltage_mrid(kv0),
            );
        }
        num_prop(&mut eq, "ACLineSegment.r", l.resistance * z);
        num_prop(&mut eq, "ACLineSegment.x", l.reactance * z);
        // Susceptance divides where impedance multiplies, because the
        // admittance base is the reciprocal of the impedance base. Multiplying
        // here would put the charging of a 400 kV circuit out by a factor of
        // 2.6 million and it would still look like a number.
        num_prop(&mut eq, "ACLineSegment.bch", l.shunt_susceptance / z);
        num_prop(&mut eq, "ACLineSegment.gch", 0.0);
        close(&mut eq, "ACLineSegment");

        // A rating becomes the current that would produce it at the nominal
        // voltage, inverting the reader's √3 · V · I. Without a voltage there
        // is no current to state and the branch goes out unrated.
        if l.s_nom < UNLIMITED {
            if kv0 > 0.0 {
                let amps = l.s_nom * 1000.0 / (3f64.sqrt() * kv0);
                let set = mrid(&format!("OperationalLimitSet:{id}"));
                defined(&mut limits, "OperationalLimitSet", &set);
                text_prop(
                    &mut limits,
                    "IdentifiedObject.name",
                    &format!("{} PATL", l.name),
                );
                link(&mut limits, "OperationalLimitSet.Terminal", &terminal(1));
                close(&mut limits, "OperationalLimitSet");

                defined(
                    &mut limits,
                    "CurrentLimit",
                    &mrid(&format!("CurrentLimit:{id}")),
                );
                text_prop(&mut limits, "IdentifiedObject.name", "PATL");
                link(&mut limits, "OperationalLimit.OperationalLimitSet", &set);
                link(
                    &mut limits,
                    "OperationalLimit.OperationalLimitType",
                    &limit_type_mrid,
                );
                num_prop(&mut limits, "CurrentLimit.value", amps);
                close(&mut limits, "CurrentLimit");
                any_limit = true;
            } else {
                unrateable += 1;
            }
        } else {
            unrated += 1;
        }
    }

    for (i, g) in net.generators.iter().enumerate() {
        let id = &gen_mrid[i];
        let unit = mrid(&format!("GeneratingUnit:{id}"));
        // The carrier decides the class of the generating unit, which is the
        // only place CIM records what a machine burns. Anything outside these
        // five becomes a plain GeneratingUnit and the carrier is lost, which is
        // counted rather than approximated to the nearest fuel.
        let class = match g.carrier.as_str() {
            "thermal" => "ThermalGeneratingUnit",
            "hydro" => "HydroGeneratingUnit",
            "wind" => "WindGeneratingUnit",
            "nuclear" => "NuclearGeneratingUnit",
            "solar" => "SolarGeneratingUnit",
            _ => {
                if g.carrier != "unknown" && !g.carrier.is_empty() {
                    invented_carriers += 1;
                }
                "GeneratingUnit"
            }
        };
        defined(&mut eq, class, &unit);
        text_prop(&mut eq, "IdentifiedObject.name", &g.name);
        link(
            &mut eq,
            "Equipment.EquipmentContainer",
            &substation_of(g.bus),
        );
        num_prop(&mut eq, "GeneratingUnit.maxOperatingP", g.p_nom);
        num_prop(
            &mut eq,
            "GeneratingUnit.minOperatingP",
            g.p_nom * g.p_min_pu,
        );
        close(&mut eq, class);

        defined(&mut eq, "SynchronousMachine", id);
        text_prop(&mut eq, "IdentifiedObject.name", &g.name);
        link(&mut eq, "Equipment.EquipmentContainer", &level_of(g.bus));
        if net.buses[g.bus].v_nom > 0.0 {
            link(
                &mut eq,
                "ConductingEquipment.BaseVoltage",
                &base_voltage_mrid(net.buses[g.bus].v_nom),
            );
        }
        link(&mut eq, "RotatingMachine.GeneratingUnit", &unit);
        // Not a guess about the plant: the object being written came out of the
        // network's generators, so it generates.
        enum_link(
            &mut eq,
            "SynchronousMachine.type",
            "SynchronousMachineKind.generator",
        );
        // The machine is modelled as sitting directly on its bus, so the
        // voltage it is rated for is the voltage of that bus. This states the
        // model rather than the nameplate of some real machine behind a unit
        // transformer the model does not contain.
        if net.buses[g.bus].v_nom > 0.0 {
            num_prop(&mut eq, "RotatingMachine.ratedU", net.buses[g.bus].v_nom);
        }
        // Apparent rating from the corner of the capability the network does
        // state: a machine allowed to reach `p_nom` at `q_max` is by definition
        // rated for at least the apparent power of that point. Where the
        // reactive range is unbounded the corner is not defined and the active
        // rating is all there is to say, which understates it by the power
        // factor and is counted below.
        let rated_s = if g.q_max.is_finite() {
            g.p_nom.hypot(g.q_max)
        } else {
            unbounded_machines += 1;
            g.p_nom
        };
        num_prop(&mut eq, "RotatingMachine.ratedS", rated_s);
        // An unbounded reactive range is written by writing nothing, which is
        // what the reader takes an absent limit to mean. A large finite number
        // would be a limit the machine does not have.
        if g.q_max.is_finite() {
            num_prop(&mut eq, "SynchronousMachine.maxQ", g.q_max);
        }
        if g.q_min.is_finite() {
            num_prop(&mut eq, "SynchronousMachine.minQ", g.q_min);
        }
        close(&mut eq, "SynchronousMachine");

        terminals.push(TerminalRecord {
            mrid: mrid(&format!("Terminal:{id}:1")),
            equipment: id.clone(),
            sequence: 1,
            bus: g.bus,
            name: format!("{} T1", g.name),
        });
    }

    for (i, l) in net.loads.iter().enumerate() {
        let id = &load_mrid[i];
        defined(&mut eq, "EnergyConsumer", id);
        text_prop(&mut eq, "IdentifiedObject.name", &l.name);
        link(&mut eq, "Equipment.EquipmentContainer", &level_of(l.bus));
        if net.buses[l.bus].v_nom > 0.0 {
            link(
                &mut eq,
                "ConductingEquipment.BaseVoltage",
                &base_voltage_mrid(net.buses[l.bus].v_nom),
            );
        }
        close(&mut eq, "EnergyConsumer");

        terminals.push(TerminalRecord {
            mrid: mrid(&format!("Terminal:{id}:1")),
            equipment: id.clone(),
            sequence: 1,
            bus: l.bus,
            name: format!("{} T1", l.name),
        });
    }

    // Bus shunts. A per-unit susceptance becomes siemens by dividing by the
    // impedance base, and without a nominal voltage there is no base and no
    // way to state it, so those are counted and left out.
    let mut shunt_ids: Vec<(usize, String)> = Vec::new();
    for (b, bus) in net.buses.iter().enumerate() {
        if bus.g_shunt.abs() < 1e-12 && bus.b_shunt.abs() < 1e-12 {
            continue;
        }
        if bus.v_nom <= 0.0 {
            unplaceable_shunts += 1;
            continue;
        }
        let z = bus.v_nom * bus.v_nom / base;
        let id = mrid(&format!("LinearShuntCompensator:{}", bus_mrid[b]));
        defined(&mut eq, "LinearShuntCompensator", &id);
        text_prop(
            &mut eq,
            "IdentifiedObject.name",
            &format!("{} shunt", bus.name),
        );
        link(&mut eq, "Equipment.EquipmentContainer", &level_of(b));
        link(
            &mut eq,
            "ConductingEquipment.BaseVoltage",
            &base_voltage_mrid(bus.v_nom),
        );
        num_prop(&mut eq, "ShuntCompensator.nomU", bus.v_nom);
        num_prop(&mut eq, "ShuntCompensator.maximumSections", 1.0);
        num_prop(
            &mut eq,
            "LinearShuntCompensator.bPerSection",
            bus.b_shunt / z,
        );
        num_prop(
            &mut eq,
            "LinearShuntCompensator.gPerSection",
            bus.g_shunt / z,
        );
        close(&mut eq, "LinearShuntCompensator");

        terminals.push(TerminalRecord {
            mrid: mrid(&format!("Terminal:{id}:1")),
            equipment: id.clone(),
            sequence: 1,
            bus: b,
            name: format!("{} shunt T1", bus.name),
        });
        shunt_ids.push((b, id));
        shunts += 1;
    }

    // Terminals last in the document, which changes nothing for a reader that
    // indexes the whole file before assembling and reads far better for anyone
    // opening it.
    for t in &terminals {
        defined(&mut eq, "Terminal", &t.mrid);
        text_prop(&mut eq, "IdentifiedObject.name", &t.name);
        num_prop(
            &mut eq,
            "ACDCTerminal.sequenceNumber",
            f64::from(t.sequence),
        );
        link(&mut eq, "Terminal.ConductingEquipment", &t.equipment);
        close(&mut eq, "Terminal");
    }

    if any_limit {
        defined(&mut eq, "OperationalLimitType", &limit_type_mrid);
        text_prop(&mut eq, "IdentifiedObject.name", "PATL");
        enum_link(
            &mut eq,
            "OperationalLimitType.direction",
            "OperationalLimitDirectionKind.absoluteValue",
        );
        close(&mut eq, "OperationalLimitType");
        eq.push_str(&limits);
    }
    eq.push_str("</rdf:RDF>\n");

    let mut profiles = vec![EQ_PROFILE];
    if any_limit {
        profiles.push(EQ_OPERATION_PROFILE);
    }
    let mut eq_out = String::new();
    header(
        &mut eq_out,
        &eq_model,
        &format!("{name} equipment"),
        &profiles,
        &[],
        options,
    );
    eq_out.push_str(&eq);

    // ---------------------------------------------------------------- TP ----

    let mut tp = String::new();
    header(
        &mut tp,
        &tp_model,
        &format!("{name} topology"),
        &[TP_PROFILE],
        &[&eq_model],
        options,
    );
    for (b, bus) in net.buses.iter().enumerate() {
        defined(&mut tp, "TopologicalNode", &bus_mrid[b]);
        text_prop(&mut tp, "IdentifiedObject.name", &bus.name);
        if bus.v_nom > 0.0 {
            link(
                &mut tp,
                "TopologicalNode.BaseVoltage",
                &base_voltage_mrid(bus.v_nom),
            );
        }
        link(
            &mut tp,
            "TopologicalNode.ConnectivityNodeContainer",
            &level_of(b),
        );
        close(&mut tp, "TopologicalNode");
    }
    // The association that makes the whole thing a network. Written with
    // `rdf:about`, because the terminal belongs to the equipment profile and
    // this one only says where it lands.
    for t in &terminals {
        updated(&mut tp, "Terminal", &t.mrid);
        link(&mut tp, "Terminal.TopologicalNode", &bus_mrid[t.bus]);
        close(&mut tp, "Terminal");
    }
    tp.push_str("</rdf:RDF>\n");

    // --------------------------------------------------------------- SSH ----

    let mut ssh = String::new();
    header(
        &mut ssh,
        &ssh_model,
        &format!("{name} steady state hypothesis"),
        &[SSH_PROFILE],
        &[&eq_model],
        options,
    );
    for (i, l) in net.loads.iter().enumerate() {
        let p = net.load_profile.at(i, 0).unwrap_or(l.p_set);
        if p.abs() < 1e-12 && l.q_set.abs() < 1e-12 {
            empty_loads += 1;
        }
        updated(&mut ssh, "EnergyConsumer", &load_mrid[i]);
        num_prop(&mut ssh, "EnergyConsumer.p", p);
        num_prop(&mut ssh, "EnergyConsumer.q", l.q_set);
        close(&mut ssh, "EnergyConsumer");
    }
    for (_, id) in &shunt_ids {
        updated(&mut ssh, "LinearShuntCompensator", id);
        num_prop(&mut ssh, "ShuntCompensator.sections", 1.0);
        bool_prop(&mut ssh, "RegulatingCondEq.controlEnabled", false);
        close(&mut ssh, "LinearShuntCompensator");
    }
    let mut machines_without_dispatch = 0usize;
    for (i, id) in gen_mrid.iter().enumerate() {
        updated(&mut ssh, "SynchronousMachine", id);
        // The machine's operating point. A network states what a machine can
        // do and a hypothesis states what it is doing, so the only honest
        // source is a solved state the caller already has. CIM signs a
        // terminal's flow into the equipment, which is the same convention the
        // state was read in, so a generating machine is negative in both and
        // nothing is flipped on the way through.
        match options
            .state
            .and_then(|s| s.generators.get(i).copied())
            .flatten()
        {
            Some(flow) => {
                num_prop(&mut ssh, "RotatingMachine.p", flow.p);
                num_prop(&mut ssh, "RotatingMachine.q", flow.q);
            }
            None => machines_without_dispatch += 1,
        }
        // It generates, and nothing here is designated the angle reference: a
        // priority of zero is CIM's way of saying a machine is not a candidate
        // for it, and choosing which bus holds the angle is the solver's
        // business rather than the model's.
        enum_link(
            &mut ssh,
            "SynchronousMachine.operatingMode",
            "SynchronousMachineOperatingMode.generator",
        );
        num_prop(&mut ssh, "SynchronousMachine.referencePriority", 0.0);
        bool_prop(&mut ssh, "RegulatingCondEq.controlEnabled", false);
        close(&mut ssh, "SynchronousMachine");
    }
    for t in &terminals {
        updated(&mut ssh, "Terminal", &t.mrid);
        bool_prop(&mut ssh, "ACDCTerminal.connected", true);
        close(&mut ssh, "Terminal");
    }
    ssh.push_str("</rdf:RDF>\n");

    // ---------------------------------------------------------------- SV ----
    //
    // Written only from a state the caller supplies. A state variables profile
    // full of flat voltages and zero flows would be the worst file this module
    // could produce: it is precisely the thing somebody checks their own answer
    // against, so an invented one is not a partial model, it is a wrong answer
    // published as an operator's.
    let sv = options.state.map(|state| {
        let mut sv = String::new();
        header(
            &mut sv,
            &sv_model,
            &format!("{name} state variables"),
            &[SV_PROFILE],
            &[&eq_model, &tp_model],
            options,
        );
        for (b, voltage) in state.voltages.iter().enumerate() {
            let Some(v) = voltage else { continue };
            defined(
                &mut sv,
                "SvVoltage",
                &mrid(&format!("SvVoltage:{}", bus_mrid[b])),
            );
            link(&mut sv, "SvVoltage.TopologicalNode", &bus_mrid[b]);
            num_prop(&mut sv, "SvVoltage.v", v.v_kv);
            // Back to degrees, which is what CIM publishes and the reader
            // converts on the way in. Leaving radians here would not fail
            // anywhere; it would make every angle difference wrong by a factor
            // of 57 and produce flows that look plausible.
            num_prop(&mut sv, "SvVoltage.angle", v.angle.to_degrees());
            close(&mut sv, "SvVoltage");
        }
        // A flow belongs to a terminal, not to a branch, which is what lets the
        // two ends of one branch disagree by exactly the losses. Each end is
        // written at the terminal that reaches the bus it was measured at,
        // rather than at whichever terminal came first.
        let flow = |sv: &mut String, terminal: &str, p: f64, q: f64| {
            defined(sv, "SvPowerFlow", &mrid(&format!("SvPowerFlow:{terminal}")));
            link(sv, "SvPowerFlow.Terminal", terminal);
            num_prop(sv, "SvPowerFlow.p", p);
            num_prop(sv, "SvPowerFlow.q", q);
            close(sv, "SvPowerFlow");
        };
        for (i, branch) in state.branches.iter().enumerate() {
            let Some(id) = branch_mrid.get(i) else {
                continue;
            };
            for (end, at) in [(1u32, branch.end0), (2, branch.end1)] {
                if let Some(f) = at {
                    let terminal = mrid(&format!("Terminal:{id}:{end}"));
                    flow(&mut sv, &terminal, f.p, f.q);
                }
            }
        }
        for (i, at) in state.generators.iter().enumerate() {
            if let (Some(f), Some(id)) = (at, gen_mrid.get(i)) {
                flow(&mut sv, &mrid(&format!("Terminal:{id}:1")), f.p, f.q);
            }
        }
        for (i, at) in state.loads.iter().enumerate() {
            if let (Some(f), Some(id)) = (at, load_mrid.get(i)) {
                flow(&mut sv, &mrid(&format!("Terminal:{id}:1")), f.p, f.q);
            }
        }
        sv.push_str("</rdf:RDF>\n");
        sv
    });

    // ------------------------------------------------------------- notes ----

    notes.push(format!(
        "CIM/CGMES: {} topological nodes, {} branches ({} as transformers), {} \
         synchronous machines, {} loads, {shunts} shunt compensators, across the {} \
         profiles",
        net.buses.len(),
        net.lines.len(),
        (0..net.lines.len()).filter(|i| is_transformer(*i)).count(),
        net.generators.len(),
        net.loads.len(),
        if sv.is_some() {
            "equipment, topology, steady state hypothesis and state variables"
        } else {
            "equipment, topology and steady state hypothesis"
        },
    ));
    notes.push(
        "demand is written into the steady state hypothesis and not into the equipment \
         profile, which is where CGMES puts it; an equipment profile read on its own \
         therefore describes this network with no load in it"
            .into(),
    );
    if options.created.is_none() {
        notes.push(
            "the model header carries no Model.created, which CGMES requires; nothing in \
             a network says when a file was made and reading the clock here would mean \
             the same network wrote a different file on every run. Pass it through \
             ModelOptions, or use write_cgmes, which stamps the moment it writes"
                .into(),
        );
    }
    if options.scenario_time.is_none() {
        notes.push(
            "the model header carries no Model.scenarioTime; a network's snapshots carry \
             weights and no calendar, so the moment this operating point describes is not \
             in the model and has to be stated through ModelOptions"
                .into(),
        );
    }
    if machines_without_dispatch > 0 {
        notes.push(format!(
            "{machines_without_dispatch} machines have no operating point in the \
             hypothesis: a network states what a machine can do and CGMES asks what it \
             is doing. Supply a solved state through ModelOptions and it is written from \
             that rather than guessed at"
        ));
    }
    if unbounded_machines > 0 {
        notes.push(format!(
            "{unbounded_machines} machines have an unbounded reactive range, so their \
             apparent rating was written as their active rating; with a reactive limit \
             it is the apparent power at the corner of the stated capability instead"
        ));
    }
    if options.state.is_none() {
        notes.push(
            "no state variables profile was written, because a network carries no solved \
             state; one full of flat voltages and zero flows would be the one file here \
             worth least, since an SV profile is what somebody checks their own answer \
             against"
                .into(),
        );
    } else {
        notes.push(
            "the state variables profile carries the published voltages and terminal \
             flows in CIM's own signs, into the equipment, so every node still sums to \
             zero on what was written"
                .into(),
        );
    }
    if let Some(state) = options.state {
        if !state.taps.is_empty() {
            notes.push(format!(
                "{} tap positions in the solved state were not written: they belong to \
                 tap changers, and this writer states a fixed ratio through the windings' \
                 rated voltages rather than emitting a changer for a control the network \
                 does not describe",
                state.taps.len()
            ));
        }
        if !state.shunts.is_empty() {
            notes.push(format!(
                "{} shunt compensator settings in the solved state were not written, \
                 since the compensators they refer to are not the bus shunts this model \
                 carries",
                state.shunts.len()
            ));
        }
        notes.push(
            "no TopologicalIsland was written, which the state variables profile asks \
             for: an island names the node its angles are measured against, and a solved \
             state does not say which node that was"
                .into(),
        );
    }
    if no_voltage > 0 {
        notes.push(format!(
            "{no_voltage} lines sit on buses with no nominal voltage, so their per-unit \
             impedance was written into a field CIM reads as ohms; the reader here \
             inverts that, another tool will not"
        ));
    }
    if lost_tap > 0 {
        notes.push(format!(
            "{lost_tap} branches carry an off-nominal tap ratio that could not be \
             written, because CIM states a ratio through the rated voltages of the two \
             windings and those buses have no nominal voltage"
        ));
    }
    if lost_shift > 0 {
        notes.push(format!(
            "{lost_shift} branches carry a phase shift, which is not written: it would \
             need a PhaseTapChanger with a step table, and a single invented step is a \
             control the network never described"
        ));
    }
    if lost_charging > 0 {
        notes.push(format!(
            "{lost_charging} lines carry a shunt susceptance; it is written as \
             ACLineSegment.bch in siemens, and the reader in this crate does not read \
             it back, so a round trip loses it"
        ));
    }
    if lost_susceptance > 0 {
        notes.push(format!(
            "{lost_susceptance} branches state a DC susceptance that is not the \
             reciprocal of their reactance; CIM has only the impedance, so the \
             susceptance a reader derives from this file will differ"
        ));
    }
    if shunts > 0 {
        notes.push(format!(
            "{shunts} bus shunts are written as LinearShuntCompensator objects, which \
             is where CIM puts them; the reader in this crate builds no bus shunt from \
             them, so a round trip loses them"
        ));
    }
    if unplaceable_shunts > 0 {
        notes.push(format!(
            "{unplaceable_shunts} bus shunts were dropped, since siemens cannot be \
             recovered from per unit without a nominal voltage"
        ));
    }
    if unrated > 0 {
        notes.push(format!(
            "{unrated} branches are unlimited and were written with no operational \
             limit, which is how CIM says the same thing"
        ));
    }
    if unrateable > 0 {
        notes.push(format!(
            "{unrateable} branch ratings were dropped, since a current limit cannot be \
             formed from an apparent power without a nominal voltage"
        ));
    }
    if empty_loads > 0 {
        notes.push(format!(
            "{empty_loads} loads draw nothing; they are written, and a reader that skips \
             a consumer with no demand will not return them"
        ));
    }
    if invented_carriers > 0 {
        notes.push(format!(
            "{invented_carriers} generators have a carrier CIM has no generating unit \
             class for and were written as plain GeneratingUnit objects"
        ));
    }
    if (base - 100.0).abs() > 1e-9 {
        notes.push(format!(
            "impedances were converted to ohms on this network's {base} MVA base; CIM \
             carries no per-unit base at all, so a reader that assumes 100 MVA will \
             recover per-unit values scaled by {}",
            cim_num(100.0 / base)
        ));
    }
    if net.n_snapshots() > 1 {
        notes.push(format!(
            "a steady state hypothesis is one operating point; the first of {} snapshots \
             was written and the rest dropped",
            net.n_snapshots()
        ));
    }
    if !net.storage.is_empty() {
        notes.push(format!(
            "{} storage units dropped; these profiles have no storage",
            net.storage.len()
        ));
    }
    if !net.links.is_empty() {
        notes.push(format!(
            "{} links dropped; a controllable link is an HVDC converter pair in CIM and \
             not one object",
            net.links.len()
        ));
    }
    let shiftable = net.loads.iter().filter(|l| l.shiftable_pu > 0.0).count();
    if shiftable > 0 {
        notes.push(format!(
            "{shiftable} shiftable loads written as fixed demand"
        ));
    }
    notes.push(
        "CIM carries no generation costs, no capital costs and no expansion candidates, \
         so marginal cost, capital cost and every extendable flag were dropped"
            .into(),
    );
    notes.push(
        "voltage limits, synchronous areas and bus carriers have no home in these \
         profiles and were not written"
            .into(),
    );

    let mut documents = vec![
        (format!("{name}_EQ.xml"), eq_out),
        (format!("{name}_TP.xml"), tp),
        (format!("{name}_SSH.xml"), ssh),
    ];
    if let Some(sv) = sv {
        documents.push((format!("{name}_SV.xml"), sv));
    }
    WrittenModel { documents, notes }
}

/// Write a CGMES model as a directory of profile documents.
///
/// A directory rather than a file, because a CGMES model is not one document,
/// and this is the layout [`crate::cgmes::load_model`] reads when pointed at an
/// unpacked model.
///
/// The header is stamped with the current time, which is the one thing this
/// entry point knows that [`to_cgmes`] does not: a file is being written, and it
/// is being written now. That is `Model.created`, a CGMES requirement with no
/// other honest source. It does mean two calls a second apart produce two
/// different files, so a caller who needs byte-identical output should use
/// [`write_cgmes_with`] and state the moment.
pub fn write_cgmes(
    net: &Network,
    dir: impl AsRef<std::path::Path>,
) -> Result<Vec<String>, crate::IoError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok());
    write_cgmes_with(
        net,
        dir,
        &ModelOptions {
            created: now,
            ..Default::default()
        },
    )
}

/// Write a CGMES model to a directory, stating the things a network cannot.
///
/// The filesystem counterpart of [`to_cgmes_with`], and the entry point to use
/// when the output has to be reproducible: everything it writes is a function
/// of its arguments, including the header timestamps.
pub fn write_cgmes_with(
    net: &Network,
    dir: impl AsRef<std::path::Path>,
    options: &ModelOptions<'_>,
) -> Result<Vec<String>, crate::IoError> {
    let dir = dir.as_ref();
    let name = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "model".into());
    std::fs::create_dir_all(dir).map_err(|source| crate::IoError::Read {
        path: dir.display().to_string(),
        source,
    })?;
    let written = to_cgmes_with(net, &name, options);
    for (file, text) in &written.documents {
        let path = dir.join(file);
        std::fs::write(&path, text).map_err(|source| crate::IoError::Read {
            path: path.display().to_string(),
            source,
        })?;
    }
    Ok(written.notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identifier_is_a_well_formed_uuid_with_the_position_at_the_front() {
        let a = ordered_mrid(0, "TopologicalNode:north");
        let b = ordered_mrid(1, "TopologicalNode:south");
        // `_` then 8-4-4-4-12 hex.
        assert_eq!(a.len(), 1 + 36);
        assert!(a.starts_with("_00000000-"));
        assert!(b.starts_with("_00000001-"));
        // The version and variant nibbles, which are what makes it a UUID
        // rather than thirty-two hex digits.
        assert_eq!(a.as_bytes()[1 + 14], b'8');
        assert!(matches!(a.as_bytes()[1 + 19], b'8' | b'9' | b'a' | b'b'));
        // Sorting by identifier has to reproduce the order of the network, or
        // a round trip returns the same components in different positions.
        assert!(a < b);
    }

    #[test]
    fn the_same_content_derives_the_same_identifier_and_different_content_does_not() {
        assert_eq!(mrid("Terminal:line1:1"), mrid("Terminal:line1:1"));
        assert_ne!(mrid("Terminal:line1:1"), mrid("Terminal:line1:2"));
    }

    #[test]
    fn a_number_is_never_written_in_exponential_notation() {
        // A CIM tool is within its rights to accept `1e-07`, and several do
        // not. The values that reach this are impedances against a kilovolt
        // base, which is exactly where the small magnitudes are.
        for v in [1e-7, 1.234_567_891_23e-9, 1e12, -0.0, 0.1 + 0.2] {
            let s = cim_num(v);
            assert!(!s.contains('e'), "{v} was written as {s}");
        }
        // And the shortest form that reads back as the same number, rather than
        // a fixed number of decimal places that would round a small impedance
        // down to a couple of significant figures.
        assert_eq!(
            cim_num(1.234_567_891_23e-9).parse(),
            Ok(1.234_567_891_23e-9)
        );
    }

    #[test]
    fn a_timestamp_becomes_the_date_and_time_it_stands_for() {
        // Hand-checked against dates that are easy to be sure of. The reason
        // this is written out rather than taken from a library is that the
        // library would be a dependency for eleven lines of arithmetic, and the
        // reason it is tested at three points rather than one is that a
        // conversion which is right at the epoch and wrong across a leap year
        // would still look right.
        assert_eq!(xsd_datetime(0), "1970-01-01T00:00:00Z");
        assert_eq!(xsd_datetime(86_399), "1970-01-01T23:59:59Z");
        // 2000-02-29, the leap day of the century that is a leap year, which is
        // the case the hundred-and-four-hundred rules disagree about.
        assert_eq!(xsd_datetime(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(xsd_datetime(1_234_567_890), "2009-02-13T23:31:30Z");
        // And before the epoch, since the arithmetic is signed and a negative
        // remainder would silently give the wrong day.
        assert_eq!(xsd_datetime(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn a_name_with_xml_in_it_is_escaped() {
        assert_eq!(escape("A&B <400>"), "A&amp;B &lt;400&gt;");
    }
}

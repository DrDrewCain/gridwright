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
//! Three profiles, as separate documents, which is how a CGMES model is
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
//!   terminals are connected, and how much of each shunt is switched in.
//!
//! The split is not cosmetic. `EnergyConsumer.p` lives in SSH and not in EQ,
//! so an equipment profile on its own describes a network with plant in it and
//! no demand. That is a property of the standard rather than of this writer,
//! and it is why the SSH document is always produced rather than only when
//! something is switched off.
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
    /// File name and content, in the order equipment, topology, hypothesis.
    pub documents: Vec<(String, String)>,
    /// What CIM could not hold, or could only hold by guessing, stated rather
    /// than discovered.
    pub notes: Vec<String>,
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
fn header(
    out: &mut String,
    model: &str,
    description: &str,
    profiles: &[&str],
    depends_on: Option<&str>,
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
    out.push_str("    <md:Model.version>1</md:Model.version>\n");
    out.push_str(&format!(
        "    <md:Model.modelingAuthoritySet>{AUTHORITY}</md:Model.modelingAuthoritySet>\n"
    ));
    for p in profiles {
        out.push_str(&format!("    <md:Model.profile>{p}</md:Model.profile>\n"));
    }
    if let Some(d) = depends_on {
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
/// See the module documentation for what is emitted and what is not. The
/// returned notes are not decoration; several of them describe information that
/// left the model at this point and cannot be recovered from the file.
pub fn to_cgmes(net: &Network, name: &str) -> WrittenModel {
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

    // ---------------------------------------------------------------- EQ ----

    let eq_model = mrid(&format!("FullModel:EQ:{name}"));
    let tp_model = mrid(&format!("FullModel:TP:{name}"));
    let ssh_model = mrid(&format!("FullModel:SSH:{name}"));

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
        None,
    );
    eq_out.push_str(&eq);

    // ---------------------------------------------------------------- TP ----

    let mut tp = String::new();
    header(
        &mut tp,
        &tp_model,
        &format!("{name} topology"),
        &[TP_PROFILE],
        Some(&eq_model),
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
        Some(&eq_model),
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
    for id in &gen_mrid {
        updated(&mut ssh, "SynchronousMachine", id);
        bool_prop(&mut ssh, "RegulatingCondEq.controlEnabled", false);
        close(&mut ssh, "SynchronousMachine");
    }
    for t in &terminals {
        updated(&mut ssh, "Terminal", &t.mrid);
        bool_prop(&mut ssh, "ACDCTerminal.connected", true);
        close(&mut ssh, "Terminal");
    }
    ssh.push_str("</rdf:RDF>\n");

    // ------------------------------------------------------------- notes ----

    notes.push(format!(
        "CIM/CGMES: {} topological nodes, {} branches ({} as transformers), {} \
         synchronous machines, {} loads, {shunts} shunt compensators, across an \
         equipment, a topology and a steady state hypothesis profile",
        net.buses.len(),
        net.lines.len(),
        (0..net.lines.len()).filter(|i| is_transformer(*i)).count(),
        net.generators.len(),
        net.loads.len(),
    ));
    notes.push(
        "demand is written into the steady state hypothesis and not into the equipment \
         profile, which is where CGMES puts it; an equipment profile read on its own \
         therefore describes this network with no load in it"
            .into(),
    );
    notes.push(
        "the model header carries no Model.created, because a wall clock timestamp \
         would make the same network write a different file on every run and \
         reproducible output was chosen over it; a strict validator will ask for one"
            .into(),
    );
    notes.push(
        "attributes CGMES asks for that a network does not carry are left absent rather \
         than invented: PowerTransformerEnd.connectionKind and phaseAngleClock, \
         RotatingMachine.ratedS and ratedU, SynchronousMachine.type, and the machine \
         set points RotatingMachine.p and q in the hypothesis. A validator will report \
         them missing, which is the honest outcome"
            .into(),
    );
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

    WrittenModel {
        documents: vec![
            (format!("{name}_EQ.xml"), eq_out),
            (format!("{name}_TP.xml"), tp),
            (format!("{name}_SSH.xml"), ssh),
        ],
        notes,
    }
}

/// Write a CGMES model as a directory of profile documents.
///
/// A directory rather than a file, because a CGMES model is not one document,
/// and this is the layout [`crate::cgmes::load_model`] reads when pointed at an
/// unpacked model.
pub fn write_cgmes(
    net: &Network,
    dir: impl AsRef<std::path::Path>,
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
    let written = to_cgmes(net, &name);
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
    fn a_name_with_xml_in_it_is_escaped() {
        assert_eq!(escape("A&B <400>"), "A&amp;B &lt;400&gt;");
    }
}

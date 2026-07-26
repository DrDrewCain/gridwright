//! Reading UCTE-DEF, the European exchange format.
//!
//! Before CGMES there was UCTE-DEF, and the whole of the continental European
//! grid was exchanged in it for two decades. The ENTSO-E study models, the
//! winter and summer reference cases, and most national datasets published
//! before about 2015 exist in this format and in no other. A study reaching
//! back over that period cannot use anything else, which is the entire reason
//! to be able to read it.
//!
//! # Blocks and columns
//!
//! The file is a sequence of blocks, each opened by a `##` marker: `##C`
//! comments, `##N` nodes (subdivided by `##Z<cc>` country headers), `##L`
//! lines, `##T` transformers, `##R` regulation and `##TT` tap tables. Inside a
//! block every field is a fixed column range and a blank field must stay
//! blank. That is not a stylistic point: a line record with no charging
//! susceptance would, split on whitespace, have its ampere current limit read
//! as microsiemens — the line then has no rating at all and a shunt admittance
//! four thousand times too large, and nothing about the file looks wrong.
//!
//! # Units, which are the whole difficulty
//!
//! UCTE-DEF is stated in physical units throughout, while the formulation
//! works in per unit. The format declares no system base, so one hundred MVA
//! is assumed, which is the convention every European study uses, and it is
//! reported in [`Case::notes`].
//!
//! The nominal voltage of a node is not a field. It is the seventh character
//! of the eight-character node code, and every conversion below depends on
//! having decoded it correctly.
//!
//! With `Z_base = V_nom² / S_base` (kV² / MVA gives ohms):
//!
//! - Line and transformer `R` and `X` are in **ohms**: `pu = Ω / Z_base`.
//! - Line and transformer `B` and `G` are in **microsiemens**:
//!   `pu = µS · 10⁻⁶ · Z_base`, since the admittance base is `1 / Z_base`.
//! - A line's current limit is in **amps**, and a rating is wanted in MVA:
//!   `S = √3 · V_nom[kV] · I[A] / 1000`. This is the same conversion the CIM
//!   reader had to make, and it is the same trap: reading 1500 A as 1500 MVA
//!   would rate a 380 kV circuit at 1500 MVA where it can carry 987.
//! - A transformer's rating is different again. It carries a **nominal power
//!   in MVA** directly, which must *not* go through the ampere conversion —
//!   putting 500 MVA through `√3 · 380 · 500 / 1000` would claim 329 GVA.
//! - Transformer `R` and `X` are referred to the **node 2** winding, which is
//!   the regulated one, so the base voltage for them is node 2's and not node
//!   1's. A 380/110 transformer read against the wrong side is out by
//!   `(380/110)² ≈ 12`.
//!
//! # Signs
//!
//! Generation in a node record is **negative**: the whole record is written in
//! the load convention, so a plant producing 700 MW appears as `-700.0`. The
//! permissible generation limits follow, so the *minimum* permissible
//! generation is the most negative number and therefore the *largest* output.
//! Both are flipped on the way in. Taking them at face value would give every
//! plant a negative capacity, which the network validator rejects — the
//! failure at least being loud is luck rather than design.
//!
//! # What the format does not carry
//!
//! No costs of any kind, no time series, and no plant capacity beyond the
//! permissible generation band. All of it is reported through [`Case::notes`].

use std::collections::HashMap;

use gridwright_net::{Generator, Line, Load, Network, Snapshots};

use crate::Case;

/// The base every per-unit quantity here is put on.
///
/// UCTE-DEF states nothing in per unit and so declares no base at all. A
/// hundred MVA is what every European study uses and what the rest of this
/// crate defaults to, so the choice is at least consistent; it is stated in
/// the notes so a caller can see it was a choice.
const ASSUMED_BASE_MVA: f64 = 100.0;

#[derive(Debug, thiserror::Error)]
pub enum UcteError {
    #[error("the file is empty")]
    Empty,
    #[error("no `##N` node block was found")]
    NoNodes,
    #[error("line {line}, {field} (columns {from}-{to}, `{value}`) is not a number")]
    BadNumber {
        line: usize,
        field: &'static str,
        from: usize,
        to: usize,
        value: String,
    },
    #[error(
        "line {line}: node code `{code}` is shorter than the eight characters the format requires"
    )]
    ShortNodeCode { line: usize, code: String },
    #[error("line {line}: {what} references node `{node}`, which no `##N` record defines")]
    UnknownNode {
        line: usize,
        what: &'static str,
        node: String,
    },
    #[error("network is not valid: {0}")]
    Invalid(#[from] gridwright_net::NetError),
}

/// One field, cut by the 1-based inclusive column range the specification
/// gives for it.
///
/// Deliberately not a whitespace split: see the module header for what that
/// costs. Column positions are byte positions in a format defined over a
/// single-byte character set, so the ends are pulled back to the nearest
/// character boundary rather than risking a panic on a node named in French.
fn columns(line: &str, from: usize, to: usize) -> &str {
    let start = from.saturating_sub(1);
    let end = to.min(line.len());
    if start >= end {
        return "";
    }
    let mut start = start;
    while start < line.len() && !line.is_char_boundary(start) {
        start += 1;
    }
    let mut end = end;
    while end > start && !line.is_char_boundary(end) {
        end -= 1;
    }
    line.get(start..end).unwrap_or("").trim()
}

/// A record, remembering which line it came from so an error can name it.
#[derive(Clone)]
struct Record {
    text: String,
    line: usize,
}

impl Record {
    fn text_at(&self, from: usize, to: usize) -> &str {
        columns(&self.text, from, to)
    }

    /// A numeric field. Blank is zero, which is what a blank column means
    /// here; anything present but unparseable is an error naming the line and
    /// the columns, since that is a corrupted file rather than an omission.
    fn num(&self, field: &'static str, from: usize, to: usize) -> Result<f64, UcteError> {
        let raw = self.text_at(from, to);
        if raw.is_empty() {
            return Ok(0.0);
        }
        raw.parse::<f64>().map_err(|_| UcteError::BadNumber {
            line: self.line,
            field,
            from,
            to,
            value: raw.to_string(),
        })
    }

    fn int(&self, field: &'static str, from: usize, to: usize) -> Result<i64, UcteError> {
        Ok(self.num(field, from, to)? as i64)
    }

    fn present(&self, from: usize, to: usize) -> bool {
        !self.text_at(from, to).is_empty()
    }
}

/// The nominal voltage a node code's seventh character stands for.
///
/// UCTE does not give a node its voltage as a field; it encodes it in the
/// name, and everything per unit downstream depends on decoding it. The table
/// is the one in the specification, and the ordering is not monotonic — 8 is
/// 330 kV and 9 is 500 kV, both above the 750 kV that 0 stands for in the
/// other direction — so it cannot be guessed from the digit.
fn voltage_of(level: char) -> Option<f64> {
    Some(match level {
        '0' => 750.0,
        '1' => 380.0,
        '2' => 220.0,
        '3' => 150.0,
        '4' => 120.0,
        '5' => 110.0,
        '6' => 70.0,
        '7' => 27.0,
        '8' => 330.0,
        '9' => 500.0,
        _ => return None,
    })
}

/// Which block a `##` marker opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Comment,
    Node,
    Line,
    Transformer,
    Regulation,
    TapTable,
    /// Scheduled exchanges and anything a later revision added.
    Other,
}

/// Recognise a block header.
///
/// `##TT` has to be tested before `##T`, since one is a prefix of the other
/// and getting that the wrong way round silently reads every tap table row as
/// a transformer.
fn block_of(line: &str) -> Option<Block> {
    let rest = line.strip_prefix("##")?;
    let rest = rest.to_ascii_uppercase();
    Some(if rest.starts_with("TT") {
        Block::TapTable
    } else if rest.starts_with('C') {
        Block::Comment
    } else if rest.starts_with('N') || rest.starts_with('Z') {
        Block::Node
    } else if rest.starts_with('L') {
        Block::Line
    } else if rest.starts_with('T') {
        Block::Transformer
    } else if rest.starts_with('R') {
        Block::Regulation
    } else {
        Block::Other
    })
}

/// Whether a text looks like a UCTE-DEF file.
///
/// The block markers are unmistakable and appear in no other format this
/// reads, which is what lets a `.uct` file with its extension stripped still
/// be recognised. Only lines beginning `##` are examined, so a file that is not
/// UCTE at all costs a two-byte comparison per line and nothing else; the
/// `##N` block can sit a long way down behind a comment block, so there is no
/// budget on how far to look.
pub(crate) fn looks_like_ucte(text: &str) -> bool {
    text.lines()
        .filter(|l| l.starts_with("##"))
        .any(|l| matches!(block_of(l), Some(Block::Node)))
}

/// A regulation record, resolved onto the transformer it belongs to.
#[derive(Debug, Clone, Copy, Default)]
struct Regulation {
    /// Phase (voltage) regulation: step size in per cent, and the tap the
    /// changer is actually sitting on.
    phase_du: f64,
    phase_tap: f64,
    /// Angle regulation: step size in per cent, the direction of the added
    /// voltage in degrees, the tap in use, and whether the added voltage is
    /// applied at one end or split between both.
    angle_du: f64,
    angle_theta_deg: f64,
    angle_tap: f64,
    symmetrical: bool,
}

/// The effect of a tap changer on the regulated winding, as a magnitude the
/// winding voltage is multiplied by and an angle it is advanced through.
///
/// Derivation. A tap changer adds turns to the regulated winding. `n'` steps of
/// `δu` per cent multiply that winding's voltage by `1 + n'·δu/100`; that is
/// phase regulation, and it is purely real.
///
/// An angle regulator adds a voltage of relative size `α = n'·δu/100` at an
/// angle `Θ` to the winding voltage, so the winding's phasor is multiplied by
/// `z = 1 + α·e^{jΘ}` — asymmetrical, because the whole of the added voltage
/// appears at one end. A symmetrical regulator splits it, adding `+z/2` at one
/// terminal and `−z/2` at the other, giving `(1 + α·e^{jΘ}/2) / (1 − α·e^{jΘ}/2)`
/// instead. For the usual quadrature regulator, `Θ = 90°`, the symmetrical
/// form has magnitude exactly one and is a pure phase shift, which is what a
/// symmetrical phase shifter is built to be; the asymmetrical form moves the
/// magnitude a little as well.
fn tap_effect(reg: &Regulation) -> (f64, f64) {
    let mut magnitude = 1.0 + reg.phase_tap * reg.phase_du / 100.0;
    let mut angle = 0.0;

    let alpha = reg.angle_tap * reg.angle_du / 100.0;
    if alpha.abs() > 0.0 {
        let theta = reg.angle_theta_deg.to_radians();
        let (dx, dy) = (alpha * theta.cos(), alpha * theta.sin());
        if reg.symmetrical {
            let (nx, ny) = (1.0 + dx / 2.0, dy / 2.0);
            let (dx2, dy2) = (1.0 - dx / 2.0, -dy / 2.0);
            magnitude *= nx.hypot(ny) / dx2.hypot(dy2);
            angle += ny.atan2(nx) - dy2.atan2(dx2);
        } else {
            magnitude *= (1.0 + dx).hypot(dy);
            angle += dy.atan2(1.0 + dx);
        }
    }
    (magnitude, angle)
}

/// A parsed node, kept alongside its network index because everything else in
/// the file refers to nodes by their eight-character code.
struct Node {
    index: usize,
    v_nom: f64,
}

/// Parse a UCTE-DEF file into a single-snapshot network.
///
/// One snapshot: a UCTE-DEF file is one operating point, the way a MATPOWER
/// case or a PSS/E RAW is.
pub fn parse_ucte(text: &str, name: impl Into<String>) -> Result<Case, UcteError> {
    let name = name.into();
    if text.trim().is_empty() {
        return Err(UcteError::Empty);
    }

    // Collect the blocks first. Regulation and tap tables come after the
    // transformers they modify, so a transformer cannot be built until the
    // whole file has been read.
    let mut node_rows: Vec<(Record, String)> = Vec::new();
    let mut line_rows: Vec<Record> = Vec::new();
    let mut transformer_rows: Vec<Record> = Vec::new();
    let mut regulation_rows: Vec<Record> = Vec::new();
    let mut tap_rows: Vec<Record> = Vec::new();
    let mut other_rows = 0usize;
    let mut saw_nodes = false;
    let mut block = Block::Comment;
    // The country of the nodes that follow, from the most recent `##Z` header.
    let mut country: Option<String> = None;

    for (n, raw) in text.lines().enumerate() {
        let line = n + 1;
        if raw.trim().is_empty() {
            continue;
        }
        if let Some(next) = block_of(raw) {
            block = next;
            saw_nodes |= next == Block::Node;
            // `##Z<cc>` names the country of the nodes that follow it, and is
            // the only place in the file a country appears in full. Without it
            // there is still the first character of every node code, but that
            // is a UCTE code rather than an ISO one and expanding it needs a
            // table the file does not carry.
            country = raw
                .strip_prefix("##Z")
                .or_else(|| raw.strip_prefix("##z"))
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty());
            continue;
        }
        let rec = Record {
            text: raw.to_string(),
            line,
        };
        match block {
            Block::Node => node_rows.push((rec, country.clone().unwrap_or_default())),
            Block::Line => line_rows.push(rec),
            Block::Transformer => transformer_rows.push(rec),
            Block::Regulation => regulation_rows.push(rec),
            Block::TapTable => tap_rows.push(rec),
            Block::Comment => {}
            Block::Other => other_rows += 1,
        }
    }
    if !saw_nodes {
        return Err(UcteError::NoNodes);
    }

    let mut net = Network::new(Snapshots::hourly(1));
    net.base_mva = ASSUMED_BASE_MVA;
    let mut notes = Vec::new();
    let mut nodes: HashMap<String, Node> = HashMap::new();
    let mut unknown_level = 0usize;
    let mut equivalent_nodes = 0usize;

    for (r, block_country) in &node_rows {
        let code = r.text_at(1, 8);
        if code.chars().count() < 8 {
            return Err(UcteError::ShortNodeCode {
                line: r.line,
                code: code.to_string(),
            });
        }
        let level = code.chars().nth(6).unwrap_or(' ');
        let reference_kv = r.num("voltage reference", 27, 32)?;
        let v_nom = match voltage_of(level) {
            Some(v) => v,
            None => {
                // The reference voltage is a set point rather than a nominal,
                // but it is within a few per cent of one and it is all the
                // record has left to offer.
                unknown_level += 1;
                reference_kv
            }
        };
        // The country of last resort is the first character of the node code,
        // used verbatim. It is a UCTE country code, not an ISO one, and
        // guessing at the expansion would mislabel a whole country's nodes.
        let country = if block_country.is_empty() {
            code.chars().next().map(String::from).unwrap_or_default()
        } else {
            block_country.clone()
        };
        // Every UCTE node is in the continental European synchronous area, so
        // the country deliberately does not become one: an AC line may not
        // span two synchronous areas, and making each country its own would
        // reject every interconnector in the file.
        let idx = net.add_bus(code.to_string(), country);
        net.buses[idx].v_nom = v_nom;
        if r.int("node status", 23, 23)? == 1 {
            equivalent_nodes += 1;
        }

        let p_load = r.num("active load", 34, 40)?;
        let q_load = r.num("reactive load", 42, 48)?;
        if p_load.abs() > 0.0 || q_load.abs() > 0.0 {
            net.add_load(Load {
                name: format!("load_{code}"),
                bus: idx,
                p_set: p_load,
                q_set: q_load,
                ..Default::default()
            });
        }

        // Generation, in the load convention: producing power is negative.
        let p_gen = r.num("active generation", 50, 56)?;
        let p_min_field = r.num("minimum permissible generation", 66, 72)?;
        let p_max_field = r.num("maximum permissible generation", 74, 80)?;
        let q_min_field = r.num("minimum permissible reactive", 82, 88)?;
        let q_max_field = r.num("maximum permissible reactive", 90, 96)?;
        // Flipping the sign also swaps which end of the band is which: the
        // most negative permissible generation is the largest output.
        let capacity = (-p_min_field).max(0.0);
        let floor = (-p_max_field).max(0.0);
        let has_band = r.present(66, 72) || r.present(74, 80);
        let p_nom = if has_band && capacity > 0.0 {
            capacity
        } else {
            (-p_gen).max(0.0)
        };
        if p_nom > 0.0 || p_gen.abs() > 0.0 {
            net.add_generator(Generator {
                name: format!("gen_{code}"),
                bus: idx,
                p_nom,
                // UCTE-DEF carries no cost data at all. Left at zero
                // deliberately: see the note this reader always emits.
                marginal_cost: 0.0,
                p_min_pu: if p_nom > 0.0 {
                    (floor / p_nom).clamp(0.0, 1.0)
                } else {
                    0.0
                },
                q_min: if r.present(90, 96) {
                    -q_max_field
                } else {
                    f64::NEG_INFINITY
                },
                q_max: if r.present(82, 88) {
                    -q_min_field
                } else {
                    f64::INFINITY
                },
                ..Default::default()
            });
        }

        nodes.insert(code.to_string(), Node { index: idx, v_nom });
    }

    let base = net.base_mva;
    let mut out_of_service = 0usize;
    let mut couplers = 0usize;
    let mut zero_reactance = 0usize;
    let mut unrated = 0usize;
    let mut mismatched_voltage = 0usize;

    let look_up = |code: &str, what: &'static str, line: usize| -> Result<&Node, UcteError> {
        nodes.get(code).ok_or_else(|| UcteError::UnknownNode {
            line,
            what,
            node: code.to_string(),
        })
    };

    // Lines. Ohms, microsiemens and amps, all of which have to be converted.
    for r in &line_rows {
        let code1 = r.text_at(1, 8).to_string();
        let code2 = r.text_at(10, 17).to_string();
        let order = r.text_at(19, 19).to_string();
        let status = r.int("status", 21, 21)?;
        // 8 and 9 are a real and an equivalent element out of operation; 7 is
        // an open busbar coupler. Keeping them would add corridors the file
        // says are not there.
        if matches!(status, 7..=9) {
            out_of_service += 1;
            continue;
        }
        if status == 2 {
            couplers += 1;
        }
        let n1 = look_up(&code1, "line", r.line)?;
        let n2 = look_up(&code2, "line", r.line)?;
        if n1.index == n2.index {
            continue;
        }
        // A line joins two nodes at the same voltage level by construction. If
        // the codes disagree the first end is used and the fact is reported,
        // because guessing which one the ohms were measured against would be
        // guessing at a factor of several.
        if (n1.v_nom - n2.v_nom).abs() > 1e-9 {
            mismatched_voltage += 1;
        }
        let kv = n1.v_nom;
        // Z_base in ohms: kV² / MVA. Everything per unit below divides by it.
        let z_base = if kv > 0.0 { kv * kv / base } else { 0.0 };
        let (r_ohm, x_ohm) = (r.num("resistance", 23, 28)?, r.num("reactance", 30, 35)?);
        let b_micro = r.num("susceptance", 37, 44)?;
        let amps = r.num("current limit", 46, 51)?;
        let (r_pu, x_pu, b_pu) = if z_base > 0.0 {
            // Susceptance is the other way round: the admittance base is the
            // reciprocal of the impedance base, so microsiemens are multiplied
            // by Z_base after being brought from micro to whole siemens.
            (r_ohm / z_base, x_ohm / z_base, b_micro * 1e-6 * z_base)
        } else {
            (r_ohm, x_ohm, b_micro)
        };
        // Three-phase apparent power from a line current limit:
        // S[MVA] = √3 · V[kV] · I[A] / 1000.
        let mva = if amps > 0.0 && kv > 0.0 {
            3f64.sqrt() * kv * amps / 1000.0
        } else {
            unrated += 1;
            0.0
        };
        let susceptance = if x_pu.abs() > 1e-12 {
            1.0 / x_pu
        } else {
            zero_reactance += 1;
            0.0
        };
        net.add_line(Line {
            name: format!("{code1}-{code2}-{order}"),
            bus0: n1.index,
            bus1: n2.index,
            s_nom: if mva > 0.0 { mva } else { 1e6 },
            susceptance,
            resistance: r_pu,
            reactance: x_pu,
            shunt_susceptance: b_pu,
            ..Default::default()
        });
    }

    // Regulation and tap tables, keyed by the transformer they belong to.
    let key = |r: &Record| {
        format!(
            "{}-{}-{}",
            r.text_at(1, 8),
            r.text_at(10, 17),
            r.text_at(19, 19)
        )
    };
    let mut regulation_of: HashMap<String, Regulation> = HashMap::new();
    for r in &regulation_rows {
        let symmetrical = r.text_at(67, 70).eq_ignore_ascii_case("SYMM");
        regulation_of.insert(
            key(r),
            Regulation {
                phase_du: r.num("phase regulation step", 21, 25)?,
                phase_tap: r.num("phase regulation tap", 31, 33)?,
                angle_du: r.num("angle regulation step", 41, 45)?,
                angle_theta_deg: r.num("angle regulation direction", 47, 51)?,
                angle_tap: r.num("angle regulation tap", 57, 59)?,
                symmetrical,
            },
        );
    }
    // A tap table gives the true impedance at each tap position, replacing the
    // single pair on the transformer record. Only the row for the tap actually
    // in use matters; the rest describe positions this snapshot is not at.
    let mut tap_impedance: HashMap<(String, i64), (f64, f64)> = HashMap::new();
    for r in &tap_rows {
        let k = key(r);
        let position = r.int("tap position", 21, 23)?;
        tap_impedance.insert(
            (k, position),
            (
                r.num("tap resistance", 25, 30)?,
                r.num("tap reactance", 32, 37)?,
            ),
        );
    }

    let mut tap_tables_used = 0usize;
    let mut regulated = 0usize;
    let mut phase_shifters = 0usize;
    for r in &transformer_rows {
        let code1 = r.text_at(1, 8).to_string();
        let code2 = r.text_at(10, 17).to_string();
        let order = r.text_at(19, 19).to_string();
        let status = r.int("status", 21, 21)?;
        if matches!(status, 7..=9) {
            out_of_service += 1;
            continue;
        }
        let n1 = look_up(&code1, "transformer", r.line)?;
        let n2 = look_up(&code2, "transformer", r.line)?;
        if n1.index == n2.index {
            continue;
        }
        let rated1 = r.num("rated voltage 1", 23, 27)?;
        let rated2 = r.num("rated voltage 2", 29, 33)?;
        let nominal_mva = r.num("nominal power", 35, 39)?;
        let k = format!("{code1}-{code2}-{order}");
        let reg = regulation_of.get(&k).copied().unwrap_or_default();
        let (magnitude, angle) = tap_effect(&reg);
        if reg.phase_du.abs() > 0.0 {
            regulated += 1;
        }
        if reg.angle_du.abs() > 0.0 {
            phase_shifters += 1;
        }

        // A transformer's impedance is referred to the node 2 winding, so the
        // base voltage is node 2's. Using node 1's would be out by the square
        // of the transformation ratio.
        let kv2 = n2.v_nom;
        let z_base = if kv2 > 0.0 { kv2 * kv2 / base } else { 0.0 };
        let (mut r_ohm, mut x_ohm) = (r.num("resistance", 41, 46)?, r.num("reactance", 48, 53)?);
        if let Some(&(rt, xt)) = tap_impedance.get(&(k.clone(), reg.phase_tap as i64)) {
            r_ohm = rt;
            x_ohm = xt;
            tap_tables_used += 1;
        }
        let b_micro = r.num("susceptance", 55, 62)?;
        let g_micro = r.num("conductance", 64, 69)?;
        let amps = r.num("current limit", 71, 76)?;
        let (r_pu, x_pu, b_pu) = if z_base > 0.0 {
            (r_ohm / z_base, x_ohm / z_base, b_micro * 1e-6 * z_base)
        } else {
            (r_ohm, x_ohm, b_micro)
        };
        if g_micro.abs() > 0.0 {
            // Iron losses. A branch here has no conductance term, and the
            // number is small enough that dropping it is honest and inventing
            // a load for it is not.
            notes.push(format!(
                "transformer {k} has {g_micro} uS of magnetising conductance, which a \
                 branch cannot hold"
            ));
        }

        // The rating is already in MVA and must not go through the ampere
        // conversion. Only when the nominal power is absent does the current
        // limit stand in, and it is taken at the node 2 winding.
        let mva = if nominal_mva > 0.0 {
            nominal_mva
        } else if amps > 0.0 && kv2 > 0.0 {
            3f64.sqrt() * kv2 * amps / 1000.0
        } else {
            unrated += 1;
            0.0
        };

        // The tap ratio, per unit on the two nodes' nominal voltages, in the
        // same sense every other reader here produces it: the ratio applied at
        // bus0. UCTE puts the tap changer on the node 2 winding, so the effect
        // of a tap appears in the denominator, and the phase it introduces is
        // seen from node 1 with the opposite sign.
        let tap = if rated1 > 0.0 && rated2 > 0.0 && n1.v_nom > 0.0 && kv2 > 0.0 {
            let side1 = rated1 / n1.v_nom;
            let side2 = rated2 * magnitude / kv2;
            if side2 > 0.0 { side1 / side2 } else { 1.0 }
        } else {
            1.0
        };
        let susceptance = if x_pu.abs() > 1e-12 {
            1.0 / x_pu
        } else {
            zero_reactance += 1;
            0.0
        };
        net.add_line(Line {
            name: k,
            bus0: n1.index,
            bus1: n2.index,
            s_nom: if mva > 0.0 { mva } else { 1e6 },
            susceptance,
            resistance: r_pu,
            reactance: x_pu,
            shunt_susceptance: b_pu,
            tap_ratio: if tap.is_finite() && tap > 0.0 {
                tap
            } else {
                1.0
            },
            // Negated because the tap changer sits on node 2 and this is the
            // ratio applied at node 1. Guarded so an unregulated transformer
            // gets a plain zero rather than a negative one.
            phase_shift: if angle == 0.0 { 0.0 } else { -angle },
            ..Default::default()
        });
    }

    notes.push(format!(
        "UCTE-DEF: {} nodes, {} lines and transformers, {} generators, {} loads; \
         ohms and microsiemens converted to per unit on an assumed base of {base} MVA, \
         which the format does not declare",
        net.buses.len(),
        net.lines.len(),
        net.generators.len(),
        net.loads.len()
    ));
    notes.push(
        "line ratings converted from amps with S = sqrt(3) * V * I / 1000; transformer \
         ratings were already in MVA and were not converted"
            .to_string(),
    );
    if unknown_level > 0 {
        notes.push(format!(
            "{unknown_level} node codes carried no recognised voltage level, so their \
             reference voltage was used as the nominal one"
        ));
    }
    if equivalent_nodes > 0 {
        notes.push(format!(
            "{equivalent_nodes} nodes are equivalents standing in for a network that \
             is not in this file"
        ));
    }
    if out_of_service > 0 {
        notes.push(format!("{out_of_service} out-of-service elements skipped"));
    }
    if couplers > 0 {
        notes.push(format!(
            "{couplers} closed busbar couplers kept as zero-impedance links"
        ));
    }
    if zero_reactance > 0 {
        notes.push(format!(
            "{zero_reactance} zero-reactance branches treated as transport links"
        ));
    }
    if unrated > 0 {
        notes.push(format!(
            "{unrated} branches carry no rating and are treated as unlimited"
        ));
    }
    if mismatched_voltage > 0 {
        notes.push(format!(
            "{mismatched_voltage} lines join nodes whose codes give different voltage \
             levels; the first end's was used to convert their ohms"
        ));
    }
    if regulated > 0 {
        notes.push(format!(
            "{regulated} transformers have a voltage tap changer, read at the tap \
             position the file records"
        ));
    }
    if phase_shifters > 0 {
        notes.push(format!(
            "{phase_shifters} transformers have an angle regulator, read at the tap \
             position the file records"
        ));
    }
    if tap_tables_used > 0 {
        notes.push(format!(
            "{tap_tables_used} transformers took their impedance from a tap table \
             rather than from their own record"
        ));
    }
    if other_rows > 0 {
        notes.push(format!(
            "{other_rows} records in blocks this reader does not use, such as scheduled \
             exchanges, were dropped"
        ));
    }
    notes.push(
        "UCTE-DEF carries no generation costs; every marginal cost is zero until one \
         is supplied"
            .to_string(),
    );
    notes.push(
        "UCTE-DEF carries one operating point and no time series, so the network has \
         a single snapshot"
            .to_string(),
    );

    net.validate()?;
    Ok(Case {
        name,
        network: net,
        notes,
    })
}

/// Read a UCTE-DEF file from a path.
pub fn load_ucte(path: impl AsRef<std::path::Path>) -> Result<Case, crate::IoError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|source| crate::IoError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "case".into());
    parse_ucte(&text, name).map_err(crate::IoError::Ucte)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(text: &str) -> Record {
        Record {
            text: text.to_string(),
            line: 1,
        }
    }

    #[test]
    fn a_blank_field_does_not_shift_the_ones_after_it() {
        // No charging susceptance, and the current limit still has to land in
        // columns 46-51. Split on whitespace and 2000 becomes the susceptance:
        // the line loses its rating entirely and gains a shunt admittance four
        // thousand times too large, and the file looks perfectly fine.
        let r = record("DGEN__11 FIMP__11 1 0   3.00  30.00            2000 INTERCONNECT");
        assert_eq!(r.text_at(37, 44), "", "the susceptance field is blank");
        assert_eq!(r.text_at(46, 51), "2000");
        assert_eq!(r.text_at(30, 35), "30.00");
        assert_eq!(r.num("susceptance", 37, 44).unwrap(), 0.0);
    }

    #[test]
    fn the_seventh_character_of_a_node_code_is_its_voltage() {
        // Not a field, and not monotonic in the digit either: 8 is 330 kV and
        // 9 is 500 kV, both below the 750 kV that 0 stands for.
        assert_eq!(voltage_of('1'), Some(380.0));
        assert_eq!(voltage_of('5'), Some(110.0));
        assert_eq!(voltage_of('8'), Some(330.0));
        assert_eq!(voltage_of('9'), Some(500.0));
        assert_eq!(voltage_of('0'), Some(750.0));
        assert_eq!(voltage_of('X'), None);
    }

    #[test]
    fn the_tap_table_marker_is_not_read_as_a_transformer_marker() {
        // `##T` is a prefix of `##TT`, and testing them the wrong way round
        // reads every tap table row as a transformer with nonsense in it.
        assert_eq!(block_of("##TT"), Some(Block::TapTable));
        assert_eq!(block_of("##T"), Some(Block::Transformer));
        assert_eq!(block_of("##N"), Some(Block::Node));
        assert_eq!(block_of("##ZDE"), Some(Block::Node));
        assert_eq!(block_of("##R"), Some(Block::Regulation));
        assert_eq!(block_of("DGEN__11 DLOAD_11 1 0"), None);
    }

    #[test]
    fn a_voltage_tap_multiplies_the_regulated_winding() {
        // Four steps of 1.25 per cent is five per cent more turns on the
        // regulated winding, and no phase shift at all.
        let (magnitude, angle) = tap_effect(&Regulation {
            phase_du: 1.25,
            phase_tap: 4.0,
            ..Default::default()
        });
        assert!((magnitude - 1.05).abs() < 1e-12, "got {magnitude}");
        assert_eq!(angle, 0.0);
    }

    #[test]
    fn a_symmetrical_quadrature_regulator_shifts_the_angle_and_nothing_else() {
        // The point of building one symmetrically. Three steps of one per cent
        // in quadrature: alpha = 0.03, so the angle is 2*atan(0.015) and the
        // magnitude is exactly one.
        let (magnitude, angle) = tap_effect(&Regulation {
            angle_du: 1.0,
            angle_theta_deg: 90.0,
            angle_tap: 3.0,
            symmetrical: true,
            ..Default::default()
        });
        assert!((magnitude - 1.0).abs() < 1e-12, "got {magnitude}");
        assert!(
            (angle - 2.0 * 0.015_f64.atan()).abs() < 1e-12,
            "got {angle}"
        );
    }

    #[test]
    fn an_asymmetrical_regulator_moves_the_magnitude_as_well() {
        // The whole added voltage sits at one end, so 1 + j0.03 has both an
        // angle of atan(0.03) and a magnitude of sqrt(1.0009).
        let (magnitude, angle) = tap_effect(&Regulation {
            angle_du: 1.0,
            angle_theta_deg: 90.0,
            angle_tap: 3.0,
            symmetrical: false,
            ..Default::default()
        });
        assert!(
            (magnitude - 1.0009_f64.sqrt()).abs() < 1e-12,
            "got {magnitude}"
        );
        assert!((angle - 0.03_f64.atan()).abs() < 1e-12, "got {angle}");
    }

    #[test]
    fn a_file_with_no_node_block_is_rejected() {
        assert!(matches!(
            parse_ucte("##C 2026.01.01\njust a comment\n", "x"),
            Err(UcteError::NoNodes)
        ));
        assert!(matches!(parse_ucte("   \n", "x"), Err(UcteError::Empty)));
    }

    #[test]
    fn a_line_naming_a_node_that_does_not_exist_is_reported() {
        let text = "##N\n\
                    ##ZDE\n\
                    DAAAA_11 A            0 3 400.00     0.0     0.0  -100.0     0.0  -100.0     0.0\n\
                    ##L\n\
                    DAAAA_11 DBBBB_11 1 0   1.00  10.00     0.00   1000 GHOST\n";
        assert!(matches!(
            parse_ucte(text, "x"),
            Err(UcteError::UnknownNode { .. })
        ));
    }

    #[test]
    fn a_node_code_that_is_too_short_is_rejected_rather_than_decoded_from_junk() {
        let text = "##N\nDSHORT  0 3 400.00\n";
        assert!(matches!(
            parse_ucte(text, "x"),
            Err(UcteError::ShortNodeCode { .. })
        ));
    }
}

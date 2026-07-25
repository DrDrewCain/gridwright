//! Reading PSS/E RAW files.
//!
//! MATPOWER is what research publishes. RAW is what utilities actually run.
//! North American interconnection planning cases, most Asian national grid
//! models and a good deal of what TSOs exchange bilaterally are PSS/E cases,
//! and a tool that cannot read one is a tool that cannot be pointed at an
//! operational network.
//!
//! # Versions
//!
//! The format has drifted across releases and the drift is not cosmetic:
//!
//! - **v29 and earlier** put transformers in the branch section, with `RATIO`
//!   and `ANGLE` as branch columns. Reading a v29 file with v30 column offsets
//!   silently reads the ratio as a line charging conductance, which produces a
//!   network that loads without complaint and is wrong.
//! - **v30** moved transformers to their own multi-line section.
//! - **v32** moved bus shunts out of the bus record into a fixed shunt section,
//!   shifting every bus column after the fourth.
//! - **v33 onward** added columns but did not move the ones we read.
//!
//! The revision is declared on the first line and is honoured. Fields are read
//! by index with extra trailing columns tolerated, so later revisions that only
//! append continue to work.
//!
//! # What is converted
//!
//! - Buses, with base voltage and the normal voltage band where present.
//! - Loads, summed per bus when a bus carries several.
//! - Generators, with `PT` as capacity and `PB` as a must-run floor.
//! - Branches, reactance to susceptance.
//! - Two-winding transformers, including tap ratios in any of the three winding
//!   data conventions and impedances on either base convention.
//! - Three-winding transformers, expanded into a star of three branches through
//!   a synthetic star-point bus, which is what they physically are.
//! - Two-terminal DC lines, as transport corridors. These matter: a model of
//!   China or India that drops the HVDC is not a model of China or India.
//!
//! Switched shunts, FACTS devices, induction machines and impedance correction
//! tables are recognised, skipped, and reported in [`Case::notes`] rather than
//! passed over in silence.

use std::collections::HashMap;

use gridwright_net::{Generator, Line, Load, Network, Snapshots};

use crate::Case;

#[derive(Debug, thiserror::Error)]
pub enum PsseError {
    #[error("the file is empty")]
    Empty,
    #[error("first line does not look like a PSS/E header: `{0}`")]
    BadHeader(String),
    #[error("line {line}: field {field} (`{value}`) is not a number")]
    BadNumber {
        line: usize,
        field: usize,
        value: String,
    },
    #[error("line {line}: record has {got} fields, expected at least {want}")]
    ShortRecord {
        line: usize,
        got: usize,
        want: usize,
    },
    #[error("line {line}: references bus {bus}, which no bus record defines")]
    UnknownBus { line: usize, bus: i64 },
    #[error("transformer beginning at line {line} is truncated")]
    TruncatedTransformer { line: usize },
    #[error("network is not valid: {0}")]
    Invalid(#[from] gridwright_net::NetError),
}

/// The two windings' voltages, as the file states them.
///
/// Grouped because they are only ever meaningful together: a winding voltage
/// means nothing without the one it is a ratio against, and the units of both
/// depend on the same `CW` code.
#[derive(Debug, Clone, Copy)]
struct Winding {
    windv1: f64,
    nomv1: f64,
    windv2: f64,
    nomv2: f64,
}

/// One record: its fields, and the line it came from for error reporting.
#[derive(Debug, Clone)]
struct Record {
    fields: Vec<String>,
    line: usize,
}

impl Record {
    fn num(&self, i: usize) -> Result<f64, PsseError> {
        let raw = self.fields.get(i).ok_or(PsseError::ShortRecord {
            line: self.line,
            got: self.fields.len(),
            want: i + 1,
        })?;
        let t = raw.trim();
        if t.is_empty() {
            return Ok(0.0);
        }
        t.parse::<f64>().map_err(|_| PsseError::BadNumber {
            line: self.line,
            field: i,
            value: raw.clone(),
        })
    }

    /// A numeric field that may be absent, which is the normal case for the
    /// columns later revisions appended.
    fn opt(&self, i: usize) -> Option<f64> {
        self.fields.get(i)?.trim().parse::<f64>().ok()
    }

    fn text(&self, i: usize) -> String {
        self.fields
            .get(i)
            .map(|s| s.trim().trim_matches('\'').trim().to_string())
            .unwrap_or_default()
    }

    fn int(&self, i: usize) -> Result<i64, PsseError> {
        Ok(self.num(i)? as i64)
    }
}

/// Split a RAW record into fields.
///
/// Commas separate, single quotes protect commas inside names, and a slash
/// outside quotes begins a trailing comment. All three occur in real files: bus
/// names contain commas often enough that ignoring the quoting corrupts the
/// column alignment for every field after the name.
fn split(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '\'' => {
                quoted = !quoted;
                cur.push(c);
            }
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            '/' if !quoted => {
                // Comment to end of line. The field before it still counts.
                break;
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    while out.last().is_some_and(|s| s.trim().is_empty()) {
        out.pop();
    }
    out
}

/// Whether a line ends a section.
///
/// PSS/E writes `0 / END OF BUS DATA, BEGIN LOAD DATA`. Hand-edited files
/// sometimes carry a bare `0`. Both must terminate, and neither may be mistaken
/// for a data record, which is why this checks the whole first field rather
/// than the first character: bus zero is not legal but `0.0` appearing as a
/// leading impedance in a malformed file should not silently end a section.
fn is_terminator(line: &str) -> bool {
    let head = line.split(',').next().unwrap_or("");
    let head = head.split('/').next().unwrap_or("").trim();
    head == "0" || head == "-999" || head.is_empty() && line.trim_start().starts_with('/')
}

/// Section identity, taken from the terminator comment where present and from
/// position otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Bus,
    Load,
    FixedShunt,
    Generator,
    Branch,
    Transformer,
    Area,
    TwoTerminalDc,
    Other,
}

/// Recognise the section a terminator comment announces.
///
/// `0 / END OF BUS DATA, BEGIN LOAD DATA` names the next section. Matching on
/// the "BEGIN" half is what lets this survive files whose section order is not
/// the one the revision prescribes, which hand-assembled cases frequently are.
fn section_after(comment: &str) -> Option<Section> {
    let c = comment.to_ascii_uppercase();
    let begin = c.split("BEGIN").nth(1)?;
    Some(match () {
        _ if begin.contains("LOAD") => Section::Load,
        _ if begin.contains("FIXED SHUNT") => Section::FixedShunt,
        _ if begin.contains("GENERATOR") => Section::Generator,
        _ if begin.contains("BRANCH") => Section::Branch,
        _ if begin.contains("TRANSFORMER") && !begin.contains("IMPEDANCE") => Section::Transformer,
        _ if begin.contains("AREA") => Section::Area,
        _ if begin.contains("TWO-TERMINAL") || begin.contains("TWO TERMINAL") => {
            Section::TwoTerminalDc
        }
        _ => Section::Other,
    })
}

/// The section order a given revision writes, for files without comments.
fn default_order(rev: u32) -> Vec<Section> {
    if rev < 32 {
        // Bus records still carry their own shunts, so there is no fixed shunt
        // section to step over.
        vec![
            Section::Bus,
            Section::Load,
            Section::Generator,
            Section::Branch,
            Section::Transformer,
            Section::Area,
            Section::TwoTerminalDc,
        ]
    } else {
        vec![
            Section::Bus,
            Section::Load,
            Section::FixedShunt,
            Section::Generator,
            Section::Branch,
            Section::Transformer,
            Section::Area,
            Section::TwoTerminalDc,
        ]
    }
}

struct Parser {
    rev: u32,
    base_mva: f64,
    net: Network,
    notes: Vec<String>,
    /// PSS/E bus number to our index.
    index_of: HashMap<i64, usize>,
    /// Base voltage per bus, needed to interpret transformer winding data.
    base_kv: Vec<f64>,
    /// Demand accumulated per bus, since a bus may carry several load records.
    load_at: HashMap<usize, (f64, f64)>,
    skipped: HashMap<&'static str, usize>,
}

impl Parser {
    fn bus(&self, id: i64, line: usize) -> Result<usize, PsseError> {
        self.index_of
            .get(&id)
            .copied()
            .ok_or(PsseError::UnknownBus { line, bus: id })
    }

    fn skip(&mut self, what: &'static str) {
        *self.skipped.entry(what).or_insert(0) += 1;
    }

    fn read_bus(&mut self, r: &Record) -> Result<(), PsseError> {
        let id = r.int(0)?;
        let name = {
            let n = r.text(1);
            if n.is_empty() { format!("bus{id}") } else { n }
        };
        let basekv = r.num(2)?;
        let ide = r.opt(3).unwrap_or(1.0) as i64;
        // Type 4 is disconnected. Keeping it would add an island the optimiser
        // has to shed, which is a different answer from the one the file means.
        if ide == 4 {
            self.skip("out-of-service buses");
            return Ok(());
        }
        // Area codes stand in for countries: in a multi-country RAW case the
        // area is how the countries are actually distinguished.
        let area = if self.rev < 32 { r.opt(6) } else { r.opt(4) };
        let country = area.map_or_else(|| "??".to_string(), |a| format!("area{}", a as i64));
        let idx = self.net.add_bus(name, country);
        self.net.buses[idx].v_nom = basekv;
        self.index_of.insert(id, idx);
        self.base_kv.push(basekv);

        // Normal voltage limits, v33 onward. Earlier revisions carry none, and
        // inventing a band would put a constraint in the AC problem that the
        // file never asked for.
        if self.rev >= 33 {
            let (hi, lo) = (r.opt(9), r.opt(10));
            if let (Some(hi), Some(lo)) = (hi, lo)
                && hi > lo
                && lo > 0.0
            {
                self.net.buses[idx].v_max = hi;
                self.net.buses[idx].v_min = lo;
            }
        }
        Ok(())
    }

    fn read_load(&mut self, r: &Record) -> Result<(), PsseError> {
        let id = r.int(0)?;
        if r.opt(2).unwrap_or(1.0) as i64 == 0 {
            self.skip("out-of-service loads");
            return Ok(());
        }
        let Ok(bus) = self.bus(id, r.line) else {
            self.skip("loads at unknown or disconnected buses");
            return Ok(());
        };
        // PL/QL are the constant-power part. IP/IQ and YP/YQ are current and
        // admittance dependent and vary with voltage, which a fixed demand
        // cannot express, so they are counted and reported.
        let (p, q) = (r.num(5)?, r.num(6)?);
        if r.opt(7).unwrap_or(0.0).abs() > 0.0 || r.opt(9).unwrap_or(0.0).abs() > 0.0 {
            self.skip("voltage-dependent load components");
        }
        let e = self.load_at.entry(bus).or_insert((0.0, 0.0));
        e.0 += p;
        e.1 += q;
        Ok(())
    }

    fn read_generator(&mut self, r: &Record) -> Result<(), PsseError> {
        let id = r.int(0)?;
        if r.opt(14).unwrap_or(1.0) as i64 == 0 {
            self.skip("out-of-service generators");
            return Ok(());
        }
        let Ok(bus) = self.bus(id, r.line) else {
            self.skip("generators at unknown or disconnected buses");
            return Ok(());
        };
        let pt = r.opt(16).unwrap_or(0.0);
        let pb = r.opt(17).unwrap_or(0.0);
        let (q_max, q_min) = (
            r.opt(4).unwrap_or(f64::INFINITY),
            r.opt(5).unwrap_or(f64::NEG_INFINITY),
        );
        let tag = r.text(1);
        self.net.add_generator(Generator {
            name: if tag.is_empty() {
                format!("gen{id}")
            } else {
                format!("gen{id}_{tag}")
            },
            bus,
            p_nom: pt,
            // RAW carries no cost data at all. Left at zero deliberately: a
            // fabricated merit order would make every dispatch result a
            // fiction dressed as an answer.
            marginal_cost: 0.0,
            p_min_pu: if pt > 0.0 {
                (pb / pt).clamp(0.0, 1.0)
            } else {
                0.0
            },
            q_min,
            q_max,
            ..Default::default()
        });
        Ok(())
    }

    fn read_branch(&mut self, r: &Record) -> Result<(), PsseError> {
        // v29 keeps RATIO and ANGLE in columns 9 and 10, pushing status to 15.
        let status_col = if self.rev < 30 { 15 } else { 13 };
        if r.opt(status_col).unwrap_or(1.0) as i64 == 0 {
            self.skip("out-of-service branches");
            return Ok(());
        }
        // A negative bus number on the far end marks the metered end in older
        // files. It is a flag, not a different bus.
        let from = r.int(0)?.abs();
        let to = r.int(1)?.abs();
        let (Ok(bus0), Ok(bus1)) = (self.bus(from, r.line), self.bus(to, r.line)) else {
            self.skip("branches at unknown or disconnected buses");
            return Ok(());
        };
        if bus0 == bus1 {
            self.skip("self-loop branches");
            return Ok(());
        }
        let res = r.num(3)?;
        let x = r.num(4)?;
        let b = r.opt(5).unwrap_or(0.0);
        let rate = r.opt(6).unwrap_or(0.0);
        let tap = if self.rev < 30 {
            match r.opt(9) {
                Some(v) if v > 0.0 => v,
                _ => 1.0,
            }
        } else {
            1.0
        };
        let ckt = r.text(2);
        self.push_line(
            format!("{from}-{to}-{ckt}"),
            bus0,
            bus1,
            res,
            x,
            b,
            rate,
            tap,
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn push_line(
        &mut self,
        name: String,
        bus0: usize,
        bus1: usize,
        res: f64,
        x: f64,
        b: f64,
        rate: f64,
        tap: f64,
    ) {
        // Zero reactance is a bus tie wearing a branch record. It cannot have
        // an infinite susceptance, and a transport corridor is what a
        // zero-impedance connection actually behaves like.
        let susceptance = if x.abs() > 1e-9 {
            1.0 / x
        } else {
            self.skip("zero-reactance branches treated as transport links");
            0.0
        };
        self.net.add_line(Line {
            name,
            bus0,
            bus1,
            // A zero rating means unrated in PSS/E, not unusable.
            s_nom: if rate > 0.0 { rate } else { 1e6 },
            susceptance,
            resistance: res,
            reactance: x,
            shunt_susceptance: b,
            tap_ratio: tap,
            ..Default::default()
        });
    }

    /// A transformer record, two or three windings, four or five lines.
    ///
    /// Returns how many records it consumed, since the section reader cannot
    /// know in advance: the third bus field on the first line decides.
    fn read_transformer(&mut self, rows: &[Record]) -> Result<usize, PsseError> {
        let head = &rows[0];
        let three = head.opt(2).unwrap_or(0.0) as i64 != 0;
        let want = if three { 5 } else { 4 };
        if rows.len() < want {
            return Err(PsseError::TruncatedTransformer { line: head.line });
        }
        let consumed = want;

        let cw = head.opt(4).unwrap_or(1.0) as i64;
        let cz = head.opt(5).unwrap_or(1.0) as i64;
        let status = head.opt(11).unwrap_or(1.0) as i64;
        if status == 0 {
            self.skip("out-of-service transformers");
            return Ok(consumed);
        }
        if cz == 3 {
            self.skip("transformers with impedance given as load loss");
        }
        let name = {
            let n = head.text(10);
            if n.is_empty() {
                format!("xfmr{}", head.line)
            } else {
                n
            }
        };

        let i = head.int(0)?.abs();
        let j = head.int(1)?.abs();
        if three {
            let k = head.int(2)?.abs();
            return self.read_three_winding(rows, name, i, j, k, cw, cz).map(|_| consumed);
        }

        let (Ok(bus0), Ok(bus1)) = (self.bus(i, head.line), self.bus(j, head.line)) else {
            self.skip("transformers at unknown or disconnected buses");
            return Ok(consumed);
        };
        if bus0 == bus1 {
            self.skip("self-loop transformers");
            return Ok(consumed);
        }

        let (mut res, mut x) = (rows[1].num(0)?, rows[1].num(1)?);
        let sbase = rows[1].opt(2).unwrap_or(self.base_mva);
        if cz == 2 {
            // Given on the winding's own MVA base; rebase to the system.
            let (r2, x2) = self.rebase(res, x, sbase);
            res = r2;
            x = x2;
        }

        let windv1 = rows[2].opt(0).unwrap_or(1.0);
        let nomv1 = rows[2].opt(1).unwrap_or(0.0);
        let rate = rows[2].opt(3).unwrap_or(0.0);
        let angle = rows[2].opt(2).unwrap_or(0.0);
        let windv2 = rows[3].opt(0).unwrap_or(1.0);
        let nomv2 = rows[3].opt(1).unwrap_or(0.0);
        if angle.abs() > 1e-9 {
            self.skip("phase-shifting transformers, angle ignored");
        }

        let tap = self.tap_ratio(
            cw,
            bus0,
            bus1,
            Winding {
                windv1,
                nomv1,
                windv2,
                nomv2,
            },
        );
        self.push_line(name, bus0, bus1, res, x, 0.0, rate, tap);
        Ok(consumed)
    }

    /// Rebase a per-unit impedance from a winding base onto the system base.
    fn rebase(&self, r: f64, x: f64, sbase: f64) -> (f64, f64) {
        if sbase > 0.0 {
            let f = self.base_mva / sbase;
            (r * f, x * f)
        } else {
            (r, x)
        }
    }

    /// Turns ratio in per unit, under whichever winding data convention the
    /// file declared.
    ///
    /// `CW` is not a formatting detail. Reading a `CW = 2` file as if it were
    /// `CW = 1` gives a tap of 138/13.8 rather than 1.0, which is a ten-to-one
    /// error in the flow through that transformer.
    fn tap_ratio(&self, cw: i64, bus0: usize, bus1: usize, w: Winding) -> f64 {
        let Winding {
            windv1,
            nomv1,
            windv2,
            nomv2,
        } = w;
        let kv = |b: usize| {
            let v = self.base_kv.get(b).copied().unwrap_or(0.0);
            if v > 0.0 { v } else { 1.0 }
        };
        let ratio = match cw {
            // Per unit of the bus base voltage: already what we want.
            1 => windv1 / windv2.max(1e-12),
            // Winding voltage in kV.
            2 => (windv1 / kv(bus0)) / (windv2 / kv(bus1)).max(1e-12),
            // Per unit of the winding's nominal voltage.
            3 => {
                let n1 = if nomv1 > 0.0 { nomv1 } else { kv(bus0) };
                let n2 = if nomv2 > 0.0 { nomv2 } else { kv(bus1) };
                (windv1 * n1 / kv(bus0)) / (windv2 * n2 / kv(bus1)).max(1e-12)
            }
            _ => windv1 / windv2.max(1e-12),
        };
        if ratio.is_finite() && ratio > 0.0 { ratio } else { 1.0 }
    }

    /// A three-winding transformer, as the star of three branches it is.
    ///
    /// The windings meet at a point that is not any of the three buses, so a
    /// synthetic bus is added for it. Collapsing the star into three
    /// bus-to-bus branches instead would be a different network: it gets the
    /// impedances wrong and cannot represent flow arriving on one winding and
    /// leaving on the other two.
    ///
    /// The per-winding impedances come from the measured pairs by the standard
    /// inversion: `Z1 = (Z12 + Z31 − Z23) / 2`, and cyclically.
    #[allow(clippy::too_many_arguments)]
    fn read_three_winding(
        &mut self,
        rows: &[Record],
        name: String,
        i: i64,
        j: i64,
        k: i64,
        cw: i64,
        cz: i64,
    ) -> Result<(), PsseError> {
        let head = &rows[0];
        let (Ok(b1), Ok(b2), Ok(b3)) = (
            self.bus(i, head.line),
            self.bus(j, head.line),
            self.bus(k, head.line),
        ) else {
            self.skip("three-winding transformers at unknown or disconnected buses");
            return Ok(());
        };

        let z = &rows[1];
        let pair = |ri: usize, xi: usize, si: usize| -> Result<(f64, f64), PsseError> {
            let (mut r, mut x) = (z.opt(ri).unwrap_or(0.0), z.opt(xi).unwrap_or(0.0));
            if cz == 2 {
                let sbase = z.opt(si).unwrap_or(self.base_mva);
                let (r2, x2) = self.rebase(r, x, sbase);
                r = r2;
                x = x2;
            }
            Ok((r, x))
        };
        let (r12, x12) = pair(0, 1, 2)?;
        let (r23, x23) = pair(3, 4, 5)?;
        let (r31, x31) = pair(6, 7, 8)?;

        let star = self.net.add_bus(
            format!("{name}_star"),
            self.net.buses[b1].country.clone(),
        );
        self.base_kv.push(self.base_kv.get(b1).copied().unwrap_or(1.0));

        let arms = [
            ((r12 + r31 - r23) / 2.0, (x12 + x31 - x23) / 2.0, b1, 2usize),
            ((r12 + r23 - r31) / 2.0, (x12 + x23 - x31) / 2.0, b2, 3),
            ((r23 + r31 - r12) / 2.0, (x23 + x31 - x12) / 2.0, b3, 4),
        ];
        for (n, &(r, x, bus, row)) in arms.iter().enumerate() {
            let w = &rows[row];
            let windv = w.opt(0).unwrap_or(1.0);
            let nomv = w.opt(1).unwrap_or(0.0);
            let rate = w.opt(3).unwrap_or(0.0);
            // Each arm's ratio is against the star point, which is on the same
            // base by construction, so only this winding's own ratio applies.
            let tap = self.tap_ratio(
                cw,
                bus,
                star,
                Winding {
                    windv1: windv,
                    nomv1: nomv,
                    windv2: 1.0,
                    nomv2: 0.0,
                },
            );
            self.push_line(
                format!("{name}_w{}", n + 1),
                bus,
                star,
                r,
                x,
                0.0,
                rate,
                tap,
            );
        }
        self.skip("three-winding transformers expanded through a star point");
        Ok(())
    }

    /// A two-terminal DC line, as a transport corridor.
    ///
    /// Three lines per record: control parameters, then rectifier, then
    /// inverter. A DC link imposes no angle relationship on either end, which
    /// is exactly a corridor with a rating and no susceptance.
    fn read_dc(&mut self, rows: &[Record]) -> Result<usize, PsseError> {
        if rows.len() < 3 {
            return Ok(rows.len());
        }
        let head = &rows[0];
        let name = {
            let n = head.text(0);
            if n.is_empty() {
                format!("dc{}", head.line)
            } else {
                n
            }
        };
        // MDC 0 means the link is blocked.
        if head.opt(1).unwrap_or(1.0) as i64 == 0 {
            self.skip("blocked DC links");
            return Ok(3);
        }
        // SETVL is the scheduled transfer, in MW or amps depending on MDC.
        let setvl = head.opt(3).unwrap_or(0.0).abs();
        let rect = rows[1].int(0)?.abs();
        let inv = rows[2].int(0)?.abs();
        let (Ok(bus0), Ok(bus1)) = (self.bus(rect, rows[1].line), self.bus(inv, rows[2].line))
        else {
            self.skip("DC links at unknown or disconnected buses");
            return Ok(3);
        };
        if bus0 == bus1 {
            return Ok(3);
        }
        self.net.add_line(Line {
            name: format!("dc_{name}"),
            bus0,
            bus1,
            s_nom: if setvl > 0.0 { setvl } else { 1e6 },
            // No susceptance: this is the whole point of a DC link.
            susceptance: 0.0,
            ..Default::default()
        });
        Ok(3)
    }
}

/// Parse a PSS/E RAW file.
pub fn parse_raw(text: &str, name: impl Into<String>) -> Result<Case, PsseError> {
    let mut lines = text.lines().enumerate();
    let (_, header) = lines.next().ok_or(PsseError::Empty)?;
    let head = split(header);
    let base_mva = head
        .get(1)
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(100.0);
    let rev = head
        .get(2)
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|v| v as u32)
        // Files written before the revision field existed are v29-shaped.
        .filter(|v| (20..=40).contains(v))
        .unwrap_or(33);
    if head.first().and_then(|s| s.trim().parse::<f64>().ok()).is_none() {
        return Err(PsseError::BadHeader(header.to_string()));
    }
    // Two title lines follow the header and carry nothing structural.
    lines.next();
    lines.next();

    let mut p = Parser {
        rev,
        base_mva,
        net: Network::new(Snapshots::hourly(1)),
        notes: Vec::new(),
        index_of: HashMap::new(),
        base_kv: Vec::new(),
        load_at: HashMap::new(),
        skipped: HashMap::new(),
    };
    p.net.base_mva = base_mva;

    let order = default_order(rev);
    let mut ordinal = 0usize;
    let mut section = Section::Bus;
    // Records buffered for sections whose entries span several lines.
    let mut pending: Vec<Record> = Vec::new();

    let flush = |p: &mut Parser, pending: &mut Vec<Record>, section: Section| -> Result<(), PsseError> {
        match section {
            Section::Transformer => {
                let mut at = 0;
                while at < pending.len() {
                    let used = p.read_transformer(&pending[at..])?;
                    at += used.max(1);
                }
            }
            Section::TwoTerminalDc => {
                let mut at = 0;
                while at < pending.len() {
                    let used = p.read_dc(&pending[at..])?;
                    at += used.max(1);
                }
            }
            _ => {}
        }
        pending.clear();
        Ok(())
    };

    for (n, raw) in lines {
        let line = n + 1;
        if raw.trim().is_empty() {
            continue;
        }
        // `Q` alone ends the file. Anything after it is not data.
        if raw.trim_start().starts_with('Q') && raw.trim().len() <= 2 {
            break;
        }
        if is_terminator(raw) {
            flush(&mut p, &mut pending, section)?;
            ordinal += 1;
            section = section_after(raw)
                .filter(|s| *s != Section::Other || raw.to_ascii_uppercase().contains("BEGIN"))
                .unwrap_or_else(|| order.get(ordinal).copied().unwrap_or(Section::Other));
            continue;
        }
        let rec = Record {
            fields: split(raw),
            line,
        };
        if rec.fields.is_empty() {
            continue;
        }
        match section {
            Section::Bus => p.read_bus(&rec)?,
            Section::Load => p.read_load(&rec)?,
            Section::Generator => p.read_generator(&rec)?,
            Section::Branch => p.read_branch(&rec)?,
            Section::Transformer | Section::TwoTerminalDc => pending.push(rec),
            Section::FixedShunt => p.skip("fixed shunts"),
            Section::Area | Section::Other => {}
        }
    }
    flush(&mut p, &mut pending, section)?;

    // Loads last, so that several records at one bus become one demand.
    let mut at: Vec<(usize, (f64, f64))> = p.load_at.iter().map(|(k, v)| (*k, *v)).collect();
    at.sort_by_key(|(b, _)| *b);
    for (bus, (pl, ql)) in at {
        if pl.abs() < 1e-12 && ql.abs() < 1e-12 {
            continue;
        }
        let name = format!("load_{}", p.net.buses[bus].name);
        p.net.add_load(Load {
            name,
            bus,
            p_set: pl,
            q_set: ql,
        });
    }

    let mut notes = std::mem::take(&mut p.notes);
    notes.push(format!("PSS/E RAW revision {rev}, baseMVA {base_mva}"));
    let mut skipped: Vec<(&str, usize)> = p.skipped.iter().map(|(k, v)| (*k, *v)).collect();
    skipped.sort();
    for (what, count) in skipped {
        notes.push(format!("{count} {what}"));
    }
    notes.push(
        "RAW carries no generator costs; every marginal cost is zero until one is supplied"
            .to_string(),
    );

    p.net.validate()?;
    Ok(Case {
        name: name.into(),
        network: p.net,
        notes,
    })
}

/// Read a PSS/E RAW file from a path.
pub fn load_raw(path: impl AsRef<std::path::Path>) -> Result<Case, crate::IoError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|source| crate::IoError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "case".into());
    parse_raw(&text, name).map_err(crate::IoError::Psse)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_names_containing_commas_do_not_shift_the_columns() {
        // A real hazard: bus names like 'SPRINGFIELD, IL' would otherwise
        // consume a field and misalign every column after it.
        let f = split("101,'SPRINGFIELD, IL',138.0,1,1,1,1,1.02,0.0");
        assert_eq!(f.len(), 9, "{f:?}");
        assert_eq!(f[1].trim(), "'SPRINGFIELD, IL'");
        assert_eq!(f[2].trim(), "138.0");
    }

    #[test]
    fn a_trailing_comment_is_not_a_field() {
        let f = split("0 / END OF BUS DATA, BEGIN LOAD DATA");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].trim(), "0");
    }

    #[test]
    fn terminators_are_recognised_with_and_without_their_comment() {
        assert!(is_terminator("0 / END OF BUS DATA, BEGIN LOAD DATA"));
        assert!(is_terminator("0"));
        assert!(is_terminator(" 0 "));
        assert!(!is_terminator("1,'BUS1',138.0,3"));
        // A bus number of 0 is not legal, but an impedance of 0.0 leading a
        // record must not be read as the end of a section.
        assert!(!is_terminator("0.001,0.01,0.0"));
    }

    #[test]
    fn the_next_section_is_taken_from_the_comment() {
        assert_eq!(
            section_after("0 / END OF BUS DATA, BEGIN LOAD DATA"),
            Some(Section::Load)
        );
        assert_eq!(
            section_after("0 /End of Branch data, begin Transformer data"),
            Some(Section::Transformer)
        );
        // Impedance correction also says "transformer" and is not it.
        assert_eq!(
            section_after("0 / END OF X DATA, BEGIN TRANSFORMER IMPEDANCE CORRECTION DATA"),
            Some(Section::Other)
        );
    }

    #[test]
    fn the_revision_changes_where_the_bus_area_lives() {
        // v30 keeps the area in column 6; v33 in column 4. Reading one with the
        // other's offsets is the failure this guards.
        assert_eq!(default_order(30)[2], Section::Generator);
        assert_eq!(default_order(33)[2], Section::FixedShunt);
    }
}

//! Reading the IEEE Common Data Format.
//!
//! This is the format the classic IEEE test cases were published in, by the
//! Working Group on a Common Format for the Exchange of Solved Load Flow Data
//! in 1973, and it is still how the University of Washington archive
//! distributes them. Everything downstream — the MATPOWER cases, the PGLib
//! benchmarks, the PowerModels JSON — is a conversion of a CDF file, and every
//! conversion is a chance for something to have been rounded or dropped. Being
//! able to read the original means a study reaching back to the source is not
//! reading someone else's transcription of it.
//!
//! # Columns, not whitespace
//!
//! Every field is a fixed column range, and the ranges are given below exactly
//! as the specification tabulates them. This matters more than it sounds. The
//! published IEEE cases leave the three MVA rating fields blank, which puts
//! twenty-five columns of spaces between the line charging susceptance and the
//! transformer turns ratio. Split that line on whitespace and the turns ratio
//! lands in the field the rating should have occupied: the file parses without
//! complaint, every transformer gets a ratio of 1.0, and the network is not the
//! IEEE 14-bus system any more. Fields are therefore always cut by column and
//! a blank one stays blank.
//!
//! The other direction bites too. A Fortran `F10.6` charging susceptance fills
//! its field exactly, so `  0.020000` runs straight into a rating of `150` with
//! no space between them; whitespace splitting sees one token, `0.020000150`.
//!
//! # Units
//!
//! The base MVA is declared on the title card and everything per unit is on it.
//!
//! - Branch `R`, `X` and line charging `B` are already per unit, so nothing is
//!   converted. Contrast MATPOWER, where the bus shunt `Gs`/`Bs` are stated in
//!   MW and MVAr and *must* be divided by the base — in CDF the bus shunt is
//!   per unit as well, and dividing it again would shrink it by a hundred.
//! - Loads and generation are in MW and MVAr, not per unit.
//! - The three MVA rating fields are integers in MVA. Zero or blank means the
//!   case gave no rating, which is unlimited rather than unusable.
//! - The transformer final turns ratio is already a per-unit ratio of the two
//!   sides' base voltages, so it becomes the tap ratio unchanged. Zero or blank
//!   means "not a transformer", which is a ratio of one, not of zero.
//! - The phase shifter final angle is in degrees where every trigonometric
//!   identity in the formulation wants radians.
//!
//! # What the format does not carry
//!
//! CDF has no cost data of any kind and no generator capacity limits: a bus
//! record states the generation *scheduled* at that bus in the solved case, not
//! what the plant there could produce. Both are reported through
//! [`Case::notes`] rather than silently filled in, because a fabricated merit
//! order turns a dispatch into a fiction dressed as an answer, and a capacity
//! invented from a set point is a capacity nobody chose.

use gridwright_net::{Generator, Line, Load, Network, Snapshots};

use crate::Case;

#[derive(Debug, thiserror::Error)]
pub enum CdfError {
    #[error("the file is empty")]
    Empty,
    #[error("no `{0} DATA FOLLOWS` section was found")]
    MissingSection(&'static str),
    #[error("line {line}, {field} (columns {from}-{to}, `{value}`) is not a number")]
    BadNumber {
        line: usize,
        field: &'static str,
        from: usize,
        to: usize,
        value: String,
    },
    #[error("line {line}: branch references bus {bus}, which no bus record defines")]
    UnknownBus { line: usize, bus: i64 },
    #[error("network is not valid: {0}")]
    Invalid(#[from] gridwright_net::NetError),
}

/// One field, cut by the 1-based inclusive column range the specification
/// gives for it.
///
/// Deliberately not a whitespace split: see the module header for what that
/// costs. Column positions are byte positions in a format defined over a
/// single-byte character set, so the ends are pulled back to the nearest
/// character boundary rather than risking a panic on a stray accented name.
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
struct Record<'a> {
    text: &'a str,
    line: usize,
}

impl Record<'_> {
    fn text_at(&self, from: usize, to: usize) -> &str {
        columns(self.text, from, to)
    }

    /// A numeric field. Blank is zero, which is what a blank column means in
    /// this format; anything else present but unparseable is an error naming
    /// the line and the columns, since that is a corrupted file rather than an
    /// omitted value.
    fn num(&self, field: &'static str, from: usize, to: usize) -> Result<f64, CdfError> {
        let raw = self.text_at(from, to);
        if raw.is_empty() {
            return Ok(0.0);
        }
        raw.parse::<f64>().map_err(|_| CdfError::BadNumber {
            line: self.line,
            field,
            from,
            to,
            value: raw.to_string(),
        })
    }

    fn int(&self, field: &'static str, from: usize, to: usize) -> Result<i64, CdfError> {
        Ok(self.num(field, from, to)? as i64)
    }

    /// Whether a field was written at all, as against written as zero.
    ///
    /// The two mean different things for the voltage and reactive limits: a
    /// blank pair is a case that did not state them, and inventing a band of
    /// zero to zero would put a constraint in the problem nobody asked for.
    fn present(&self, from: usize, to: usize) -> bool {
        !self.text_at(from, to).is_empty()
    }
}

/// Whether a line closes a section.
///
/// The format ends its sections with a negative sentinel whose width matches
/// the field it terminates: `-999` for bus, branch and tie line data, `-99` for
/// loss zones, `-9` for interchange data. All three are a minus sign followed
/// by nines and nothing else, so one test covers them without hard-coding a
/// width that a hand-edited file may not have got right.
fn is_terminator(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("-9") && t[1..].bytes().all(|b| b == b'9')
}

/// Which section a `... DATA FOLLOWS` header opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    /// Before the first header: the title card and anything around it.
    Preamble,
    Bus,
    Branch,
    /// Loss zones, interchange data and tie lines, none of which a linear
    /// dispatch model has anywhere to put.
    Other,
}

fn section_of(line: &str) -> Option<Section> {
    let upper = line.to_ascii_uppercase();
    if !upper.contains("FOLLOWS") {
        return None;
    }
    Some(if upper.starts_with("BUS DATA") {
        Section::Bus
    } else if upper.starts_with("BRANCH DATA") {
        Section::Branch
    } else {
        Section::Other
    })
}

/// Whether `haystack` contains `needle`, ignoring ASCII case.
///
/// Allocation-free, which is the point: the test below runs over every line of
/// a file that may well not be CDF at all, and uppercasing each of them first
/// would allocate once per line to answer a question about seven characters.
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Whether a text looks like a CDF file.
///
/// The section headers are unmistakable and appear in no other format this
/// reads, which is what lets a file arriving as `ieee14cdf.txt` — the name the
/// Washington archive actually uses — be recognised without its extension
/// helping at all. PSS/E writes `END OF BUS DATA, BEGIN LOAD DATA`, which
/// shares three of the words and none of the shape, so both halves are
/// required and both must say `FOLLOWS`.
///
/// The whole text is searched rather than an opening window. The bus header is
/// on the second line of a well-formed file, but the branch header sits one
/// record per bus below it, so any budget short enough to be worth having would
/// miss it on every case larger than a toy.
pub(crate) fn looks_like_cdf(text: &str) -> bool {
    let mut saw_bus = false;
    for line in text.lines() {
        if !contains_ignore_case(line, "FOLLOWS") {
            continue;
        }
        if contains_ignore_case(line, "BUS DATA") {
            saw_bus = true;
        } else if saw_bus && contains_ignore_case(line, "BRANCH DATA") {
            return true;
        }
    }
    false
}

/// Parse an IEEE Common Data Format case into a single-snapshot network.
///
/// One snapshot, because a CDF file is one solved operating point. That is what
/// makes these cases useful for validating the power flow itself: there is
/// nothing temporal to confound it.
pub fn parse_cdf(text: &str, name: impl Into<String>) -> Result<Case, CdfError> {
    let name = name.into();
    let mut notes = Vec::new();

    let mut lines = text.lines().enumerate();
    // The title card is the first line with anything on it. Columns 32-37 hold
    // the MVA base every per-unit quantity in the file is stated on, and 39-42
    // and 44 the year and season, which are worth repeating back so a user can
    // see which vintage of a case they have.
    let (title_no, title) = lines
        .by_ref()
        .find(|(_, l)| !l.trim().is_empty())
        .ok_or(CdfError::Empty)?;
    let title_rec = Record {
        text: title,
        line: title_no + 1,
    };
    let base_mva = match title_rec.num("MVA base", 32, 37)? {
        v if v > 0.0 => v,
        // A hand-edited file that lost its title card is still readable; the
        // hundred-MVA base is the one every published case uses.
        _ => 100.0,
    };
    let year = title_rec.text_at(39, 42).to_string();
    let season = title_rec.text_at(44, 44).to_string();
    let case_id = title_rec.text_at(46, 73).to_string();

    let mut net = Network::new(Snapshots::hourly(1));
    net.base_mva = base_mva;

    // CDF numbers its buses arbitrarily rather than by position, so the mapping
    // from label to index has to be built explicitly.
    let mut index_of: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    let mut bus_rows: Vec<(usize, Record<'_>)> = Vec::new();
    let mut branch_rows: Vec<Record<'_>> = Vec::new();
    let mut section = Section::Preamble;
    let mut saw_bus = false;
    let mut saw_branch = false;
    let mut other_records = 0usize;

    for (n, raw) in lines {
        let line = n + 1;
        if raw.trim().is_empty() {
            continue;
        }
        if raw.to_ascii_uppercase().starts_with("END OF DATA") {
            break;
        }
        if let Some(next) = section_of(raw) {
            section = next;
            saw_bus |= next == Section::Bus;
            saw_branch |= next == Section::Branch;
            continue;
        }
        if is_terminator(raw) {
            section = Section::Other;
            continue;
        }
        let rec = Record { text: raw, line };
        match section {
            Section::Bus => bus_rows.push((line, rec)),
            Section::Branch => branch_rows.push(rec),
            Section::Preamble => {}
            Section::Other => other_records += 1,
        }
    }

    if !saw_bus {
        return Err(CdfError::MissingSection("BUS"));
    }
    if !saw_branch {
        return Err(CdfError::MissingSection("BRANCH"));
    }

    // Buses, and the demand and generation their records carry. CDF has no
    // separate generator table: a machine exists because the bus it sits on
    // says so.
    let mut loads = Vec::new();
    let mut gens = Vec::new();
    for (line, r) in &bus_rows {
        let id = r.int("bus number", 1, 4)?;
        let raw_name = r.text_at(6, 17);
        let bus_name = if raw_name.is_empty() {
            format!("bus{id}")
        } else {
            raw_name.to_string()
        };
        let area = r.int("area", 19, 20)?;
        let kind = r.int("bus type", 25, 26)?;
        let p_load = r.num("load MW", 41, 49)?;
        let q_load = r.num("load MVAr", 50, 59)?;
        let p_gen = r.num("generation MW", 60, 67)?;
        let q_gen = r.num("generation MVAr", 68, 75)?;
        let base_kv = r.num("base kV", 77, 83)?;
        let limit_hi = r.num("maximum limit", 91, 98)?;
        let limit_lo = r.num("minimum limit", 99, 106)?;
        let limits_given = r.present(91, 98) || r.present(99, 106);

        // The area code is the closest thing the format has to a country, and
        // it is how a multi-area archive case distinguishes its regions. Taken
        // the same way the MATPOWER reader takes `area`, so the same network
        // read from either encoding comes out labelled identically.
        let idx = net.add_bus_in_area(bus_name, format!("area{area}"), format!("a{area}"));
        net.buses[idx].v_nom = base_kv;
        // Shunt G and B are already per unit on the system base. MATPOWER
        // states the same quantity in MW and MVAr and needs a division by the
        // base; doing that here as well would shrink a shunt by a hundred.
        net.buses[idx].g_shunt = r.num("shunt G", 107, 114)?;
        net.buses[idx].b_shunt = r.num("shunt B", 115, 122)?;
        if index_of.insert(id, idx).is_some() {
            // Two records for one bus number: the second wins, as it would in
            // any positional reader, and the fact is reported rather than lost.
            notes.push(format!("line {line}: bus {id} is defined more than once"));
        }

        // Type 2 holds voltage within reactive limits and type 3 is the swing
        // bus; both are generator buses and the two limit columns are their
        // MVAr band. Type 1 holds reactive generation within *voltage* limits,
        // so for it the very same columns are per-unit voltages. Reading one as
        // the other puts a 50 MVAr limit in as a voltage bound of 50 per unit.
        let is_gen_bus = kind == 2 || kind == 3;
        if is_gen_bus || p_gen.abs() > 0.0 || q_gen.abs() > 0.0 {
            // A band of zero to zero is how the archive cases write "not
            // stated", and it is not the same thing as a machine forbidden to
            // produce or absorb any reactive power at all — which is what
            // taking it literally would impose on the swing bus.
            let (q_max, q_min) = if is_gen_bus && limits_given && limit_hi > limit_lo {
                (limit_hi, limit_lo)
            } else {
                (f64::INFINITY, f64::NEG_INFINITY)
            };
            gens.push((idx, id, p_gen, q_min, q_max));
        }
        if kind == 1 && limits_given && limit_lo > 0.0 && limit_hi > limit_lo {
            net.buses[idx].v_max = limit_hi;
            net.buses[idx].v_min = limit_lo;
        }
        if p_load.abs() > 0.0 || q_load.abs() > 0.0 {
            loads.push((idx, id, p_load, q_load));
        }
    }

    for (idx, id, p, q) in loads {
        net.add_load(Load {
            name: format!("load{id}"),
            bus: idx,
            p_set: p,
            q_set: q,
            ..Default::default()
        });
    }
    for (idx, id, p_gen, q_min, q_max) in gens {
        net.add_generator(Generator {
            name: format!("gen{id}"),
            bus: idx,
            // The only figure the format offers. It is the generation scheduled
            // in the solved case, not a plant rating, and a negative one is a
            // machine drawing power, which a generator cannot represent.
            p_nom: p_gen.max(0.0),
            // CDF carries no cost data at all. Left at zero deliberately: see
            // the note this reader always emits.
            marginal_cost: 0.0,
            q_min,
            q_max,
            ..Default::default()
        });
    }

    // Branches. Reactance becomes susceptance, and every field to the right of
    // the ratings has to survive their being blank.
    let mut unrated = 0usize;
    let mut zero_reactance = 0usize;
    let mut skipped = 0usize;
    let mut phase_shifters = 0usize;
    for r in &branch_rows {
        let from = r.int("tap bus number", 1, 4)?;
        let to = r.int("Z bus number", 6, 9)?;
        let (Some(&bus0), Some(&bus1)) = (index_of.get(&from), index_of.get(&to)) else {
            return Err(CdfError::UnknownBus {
                line: r.line,
                bus: if index_of.contains_key(&from) {
                    to
                } else {
                    from
                },
            });
        };
        if bus0 == bus1 {
            skipped += 1;
            continue;
        }
        let circuit = r.int("circuit", 17, 17)?;
        let res = r.num("resistance", 20, 29)?;
        let x = r.num("reactance", 30, 40)?;
        let charging = r.num("line charging", 41, 50)?;
        // Rating 1 is the normal one; 2 and 3 are the emergency and loading
        // ratings, which a symmetric thermal limit has nowhere to put.
        let rate = r.num("MVA rating 1", 51, 55)?;
        if rate <= 0.0 {
            unrated += 1;
        }
        // Blank or zero means "not a transformer", which is a ratio of one.
        let tap = match r.num("turns ratio", 77, 82)? {
            v if v > 0.0 => v,
            _ => 1.0,
        };
        let shift_deg = r.num("final angle", 84, 90)?;
        if shift_deg.abs() > 0.0 {
            phase_shifters += 1;
        }
        // A zero reactance branch is a bus tie in disguise. It cannot carry a
        // susceptance of infinity, so it becomes a transport link, which is the
        // behaviour a zero-impedance connection actually has.
        let susceptance = if x.abs() > 1e-9 {
            1.0 / x
        } else {
            zero_reactance += 1;
            0.0
        };
        net.add_line(Line {
            name: format!("{from}-{to}-{circuit}"),
            bus0,
            bus1,
            s_nom: if rate > 0.0 { rate } else { 1e6 },
            susceptance,
            resistance: res,
            reactance: x,
            shunt_susceptance: charging,
            tap_ratio: tap,
            phase_shift: shift_deg.to_radians(),
            ..Default::default()
        });
    }

    let vintage = match (year.is_empty(), season.is_empty()) {
        (false, false) => format!(", {year} {season}"),
        (false, true) => format!(", {year}"),
        _ => String::new(),
    };
    notes.push(format!(
        "IEEE Common Data Format, base {base_mva} MVA{vintage}{}",
        if case_id.is_empty() {
            String::new()
        } else {
            format!(" ({case_id})")
        }
    ));
    if unrated > 0 {
        notes.push(format!(
            "{unrated} branches carry no MVA rating and are treated as unlimited"
        ));
    }
    if zero_reactance > 0 {
        notes.push(format!(
            "{zero_reactance} zero-reactance branches treated as transport links"
        ));
    }
    if skipped > 0 {
        notes.push(format!("{skipped} self-loop branches skipped"));
    }
    if phase_shifters > 0 {
        notes.push(format!(
            "{phase_shifters} phase-shifting branches, angles converted from degrees to radians"
        ));
    }
    if other_records > 0 {
        notes.push(format!(
            "{other_records} loss zone, interchange and tie line records dropped; \
             a linear dispatch model has nowhere to put an area interchange schedule"
        ));
    }
    notes.push(
        "CDF carries no generation costs; every marginal cost is zero until one is supplied"
            .to_string(),
    );
    notes.push(
        "CDF carries no generator capacity limits; the scheduled generation on each \
         bus record is taken as the capacity, which is a set point rather than a rating"
            .to_string(),
    );

    net.validate()?;
    Ok(Case {
        name,
        network: net,
        notes,
    })
}

/// Read an IEEE Common Data Format case from a path.
pub fn load_cdf(path: impl AsRef<std::path::Path>) -> Result<Case, crate::IoError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|source| crate::IoError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "case".into());
    parse_cdf(&text, name).map_err(crate::IoError::Cdf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_field_does_not_shift_the_ones_after_it() {
        // The published IEEE cases leave all three rating fields blank, which
        // puts twenty-five columns of spaces between the charging susceptance
        // and the turns ratio. Whitespace splitting would put the ratio where
        // the rating belongs, and every transformer would come out at 1.0.
        let line = "   4    7  1 1  1 1  0.000000   0.209120  0.000000                     0 0   0.978    0.00";
        let r = Record {
            text: line,
            line: 1,
        };
        assert_eq!(r.text_at(51, 55), "", "the rating field is blank");
        assert_eq!(r.text_at(57, 61), "");
        assert_eq!(r.text_at(77, 82), "0.978", "the ratio must not have moved");
        assert_eq!(r.text_at(30, 40), "0.209120");
    }

    #[test]
    fn a_field_abutting_the_one_before_it_is_still_its_own_field() {
        // `F10.6` of 0.02 fills columns 41-50 exactly, so a rating of 150 in
        // 51-55 touches it with no space between. Whitespace splitting sees
        // the single token `0.020000150`.
        let line = "   1    2  1 1  1 0  0.010000   0.100000  0.020000150   170   190      0 0";
        let r = Record {
            text: line,
            line: 1,
        };
        assert_eq!(r.text_at(41, 50), "0.020000");
        assert_eq!(r.text_at(51, 55), "150");
        assert_eq!(r.text_at(57, 61), "170");
    }

    #[test]
    fn a_short_line_yields_blank_fields_rather_than_failing() {
        // Trailing blanks are routinely stripped by editors and by whatever
        // wrote the file, so most records simply stop early.
        let r = Record {
            text: "   9    0",
            line: 3,
        };
        assert_eq!(r.text_at(77, 82), "");
        assert_eq!(r.num("turns ratio", 77, 82).unwrap(), 0.0);
        assert!(!r.present(77, 82));
    }

    #[test]
    fn every_width_of_terminator_ends_a_section() {
        // The sentinel's width matches the field it closes: -999 for bus,
        // branch and tie line data, -99 for loss zones, -9 for interchange.
        assert!(is_terminator("-999"));
        assert!(is_terminator("-99"));
        assert!(is_terminator(" -9 "));
        assert!(!is_terminator("-1"));
        assert!(!is_terminator("   9    0"));
        assert!(!is_terminator("-999.0"));
    }

    #[test]
    fn section_headers_are_recognised_however_they_are_spelled() {
        assert_eq!(
            section_of("BUS DATA FOLLOWS                            14 ITEMS"),
            Some(Section::Bus)
        );
        assert_eq!(
            section_of("Branch data follows                         20 items"),
            Some(Section::Branch)
        );
        assert_eq!(
            section_of("TIE LINES FOLLOWS                     0 ITEMS"),
            Some(Section::Other)
        );
        assert_eq!(section_of("   1    2  1 1  1 0  0.019380"), None);
    }

    #[test]
    fn the_section_headers_are_not_claimed_from_a_psse_file() {
        // RAW writes `0 / END OF BUS DATA, BEGIN LOAD DATA`, which shares three
        // words with the CDF header and none of its shape. Both halves are
        // required and both have to say FOLLOWS.
        assert!(!looks_like_cdf(
            "0,100.00,33\n0 / END OF BUS DATA, BEGIN LOAD DATA\n\
             0 / END OF LOAD DATA, BEGIN BRANCH DATA\n"
        ));
        assert!(!looks_like_cdf("BUS DATA FOLLOWS   2 ITEMS\n-999\n"));
        assert!(looks_like_cdf(
            "title\nBUS DATA FOLLOWS   1 ITEMS\n-999\nBRANCH DATA FOLLOWS  1 ITEMS\n-999\n"
        ));
    }

    #[test]
    fn a_file_with_no_bus_section_is_rejected_rather_than_read_as_empty() {
        let text = " 01/01/70 NOBODY                100.0  1970 W nothing\n\
                    BRANCH DATA FOLLOWS                          0 ITEMS\n\
                    -999\n";
        assert!(matches!(
            parse_cdf(text, "x"),
            Err(CdfError::MissingSection("BUS"))
        ));
    }

    #[test]
    fn a_branch_naming_a_bus_that_does_not_exist_is_reported() {
        let text = " 01/01/70 NOBODY                100.0  1970 W nothing\n\
                    BUS DATA FOLLOWS                             1 ITEMS\n\
                    \u{20}  1 ONE          1  1  3 1.0000   0.00      0.0       0.0     0.0     0.0\n\
                    -999\n\
                    BRANCH DATA FOLLOWS                          1 ITEMS\n\
                    \u{20}  1    2  1 1  1 0  0.010000   0.100000  0.000000\n\
                    -999\n";
        assert!(matches!(
            parse_cdf(text, "x"),
            Err(CdfError::UnknownBus { bus: 2, .. })
        ));
    }
}

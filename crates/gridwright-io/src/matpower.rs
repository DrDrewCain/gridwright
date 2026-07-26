//! Reading MATPOWER case files.
//!
//! MATPOWER's `.m` format is the lingua franca of power systems research. The
//! IEEE test cases, the PGLib-OPF benchmark library, RTE's French network and
//! the PEGASE European models are all distributed this way, most of them under
//! CC-BY. Being able to read it means the engine can be pointed at real
//! networks with published reference solutions instead of at topologies we
//! invented and then graded ourselves against.
//!
//! Only the sections a DC model needs are read: `baseMVA`, `bus`, `gen`,
//! `branch` and `gencost`. Reactive power, voltage magnitudes and shunt
//! admittances are parsed and ignored, because a linear DC formulation has
//! nowhere to put them. That is a real limitation and is stated rather than
//! papered over: results here are DC-OPF results, comparable to other DC-OPF
//! results, and not to an AC solution.
//!
//! # Conversions
//!
//! - Susceptance for DC flow is `1/x`, from the branch reactance.
//! - A `rateA` of zero means unlimited in MATPOWER, not forbidden, so it maps
//!   to an effectively infinite rating rather than to a line nobody can use.
//! - Generator cost comes from the linear term of the `gencost` polynomial.
//!   Quadratic costs are approximated by their linear coefficient, which is
//!   noted per case rather than silently applied.
//! - Out-of-service rows, flagged by a zero status, are skipped entirely.

use gridwright_net::{Generator, Line, Load, Network, Snapshots};

pub use crate::Case;

#[derive(Debug, thiserror::Error)]
pub enum MatpowerError {
    #[error("no `mpc.{0}` section found")]
    MissingSection(&'static str),
    #[error("in mpc.{section} row {row}: expected at least {want} columns, found {got}")]
    ShortRow {
        section: &'static str,
        row: usize,
        want: usize,
        got: usize,
    },
    #[error("in mpc.{section} row {row}: `{value}` is not a number")]
    BadNumber {
        section: &'static str,
        row: usize,
        value: String,
    },
    #[error("generator {row} sits at bus {bus}, which no bus row defines")]
    UnknownBus { row: usize, bus: i64 },
    #[error("network is not valid: {0}")]
    Invalid(#[from] gridwright_net::NetError),
}

/// Strip comments and pull out the rows of one `mpc.<name> = [ ... ];` block.
fn section(text: &str, name: &'static str) -> Option<Vec<Vec<String>>> {
    let needle = format!("mpc.{name}");
    let start = text.find(&needle)?;
    let open = text[start..].find('[')? + start + 1;
    let close = text[open..].find(']')? + open;
    let body = &text[open..close];

    let mut rows = Vec::new();
    for line in body.lines() {
        // MATLAB comments run to end of line and may follow data.
        let line = line.split('%').next().unwrap_or("");
        let line = line.trim().trim_end_matches(';');
        if line.is_empty() {
            continue;
        }
        rows.push(
            line.split_whitespace()
                .map(|s| s.trim_end_matches(',').to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>(),
        );
    }
    Some(rows)
}

fn num(
    row: &[String],
    i: usize,
    sec: &'static str,
    r: usize,
    want: usize,
) -> Result<f64, MatpowerError> {
    let raw = row.get(i).ok_or(MatpowerError::ShortRow {
        section: sec,
        row: r,
        want,
        got: row.len(),
    })?;
    raw.parse().map_err(|_| MatpowerError::BadNumber {
        section: sec,
        row: r,
        value: raw.clone(),
    })
}

/// A scalar assignment such as `mpc.baseMVA = 100;`.
fn scalar(text: &str, name: &str) -> Option<f64> {
    let needle = format!("mpc.{name}");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let eq = rest.find('=')? + 1;
    let end = rest[eq..].find(';')? + eq;
    rest[eq..end].trim().parse().ok()
}

/// Parse a MATPOWER case into a single-snapshot network.
///
/// One snapshot, because a MATPOWER case is a single operating point rather
/// than a time series. That is what makes these cases useful for validating
/// the power flow itself: there is nothing temporal to confound it.
pub fn parse_case(text: &str, name: impl Into<String>) -> Result<Case, MatpowerError> {
    let name = name.into();
    let mut notes = Vec::new();

    let base_mva = scalar(text, "baseMVA").unwrap_or(100.0);
    let bus_rows = section(text, "bus").ok_or(MatpowerError::MissingSection("bus"))?;
    let branch_rows = section(text, "branch").ok_or(MatpowerError::MissingSection("branch"))?;
    let gen_rows = section(text, "gen").unwrap_or_default();
    let cost_rows = section(text, "gencost").unwrap_or_default();

    let mut net = Network::new(Snapshots::hourly(1));
    net.base_mva = base_mva;

    // Buses. MATPOWER identifies them by an arbitrary integer, not by position,
    // so the mapping from label to index has to be built explicitly.
    let mut index_of: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    let mut loads = Vec::new();
    for (r, row) in bus_rows.iter().enumerate() {
        let id = num(row, 0, "bus", r, 13)? as i64;
        let pd = num(row, 2, "bus", r, 13)?;
        let area = num(row, 6, "bus", r, 13)? as i64;
        // MATPOWER's `area` is the closest thing the format has to a
        // synchronous area, and using it means a multi-area case such as a US
        // interconnection model is read correctly rather than fused into one.
        let idx = net.add_bus_in_area(format!("bus{id}"), format!("area{area}"), format!("a{area}"));
        // Voltage limits are columns 12 and 11, and matter only to the AC model.
        if let (Ok(vmax), Ok(vmin)) = (num(row, 11, "bus", r, 13), num(row, 12, "bus", r, 13))
            && vmin > 0.0
            && vmax > vmin
        {
            net.buses[idx].v_max = vmax;
            net.buses[idx].v_min = vmin;
        }
        index_of.insert(id, idx);
        // Column 9 is the base voltage in kV, which nothing needed until the
        // ohms conversions did. Note that PGLib normalises it to 1.0, so a
        // value being present does not make it meaningful.
        net.buses[idx].v_nom = num(row, 9, "bus", r, 13).unwrap_or(0.0);
        // Gs and Bs are stated as the MW and MVAr a shunt would draw or inject
        // at one per unit voltage, so dividing by the system base puts them in
        // per unit, which is where the formulation wants them.
        net.buses[idx].g_shunt = num(row, 4, "bus", r, 13).unwrap_or(0.0) / base_mva;
        net.buses[idx].b_shunt = num(row, 5, "bus", r, 13).unwrap_or(0.0) / base_mva;
        let qd = num(row, 3, "bus", r, 13)?;
        if pd.abs() > 0.0 || qd.abs() > 0.0 {
            loads.push((idx, pd, qd, id));
        }
    }
    for (idx, pd, qd, id) in loads {
        net.add_load(Load {
            name: format!("load{id}"),
            bus: idx,
            p_set: pd,
            q_set: qd,
            ..Default::default()
        });
    }

    // Generators, with cost taken from the matching gencost row.
    let mut skipped_gens = 0;
    let mut quadratic = 0;
    for (r, row) in gen_rows.iter().enumerate() {
        let bus_id = num(row, 0, "gen", r, 10)? as i64;
        let status = num(row, 7, "gen", r, 10)?;
        if status <= 0.0 {
            skipped_gens += 1;
            continue;
        }
        let p_max = num(row, 8, "gen", r, 10)?;
        let p_min = num(row, 9, "gen", r, 10)?;
        // Reactive limits are columns 3 and 4; the AC formulation needs them.
        let q_max = num(row, 3, "gen", r, 10).unwrap_or(f64::INFINITY);
        let q_min = num(row, 4, "gen", r, 10).unwrap_or(f64::NEG_INFINITY);
        let bus = *index_of
            .get(&bus_id)
            .ok_or(MatpowerError::UnknownBus { row: r, bus: bus_id })?;

        // gencost model 2 is polynomial with `n` coefficients, highest order
        // first, so the linear term is the second from last.
        let mut marginal = 0.0;
        if let Some(c) = cost_rows.get(r)
            && c.len() >= 4
        {
            let model = num(c, 0, "gencost", r, 4)?;
            let n = num(c, 3, "gencost", r, 4)? as usize;
            if model == 2.0 && n >= 2 && c.len() >= 4 + n {
                if n > 2 {
                    quadratic += 1;
                }
                marginal = num(c, 4 + n - 2, "gencost", r, 4 + n)?;
            }
        }

        net.add_generator(Generator {
            name: format!("gen{r}"),
            bus,
            p_nom: p_max,
            marginal_cost: marginal,
            // A positive Pmin is a real must-run floor, expressed per unit.
            p_min_pu: if p_max > 0.0 {
                (p_min / p_max).clamp(0.0, 1.0)
            } else {
                0.0
            },
            q_min,
            q_max,
            ..Default::default()
        });
    }

    // Branches. Reactance becomes susceptance; a zero rating means unlimited.
    let mut skipped_branches = 0;
    let mut zero_reactance = 0;
    for (r, row) in branch_rows.iter().enumerate() {
        let status = num(row, 10, "branch", r, 11)?;
        if status <= 0.0 {
            skipped_branches += 1;
            continue;
        }
        let f = num(row, 0, "branch", r, 11)? as i64;
        let t = num(row, 1, "branch", r, 11)? as i64;
        let res = num(row, 2, "branch", r, 11)?;
        let x = num(row, 3, "branch", r, 11)?;
        let shunt = num(row, 4, "branch", r, 11).unwrap_or(0.0);
        let rate = num(row, 5, "branch", r, 11)?;
        // MATPOWER writes 0 for "not a transformer", meaning a ratio of one.
        let tap = match num(row, 8, "branch", r, 11) {
            Ok(v) if v > 0.0 => v,
            _ => 1.0,
        };
        let (Some(&bus0), Some(&bus1)) = (index_of.get(&f), index_of.get(&t)) else {
            skipped_branches += 1;
            continue;
        };
        if bus0 == bus1 {
            skipped_branches += 1;
            continue;
        }
        // A zero reactance branch is a bus tie in disguise. It cannot carry a
        // susceptance of infinity, so it becomes a transport link, which is the
        // behaviour a zero impedance connection actually has.
        let susceptance = if x.abs() > 1e-9 {
            1.0 / x
        } else {
            zero_reactance += 1;
            0.0
        };
        net.add_line(Line {
            name: format!("branch{r}"),
            bus0,
            bus1,
            s_nom: if rate > 0.0 { rate } else { 1e6 },
            susceptance,
            // Kept alongside the DC susceptance rather than instead of it: the
            // two formulations want different things from the same line, and
            // deriving one from the other loses information the file had.
            resistance: res,
            reactance: x,
            shunt_susceptance: shunt,
            tap_ratio: tap,
            // Column 9 is the phase shift, and MATPOWER states it in degrees
            // where every trigonometric identity in the formulation wants
            // radians.
            phase_shift: num(row, 9, "branch", r, 11)
                .unwrap_or(0.0)
                .to_radians(),
            ..Default::default()
        });
    }

    if skipped_gens > 0 {
        notes.push(format!("{skipped_gens} out-of-service generators skipped"));
    }
    if skipped_branches > 0 {
        notes.push(format!("{skipped_branches} branches skipped (out of service, self-loop, or dangling)"));
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
    notes.push(format!(
        "baseMVA {base_mva}; resistance, shunts, voltage limits and reactive power \
         are read and used by the AC formulation, and ignored by the DC one"
    ));

    net.validate()?;
    Ok(Case {
        name,
        network: net,
        notes,
    })
}

/// Read a MATPOWER case from a path.
pub fn load_case(path: impl AsRef<std::path::Path>) -> Result<Case, crate::IoError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|source| crate::IoError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "case".into());
    parse_case(&text, name).map_err(crate::IoError::Matpower)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A three bus case in the real format, small enough to reason about.
    const TINY: &str = r#"
function mpc = tiny
mpc.version = '2';
mpc.baseMVA = 100.0;

%% bus data
%	bus_i	type	Pd	Qd	Gs	Bs	area	Vm	Va	baseKV	zone	Vmax	Vmin
mpc.bus = [
	1	 3	 0.0	 0.0	 0.0	 0.0	 1	 1.0	 0.0	 230	 1	 1.1	 0.9;
	2	 1	 100.0	 30.0	 0.0	 0.0	 1	 1.0	 0.0	 230	 1	 1.1	 0.9;
	3	 2	 50.0	 10.0	 0.0	 0.0	 1	 1.0	 0.0	 230	 1	 1.1	 0.9;
];

%% generator data
%	bus	Pg	Qg	Qmax	Qmin	Vg	mBase	status	Pmax	Pmin
mpc.gen = [
	1	 0.0	 0.0	 100	 -100	 1.0	 100	 1	 200.0	 0.0;
	3	 0.0	 0.0	 100	 -100	 1.0	 100	 1	 100.0	 20.0;
	2	 0.0	 0.0	 100	 -100	 1.0	 100	 0	 500.0	 0.0;
];

%% branch data
%	fbus	tbus	r	x	b	rateA	rateB	rateC	ratio	angle	status
mpc.branch = [
	1	 2	 0.01	 0.05	 0.0	 150.0	 0	 0	 0	 0	 1;
	2	 3	 0.01	 0.10	 0.0	 0.0	 0	 0	 0	 0	 1;
	1	 3	 0.01	 0.20	 0.0	 80.0	 0	 0	 0	 0	 0;
];

%% generator cost data
%	2	startup	shutdown	n	c(n-1)	...	c0
mpc.gencost = [
	2	 0.0	 0.0	 3	 0.01	 20.0	 0.0;
	2	 0.0	 0.0	 2	 45.0	 0.0;
	2	 0.0	 0.0	 2	 10.0	 0.0;
];
"#;

    #[test]
    fn reads_buses_and_their_demand() {
        let c = parse_case(TINY, "tiny").unwrap();
        assert_eq!(c.network.buses.len(), 3);
        // Only buses with non-zero Pd become loads.
        assert_eq!(c.network.loads.len(), 2);
        let total: f64 = c.network.loads.iter().map(|l| l.p_set).sum();
        assert_eq!(total, 150.0);
    }

    #[test]
    fn skips_out_of_service_generators_and_branches() {
        let c = parse_case(TINY, "tiny").unwrap();
        // Three generator rows, one with status 0.
        assert_eq!(c.network.generators.len(), 2);
        // Three branch rows, one with status 0.
        assert_eq!(c.network.lines.len(), 2);
        assert!(c.notes.iter().any(|n| n.contains("out-of-service generators")));
    }

    #[test]
    fn susceptance_is_the_reciprocal_of_reactance() {
        let c = parse_case(TINY, "tiny").unwrap();
        // x = 0.05 -> B = 20; x = 0.10 -> B = 10.
        assert!((c.network.lines[0].susceptance - 20.0).abs() < 1e-9);
        assert!((c.network.lines[1].susceptance - 10.0).abs() < 1e-9);
    }

    #[test]
    fn a_zero_rating_means_unlimited_not_forbidden() {
        let c = parse_case(TINY, "tiny").unwrap();
        assert_eq!(c.network.lines[0].s_nom, 150.0);
        assert!(
            c.network.lines[1].s_nom > 1e5,
            "rateA of 0 must not become a zero-capacity line"
        );
    }

    #[test]
    fn marginal_cost_comes_from_the_linear_term() {
        let c = parse_case(TINY, "tiny").unwrap();
        // First generator: quadratic 0.01, linear 20, constant 0.
        assert_eq!(c.network.generators[0].marginal_cost, 20.0);
        // Second: linear 45.
        assert_eq!(c.network.generators[1].marginal_cost, 45.0);
        assert!(c.notes.iter().any(|n| n.contains("quadratic")));
    }

    #[test]
    fn a_minimum_output_becomes_a_per_unit_floor() {
        let c = parse_case(TINY, "tiny").unwrap();
        // Pmin 20 of Pmax 100.
        assert!((c.network.generators[1].p_min_pu - 0.2).abs() < 1e-9);
        assert_eq!(c.network.generators[0].p_min_pu, 0.0);
    }

    #[test]
    fn matpower_areas_become_synchronous_areas() {
        let c = parse_case(TINY, "tiny").unwrap();
        assert_eq!(c.network.synchronous_areas().len(), 1);
        assert_eq!(c.network.buses[0].synchronous_area, "a1");
    }

    #[test]
    fn a_case_without_a_bus_section_is_rejected() {
        assert!(matches!(
            parse_case("mpc.baseMVA = 100;", "x"),
            Err(MatpowerError::MissingSection("bus"))
        ));
    }

    #[test]
    fn comments_after_data_are_ignored() {
        let text = TINY.replace("	1	 2	 0.01	 0.05	 0.0	 150.0	 0	 0	 0	 0	 1;",
                                "	1	 2	 0.01	 0.05	 0.0	 150.0	 0	 0	 0	 0	 1;  % a trailing note");
        let c = parse_case(&text, "tiny").unwrap();
        assert_eq!(c.network.lines.len(), 2);
    }
}

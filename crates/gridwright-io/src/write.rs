//! Writing networks back out, in the formats other tools read.
//!
//! Nine formats can be read and three could be written, so conversion only ever
//! went one way. That is half an import layer: someone who brings a PSS/E case
//! in, edits it, and wants it back in the shape the rest of their toolchain
//! speaks was stuck.
//!
//! # What a writer can and cannot promise
//!
//! Every format here holds a different subset of what a [`Network`] does, so no
//! writer round-trips everything and pretending otherwise would be the failure
//! mode worth avoiding. Each returns the notes describing what it dropped, in
//! the same way every reader does, so the loss is visible at the point it
//! happens rather than discovered later by someone comparing two files.
//!
//! MATPOWER carries no time series at all, so a model with 8,760 snapshots
//! writes its first one and says so. PSS/E carries no costs. Neither carries
//! storage. Those are properties of the formats, not gaps here.

use gridwright_net::Network;

/// A written file, and what had to be left out of it.
#[derive(Debug, Clone)]
pub struct Written {
    pub text: String,
    /// What the format could not hold, stated rather than discovered.
    pub notes: Vec<String>,
}

/// Render a float the way these formats expect: fixed, not exponential.
///
/// A MATPOWER or PSS/E parser reading `1e-05` is within its rights to reject
/// it, and Rust's default float formatting produces exponentials for small
/// magnitudes. Nine significant figures is more than any of these formats
/// carries and costs nothing.
fn num(v: f64) -> String {
    if !v.is_finite() {
        // These formats have no infinity. A very large finite number is what
        // they use for "unlimited", and it is what their own writers emit.
        return if v > 0.0 { "9999.0".into() } else { "-9999.0".into() };
    }
    let mut t = format!("{v:.9}");
    if t.contains('.') {
        t = t.trim_end_matches('0').to_string();
        // A trailing point, and a bare integer, both read badly: PSS/E treats a
        // lone `0` at the head of a record as the end of a section, so a
        // transformer with no resistance would truncate the file.
        if t.ends_with('.') {
            t.push('0');
        }
    }
    t
}

/// Write a network as a MATPOWER case.
///
/// The format the IEEE cases, PGLib and most optimisation papers use, and
/// therefore the one to write when handing a network to someone else's tooling.
pub fn to_matpower(net: &Network, name: &str) -> Written {
    let base = if net.base_mva > 0.0 { net.base_mva } else { 100.0 };
    let mut notes = Vec::new();
    let mut out = String::new();
    out.push_str(&format!("function mpc = {name}\n"));
    out.push_str("%% MATPOWER case, written by gridwright\n");
    out.push_str("mpc.version = '2';\n");
    out.push_str(&format!("mpc.baseMVA = {};\n\n", num(base)));

    // Demand per bus, since MATPOWER puts load on the bus rather than in its
    // own table.
    let mut pd = vec![0.0; net.buses.len()];
    let mut qd = vec![0.0; net.buses.len()];
    for (l, load) in net.loads.iter().enumerate() {
        pd[load.bus] += net.load_profile.at(l, 0).unwrap_or(load.p_set);
        qd[load.bus] += load.q_set;
    }
    if net.n_snapshots() > 1 {
        notes.push(format!(
            "MATPOWER holds a single operating point; the first of {} snapshots \
             was written and the rest dropped",
            net.n_snapshots()
        ));
    }

    // Bus type: 3 for a reference, 2 where there is generation, 1 otherwise.
    // Every synchronous area needs exactly one reference, or the case has no
    // angle datum and will not solve in anyone else's tool either.
    let mut is_gen = vec![false; net.buses.len()];
    for g in &net.generators {
        is_gen[g.bus] = true;
    }
    let mut referenced: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut kind = vec![1u8; net.buses.len()];
    for b in 0..net.buses.len() {
        if !is_gen[b] {
            continue;
        }
        let area = net.buses[b].synchronous_area.as_str();
        if referenced.insert(area) {
            kind[b] = 3;
        } else {
            kind[b] = 2;
        }
    }

    out.push_str("%% bus data\n%\tbus_i\ttype\tPd\tQd\tGs\tBs\tarea\tVm\tVa\tbaseKV\tzone\tVmax\tVmin\n");
    out.push_str("mpc.bus = [\n");
    for (b, bus) in net.buses.iter().enumerate() {
        out.push_str(&format!(
            "\t{}\t{}\t{}\t{}\t{}\t{}\t1\t1.0\t0.0\t{}\t1\t{}\t{};\n",
            b + 1,
            kind[b],
            num(pd[b]),
            num(qd[b]),
            num(bus.g_shunt * base),
            num(bus.b_shunt * base),
            num(bus.v_nom),
            num(bus.v_max),
            num(bus.v_min),
        ));
    }
    out.push_str("];\n\n");

    out.push_str("%% generator data\n%\tbus\tPg\tQg\tQmax\tQmin\tVg\tmBase\tstatus\tPmax\tPmin\n");
    out.push_str("mpc.gen = [\n");
    for g in &net.generators {
        out.push_str(&format!(
            "\t{}\t0.0\t0.0\t{}\t{}\t1.0\t{}\t1\t{}\t{};\n",
            g.bus + 1,
            num(g.q_max),
            num(g.q_min),
            num(base),
            num(g.p_nom),
            num(g.p_nom * g.p_min_pu),
        ));
    }
    out.push_str("];\n\n");

    out.push_str("%% branch data\n%\tfbus\ttbus\tr\tx\tb\trateA\trateB\trateC\tratio\tangle\tstatus\tangmin\tangmax\n");
    out.push_str("mpc.branch = [\n");
    for l in &net.lines {
        // A transport corridor has no reactance, and MATPOWER has no way to
        // say so: every branch there is an impedance. Written with the
        // susceptance inverted where there is one, and reported otherwise.
        let x = if l.reactance != 0.0 {
            l.reactance
        } else if l.susceptance.abs() > 1e-12 {
            1.0 / l.susceptance
        } else {
            0.0
        };
        out.push_str(&format!(
            "\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t1\t-360.0\t360.0;\n",
            l.bus0 + 1,
            l.bus1 + 1,
            num(l.resistance),
            num(x),
            num(l.shunt_susceptance),
            num(if l.s_nom >= 1e6 { 0.0 } else { l.s_nom }),
            num(if l.s_nom >= 1e6 { 0.0 } else { l.s_nom }),
            num(if l.s_nom >= 1e6 { 0.0 } else { l.s_nom }),
            num(if (l.tap_ratio - 1.0).abs() < 1e-12 { 0.0 } else { l.tap_ratio }),
            num(l.phase_shift.to_degrees()),
        ));
    }
    out.push_str("];\n\n");

    // Cost, as the linear polynomial MATPOWER calls model 2.
    out.push_str("%% generator cost data\n%\tmodel\tstartup\tshutdown\tn\tc1\tc0\n");
    out.push_str("mpc.gencost = [\n");
    for g in &net.generators {
        out.push_str(&format!(
            "\t2\t{}\t{}\t2\t{}\t0.0;\n",
            num(g.start_up_cost),
            num(g.shut_down_cost),
            num(g.marginal_cost),
        ));
    }
    out.push_str("];\n");

    let transports = net.lines.iter().filter(|l| l.is_transport()).count();
    if transports > 0 {
        notes.push(format!(
            "{transports} transport corridors were written as branches; MATPOWER \
             has no way to say a branch imposes no angle relationship"
        ));
    }
    if !net.storage.is_empty() {
        notes.push(format!(
            "{} storage units dropped; MATPOWER has no storage",
            net.storage.len()
        ));
    }
    if !net.links.is_empty() {
        notes.push(format!("{} links dropped", net.links.len()));
    }
    let shiftable = net.loads.iter().filter(|l| l.shiftable_pu > 0.0).count();
    if shiftable > 0 {
        notes.push(format!(
            "{shiftable} shiftable loads written as fixed demand"
        ));
    }
    Written { text: out, notes }
}

/// Write a network as a PSS/E RAW case, revision 33.
///
/// What utilities exchange. Carries no cost data at all, which is stated rather
/// than worked around: a RAW file with invented costs would be worse than one
/// with none.
pub fn to_psse(net: &Network) -> Written {
    let base = if net.base_mva > 0.0 { net.base_mva } else { 100.0 };
    let mut notes = Vec::new();
    let mut out = String::new();
    out.push_str(&format!(
        "0, {}, 33, 0, 0, 50.00     / written by gridwright\n",
        num(base)
    ));
    out.push_str("gridwright export\n\n");

    let mut is_gen = vec![false; net.buses.len()];
    for g in &net.generators {
        is_gen[g.bus] = true;
    }
    let mut referenced: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (b, bus) in net.buses.iter().enumerate() {
        let ide = if is_gen[b] {
            if referenced.insert(bus.synchronous_area.as_str()) {
                3
            } else {
                2
            }
        } else {
            1
        };
        // Names are eight characters in RAW and are truncated rather than
        // silently corrupting the column alignment.
        let short: String = bus.name.chars().take(8).collect();
        out.push_str(&format!(
            "{},'{:<8}',{},{},1,1,1,1.00000,0.0000,{},{}\n",
            b + 1,
            short,
            num(if bus.v_nom > 0.0 { bus.v_nom } else { 1.0 }),
            ide,
            num(bus.v_max),
            num(bus.v_min),
        ));
    }
    out.push_str("0 / END OF BUS DATA, BEGIN LOAD DATA\n");

    for (l, load) in net.loads.iter().enumerate() {
        let p = net.load_profile.at(l, 0).unwrap_or(load.p_set);
        if p.abs() < 1e-12 && load.q_set.abs() < 1e-12 {
            continue;
        }
        out.push_str(&format!(
            "{},'1 ',1,1,1,{},{},0.0,0.0,0.0,0.0,1,1,0\n",
            load.bus + 1,
            num(p),
            num(load.q_set),
        ));
    }
    out.push_str("0 / END OF LOAD DATA, BEGIN FIXED SHUNT DATA\n");
    for (b, bus) in net.buses.iter().enumerate() {
        if bus.g_shunt != 0.0 || bus.b_shunt != 0.0 {
            out.push_str(&format!(
                "{},'1 ',1,{},{}\n",
                b + 1,
                num(bus.g_shunt * base),
                num(bus.b_shunt * base),
            ));
        }
    }
    out.push_str("0 / END OF FIXED SHUNT DATA, BEGIN GENERATOR DATA\n");
    for g in &net.generators {
        out.push_str(&format!(
            "{},'1 ',0.0,0.0,{},{},1.00000,0,{},0.0,1.0,0.0,0.0,1.0,1,100.0,{},{},1,1.0\n",
            g.bus + 1,
            num(g.q_max),
            num(g.q_min),
            num(base),
            num(g.p_nom),
            num(g.p_nom * g.p_min_pu),
        ));
    }
    out.push_str("0 / END OF GENERATOR DATA, BEGIN BRANCH DATA\n");

    // Transformers go in their own section, which is what makes a v33 file a
    // v33 file. A line with a tap or a shift is one.
    let is_transformer =
        |l: &gridwright_net::Line| (l.tap_ratio - 1.0).abs() > 1e-12 || l.phase_shift != 0.0;
    for l in net.lines.iter().filter(|l| !is_transformer(l)) {
        let x = if l.reactance != 0.0 {
            l.reactance
        } else if l.susceptance.abs() > 1e-12 {
            1.0 / l.susceptance
        } else {
            0.0
        };
        out.push_str(&format!(
            "{},{},'1 ',{},{},{},{},{},{},0.0,0.0,0.0,0.0,1,1,0.0,1,1.0\n",
            l.bus0 + 1,
            l.bus1 + 1,
            num(l.resistance),
            num(x),
            num(l.shunt_susceptance),
            num(if l.s_nom >= 1e6 { 0.0 } else { l.s_nom }),
            num(if l.s_nom >= 1e6 { 0.0 } else { l.s_nom }),
            num(if l.s_nom >= 1e6 { 0.0 } else { l.s_nom }),
        ));
    }
    out.push_str("0 / END OF BRANCH DATA, BEGIN TRANSFORMER DATA\n");
    for l in net.lines.iter().filter(|l| is_transformer(l)) {
        let x = if l.reactance != 0.0 { l.reactance } else { 0.0 };
        let short: String = l.name.chars().take(12).collect();
        out.push_str(&format!(
            "{},{},0,'1 ',1,1,1,0.0,0.0,2,'{:<12}',1,1,1.0\n",
            l.bus0 + 1,
            l.bus1 + 1,
            short
        ));
        out.push_str(&format!("{},{},{}\n", num(l.resistance), num(x), num(base)));
        out.push_str(&format!(
            "{},0.0,{},{},{},{},0,0,1.1,0.9,1.1,0.9,33,0,0.0,0.0,0.0\n",
            num(l.tap_ratio),
            num(l.phase_shift.to_degrees()),
            num(if l.s_nom >= 1e6 { 0.0 } else { l.s_nom }),
            num(if l.s_nom >= 1e6 { 0.0 } else { l.s_nom }),
            num(if l.s_nom >= 1e6 { 0.0 } else { l.s_nom }),
        ));
        out.push_str("1.0,0.0\n");
    }
    out.push_str("0 / END OF TRANSFORMER DATA, BEGIN AREA DATA\n");
    out.push_str("0 / END OF AREA DATA, BEGIN TWO-TERMINAL DC DATA\n");
    out.push_str("0 / END OF TWO-TERMINAL DC DATA\nQ\n");

    notes.push("PSS/E RAW carries no generator costs; every marginal cost was dropped".into());
    if net.n_snapshots() > 1 {
        notes.push(format!(
            "RAW holds a single operating point; the first of {} snapshots was \
             written",
            net.n_snapshots()
        ));
    }
    if !net.storage.is_empty() {
        notes.push(format!(
            "{} storage units dropped; RAW has no storage",
            net.storage.len()
        ));
    }
    Written { text: out, notes }
}

/// Write a MATPOWER case to a path.
pub fn write_matpower(
    net: &Network,
    path: impl AsRef<std::path::Path>,
) -> Result<Vec<String>, crate::IoError> {
    let path = path.as_ref();
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "case".into());
    let w = to_matpower(net, &name);
    std::fs::write(path, &w.text).map_err(|source| crate::IoError::Read {
        path: path.display().to_string(),
        source,
    })?;
    Ok(w.notes)
}

/// Write a PSS/E RAW case to a path.
pub fn write_psse(
    net: &Network,
    path: impl AsRef<std::path::Path>,
) -> Result<Vec<String>, crate::IoError> {
    let path = path.as_ref();
    let w = to_psse(net);
    std::fs::write(path, &w.text).map_err(|source| crate::IoError::Read {
        path: path.display().to_string(),
        source,
    })?;
    Ok(w.notes)
}

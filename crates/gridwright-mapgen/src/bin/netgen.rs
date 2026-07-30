//! Builds an example network from published transmission data.
//!
//! ```text
//! cargo run -p gridwright-mapgen --bin netgen --release -- \
//!     <gridkit-dir> <opsd-load.csv> <natural-earth-dir> <out.json>
//! ```
//!
//! The output is committed, so an ordinary build never runs this.
//!
//! # What is real here and what is not
//!
//! **This distinction is the whole point of the file, so it is stated before
//! anything else.** The network that comes out is a join of three published
//! sources plus one allocation that is ours, and a reader who cannot tell which
//! is which will believe things about Europe's grid that nobody published.
//!
//! | part | where it comes from |
//! | --- | --- |
//! | buses, positions, names, voltages | GridKit extract of the ENTSO-E interactive map (CC BY 4.0) |
//! | lines, voltages, circuits, lengths, DC flag | the same extract |
//! | generator sites, fuels, capacities | the same extract, where a capacity is given |
//! | national demand | Open Power System Data, from ENTSO-E Transparency |
//! | **which bus that demand sits on** | **ours: see [`allocate`]** |
//! | **cost per MWh by fuel** | **ours: a merit order, see [`MERIT`]** |
//!
//! Nobody publishes per-bus demand for Europe. National totals are published and
//! substation positions are published, and getting from one to the other is a
//! modelling step however it is done — so it is done explicitly, in one function,
//! and named in the network's own reader notes.
//!
//! The extract is also *unofficial* and dated May 2016. It is not endorsed by
//! ENTSO-E, and it is a snapshot of a map rather than of a grid.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gridwright_mapgen::{dbf, shapefile};
use gridwright_net::{Bus, Coord, Generator, Line, Load, Network};

/// Cost per MWh by fuel, as a merit order rather than as a price.
///
/// **Ours, and not a claim about anybody's costs.** GridKit carries no cost data,
/// and a dispatch with no costs is not a dispatch — every feasible answer is
/// optimal and the nodal prices are meaningless. So these exist to put the fuels
/// in the order the physical system runs them in: variable renewables first
/// because their marginal cost really is near zero, then nuclear and lignite,
/// then hard coal, then gas, then oil.
///
/// The *ordering* is uncontroversial and is what the model needs. The magnitudes
/// are round numbers in the right region for European wholesale markets and
/// should not be read as more than that.
const MERIT: [(&str, f64); 12] = [
    ("wind", 0.5),
    ("solar", 0.5),
    ("hydro", 2.0),
    ("nuclear", 12.0),
    ("lignite", 28.0),
    ("coal", 42.0),
    ("waste", 45.0),
    ("biomass", 55.0),
    ("gas", 72.0),
    ("oil", 130.0),
    ("storage", 65.0),
    ("other", 80.0),
];

/// How many substations share a populated place's demand.
///
/// Five. One is wrong for the reason described in [`population_weight`]; the whole
/// country is wrong because then geography stops meaning anything. Five puts a
/// city's load on the ring of stations around it, which is roughly how a city is
/// actually supplied.
const NEAR: usize = 5;

/// A demand this small is not worth a row.
///
/// Every substation gets some share of its country's load under [`allocate`], and
/// the tail of that distribution is substations with a few hundred kilowatts on
/// them. They cost a variable each and change no answer.
const MIN_LOAD_MW: f64 = 1.0;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(grid), Some(load), Some(earth), Some(out)) =
        (args.next(), args.next(), args.next(), args.next())
    else {
        eprintln!(
            "usage: netgen <gridkit-dir> <opsd-load.csv> <natural-earth-dir> <out.json>"
        );
        std::process::exit(2);
    };

    let built = build(
        Path::new(&grid),
        Path::new(&load),
        Path::new(&earth),
    );
    let net = match built {
        Ok(net) => net,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    // Validated before it is written. A committed artefact that the engine
    // refuses to read is a build that succeeded and a repository that is broken.
    if let Err(problem) = net.validate() {
        eprintln!("the network this produced does not validate: {problem}");
        std::process::exit(1);
    }

    eprintln!(
        "network: {} buses, {} lines, {} generators, {} loads",
        net.buses.len(),
        net.lines.len(),
        net.generators.len(),
        net.loads.len(),
    );
    let demand: f64 = net.loads.iter().map(|l| l.p_set).sum();
    let capacity: f64 = net.generators.iter().map(|g| g.p_nom).sum();
    eprintln!("         {demand:.0} MW of demand against {capacity:.0} MW of capacity");

    if let Err(e) = gridwright_io::json::write_network(&net, PathBuf::from(&out)) {
        eprintln!("cannot write {out}: {e}");
        std::process::exit(1);
    }
    let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    eprintln!("wrote {out}: {:.1} KB", bytes as f64 / 1024.0);
}

fn build(grid: &Path, load: &Path, earth: &Path) -> Result<Network, String> {
    let buses = read_csv(&grid.join("buses.csv"))?;
    let links = read_csv(&grid.join("links.csv"))?;
    let gens = read_csv(&grid.join("generators.csv"))?;
    // **Transformers are not optional and leaving them out is not a small error.**
    // The extract gives every voltage level of a station its own bus and couples
    // them in this file, so without it each voltage level is electrically its own
    // island: 321 of them, the largest holding 1,837 of 7,893 buses, and 41% of
    // demand unservable because the generation was on the other side of a
    // transformer that did not exist.
    let transformers = read_csv(&grid.join("transformers.csv"))?;
    eprintln!(
        "extract: {} buses, {} links, {} transformers, {} generators",
        buses.len(),
        links.len(),
        transformers.len(),
        gens.len()
    );

    // One snapshot. The load series gives an hourly year, and the mean of it is
    // what `national_demand` returns -- see there for why a single representative
    // level is the defensible summary rather than a chosen hour.
    let mut net = Network::new(gridwright_net::Snapshots::hourly(1));
    // GridKit's own bus ids are not contiguous, so a map is needed rather than
    // arithmetic on them.
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut country_of: Vec<String> = Vec::new();

    for row in &buses {
        let Some(at) = point(row.get("geometry")) else { continue };
        let tags = hstore(row.get("tags"));
        let country = tags.get("country").cloned().unwrap_or_default();
        // The extract's English name where it has one. 6,664 of 7,893 do, and the
        // rest fall back to their id -- which is at least a stable handle, and is
        // visibly not a place name.
        let name = tags
            .get("name_eng")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("node {}", row.get("bus_id").cloned().unwrap_or_default()));

        let mut bus = Bus {
            name: unique(&name, &index),
            country: country.clone(),
            // One area. The extract spans the European synchronous area plus
            // North Africa and parts of the Middle East, and it does not record
            // which of those are asynchronous -- so declaring areas from this
            // data would be inventing them.
            synchronous_area: "entsoe".into(),
            v_nom: row
                .get("voltage")
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0),
            ..Bus::default()
        };
        bus.position = Coord::new(at[0], at[1]);
        let id = row.get("bus_id").cloned().unwrap_or_default();
        index.insert(id, net.buses.len());
        country_of.push(country);
        net.buses.push(bus);
    }

    // Lines. A susceptance is needed for a DC flow and the extract gives length
    // and voltage rather than impedance, so it is estimated from those -- the one
    // electrical quantity here that is derived rather than published.
    let mut skipped = 0usize;
    for row in &links {
        let (Some(a), Some(b)) = (
            row.get("src_bus_id").and_then(|k| index.get(k)).copied(),
            row.get("dst_bus_id").and_then(|k| index.get(k)).copied(),
        ) else {
            skipped += 1;
            continue;
        };
        if a == b {
            skipped += 1;
            continue;
        }
        let kv = row
            .get("voltage")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(220.0)
            .max(1.0);
        let circuits = row
            .get("circuits")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|c| *c >= 1.0)
            .unwrap_or(1.0);
        let km = row
            .get("length_m")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(50_000.0)
            .max(100.0)
            / 1000.0;

        net.lines.push(Line {
            name: format!(
                "{} — {}",
                net.buses[a].name, net.buses[b].name
            ),
            bus0: a,
            bus1: b,
            s_nom: thermal_mw(kv) * circuits,
            susceptance: susceptance(kv, km, circuits),
            s_nom_max: f64::INFINITY,
            ..Line::default()
        });
    }
    for row in &transformers {
        let (Some(a), Some(b)) = (
            row.get("src_bus_id").and_then(|k| index.get(k)).copied(),
            row.get("dst_bus_id").and_then(|k| index.get(k)).copied(),
        ) else {
            skipped += 1;
            continue;
        };
        if a == b {
            skipped += 1;
            continue;
        }
        let lo = net.buses[a].v_nom.min(net.buses[b].v_nom).max(1.0);
        let hi = net.buses[a].v_nom.max(net.buses[b].v_nom).max(1.0);
        net.lines.push(Line {
            name: format!("{} {lo:.0}/{hi:.0} kV", net.buses[a].name),
            bus0: a,
            bus1: b,
            // Rated on the lower side, which is what limits a transformer.
            s_nom: thermal_mw(lo) * 2.0,
            // A transformer is electrically short but not negligible: a few per
            // cent reactance on its own base, which is a much larger susceptance
            // than any line here and a much smaller one than a busbar.
            susceptance: 400.0,
            s_nom_max: f64::INFINITY,
            ..Line::default()
        });
    }
    if skipped > 0 {
        eprintln!("         {skipped} branches dropped: an endpoint outside the bus table, or a self-loop");
    }

    // Generators, only where the extract gives a capacity. The other 672 are real
    // sites with no published rating, and a made-up rating on a real power
    // station is the single most misleading thing this file could contain.
    let mut unrated = 0usize;
    for row in &gens {
        let Some(&bus) = row.get("bus_id").and_then(|k| index.get(k)) else { continue };
        let capacity = row
            .get("capacity")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(-1.0);
        if capacity <= 0.0 {
            unrated += 1;
            continue;
        }
        let carrier = fuel(row.get("symbol").map(String::as_str).unwrap_or(""));
        let cost = MERIT
            .iter()
            .find(|(f, _)| *f == carrier)
            .map(|(_, c)| *c)
            .unwrap_or(80.0);
        net.generators.push(Generator {
            name: format!("{} {carrier}", net.buses[bus].name),
            bus,
            p_nom: capacity,
            marginal_cost: cost,
            carrier: carrier.to_string(),
            p_nom_max: f64::INFINITY,
            ..Generator::default()
        });
    }
    eprintln!("         {unrated} generator sites had no published capacity and were left out");

    let national = national_demand(load)?;
    let weights = population_weight(earth, &net.buses)?;
    allocate(&mut net, &country_of, &national, &weights);

    Ok(net)
}

/// Spread each country's demand across its own substations.
///
/// **This is the modelling step, and it is one function so it can be read.**
/// Weights come from the Natural Earth gazetteer: each substation takes the
/// population of the places nearest to it, so demand lands where people are
/// rather than spreading evenly over a country that is mostly field. A substation
/// with no populated place near it still takes a floor share, because a
/// transmission node with exactly zero load is a modelling artefact rather than a
/// fact about the grid.
///
/// It is an allocation, not a measurement. Two substations either side of a city
/// will split its load in a way no operator would recognise.
fn allocate(
    net: &mut Network,
    country_of: &[String],
    national: &HashMap<String, f64>,
    weights: &[f64],
) {
    // Total weight per country first, so each country's demand is conserved
    // whatever its substations look like.
    let mut total: HashMap<&str, f64> = HashMap::new();
    for (b, country) in country_of.iter().enumerate() {
        if national.contains_key(country.as_str()) {
            *total.entry(country.as_str()).or_default() += weights[b];
        }
    }

    let mut placed = 0usize;
    let mut without = 0usize;
    for (b, country) in country_of.iter().enumerate() {
        let Some(mw) = national.get(country.as_str()) else {
            without += 1;
            continue;
        };
        let share = total.get(country.as_str()).copied().unwrap_or(0.0);
        if share <= 0.0 {
            continue;
        }
        let p_set = mw * weights[b] / share;
        if p_set < MIN_LOAD_MW {
            continue;
        }
        net.loads.push(Load {
            name: format!("{} demand", net.buses[b].name),
            bus: b,
            p_set,
            ..Load::default()
        });
        placed += 1;
    }
    eprintln!(
        "demand : {placed} buses carry load; {without} are in countries the load series does not cover"
    );
}

/// Mean hourly load per country, MW, from the Open Power System Data series.
///
/// The mean rather than a peak or a chosen hour. A peak would make the whole
/// continent congested at once, which is not a normal state and would make every
/// price look extreme; a single named hour would need a year to be chosen and
/// defended. The mean is the least interesting and most defensible summary, and
/// it is stated as what it is.
///
/// Only the plain national columns are read. The series also carries control-area
/// columns for Germany, and adding those to the national one would double the
/// country's demand.
fn national_demand(path: &Path) -> Result<HashMap<String, f64>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().ok_or("the load series is empty")?.split(',').collect();

    // `DE_load_actual_entsoe_transparency`, but not
    // `DE_tennet_load_actual_entsoe_transparency`.
    const TAIL: &str = "_load_actual_entsoe_transparency";
    let wanted: Vec<(usize, String)> = header
        .iter()
        .enumerate()
        .filter_map(|(i, name)| {
            let code = name.strip_suffix(TAIL)?;
            (code.len() == 2 && code.chars().all(|c| c.is_ascii_uppercase()))
                .then(|| (i, code.to_string()))
        })
        .collect();
    if wanted.is_empty() {
        return Err(format!("{}: no national load columns", path.display()));
    }

    let mut sum: HashMap<String, (f64, usize)> = HashMap::new();
    for row in lines {
        let cells: Vec<&str> = row.split(',').collect();
        for (i, code) in &wanted {
            let Some(v) = cells.get(*i).and_then(|c| c.parse::<f64>().ok()) else { continue };
            if v <= 0.0 {
                continue;
            }
            let e = sum.entry(code.clone()).or_insert((0.0, 0));
            e.0 += v;
            e.1 += 1;
        }
    }

    let mean: HashMap<String, f64> = sum
        .into_iter()
        .filter(|(_, (_, n))| *n > 1000)
        .map(|(k, (t, n))| (k, t / n as f64))
        .collect();
    eprintln!("load   : mean hourly demand for {} countries", mean.len());
    Ok(mean)
}

/// How much demand each bus should attract, from the population near it.
///
/// The nearest-place assignment is deliberately the cheap direction: for each
/// *place*, find its nearest bus, rather than for each bus scan every place. Both
/// are the same product of two lists, but this way the inner loop runs 7,342
/// times over 7,893 buses with an early exit on a bounding box, and the result is
/// that a city's population lands on exactly one substation instead of being
/// smeared over every substation within some radius.
fn population_weight(earth: &Path, buses: &[Bus]) -> Result<Vec<f64>, String> {
    let stem = earth.join("ne_10m_populated_places");
    let shp = std::fs::read(stem.with_extension("shp")).map_err(|e| e.to_string())?;
    let dbf_bytes = std::fs::read(stem.with_extension("dbf")).map_err(|e| e.to_string())?;
    let shapes = shapefile::read_by_record(&shp).map_err(|e| e.to_string())?;
    let rows = dbf::read(&dbf_bytes, &["POP_MAX"]).map_err(|e| e.to_string())?;
    if shapes.len() != rows.len() {
        return Err(format!(
            "{} shapes against {} attribute rows",
            shapes.len(),
            rows.len()
        ));
    }

    // A floor, so a substation with no city near it still carries something. A
    // transmission node with exactly zero load is a modelling artefact.
    let mut weight = vec![1.0f64; buses.len()];
    let mut assigned = 0usize;

    for ((_, parts), row) in shapes.iter().zip(&rows) {
        if row.deleted {
            continue;
        }
        let Some(at) = parts.first().and_then(|p| p.first()) else { continue };
        let pop: f64 = row.values[0].parse().unwrap_or(0.0);
        if pop <= 0.0 {
            continue;
        }
        // The nearest few, not the nearest one. **A city is not fed through a
        // single substation**, and pretending it is put 21,777 MW -- the whole of
        // Paris -- onto one French node, which no set of lines around it could
        // deliver. That was the largest single cause of unserved demand.
        //
        // Inverse-distance shares among them, so the closest still takes the most.
        let mut near: Vec<(usize, f64)> = Vec::with_capacity(NEAR + 1);
        for (b, bus) in buses.iter().enumerate() {
            let Some(p) = bus.position else { continue };
            // Squared degrees. Comparing distances, never reporting one, so the
            // square root would be arithmetic nobody reads.
            let d = (p.lon - at[0]).powi(2) + (p.lat - at[1]).powi(2);
            // Roughly 300 km. A place further than that from any substation in
            // the extract is outside its coverage, and attaching its population
            // to the nearest node would move a capital's demand to another
            // country.
            if d >= 9.0 {
                continue;
            }
            // An insertion into a list of at most NEAR, which is why this is not a
            // sort of every bus per place: 7,342 places against 7,893 buses is
            // already the whole product, and sorting inside it would add a log
            // factor for the sake of five entries.
            let at_index = near.partition_point(|(_, seen)| *seen < d);
            if at_index < NEAR {
                near.insert(at_index, (b, d));
                near.truncate(NEAR);
            }
        }
        if near.is_empty() {
            continue;
        }
        // A floor on the distance, or a substation sitting on top of a city's own
        // coordinate divides by zero and takes everything.
        let shares: Vec<f64> = near.iter().map(|(_, d)| 1.0 / d.max(0.0025)).collect();
        let total: f64 = shares.iter().sum();
        for ((b, _), share) in near.iter().zip(&shares) {
            weight[*b] += pop * share / total;
        }
        assigned += 1;
    }
    eprintln!("places : {assigned} populated places weighted onto the nearest substation");
    Ok(weight)
}

/// Thermal rating for a voltage, MW per circuit.
///
/// Derived, because the extract gives no ratings. These are ordinary figures for
/// a single circuit at each level; they decide where congestion appears, so they
/// are the assumption most worth knowing about.
fn thermal_mw(kv: f64) -> f64 {
    match kv {
        v if v >= 700.0 => 4000.0,
        v if v >= 500.0 => 2500.0,
        v if v >= 360.0 => 1700.0,
        v if v >= 280.0 => 1200.0,
        v if v >= 200.0 => 550.0,
        v if v >= 100.0 => 180.0,
        _ => 100.0,
    }
}

/// Susceptance from voltage and length, per unit on a 100 MVA base.
///
/// A line's reactance goes roughly as its length, and its per-unit value as
/// `length / voltage²`, so susceptance goes as `voltage² / length`. The constant
/// puts a 100 km 380 kV double circuit in the right region. Derived, like the
/// ratings, and for the same reason: the extract carries geometry, not impedance.
fn susceptance(kv: f64, km: f64, circuits: f64) -> f64 {
    let per_circuit = kv * kv / (0.3 * km * 100.0);
    (per_circuit * circuits).clamp(0.5, 5000.0)
}

/// GridKit's fuel description to a carrier this engine groups by.
fn fuel(symbol: &str) -> &'static str {
    let s = symbol.to_ascii_lowercase();
    // Order matters: "hydro pure storage" is storage before it is hydro.
    for (needle, carrier) in [
        ("pure storage", "storage"),
        ("pumped", "storage"),
        ("wind", "wind"),
        ("solar", "solar"),
        ("photovolt", "solar"),
        ("hydro", "hydro"),
        ("nuclear", "nuclear"),
        ("brown coal", "lignite"),
        ("lignite", "lignite"),
        ("hard coal", "coal"),
        ("coal", "coal"),
        ("gas", "gas"),
        ("oil", "oil"),
        ("waste", "waste"),
        ("biomass", "biomass"),
        ("geotherm", "other"),
    ] {
        if s.contains(needle) {
            return carrier;
        }
    }
    "other"
}

/// A name nothing else has yet.
///
/// The extract has several substations called Voerde and two called Biblis, being
/// separate voltage levels of one station. Distinct names matter because the
/// palette and the inspector address buses by name.
fn unique(name: &str, taken: &HashMap<String, usize>) -> String {
    if !taken.keys().any(|k| k == name) {
        return name.to_string();
    }
    (2..)
        .map(|n| format!("{name} {n}"))
        .find(|candidate| !taken.keys().any(|k| k == candidate))
        .unwrap_or_else(|| name.to_string())
}

/// `POINT(lon lat)` to a pair.
fn point(text: Option<&String>) -> Option<[f64; 2]> {
    let inner = text?.trim().strip_prefix("POINT(")?.strip_suffix(')')?;
    let (a, b) = inner.split_once(' ')?;
    Some([a.trim().parse().ok()?, b.trim().parse().ok()?])
}

/// PostgreSQL hstore, as GridKit writes its tags: `"key"=>"value", ...`.
fn hstore(text: Option<&String>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(text) = text else { return out };
    let bytes = text.as_bytes();
    let mut at = 0;
    while let Some(key) = quoted(bytes, &mut at) {
        // Skip the `=>` between them, whatever spacing it has.
        while at < bytes.len() && bytes[at] != b'"' {
            at += 1;
        }
        let Some(value) = quoted(bytes, &mut at) else { break };
        out.insert(key, value);
    }
    out
}

/// The next `"..."` from `at`, advancing past it.
fn quoted(bytes: &[u8], at: &mut usize) -> Option<String> {
    while *at < bytes.len() && bytes[*at] != b'"' {
        *at += 1;
    }
    if *at >= bytes.len() {
        return None;
    }
    *at += 1;
    let start = *at;
    while *at < bytes.len() && bytes[*at] != b'"' {
        *at += 1;
    }
    let s = String::from_utf8_lossy(&bytes[start..*at]).into_owned();
    *at += 1;
    Some(s)
}

/// GridKit's CSV, whose tag column is single-quoted rather than double.
///
/// **Not standard CSV, and getting this wrong is silent.** The tags carry commas,
/// and a parser using the usual `"` quote character splits straight through them
/// — every column after tags shifts, and a capacity reads as the tail of a
/// description. That produced a plausible-looking network with nonsense in it.
fn read_csv(path: &Path) -> Result<Vec<HashMap<String, String>>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut lines = text.lines();
    let header: Vec<String> = split(lines.next().ok_or("empty file")?);
    let mut out = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let cells = split(line);
        out.push(
            header
                .iter()
                .cloned()
                .zip(cells.into_iter().chain(std::iter::repeat(String::new())))
                .collect(),
        );
    }
    Ok(out)
}

/// One row, honouring `'` as the quote character.
fn split(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '\'' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cell)),
            _ => cell.push(c),
        }
    }
    out.push(cell);
    out
}

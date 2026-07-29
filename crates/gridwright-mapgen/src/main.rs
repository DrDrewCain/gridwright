//! Builds the studio's map pyramid from Natural Earth shapefiles.
//!
//! Run when the source data or the target resolutions change. The output is
//! committed, so an ordinary build never touches this:
//!
//! ```text
//! cargo run -p gridwright-mapgen --release -- <source-dir> <out-dir>
//! ```
//!
//! `<source-dir>` holds the unzipped Natural Earth 10m shapefiles. Natural Earth
//! is public domain, which is why it is the source: a bundled map has to be
//! redistributable without conditions, and every alternative with better detail
//! carries an attribution or share-alike term that a library cannot impose on
//! its users.
//!
//! **Release matters.** Triangulating the 10m land layer unoptimised takes
//! minutes; the same work in release takes seconds.

mod pyramid;
mod shapefile;
mod simplify;
mod triangulate;

use std::path::{Path, PathBuf};

use pyramid::{Layer, Shape};

/// Which file feeds which layer.
///
/// Only these five. Bathymetry, reefs, glaciated areas and the rest of Natural
/// Earth's physical set are all available and all noise under a wire diagram: the
/// test is whether a power engineer would orient by it, and coastline, water,
/// rivers, cities and borders are what pass it.
const SOURCES: [(Layer, &str); 5] = [
    (Layer::Land, "ne_10m_land"),
    (Layer::Lakes, "ne_10m_lakes"),
    (Layer::Rivers, "ne_10m_rivers_lake_centerlines"),
    (Layer::Urban, "ne_10m_urban_areas"),
    (Layer::Borders, "ne_10m_admin_0_boundary_lines_land"),
];

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(src), Some(out)) = (args.next(), args.next()) else {
        eprintln!("usage: mapgen <source-dir> <out-dir>");
        std::process::exit(2);
    };
    let (src, out) = (PathBuf::from(src), PathBuf::from(out));

    if let Err(e) = std::fs::create_dir_all(&out) {
        eprintln!("cannot create {}: {e}", out.display());
        std::process::exit(1);
    }

    // Parsed once each, not once per level. Re-reading the 19 MB urban file for
    // every level would dominate the run.
    let mut raw: Vec<(Layer, Vec<shapefile::Ring>)> = Vec::new();
    for (layer, stem) in SOURCES {
        let path = src.join(format!("{stem}.shp"));
        match read_layer(&path) {
            Ok(rings) => {
                eprintln!("{:<8} {:>6} parts from {stem}", layer.name(), rings.len());
                raw.push((layer, rings));
            }
            Err(e) => {
                // Named and fatal. A missing layer treated as an empty one would
                // ship a map with a silently absent feature class.
                eprintln!("{}: {e}", path.display());
                std::process::exit(1);
            }
        }
    }

    let mut total = 0usize;
    for level in 0..pyramid::TOLERANCE.len() {
        let tolerance = pyramid::TOLERANCE[level];
        let wanted = pyramid::layers_at(level);

        let mut built: Vec<(Layer, Vec<Shape>)> = Vec::new();
        for (layer, rings) in &raw {
            if !wanted.contains(layer) {
                continue;
            }
            let shapes = build_layer(*layer, rings, tolerance);
            eprintln!(
                "  level {level} {:<8} {:>5} shapes {:>7} points",
                layer.name(),
                shapes.len(),
                shapes.iter().map(|s| s.points.len()).sum::<usize>(),
            );
            built.push((*layer, shapes));
        }

        let blob = pyramid::encode(&built);
        let file = out.join(format!("level{level}.bin"));
        if let Err(e) = std::fs::write(&file, &blob) {
            eprintln!("cannot write {}: {e}", file.display());
            std::process::exit(1);
        }
        eprintln!("level {level}: {:>7.1} KB", blob.len() as f64 / 1024.0);
        total += blob.len();
    }
    eprintln!("total  : {:>7.1} KB", total as f64 / 1024.0);
}

fn read_layer(path: &Path) -> Result<Vec<shapefile::Ring>, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let parts = shapefile::read(&bytes).map_err(|e| e.to_string())?;
    Ok(parts.into_iter().map(|(_, ring)| ring).collect())
}

/// Simplify every part, and triangulate the ones that will be filled.
fn build_layer(layer: Layer, rings: &[shapefile::Ring], tolerance: f64) -> Vec<Shape> {
    // The minimum worth drawing. Below four points a filled shape is a sliver and
    // a line is a tick, and at world scale thousands of islands reduce to exactly
    // that.
    const MIN_POINTS: usize = 4;

    let mut out = Vec::new();
    let mut dropped = 0usize;

    for ring in rings {
        let simple = simplify::dedup_closed(&simplify::douglas_peucker(ring, tolerance));
        if simple.len() < MIN_POINTS {
            continue;
        }

        if !layer.filled() {
            out.push(Shape {
                points: simple,
                triangles: Vec::new(),
            });
            continue;
        }

        match triangulate::ear_clip(&simple) {
            triangulate::Outcome::Complete(triangles) => out.push(Shape {
                points: simple,
                triangles,
            }),
            // Dropped rather than drawn. A partial triangulation renders as a fan
            // of stray triangles across the shape — worse than the shape being
            // absent, and much harder to notice.
            triangulate::Outcome::Partial { .. } | triangulate::Outcome::Unusable => {
                dropped += 1;
            }
        }
    }

    if dropped > 0 {
        // Reported, never swallowed. A layer quietly losing a tenth of its
        // shapes is exactly the regression that ships.
        eprintln!(
            "    {dropped} of {} shapes defeated triangulation and were dropped",
            rings.len()
        );
    }
    out
}

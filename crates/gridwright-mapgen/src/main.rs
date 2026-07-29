//! Builds the studio's map pyramid.
//!
//! Run it when the source data or the target resolutions change; the output is
//! committed, so an ordinary build never touches this.
//!
//! ```text
//! cargo run -p gridwright-mapgen -- /path/to/naturalearth crates/gridwright-studio/src/map
//! ```

mod shapefile;
mod simplify;
mod triangulate;

fn main() {
    eprintln!("mapgen: see the module docs; wiring in progress");
}

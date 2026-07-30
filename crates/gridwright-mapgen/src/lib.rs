//! Turning published source data into the artefacts this repository commits.
//!
//! A library only so the two binaries beside it can share the readers. Nothing
//! here ships: the crate is `publish = false`, and a consumer of the engine has
//! no use for a shapefile parser.
//!
//! - `mapgen` builds the studio's basemap pyramid and gazetteer from Natural
//!   Earth.
//! - `netgen` builds an example network from a published transmission extract.
//!
//! They share [`shapefile`], [`dbf`] and [`places`], because the gazetteer that
//! names cities on the map is also the only public thing that says where people
//! live — which is what a network extract with no demand in it needs.

pub mod dbf;
pub mod places;
pub mod pyramid;
pub mod shapefile;
pub mod simplify;
pub mod triangulate;

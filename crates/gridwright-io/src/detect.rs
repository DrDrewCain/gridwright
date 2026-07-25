//! Working out what a file is, so a caller does not have to.
//!
//! Someone with a network to model has a file, not a format. They downloaded
//! it from a TSO, a ministry, a paper's supplementary material or a colleague,
//! and being made to identify it before anything will read it is a tax on
//! exactly the people this is meant to serve.
//!
//! [`load_any`] takes a path and returns a network. It looks at the extension,
//! and where the extension is absent, wrong or ambiguous it looks at the
//! content. Both are needed: `.json` covers two different dialects here and a
//! dozen elsewhere, `.xml` could be anything, and files arrive named `.txt` or
//! with no extension at all often enough to matter.
//!
//! # Content beats extension
//!
//! A file's first bytes are harder to get wrong than its name. HDF5, Zip and
//! Parquet all declare themselves, and the text formats have opening lines
//! that are unmistakable: a MATPOWER case says `function mpc`, a PSS/E case
//! opens with a comma-separated header whose third field is a revision number.
//! Where the two disagree the content wins, because a renamed file is far more
//! common than a file that lies about its own contents.

use std::path::Path;

use crate::{Case, IoError};

/// What a path turned out to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A directory of CSV files, or a single one alongside its siblings.
    CsvDirectory,
    /// A directory of Parquet files.
    ParquetDirectory,
    Matpower,
    Psse,
    /// PowerModels.jl, per-unit and keyed by component number.
    PowerModels,
    /// This crate's own lossless JSON.
    NativeJson,
    /// PyPSA netCDF, or any HDF5 laid out the same way.
    Netcdf,
    /// A spreadsheet, in any of the formats the reader accepts.
    Excel,
    /// CIM/CGMES RDF/XML, one file or a directory of profiles.
    Cgmes,
}

impl Format {
    /// A name to put in front of a user.
    pub fn label(self) -> &'static str {
        match self {
            Format::CsvDirectory => "CSV directory",
            Format::ParquetDirectory => "Parquet directory",
            Format::Matpower => "MATPOWER case",
            Format::Psse => "PSS/E RAW",
            Format::PowerModels => "PowerModels JSON",
            Format::NativeJson => "gridwright JSON",
            Format::Netcdf => "PyPSA netCDF",
            Format::Excel => "spreadsheet",
            Format::Cgmes => "CIM/CGMES",
        }
    }

    /// Whether this crate was built with the support this format needs.
    ///
    /// Detection does not depend on the feature flags, on purpose: a build
    /// without Parquet should say "this is Parquet and this build cannot read
    /// it" rather than "I do not recognise this file", which would send
    /// someone looking for a problem with their data.
    pub fn available(self) -> bool {
        match self {
            Format::CsvDirectory | Format::Matpower | Format::Psse => true,
            Format::PowerModels | Format::NativeJson => cfg!(feature = "json"),
            Format::ParquetDirectory => cfg!(feature = "parquet"),
            Format::Excel => cfg!(feature = "excel"),
            Format::Netcdf => cfg!(feature = "netcdf"),
            Format::Cgmes => cfg!(feature = "cgmes"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DetectError {
    #[error("{path} does not look like any format this reads")]
    Unrecognised { path: String },
    #[error(
        "{path} is {format}, which this build cannot read; rebuild with the \
         `{feature}` feature enabled"
    )]
    NotBuilt {
        path: String,
        format: &'static str,
        feature: &'static str,
    },
}

const HDF5_MAGIC: &[u8] = b"\x89HDF\r\n\x1a\n";
const ZIP_MAGIC: &[u8] = b"PK\x03\x04";
const PARQUET_MAGIC: &[u8] = b"PAR1";

/// The first few bytes of a file, for the formats that declare themselves.
fn head(path: &Path, n: usize) -> Vec<u8> {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut buf = vec![0u8; n];
    match f.read(&mut buf) {
        Ok(read) => {
            buf.truncate(read);
            buf
        }
        Err(_) => Vec::new(),
    }
}

/// Whether a text opening looks like a PSS/E header.
///
/// `0, 100.00, 33, 0, 0, 60.00` — a numeric first field, a positive base MVA,
/// and a revision in a plausible range. Loose enough for the hand-edited files
/// that omit the trailing fields, tight enough not to claim a stray CSV.
fn looks_like_psse(text: &str) -> bool {
    let Some(first) = text.lines().next() else {
        return false;
    };
    let fields: Vec<&str> = first
        .split('/')
        .next()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .collect();
    if fields.len() < 2 {
        return false;
    }
    let ic_ok = fields[0].parse::<f64>().is_ok();
    let base_ok = fields[1].parse::<f64>().is_ok_and(|v| v > 0.0);
    let rev_ok = fields
        .get(2)
        .and_then(|s| s.parse::<f64>().ok())
        .is_none_or(|v| (20.0..=40.0).contains(&v));
    ic_ok && base_ok && rev_ok
}

fn looks_like_matpower(text: &str) -> bool {
    text.contains("mpc.bus") || text.contains("function mpc")
}

fn looks_like_cim(text: &str) -> bool {
    let start = &text[..text.len().min(4096)];
    start.contains("rdf:RDF") || start.contains("CIM-schema-cim")
}

/// Identify what a path holds.
pub fn sniff(path: impl AsRef<Path>) -> Result<Format, DetectError> {
    let path = path.as_ref();
    let unrecognised = || DetectError::Unrecognised {
        path: path.display().to_string(),
    };

    if path.is_dir() {
        let has = |name: &str| path.join(name).exists();
        // A directory is identified by what it contains, in the order a
        // directory holding several is most likely to have meant.
        if has("buses.csv") {
            return Ok(Format::CsvDirectory);
        }
        if has("buses.parquet") {
            return Ok(Format::ParquetDirectory);
        }
        let any_xml = std::fs::read_dir(path)
            .map(|entries| {
                entries.flatten().any(|e| {
                    e.path()
                        .extension()
                        .is_some_and(|x| x.eq_ignore_ascii_case("xml"))
                })
            })
            .unwrap_or(false);
        if any_xml {
            return Ok(Format::Cgmes);
        }
        return Err(unrecognised());
    }

    let magic = head(path, 512);
    if magic.starts_with(HDF5_MAGIC) {
        return Ok(Format::Netcdf);
    }
    if magic.starts_with(PARQUET_MAGIC) {
        return Ok(Format::ParquetDirectory);
    }
    if magic.starts_with(ZIP_MAGIC) {
        // Every modern spreadsheet is a zip; so are several things that are
        // not, but nothing else here is.
        return Ok(Format::Excel);
    }

    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    // The older binary spreadsheet formats have their own signatures, and
    // there is no point reimplementing them: the extension is enough, since
    // the reader will complain clearly if it is wrong.
    if matches!(ext.as_str(), "xls" | "xlsb" | "ods") {
        return Ok(Format::Excel);
    }

    let text = std::fs::read_to_string(path).unwrap_or_default();
    if text.trim().is_empty() {
        return Err(unrecognised());
    }
    let trimmed = text.trim_start();

    // Content first, and only then the extension, so a renamed file still
    // reads.
    if trimmed.starts_with('{') {
        #[cfg(feature = "json")]
        {
            return Ok(if crate::json::looks_like_powermodels(&text) {
                Format::PowerModels
            } else {
                Format::NativeJson
            });
        }
        #[cfg(not(feature = "json"))]
        return Ok(Format::NativeJson);
    }
    if trimmed.starts_with("<?xml") || looks_like_cim(trimmed) {
        return Ok(Format::Cgmes);
    }
    if looks_like_matpower(&text) {
        return Ok(Format::Matpower);
    }
    if looks_like_psse(trimmed) {
        return Ok(Format::Psse);
    }

    // Nothing in the content settled it, so fall back to the name.
    match ext.as_str() {
        "m" => Ok(Format::Matpower),
        "raw" | "rawx" => Ok(Format::Psse),
        "json" => Ok(Format::NativeJson),
        "nc" | "h5" | "hdf5" | "cdf" => Ok(Format::Netcdf),
        "xlsx" => Ok(Format::Excel),
        "xml" | "rdf" => Ok(Format::Cgmes),
        // A lone CSV is only meaningful next to its siblings, so the directory
        // is what to point at.
        "csv" if path.file_stem().is_some_and(|s| s == "buses") => Ok(Format::CsvDirectory),
        _ => Err(unrecognised()),
    }
}

#[cfg_attr(feature = "all-formats", allow(dead_code))]
fn not_built(path: &Path, format: Format, feature: &'static str) -> IoError {
    IoError::Detect(DetectError::NotBuilt {
        path: path.display().to_string(),
        format: format.label(),
        feature,
    })
}

/// Read a network from a path, whatever format it turns out to be in.
///
/// Returns a [`Case`] uniformly, including for the formats whose readers
/// return a bare network, so a caller always has the notes channel and never
/// has to know which reader ran to find out what was dropped.
pub fn load_any(path: impl AsRef<Path>) -> Result<Case, IoError> {
    let path = path.as_ref();
    let format = sniff(path).map_err(IoError::Detect)?;
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "network".into());

    // Every branch prefixes its notes with what the file was taken to be. A
    // misidentified file is the one failure mode this whole module can have,
    // and saying the answer out loud is what makes it visible.
    let mut case = match format {
        Format::CsvDirectory => {
            let dir = if path.is_dir() {
                path.to_path_buf()
            } else {
                path.parent().unwrap_or(Path::new(".")).to_path_buf()
            };
            Case {
                name,
                network: crate::load_network(dir)?,
                notes: Vec::new(),
            }
        }
        Format::Matpower => crate::matpower::load_case(path)?,
        Format::Psse => crate::psse::load_raw(path)?,
        Format::ParquetDirectory => {
            #[cfg(feature = "parquet")]
            {
                let dir = if path.is_dir() {
                    path.to_path_buf()
                } else {
                    path.parent().unwrap_or(Path::new(".")).to_path_buf()
                };
                Case {
                    name,
                    network: crate::parquet::load_network(dir)?,
                    notes: Vec::new(),
                }
            }
            #[cfg(not(feature = "parquet"))]
            return Err(not_built(path, format, "parquet"));
        }
        Format::PowerModels => {
            #[cfg(feature = "json")]
            {
                crate::json::load_powermodels(path)?
            }
            #[cfg(not(feature = "json"))]
            return Err(not_built(path, format, "json"));
        }
        Format::NativeJson => {
            #[cfg(feature = "json")]
            {
                Case {
                    name,
                    network: crate::json::load_network(path)?,
                    notes: Vec::new(),
                }
            }
            #[cfg(not(feature = "json"))]
            return Err(not_built(path, format, "json"));
        }
        Format::Netcdf => {
            #[cfg(feature = "netcdf")]
            {
                crate::netcdf::load_network(path)?
            }
            #[cfg(not(feature = "netcdf"))]
            return Err(not_built(path, format, "netcdf"));
        }
        Format::Excel => {
            #[cfg(feature = "excel")]
            {
                Case {
                    name,
                    network: crate::excel::load_network(path)?,
                    notes: Vec::new(),
                }
            }
            #[cfg(not(feature = "excel"))]
            return Err(not_built(path, format, "excel"));
        }
        Format::Cgmes => {
            #[cfg(feature = "cgmes")]
            {
                crate::cgmes::load_model(path)?
            }
            #[cfg(not(feature = "cgmes"))]
            return Err(not_built(path, format, "cgmes"));
        }
    };
    case.notes.insert(0, format!("read as {}", format.label()));
    Ok(case)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_psse_header_is_told_apart_from_an_ordinary_csv() {
        assert!(looks_like_psse("0,   100.00, 33, 0, 0, 60.00     / PSS(R)E-33"));
        assert!(looks_like_psse("0, 100.00, 29"));
        // A network CSV's header row is not a PSS/E header.
        assert!(!looks_like_psse("name,bus,p_nom,marginal_cost"));
        // Nor is a revision far outside the range PSS/E has ever used.
        assert!(!looks_like_psse("0, 100.00, 1998"));
        assert!(!looks_like_psse(""));
    }

    #[test]
    fn a_format_reports_whether_this_build_can_read_it() {
        // Detection is deliberately independent of the feature flags, so that
        // a build without Parquet says so instead of claiming not to
        // recognise the file.
        assert!(Format::Matpower.available());
        assert!(Format::CsvDirectory.available());
        assert_eq!(Format::Netcdf.available(), cfg!(feature = "netcdf"));
    }
}

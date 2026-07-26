//! Reading networks with no filesystem in sight.
//!
//! Every reader in this crate also has a path-free entry point, and this is
//! where they meet. The reason is not tidiness: the interface this engine is
//! headed for runs in a browser, where there is no filesystem to open, and a
//! file picker hands over a name and a buffer. A library that can only read
//! from disk cannot be the library that interface imports.
//!
//! Two shapes cover what a user can actually hand over:
//!
//! - [`load_bytes`] for the formats that are one file — MATPOWER, PSS/E, both
//!   JSON dialects, a spreadsheet, a PyPSA netCDF, a self-contained CIM
//!   document.
//! - [`load_files`] for the formats that are several — a CSV or Parquet
//!   directory, a CGMES model split across its profiles. This is what a
//!   multiple-selection file picker or a dropped folder produces.
//!
//! Neither touches the disk, so both work on `wasm32`.

use std::collections::BTreeMap;

use crate::csv::Table;
use crate::{Case, DetectError, Format, IoError, TableSource};

/// A set of named buffers standing in for a directory.
///
/// Names are matched on their last path component, so a picker that hands over
/// `network/buses.csv` and one that hands over `buses.csv` behave the same.
pub struct Files {
    entries: BTreeMap<String, Vec<u8>>,
}

fn basename(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase()
}

impl Files {
    pub fn new<I, S>(files: I) -> Self
    where
        I: IntoIterator<Item = (S, Vec<u8>)>,
        S: AsRef<str>,
    {
        Self {
            entries: files
                .into_iter()
                .map(|(name, bytes)| (basename(name.as_ref()), bytes))
                .collect(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.entries.get(&basename(name)).map(Vec::as_slice)
    }

    pub fn has(&self, name: &str) -> bool {
        self.entries.contains_key(&basename(name))
    }

    /// Names, in a stable order.
    pub fn names(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn with_extension<'a>(&'a self, ext: &str) -> Vec<(&'a str, &'a [u8])> {
        self.entries
            .iter()
            .filter(|(name, _)| name.ends_with(ext))
            .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
            .collect()
    }
}

/// A [`Files`] read as a directory of CSVs.
struct CsvFiles<'a>(&'a Files);

impl TableSource for CsvFiles<'_> {
    fn table(&self, stem: &str) -> Result<Option<Table>, IoError> {
        let name = self.label(stem);
        let Some(bytes) = self.0.get(&name) else {
            return Ok(None);
        };
        let text = String::from_utf8_lossy(bytes);
        Table::parse(&text).map(Some).map_err(|source| IoError::Csv {
            file: name,
            source,
        })
    }

    fn text(&self, name: &str) -> Result<Option<String>, IoError> {
        Ok(self
            .0
            .get(name)
            .map(|b| String::from_utf8_lossy(b).into_owned()))
    }

    fn label(&self, stem: &str) -> String {
        format!("{stem}.csv")
    }
}

/// A [`Files`] read as a directory of Parquet tables.
#[cfg(feature = "parquet")]
struct ParquetFiles<'a>(&'a Files);

#[cfg(feature = "parquet")]
impl TableSource for ParquetFiles<'_> {
    fn table(&self, stem: &str) -> Result<Option<Table>, IoError> {
        let name = self.label(stem);
        let Some(bytes) = self.0.get(&name) else {
            return Ok(None);
        };
        crate::parquet::table_from_bytes(bytes.to_vec(), &name)
    }

    fn text(&self, name: &str) -> Result<Option<String>, IoError> {
        Ok(self
            .0
            .get(name)
            .map(|b| String::from_utf8_lossy(b).into_owned()))
    }

    fn label(&self, stem: &str) -> String {
        format!("{stem}.parquet")
    }
}

const HDF5_MAGIC: &[u8] = b"\x89HDF\r\n\x1a\n";
const ZIP_MAGIC: &[u8] = b"PK\x03\x04";
const PARQUET_MAGIC: &[u8] = b"PAR1";

/// Identify a single file from its bytes, and its name if one is known.
///
/// The name is a hint and nothing more. Content decides wherever content can,
/// which is most of the time and always for the binary formats.
pub fn sniff_bytes(name: Option<&str>, bytes: &[u8]) -> Result<Format, DetectError> {
    let label = name.unwrap_or("<buffer>").to_string();
    let unrecognised = || DetectError::Unrecognised {
        path: label.clone(),
    };

    if bytes.starts_with(HDF5_MAGIC) {
        return Ok(Format::Netcdf);
    }
    if bytes.starts_with(PARQUET_MAGIC) {
        return Ok(Format::ParquetDirectory);
    }
    if bytes.starts_with(ZIP_MAGIC) {
        // A spreadsheet and a published CGMES model are both zips, and are
        // told apart by what is inside rather than by the extension.
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
        return Ok(
            if head.contains("[Content_Types].xml")
                || head.contains("xl/")
                || head.contains("mimetype")
            {
                Format::Excel
            } else {
                Format::Cgmes
            },
        );
    }

    let ext = name
        .and_then(|n| n.rsplit('.').next())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if matches!(ext.as_str(), "xls" | "xlsb" | "ods") {
        return Ok(Format::Excel);
    }

    // Lossy on purpose: a stray byte in a comment should not stop a MATPOWER
    // case being recognised.
    let text = String::from_utf8_lossy(bytes);
    if text.trim().is_empty() {
        return Err(unrecognised());
    }
    crate::detect::sniff_text(&text, &ext).ok_or_else(unrecognised)
}

/// Read a single-file network from bytes.
pub fn load_bytes(name: Option<&str>, bytes: &[u8]) -> Result<Case, IoError> {
    let format = sniff_bytes(name, bytes).map_err(IoError::Detect)?;
    let label = name.unwrap_or("network");
    let stem = label
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(label)
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(label)
        .to_string();

    let text = || String::from_utf8_lossy(bytes).into_owned();

    let mut case = match format {
        Format::Matpower => crate::matpower::parse_case(&text(), stem)?,
        Format::Psse => crate::psse::parse_raw(&text(), stem)?,
        Format::Cgmes => {
            #[cfg(feature = "cgmes")]
            {
                // An archive holds the profiles; a bare document is one of them.
                if bytes.starts_with(ZIP_MAGIC) {
                    let docs = crate::cgmes::documents_from_zip(bytes.to_vec(), label)?;
                    crate::cgmes::parse_model(&docs, stem)?
                } else {
                    crate::cgmes::parse_model(&[(label.to_string(), text())], stem)?
                }
            }
            #[cfg(not(feature = "cgmes"))]
            return Err(missing(label, format, "cgmes"));
        }
        Format::PowerModels => {
            #[cfg(feature = "json")]
            {
                crate::json::parse_powermodels(&text(), stem)?
            }
            #[cfg(not(feature = "json"))]
            return Err(missing(label, format, "json"));
        }
        Format::NativeJson => {
            #[cfg(feature = "json")]
            {
                Case {
                    name: stem,
                    network: crate::json::from_str(&text())?,
                    notes: Vec::new(),
                }
            }
            #[cfg(not(feature = "json"))]
            return Err(missing(label, format, "json"));
        }
        Format::Netcdf => {
            #[cfg(feature = "netcdf")]
            {
                crate::netcdf::parse_network(bytes.to_vec(), &stem)?
            }
            #[cfg(not(feature = "netcdf"))]
            return Err(missing(label, format, "netcdf"));
        }
        Format::Excel => {
            #[cfg(feature = "excel")]
            {
                Case {
                    name: stem,
                    network: crate::excel::parse_network(bytes.to_vec(), label)?,
                    notes: Vec::new(),
                }
            }
            #[cfg(not(feature = "excel"))]
            return Err(missing(label, format, "excel"));
        }
        // A single table out of a directory is not a network. Saying which
        // call to use is more helpful than reporting a parse failure inside a
        // file that is perfectly valid.
        Format::CsvDirectory | Format::ParquetDirectory => {
            return Err(IoError::Detect(DetectError::NeedsSeveralFiles {
                path: label.to_string(),
                format: format.label(),
            }));
        }
    };
    case.notes.insert(0, format!("read as {}", format.label()));
    Ok(case)
}

#[cfg_attr(feature = "all-formats", allow(dead_code))]
fn missing(path: &str, format: Format, feature: &'static str) -> IoError {
    IoError::Detect(DetectError::NotBuilt {
        path: path.to_string(),
        format: format.label(),
        feature,
    })
}

/// Read a network from a set of files with no directory behind them.
///
/// What a multiple-selection picker gives you. The set is identified the same
/// way a directory is — by what is in it — and a set holding exactly one file
/// falls through to [`load_bytes`], so a caller never has to decide which
/// entry point applies.
pub fn load_files(files: &Files) -> Result<Case, IoError> {
    if files.is_empty() {
        return Err(IoError::Detect(DetectError::Unrecognised {
            path: "<no files>".into(),
        }));
    }

    if files.has("buses.csv") {
        let mut case = Case {
            name: "network".into(),
            network: crate::assemble(&CsvFiles(files))?,
            notes: Vec::new(),
        };
        case.notes
            .insert(0, format!("read as {}", Format::CsvDirectory.label()));
        return Ok(case);
    }

    if files.has("buses.parquet") {
        #[cfg(feature = "parquet")]
        {
            let mut net = crate::assemble(&ParquetFiles(files))?;
            // The wide series stay numeric, exactly as they do from disk.
            let gen_names: Vec<String> = net.generators.iter().map(|g| g.name.clone()).collect();
            let load_names: Vec<String> = net.loads.iter().map(|l| l.name.clone()).collect();
            let n = net.n_snapshots();
            if let Some(bytes) = files.get("gen_availability.parquet")
                && let Some(ts) = crate::parquet::wide_from_bytes(
                    bytes.to_vec(),
                    "gen_availability.parquet",
                    &gen_names,
                    n,
                    &vec![1.0; gen_names.len()],
                )?
            {
                net.gen_availability = ts;
            }
            let defaults: Vec<f64> = net.loads.iter().map(|l| l.p_set).collect();
            if let Some(bytes) = files.get("load_profile.parquet")
                && let Some(ts) = crate::parquet::wide_from_bytes(
                    bytes.to_vec(),
                    "load_profile.parquet",
                    &load_names,
                    n,
                    &defaults,
                )?
            {
                net.load_profile = ts;
            }
            net.validate()?;
            let mut case = Case {
                name: "network".into(),
                network: net,
                notes: Vec::new(),
            };
            case.notes
                .insert(0, format!("read as {}", Format::ParquetDirectory.label()));
            return Ok(case);
        }
        #[cfg(not(feature = "parquet"))]
        return Err(missing(
            "buses.parquet",
            Format::ParquetDirectory,
            "parquet",
        ));
    }

    // Several XML documents are a CGMES model split across its profiles, and
    // none of them is a network alone.
    let xml = files.with_extension(".xml");
    if !xml.is_empty() {
        #[cfg(feature = "cgmes")]
        {
            let documents: Vec<(String, String)> = xml
                .iter()
                .map(|(name, bytes)| {
                    (
                        (*name).to_string(),
                        String::from_utf8_lossy(bytes).into_owned(),
                    )
                })
                .collect();
            let mut case = crate::cgmes::parse_model(&documents, "model")?;
            case.notes
                .insert(0, format!("read as {}", Format::Cgmes.label()));
            return Ok(case);
        }
        #[cfg(not(feature = "cgmes"))]
        return Err(missing("model.xml", Format::Cgmes, "cgmes"));
    }

    // One file and nothing recognisable about the set: read it on its own.
    let names = files.names();
    if names.len() == 1 {
        let name = names[0].to_string();
        let bytes = files.get(&name).unwrap().to_vec();
        return load_bytes(Some(&name), &bytes);
    }

    Err(IoError::Detect(DetectError::Unrecognised {
        path: names.join(", "),
    }))
}

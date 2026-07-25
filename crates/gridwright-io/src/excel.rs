//! Spreadsheets, because that is how a great deal of the world publishes.
//!
//! Europe has PyPSA and ENTSO-E. North America has PSS/E. Much of the rest of
//! the world publishes its energy statistics as a workbook: national grid
//! companies, ministry planning annexes, the IEA's country tables. Refusing to
//! read those means refusing to model the places that publish that way, and
//! that is most of Asia.
//!
//! One workbook stands in for a directory: a sheet named `buses` is
//! `buses.csv`, a sheet named `generators` is `generators.csv`, and so on.
//! Sheet names are matched without regard to case or surrounding whitespace,
//! because a sheet tab called `Generators ` is extremely common and is not a
//! different sheet.
//!
//! `.xlsx`, `.xls`, `.xlsb` and OpenDocument `.ods` all work, since they all
//! arrive through the same reader.
//!
//! # What a spreadsheet does to numbers
//!
//! A cell holding `1.0` may arrive as a float, an integer, a string, a date,
//! or an error value, depending on how it was typed and what the sheet's
//! formatting did to it. Every case is rendered to text and handed to the same
//! column logic the CSV reader uses, so a workbook and a CSV of the same data
//! produce the same network rather than two subtly different ones.

use std::path::Path;

use calamine::{Data, Reader, open_workbook_auto};
use gridwright_net::Network;

use crate::csv::Table;
use crate::{IoError, TableSource};

#[derive(Debug, thiserror::Error)]
pub enum ExcelError {
    #[error("{file}: {message}")]
    Open { file: String, message: String },
    #[error("{file}: sheet `{sheet}` could not be read: {message}")]
    Sheet {
        file: String,
        sheet: String,
        message: String,
    },
    #[error("{file} has no sheet named `buses`; sheets present: {found}")]
    NoBuses { file: String, found: String },
}

/// One cell as text.
///
/// Floats use the shortest round-tripping form, and an integer-valued float
/// keeps its fractional zero rather than being rendered as an integer, so that
/// nothing downstream has to guess which it was.
fn cell(d: &Data) -> String {
    match d {
        Data::Empty => String::new(),
        Data::String(s) => s.trim().to_string(),
        Data::Float(f) => format!("{f:?}"),
        Data::Int(i) => i.to_string(),
        Data::Bool(b) => b.to_string(),
        Data::DateTime(dt) => dt.as_f64().to_string(),
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        // An error cell is left empty rather than propagated: a spreadsheet
        // full of `#N/A` in an optional column should still load, and the
        // column logic already treats an empty cell as "use the default".
        Data::Error(_) => String::new(),
    }
}

/// A workbook, standing in for a directory of tables.
pub struct Workbook {
    path: String,
    /// Sheet name as written, keyed by its normalised form.
    sheets: std::collections::HashMap<String, String>,
    book: std::cell::RefCell<calamine::Sheets<std::io::BufReader<std::fs::File>>>,
}

fn normalise(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace([' ', '-'], "_")
}

impl Workbook {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, IoError> {
        let path = path.as_ref();
        let label = path.display().to_string();
        let book = open_workbook_auto(path).map_err(|e| {
            IoError::Excel(ExcelError::Open {
                file: label.clone(),
                message: e.to_string(),
            })
        })?;
        let sheets = book
            .sheet_names()
            .iter()
            .map(|n| (normalise(n), n.clone()))
            .collect();
        Ok(Self {
            path: label,
            sheets,
            book: std::cell::RefCell::new(book),
        })
    }

    /// Sheet names as the workbook spells them.
    pub fn sheet_names(&self) -> Vec<String> {
        self.book.borrow().sheet_names().to_vec()
    }
}

impl TableSource for Workbook {
    fn table(&self, stem: &str) -> Result<Option<Table>, IoError> {
        let Some(actual) = self.sheets.get(&normalise(stem)) else {
            return Ok(None);
        };
        let range = self
            .book
            .borrow_mut()
            .worksheet_range(actual)
            .map_err(|e| {
                IoError::Excel(ExcelError::Sheet {
                    file: self.path.clone(),
                    sheet: actual.clone(),
                    message: e.to_string(),
                })
            })?;

        let mut rows = range.rows();
        let Some(head) = rows.next() else {
            return Ok(Some(Table::default()));
        };
        let header: Vec<String> = head.iter().map(cell).collect();
        // Trailing empty rows are what a spreadsheet leaves behind when
        // someone deletes content without deleting the rows, and they are not
        // components.
        let body: Vec<Vec<String>> = rows
            .map(|r| r.iter().map(cell).collect::<Vec<String>>())
            .filter(|r| r.iter().any(|c| !c.is_empty()))
            .collect();
        Ok(Some(Table::from_parts(header, body)))
    }

    fn text(&self, name: &str) -> Result<Option<String>, IoError> {
        // The single-value settings become a one-cell sheet: `co2_price.txt`
        // is a sheet called `co2_price` whose first cell holds the number.
        let stem = name.trim_end_matches(".txt");
        let Some(t) = self.table(stem)? else {
            return Ok(None);
        };
        // The value may be the header itself, since a one-cell sheet has no
        // separate heading row.
        Ok(t.header
            .first()
            .cloned()
            .filter(|s| !s.is_empty())
            .or_else(|| t.rows.first().and_then(|r| r.first().cloned())))
    }

    fn label(&self, stem: &str) -> String {
        format!("sheet `{stem}`")
    }
}

/// Load a network from a workbook.
pub fn load_network(path: impl AsRef<Path>) -> Result<Network, IoError> {
    let path = path.as_ref();
    let book = Workbook::open(path)?;
    if book.table("buses")?.is_none() {
        return Err(IoError::Excel(ExcelError::NoBuses {
            file: path.display().to_string(),
            found: book.sheet_names().join(", "),
        }));
    }
    crate::assemble(&book)
}

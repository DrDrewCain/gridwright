//! A small CSV reader and writer.
//!
//! Written rather than depended on. A library other people embed should not
//! drag in a parser for a format this small, and the subset that matters here
//! is well defined: comma separated, optional double quotes, doubled quotes
//! for a literal quote, CRLF or LF line endings.
//!
//! What it deliberately does not do: custom delimiters, comments, or streaming
//! over files too large to hold. Network description files are small; the time
//! series files are the large ones, and those are handled by reading a row at a
//! time rather than by making this parser cleverer.

use std::collections::HashMap;

/// A parsed table: a header and its rows.
#[derive(Debug, Clone, Default)]
pub struct Table {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
    index: HashMap<String, usize>,
}

impl Table {
    /// Parse a whole CSV document.
    pub fn parse(text: &str) -> Result<Self, CsvError> {
        let mut records = parse_records(text)?;
        if records.is_empty() {
            return Ok(Self::default());
        }
        let header = records.remove(0);
        let index = header
            .iter()
            .enumerate()
            .map(|(i, h)| (h.trim().to_ascii_lowercase(), i))
            .collect();
        // A short row is padded rather than rejected: trailing empty columns
        // are extremely common in files exported from spreadsheets, and
        // treating them as a fatal error would reject valid data.
        let width = header.len();
        for (i, r) in records.iter_mut().enumerate() {
            if r.len() > width {
                return Err(CsvError::TooManyFields {
                    line: i + 2,
                    got: r.len(),
                    want: width,
                });
            }
            r.resize(width, String::new());
        }
        Ok(Self {
            header,
            rows: records,
            index,
        })
    }

    /// Column position by case-insensitive name.
    pub fn column(&self, name: &str) -> Option<usize> {
        self.index.get(&name.to_ascii_lowercase()).copied()
    }

    fn cell(&self, row: usize, name: &str) -> Option<&str> {
        let c = self.column(name)?;
        self.rows[row].get(c).map(|s| s.trim())
    }

    /// A required string field.
    pub fn text(&self, row: usize, name: &str) -> Result<String, CsvError> {
        match self.cell(row, name) {
            Some(v) if !v.is_empty() => Ok(v.to_string()),
            _ => Err(CsvError::MissingField {
                line: row + 2,
                field: name.to_string(),
            }),
        }
    }

    /// A numeric field with a default when the column or the cell is absent.
    ///
    /// Absent and empty both fall back, because a column that exists but is
    /// blank for one row means the same thing as no column at all: use the
    /// default. Anything present but unparseable is an error rather than a
    /// silent zero.
    pub fn number(&self, row: usize, name: &str, default: f64) -> Result<f64, CsvError> {
        match self.cell(row, name) {
            None | Some("") => Ok(default),
            Some(v) => {
                // `inf` is genuinely useful here: an unbounded capacity ceiling
                // is the natural way to say "as much as you like".
                let lower = v.to_ascii_lowercase();
                if lower == "inf" || lower == "infinity" {
                    return Ok(f64::INFINITY);
                }
                v.parse().map_err(|_| CsvError::BadNumber {
                    line: row + 2,
                    field: name.to_string(),
                    value: v.to_string(),
                })
            }
        }
    }

    /// A boolean field. Accepts the spellings real files actually contain.
    pub fn boolean(&self, row: usize, name: &str, default: bool) -> Result<bool, CsvError> {
        match self.cell(row, name) {
            None | Some("") => Ok(default),
            Some(v) => match v.to_ascii_lowercase().as_str() {
                "true" | "t" | "yes" | "y" | "1" => Ok(true),
                "false" | "f" | "no" | "n" | "0" => Ok(false),
                other => Err(CsvError::BadBool {
                    line: row + 2,
                    field: name.to_string(),
                    value: other.to_string(),
                }),
            },
        }
    }
}

/// Split a document into records, honouring quotes.
fn parse_records(text: &str) -> Result<Vec<Vec<String>>, CsvError> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    let mut started = false;

    while let Some(c) = chars.next() {
        started = true;
        if in_quotes {
            if c == '"' {
                // A doubled quote inside a quoted field is a literal quote.
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => in_quotes = true,
            ',' => record.push(std::mem::take(&mut field)),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
            }
            '\n' => {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
            }
            _ => field.push(c),
        }
    }
    if in_quotes {
        return Err(CsvError::UnterminatedQuote);
    }
    // A file not ending in a newline still has a final record.
    if started && (!field.is_empty() || !record.is_empty()) {
        record.push(field);
        records.push(record);
    }
    // Blank lines carry no data and are dropped rather than becoming rows of
    // one empty field.
    records.retain(|r| !(r.len() == 1 && r[0].trim().is_empty()));
    Ok(records)
}

/// Quote a field if it contains anything that would otherwise break the format.
pub fn escape(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum CsvError {
    #[error("line {line}: {got} fields where the header has {want}")]
    TooManyFields {
        line: usize,
        got: usize,
        want: usize,
    },
    #[error("line {line}: required field `{field}` is missing or empty")]
    MissingField { line: usize, field: String },
    #[error("line {line}: field `{field}` has value `{value}`, which is not a number")]
    BadNumber {
        line: usize,
        field: String,
        value: String,
    },
    #[error("line {line}: field `{field}` has value `{value}`, which is not a boolean")]
    BadBool {
        line: usize,
        field: String,
        value: String,
    },
    #[error("file ended inside a quoted field")]
    UnterminatedQuote,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_table() {
        let t = Table::parse("name,bus\na,0\nb,1\n").unwrap();
        assert_eq!(t.header, vec!["name", "bus"]);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.text(0, "name").unwrap(), "a");
        assert_eq!(t.number(1, "bus", -1.0).unwrap(), 1.0);
    }

    #[test]
    fn column_lookup_ignores_case() {
        let t = Table::parse("Name,P_Nom\nx,5\n").unwrap();
        assert_eq!(t.text(0, "name").unwrap(), "x");
        assert_eq!(t.number(0, "p_nom", 0.0).unwrap(), 5.0);
    }

    #[test]
    fn quoted_fields_may_contain_commas_and_quotes() {
        let t = Table::parse("name,note\n\"a,b\",\"he said \"\"hi\"\"\"\n").unwrap();
        assert_eq!(t.text(0, "name").unwrap(), "a,b");
        assert_eq!(t.text(0, "note").unwrap(), "he said \"hi\"");
    }

    #[test]
    fn handles_crlf_and_a_missing_final_newline() {
        let t = Table::parse("a,b\r\n1,2\r\n3,4").unwrap();
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.number(1, "a", 0.0).unwrap(), 3.0);
    }

    #[test]
    fn short_rows_are_padded_rather_than_rejected() {
        // Spreadsheet exports do this constantly.
        let t = Table::parse("a,b,c\n1,2\n").unwrap();
        assert_eq!(t.rows[0].len(), 3);
        assert_eq!(t.number(0, "c", 9.0).unwrap(), 9.0);
    }

    #[test]
    fn a_row_longer_than_the_header_is_an_error() {
        assert!(matches!(
            Table::parse("a,b\n1,2,3\n"),
            Err(CsvError::TooManyFields { line: 2, .. })
        ));
    }

    #[test]
    fn blank_lines_are_ignored() {
        let t = Table::parse("a\n1\n\n2\n").unwrap();
        assert_eq!(t.rows.len(), 2);
    }

    #[test]
    fn missing_columns_fall_back_to_the_default() {
        let t = Table::parse("name\nx\n").unwrap();
        assert_eq!(t.number(0, "absent", 7.5).unwrap(), 7.5);
        assert!(t.boolean(0, "absent", true).unwrap());
    }

    #[test]
    fn infinity_is_spelled_out_for_unbounded_capacity() {
        let t = Table::parse("p_nom_max\ninf\n").unwrap();
        assert!(t.number(0, "p_nom_max", 0.0).unwrap().is_infinite());
    }

    #[test]
    fn booleans_accept_the_spellings_real_files_use() {
        let t = Table::parse("a,b,c,d\ntrue,False,1,no\n").unwrap();
        assert!(t.boolean(0, "a", false).unwrap());
        assert!(!t.boolean(0, "b", true).unwrap());
        assert!(t.boolean(0, "c", false).unwrap());
        assert!(!t.boolean(0, "d", true).unwrap());
    }

    #[test]
    fn nonsense_numbers_are_errors_rather_than_silent_zeroes() {
        let t = Table::parse("x\nbanana\n").unwrap();
        assert!(matches!(
            t.number(0, "x", 0.0),
            Err(CsvError::BadNumber { .. })
        ));
    }

    #[test]
    fn an_unterminated_quote_is_reported() {
        assert_eq!(
            Table::parse("a\n\"oops\n").unwrap_err(),
            CsvError::UnterminatedQuote
        );
    }

    #[test]
    fn escaping_round_trips() {
        for s in ["plain", "with,comma", "with\"quote", "with\nnewline"] {
            let doc = format!("v\n{}\n", escape(s));
            let t = Table::parse(&doc).unwrap();
            assert_eq!(t.rows[0][0], s, "round trip failed for {s:?}");
        }
    }

    #[test]
    fn an_empty_document_is_an_empty_table() {
        let t = Table::parse("").unwrap();
        assert!(t.header.is_empty());
        assert!(t.rows.is_empty());
    }
}

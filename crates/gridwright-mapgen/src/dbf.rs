//! The attribute table that sits beside a shapefile, for the columns we want.
//!
//! A shapefile carries geometry and nothing else. Every name, population and
//! class lives in a `.dbf` next to it — dBASE III, a header, a list of fixed
//! width fields, and records of space-padded ASCII. **The join is by position:**
//! record `k` of the `.dbf` describes shape `k` of the `.shp`, and there is no key
//! to check that against. That single fact drives the whole interface below.
//!
//! Written rather than depended on, for the same reason as the shapefile reader:
//! the format is a header and a loop, and a crate would bring a type system for
//! dBASE's date and logical columns that a map generator has no use for.
//!
//! Deliberately narrow: only the columns the caller asks for by name are decoded.
//! Natural Earth's populated-places table has 138 of them across 7,342 records,
//! and building a million strings to read four of them would be the slowest thing
//! in this crate.

/// One record. `deleted` is kept rather than filtered.
///
/// dBASE marks a deleted record with a `*` in its first byte and leaves it in
/// place. Dropping it here would shift every later row and silently re-pair every
/// name with the wrong shape, because the join to the `.shp` is by position and
/// the `.shp` has no corresponding deletion. So the row stays, flagged, and the
/// caller decides.
#[derive(Debug)]
pub struct Row {
    pub deleted: bool,
    /// One value per name passed to [`read`], in that order, trimmed.
    pub values: Vec<String>,
}

#[derive(Debug)]
pub enum Error {
    /// Shorter than its own header says, or a record that runs off the end.
    Truncated { at: usize },
    /// A requested column is not in the table. Named, because the alternative is
    /// a map where every label is silently empty.
    NoSuchField { name: String, available: usize },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Truncated { at } => write!(f, "dbf truncated at byte {at}"),
            Error::NoSuchField { name, available } => {
                write!(f, "no field named {name:?} among the {available} in the table")
            }
        }
    }
}

/// Read `wanted` out of every record, in file order.
///
/// Values are trimmed of the padding dBASE writes and decoded as UTF-8, which is
/// what Natural Earth's `.cpg` files declare. A field that is not valid UTF-8
/// falls back to Latin-1 rather than failing: the value is a label on a map, and
/// refusing to build the whole thing because one city name carries a stray byte
/// would be the wrong trade.
pub fn read(bytes: &[u8], wanted: &[&str]) -> Result<Vec<Row>, Error> {
    if bytes.len() < 32 {
        return Err(Error::Truncated { at: bytes.len() });
    }
    let count = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let header = u16::from_le_bytes(bytes[8..10].try_into().unwrap()) as usize;
    let stride = u16::from_le_bytes(bytes[10..12].try_into().unwrap()) as usize;
    if header < 33 || stride == 0 || header > bytes.len() {
        return Err(Error::Truncated { at: header.min(bytes.len()) });
    }

    // Field descriptors: 32 bytes each, ending at a 0x0D terminator. The offset
    // of each field inside a record is the running sum of the widths before it,
    // after the one-byte deletion flag.
    let mut at = 32;
    let mut fields: Vec<(String, usize, usize)> = Vec::new();
    let mut offset = 1;
    while at + 32 <= header && bytes[at] != 0x0D {
        let name = bytes[at..at + 11]
            .split(|b| *b == 0)
            .next()
            .unwrap_or(&[])
            .iter()
            .map(|b| *b as char)
            .collect::<String>();
        let width = bytes[at + 16] as usize;
        fields.push((name, offset, width));
        offset += width;
        at += 32;
    }

    // Resolved once, not per record. Case-insensitive because Natural Earth
    // capitalises its columns in one file and not in the next -- roads are
    // `name`, places are `NAME`, and a caller should not have to know that.
    let picked: Vec<(usize, usize)> = wanted
        .iter()
        .map(|w| {
            fields
                .iter()
                .find(|(n, _, _)| n.eq_ignore_ascii_case(w))
                .map(|(_, o, l)| (*o, *l))
                .ok_or_else(|| Error::NoSuchField {
                    name: (*w).to_string(),
                    available: fields.len(),
                })
        })
        .collect::<Result<_, _>>()?;

    let mut out = Vec::with_capacity(count);
    for r in 0..count {
        let base = header + r * stride;
        if base + stride > bytes.len() {
            return Err(Error::Truncated { at: base });
        }
        out.push(Row {
            deleted: bytes[base] == b'*',
            values: picked
                .iter()
                .map(|(o, l)| text(&bytes[base + o..base + o + l]))
                .collect(),
        });
    }
    Ok(out)
}

/// Trim dBASE's padding and decode.
fn text(raw: &[u8]) -> String {
    let raw = raw
        .iter()
        .position(|b| *b == 0)
        .map_or(raw, |z| &raw[..z]);
    match std::str::from_utf8(raw) {
        Ok(s) => s.trim().to_string(),
        // Latin-1 maps every byte to a code point, so this cannot fail in turn.
        Err(_) => raw.iter().map(|b| *b as char).collect::<String>().trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table built by hand, so the test does not depend on a fixture nobody
    /// can read.
    fn table(fields: &[(&str, usize)], rows: &[(bool, Vec<&str>)]) -> Vec<u8> {
        let stride: usize = 1 + fields.iter().map(|(_, w)| *w).sum::<usize>();
        let header = 32 + fields.len() * 32 + 1;

        let mut out = vec![0u8; 32];
        out[0] = 3;
        out[4..8].copy_from_slice(&(rows.len() as u32).to_le_bytes());
        out[8..10].copy_from_slice(&(header as u16).to_le_bytes());
        out[10..12].copy_from_slice(&(stride as u16).to_le_bytes());

        for (name, width) in fields {
            let mut d = vec![0u8; 32];
            d[..name.len()].copy_from_slice(name.as_bytes());
            d[10] = b'C';
            d[16] = *width as u8;
            out.extend(d);
        }
        out.push(0x0D);

        for (deleted, values) in rows {
            out.push(if *deleted { b'*' } else { b' ' });
            for ((_, width), v) in fields.iter().zip(values) {
                let mut cell = vec![b' '; *width];
                let b = v.as_bytes();
                cell[..b.len().min(*width)].copy_from_slice(&b[..b.len().min(*width)]);
                out.extend(cell);
            }
        }
        out
    }

    #[test]
    fn reads_the_columns_asked_for_and_no_others() {
        let f = table(
            &[("NAME", 8), ("POP_MAX", 8), ("ADM0NAME", 8)],
            &[(false, vec!["Hamburg", "1757000", "Germany"])],
        );
        let rows = read(&f, &["NAME", "ADM0NAME"]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values, vec!["Hamburg", "Germany"]);
    }

    #[test]
    fn columns_come_back_in_the_order_requested() {
        // Not in table order. A caller unpacking by index would otherwise read
        // the population as the name and be none the wiser.
        let f = table(
            &[("NAME", 8), ("POP_MAX", 8)],
            &[(false, vec!["Berlin", "3406000"])],
        );
        let rows = read(&f, &["POP_MAX", "NAME"]).unwrap();
        assert_eq!(rows[0].values, vec!["3406000", "Berlin"]);
    }

    #[test]
    fn field_names_match_whatever_case_the_file_used() {
        // Natural Earth capitalises places as NAME and roads as name.
        let f = table(&[("name", 8)], &[(false, vec!["A7"])]);
        assert_eq!(read(&f, &["NAME"]).unwrap()[0].values, vec!["A7"]);
        let f = table(&[("NAME", 8)], &[(false, vec!["A7"])]);
        assert_eq!(read(&f, &["name"]).unwrap()[0].values, vec!["A7"]);
    }

    #[test]
    fn a_deleted_record_is_flagged_and_kept_in_place() {
        // **Dropping it would re-pair every later name with the wrong shape.**
        // The join to the .shp is by position and the .shp has no matching
        // deletion, so the row has to stay where it is.
        let f = table(
            &[("NAME", 8)],
            &[
                (false, vec!["Kiel"]),
                (true, vec!["gone"]),
                (false, vec!["Mainz"]),
            ],
        );
        let rows = read(&f, &["NAME"]).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(!rows[0].deleted && rows[1].deleted && !rows[2].deleted);
        assert_eq!(rows[2].values, vec!["Mainz"]);
    }

    #[test]
    fn padding_is_trimmed() {
        let f = table(&[("NAME", 12)], &[(false, vec!["Ulm"])]);
        assert_eq!(read(&f, &["NAME"]).unwrap()[0].values, vec!["Ulm"]);
    }

    #[test]
    fn an_empty_cell_reads_as_an_empty_string() {
        // Roads have a name on some records and not others, and an absent name
        // is not an error.
        let f = table(&[("NAME", 8)], &[(false, vec![""])]);
        assert_eq!(read(&f, &["NAME"]).unwrap()[0].values, vec![""]);
    }

    #[test]
    fn a_missing_column_is_named_rather_than_left_empty() {
        // Silently empty labels would look like a map with no cities on it, and
        // nothing would say why.
        let f = table(&[("NAME", 8)], &[(false, vec!["Kiel"])]);
        match read(&f, &["POP_MAX"]) {
            Err(Error::NoSuchField { name, available }) => {
                assert_eq!(name, "POP_MAX");
                assert_eq!(available, 1);
            }
            other => panic!("expected a named field error, got {other:?}"),
        }
    }

    #[test]
    fn utf8_names_survive() {
        // Natural Earth's .cpg declares UTF-8, and the table has names in it that
        // matter to this project: Duesseldorf, Saarbruecken, Zuerich.
        let f = table(&[("NAME", 16)], &[(false, vec!["Düsseldorf"])]);
        assert_eq!(read(&f, &["NAME"]).unwrap()[0].values, vec!["Düsseldorf"]);
    }

    #[test]
    fn a_field_that_is_not_utf8_falls_back_rather_than_failing() {
        // A label is a label. Refusing to build the whole map because one byte
        // is stray would be the wrong trade.
        let mut f = table(&[("NAME", 4)], &[(false, vec!["ab"])]);
        let last = f.len() - 3;
        f[last] = 0xFF;
        let rows = read(&f, &["NAME"]).unwrap();
        assert!(!rows[0].values[0].is_empty());
    }

    #[test]
    fn a_truncated_table_is_an_error_rather_than_a_short_read() {
        let f = table(&[("NAME", 8)], &[(false, vec!["Kiel"]), (false, vec!["Mainz"])]);
        assert!(matches!(read(&[0u8; 8], &["NAME"]), Err(Error::Truncated { .. })));
        assert!(matches!(
            read(&f[..f.len() - 4], &["NAME"]),
            Err(Error::Truncated { .. })
        ));
    }

    #[test]
    fn a_table_with_no_records_reads_as_no_rows() {
        let f = table(&[("NAME", 8)], &[]);
        assert!(read(&f, &["NAME"]).unwrap().is_empty());
    }
}

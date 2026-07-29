//! A shapefile reader, for exactly the two shape types the map layers use.
//!
//! Written rather than depended on. The format is thirty years old and the part
//! that matters here is a header, a record loop, and two geometry types — about
//! eighty lines. A crate would bring attribute-table parsing, projection
//! handling, an error hierarchy and a geometry model, none of which this needs:
//! the output is a flat list of rings, and the projection is known from the
//! source rather than read from the file.
//!
//! Deliberately narrow. Multipoints, measured and Z-bearing variants are all
//! *rejected* rather than half-handled, because a silently skipped shape type is a
//! layer that comes out mysteriously incomplete.

/// A ring or polyline part, in source coordinates (degrees, WGS 84).
pub type Ring = Vec<[f64; 2]>;

/// What a record turned out to be.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Kind {
    /// Shape type 1: a single position. Populated places.
    ///
    /// Carried as a one-point ring rather than a separate variant of the return
    /// type. A point is not geometry anyone simplifies or triangulates, but it
    /// *is* something the reader has to pair with a `.dbf` row by position, and
    /// keeping one flat list per record is what makes that pairing trivial.
    Point,
    /// Shape type 3: an open polyline. Rivers, boundary lines and roads.
    Polyline,
    /// Shape type 5: a closed polygon. Land, lakes, urban areas.
    Polygon,
}

#[derive(Debug)]
pub enum Error {
    /// Shorter than the 100-byte header, or a record that runs off the end.
    Truncated { at: usize },
    /// A shape type this does not read. Named rather than skipped.
    Unsupported { shape_type: i32 },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Truncated { at } => write!(f, "shapefile truncated at byte {at}"),
            Error::Unsupported { shape_type } => {
                write!(f, "unsupported shapefile shape type {shape_type}")
            }
        }
    }
}

/// Every part of every record, with the kind it came from.
///
/// Parts rather than shapes: a polygon's outer ring and its islands arrive as
/// separate parts of one record, and for a backdrop they are drawn identically.
/// Flattening here means nothing downstream has to know about multi-part shapes.
///
/// **A caller that needs to join against a `.dbf` cannot use this**, because
/// flattening loses which record a part came from. Use [`read_by_record`], which
/// keeps them grouped.
pub fn read(bytes: &[u8]) -> Result<Vec<(Kind, Ring)>, Error> {
    Ok(read_by_record(bytes)?
        .into_iter()
        .flat_map(|(kind, parts)| parts.into_iter().map(move |p| (kind, p)))
        .collect())
}

/// Every record, with its parts still grouped under it.
///
/// One entry per record in file order, including a record whose geometry is null
/// and therefore has no parts. That is what makes a positional join to the
/// attribute table sound: record `k` here is row `k` there, and a null shape that
/// silently vanished would shift every name after it onto the wrong place.
pub fn read_by_record(bytes: &[u8]) -> Result<Vec<(Kind, Vec<Ring>)>, Error> {
    if bytes.len() < 100 {
        return Err(Error::Truncated { at: bytes.len() });
    }

    let mut out = Vec::new();
    let mut at = 100; // the file header, whose contents this does not need

    while at + 8 <= bytes.len() {
        // Record header: number and content length, both big-endian, the length
        // in 16-bit words — the one place this format is not little-endian.
        let words = i32::from_be_bytes(bytes[at + 4..at + 8].try_into().unwrap());
        at += 8;
        let end = at + (words as usize) * 2;
        if end > bytes.len() || words < 2 {
            return Err(Error::Truncated { at });
        }

        let shape_type = i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let kind = match shape_type {
            0 => {
                // A null shape is legal and carries no geometry. Recorded with no
                // parts rather than skipped, so the record count still matches the
                // attribute table's.
                out.push((Kind::Point, Vec::new()));
                at = end;
                continue;
            }
            1 => {
                // A point is just the two doubles after the type, with none of
                // the box, part and offset machinery the other types carry.
                if at + 20 > end {
                    return Err(Error::Truncated { at });
                }
                let x = f64::from_le_bytes(bytes[at + 4..at + 12].try_into().unwrap());
                let y = f64::from_le_bytes(bytes[at + 12..at + 20].try_into().unwrap());
                out.push((Kind::Point, vec![vec![[x, y]]]));
                at = end;
                continue;
            }
            3 => Kind::Polyline,
            5 => Kind::Polygon,
            other => return Err(Error::Unsupported { shape_type: other }),
        };

        // Both remaining types share a layout: bounding box, part count, point
        // count, part offsets, then interleaved x/y doubles.
        let head = at + 4 + 32; // shape type, then the box
        if head + 8 > end {
            return Err(Error::Truncated { at: head });
        }
        let n_parts = i32::from_le_bytes(bytes[head..head + 4].try_into().unwrap()) as usize;
        let n_points = i32::from_le_bytes(bytes[head + 4..head + 8].try_into().unwrap()) as usize;

        let parts_at = head + 8;
        let points_at = parts_at + n_parts * 4;
        if points_at + n_points * 16 > end {
            return Err(Error::Truncated { at: points_at });
        }

        let part_start = |k: usize| -> usize {
            let o = parts_at + k * 4;
            i32::from_le_bytes(bytes[o..o + 4].try_into().unwrap()) as usize
        };
        let point = |i: usize| -> [f64; 2] {
            let o = points_at + i * 16;
            [
                f64::from_le_bytes(bytes[o..o + 8].try_into().unwrap()),
                f64::from_le_bytes(bytes[o + 8..o + 16].try_into().unwrap()),
            ]
        };

        let mut parts = Vec::with_capacity(n_parts);
        for k in 0..n_parts {
            let from = part_start(k);
            let to = if k + 1 < n_parts {
                part_start(k + 1)
            } else {
                n_points
            };
            if from >= to || to > n_points {
                continue;
            }
            parts.push((from..to).map(point).collect());
        }
        out.push((kind, parts));

        at = end;
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal one-record file, built by hand so the test does not depend on
    /// a fixture nobody can read.
    fn polygon_file(rings: &[&[[f64; 2]]]) -> Vec<u8> {
        let mut content = Vec::new();
        content.extend(5i32.to_le_bytes()); // polygon
        content.extend([0f64; 4].iter().flat_map(|v| v.to_le_bytes())); // box
        content.extend((rings.len() as i32).to_le_bytes());
        let total: usize = rings.iter().map(|r| r.len()).sum();
        content.extend((total as i32).to_le_bytes());
        let mut offset = 0i32;
        for r in rings {
            content.extend(offset.to_le_bytes());
            offset += r.len() as i32;
        }
        for r in rings {
            for p in *r {
                content.extend(p[0].to_le_bytes());
                content.extend(p[1].to_le_bytes());
            }
        }

        let mut file = vec![0u8; 100];
        file.extend(1i32.to_be_bytes()); // record number
        file.extend(((content.len() / 2) as i32).to_be_bytes());
        file.extend(content);
        file
    }

    #[test]
    fn reads_a_single_ring() {
        let ring: &[[f64; 2]] = &[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]];
        let got = read(&polygon_file(&[ring])).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, Kind::Polygon);
        assert_eq!(got[0].1, ring.to_vec());
    }

    #[test]
    fn a_multi_part_shape_becomes_separate_rings() {
        // An outer ring and an island arrive as two parts of one record. They
        // are drawn identically, so flattening here means nothing downstream
        // has to know multi-part shapes exist.
        let a: &[[f64; 2]] = &[[0.0, 0.0], [2.0, 0.0], [2.0, 2.0]];
        let b: &[[f64; 2]] = &[[5.0, 5.0], [6.0, 5.0], [6.0, 6.0], [5.0, 6.0]];
        let got = read(&polygon_file(&[a, b])).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].1.len(), 3);
        assert_eq!(got[1].1.len(), 4);
    }

    #[test]
    fn a_file_shorter_than_its_header_is_an_error() {
        assert!(matches!(read(&[0u8; 40]), Err(Error::Truncated { .. })));
    }

    #[test]
    fn a_record_running_past_the_end_is_an_error() {
        let mut f = polygon_file(&[&[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]]);
        f.truncate(f.len() - 20);
        assert!(matches!(read(&f), Err(Error::Truncated { .. })));
    }

    #[test]
    fn an_unsupported_shape_type_is_named_rather_than_skipped() {
        // A silently skipped shape type is a layer that comes out mysteriously
        // incomplete, which is far harder to notice than a failed build.
        let mut f = vec![0u8; 100];
        let mut content = Vec::new();
        content.extend(11i32.to_le_bytes()); // PolygonZ
        content.extend([0u8; 64]);
        f.extend(1i32.to_be_bytes());
        f.extend(((content.len() / 2) as i32).to_be_bytes());
        f.extend(content);
        assert!(matches!(
            read(&f),
            Err(Error::Unsupported { shape_type: 11 })
        ));
    }

    #[test]
    fn a_null_shape_is_skipped_without_failing() {
        // Legal, and carries no geometry.
        let mut f = vec![0u8; 100];
        let content = 0i32.to_le_bytes();
        f.extend(1i32.to_be_bytes());
        f.extend(((content.len() / 2) as i32).to_be_bytes());
        f.extend(content);
        assert!(read(&f).unwrap().is_empty());
    }

    /// A file of point records, which is how populated places arrive.
    fn point_file(points: &[[f64; 2]]) -> Vec<u8> {
        let mut file = vec![0u8; 100];
        for (i, p) in points.iter().enumerate() {
            let mut content = Vec::new();
            content.extend(1i32.to_le_bytes());
            content.extend(p[0].to_le_bytes());
            content.extend(p[1].to_le_bytes());
            file.extend((i as i32 + 1).to_be_bytes());
            file.extend(((content.len() / 2) as i32).to_be_bytes());
            file.extend(content);
        }
        file
    }

    #[test]
    fn reads_point_records() {
        let got = read(&point_file(&[[13.405, 52.52], [9.993, 53.551]])).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, Kind::Point);
        assert_eq!(got[0].1, vec![[13.405, 52.52]]);
        assert_eq!(got[1].1, vec![[9.993, 53.551]]);
    }

    #[test]
    fn a_truncated_point_record_is_an_error() {
        let mut f = point_file(&[[13.405, 52.52]]);
        f.truncate(f.len() - 6);
        assert!(matches!(read(&f), Err(Error::Truncated { .. })));
    }

    #[test]
    fn a_record_read_keeps_one_entry_per_record() {
        // **This is what makes a join to the .dbf sound.** Flattening parts is
        // right for drawing and wrong for naming: a two-part polygon would look
        // like two records and every name after it would land on the wrong shape.
        let a: &[[f64; 2]] = &[[0.0, 0.0], [2.0, 0.0], [2.0, 2.0]];
        let b: &[[f64; 2]] = &[[5.0, 5.0], [6.0, 5.0], [6.0, 6.0], [5.0, 6.0]];
        let f = polygon_file(&[a, b]);
        assert_eq!(read(&f).unwrap().len(), 2, "flattened parts");
        let by_record = read_by_record(&f).unwrap();
        assert_eq!(by_record.len(), 1, "one record");
        assert_eq!(by_record[0].1.len(), 2, "two parts under it");
    }

    #[test]
    fn a_null_shape_still_occupies_a_record() {
        // Legal, carries no geometry, and must not shift the records after it --
        // the attribute table has a row for it either way.
        let mut f = vec![0u8; 100];
        let content = 0i32.to_le_bytes();
        f.extend(1i32.to_be_bytes());
        f.extend(((content.len() / 2) as i32).to_be_bytes());
        f.extend(content);
        let mut tail = point_file(&[[1.0, 2.0]]);
        f.extend(tail.drain(100..));

        let by_record = read_by_record(&f).unwrap();
        assert_eq!(by_record.len(), 2);
        assert!(by_record[0].1.is_empty(), "null record has no parts");
        assert_eq!(by_record[1].1, vec![vec![[1.0, 2.0]]]);
        // And the flat view still drops it, because there is nothing to draw.
        assert_eq!(read(&f).unwrap().len(), 1);
    }

    #[test]
    fn an_empty_file_body_reads_as_no_shapes() {
        assert!(read(&[0u8; 100]).unwrap().is_empty());
    }
}

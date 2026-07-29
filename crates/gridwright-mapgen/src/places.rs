//! Named places: the table that turns a wire diagram into somewhere.
//!
//! **Not part of the level pyramid, and that is the point.** Coastlines belong to
//! a pyramid because a coastline has a shape that costs more the closer you look.
//! A place has no shape — it is a position and a name — so there is nothing to
//! simplify per level, and three copies of the same 7,342 names would be waste.
//! One table, filtered at draw time by where the reader is and how much room the
//! labels need.
//!
//! # What the source can and cannot give
//!
//! Natural Earth's populated places is the gazetteer that comes with the licence
//! this project needs, and it is worth being exact about its resolution: it holds
//! **58 places for all of Germany** — national and state capitals and the larger
//! cities. It does not hold towns and it does not hold villages. Naming a place of
//! four thousand people means GeoNames (CC BY, which imposes attribution on every
//! user of this library) or OpenStreetMap (ODbL, which imposes share-alike on
//! derived databases). Both are conditions a library cannot pass on to whoever
//! embeds it, which is the same reason the coastline comes from here.
//!
//! So: cities and regional capitals, named accurately, and no pretence about
//! towns.
//!
//! # Encoding
//!
//! Country and region names are interned. There are 7,342 places across roughly
//! 250 countries and 4,000 first-level regions, so storing "Baden-Württemberg"
//! once and referring to it costs two bytes a place instead of eighteen.
//!
//! ```text
//! u16  label count
//! per label:  u8 length, UTF-8 bytes
//! u32  place count
//! per place:
//!   i16  x, i16 y      quantised over the globe, as in the pyramid
//!   u8   rank << 4 | kind
//!   u16  population in thousands, saturating
//!   u16  region label, u16 country label   (0xFFFF for none)
//!   u8   name length, UTF-8 bytes
//! ```

/// What a place is, which decides how it is drawn rather than how important it is.
///
/// Prominence is `rank`, and the two are deliberately separate: a national
/// capital is drawn with a capital's mark whether or not it is the biggest thing
/// on screen, and a large city is drawn large without being promoted to a capital.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Country,
    Region,
    City,
    Town,
}

impl Kind {
    /// From Natural Earth's `FEATURECLA`, with population settling city from town.
    ///
    /// The source does not distinguish them — everything that is not a capital is
    /// `Populated place` — so the split is ours, at 100,000 people. That is a
    /// choice about drawing, not a claim about status.
    pub fn classify(featurecla: &str, population: u32) -> Kind {
        let f = featurecla.to_ascii_lowercase();
        if f.starts_with("admin-0 capital") {
            Kind::Country
        } else if f.starts_with("admin-1") {
            Kind::Region
        } else if population >= 100_000 {
            Kind::City
        } else {
            Kind::Town
        }
    }

    fn tag(self) -> u8 {
        match self {
            Kind::Country => 0,
            Kind::Region => 1,
            Kind::City => 2,
            Kind::Town => 3,
        }
    }
}

pub struct Place {
    pub lon: f64,
    pub lat: f64,
    pub name: String,
    /// First-level region: a state, province or Land. Empty if the source has none.
    pub region: String,
    pub country: String,
    pub kind: Kind,
    /// Natural Earth's `LABELRANK`, 1 to 10, lower being more prominent. Clamped
    /// into four bits, which is exactly the range it uses.
    pub rank: u8,
    pub population: u32,
}

/// No region or country recorded. Distinct from label 0, which is a real name.
pub const NO_LABEL: u16 = u16::MAX;

pub fn encode(places: &[Place]) -> Vec<u8> {
    // Interned in first-seen order.
    let mut labels: Vec<String> = Vec::new();

    let mut body = Vec::new();
    body.extend((places.len() as u32).to_le_bytes());
    for p in places {
        let region = intern(&p.region, &mut labels);
        let country = intern(&p.country, &mut labels);
        body.extend(quantise(p.lon, 180.0).to_le_bytes());
        body.extend(quantise(p.lat, 90.0).to_le_bytes());
        body.push((p.rank.min(15) << 4) | p.kind.tag());
        body.extend(((p.population / 1000).min(u16::MAX as u32) as u16).to_le_bytes());
        body.extend(region.to_le_bytes());
        body.extend(country.to_le_bytes());
        let name = truncate(&p.name);
        body.push(name.len() as u8);
        body.extend(name.as_bytes());
    }

    let mut out = Vec::new();
    out.extend((labels.len() as u16).to_le_bytes());
    for l in &labels {
        let l = truncate(l);
        out.push(l.len() as u8);
        out.extend(l.as_bytes());
    }
    out.extend(body);
    out
}

/// Add `s` to the table if it is new, and give back its index.
///
/// Linear search. There are a few thousand distinct labels and the alternative is
/// hashing every string in the table anyway; in a build step that runs when the
/// source data changes, the lookup is not the cost.
fn intern(s: &str, labels: &mut Vec<String>) -> u16 {
    if s.is_empty() {
        return NO_LABEL;
    }
    if let Some(i) = labels.iter().position(|l| l == s) {
        return i as u16;
    }
    // Saturating rather than wrapping. At 65,535 distinct labels the rest go
    // unnamed, which a reader can see, where a wrap would quietly relabel Bavaria
    // as some other place's region.
    if labels.len() >= NO_LABEL as usize {
        return NO_LABEL;
    }
    labels.push(s.to_string());
    (labels.len() - 1) as u16
}

/// Cut a name to what a length byte can hold, on a character boundary.
///
/// On a boundary, because cutting mid-sequence would emit bytes that are not
/// UTF-8 and the reader would have to decide what to do about it. Nothing in the
/// source is close to 255 bytes; this exists so the format cannot be violated
/// rather than because it will be tested.
fn truncate(s: &str) -> &str {
    if s.len() <= 255 {
        return s;
    }
    let mut cut = 255;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

/// Degrees to `i16`, saturating. The pyramid's quantisation, for the same reason:
/// a latitude of 90.0001 is a rounding artefact, and wrapping would move it to
/// the other pole.
fn quantise(v: f64, full: f64) -> i16 {
    (v / full * 32767.0).round().clamp(-32767.0, 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(name: &str, region: &str, country: &str) -> Place {
        Place {
            lon: 9.993,
            lat: 53.551,
            name: name.to_string(),
            region: region.to_string(),
            country: country.to_string(),
            kind: Kind::City,
            rank: 3,
            population: 1_757_000,
        }
    }

    #[test]
    fn a_capital_is_classified_by_its_class_not_its_size() {
        // Vaduz has five thousand people and is a national capital.
        assert_eq!(Kind::classify("Admin-0 capital", 5_197), Kind::Country);
        assert_eq!(Kind::classify("Admin-0 capital alt", 100), Kind::Country);
        assert_eq!(Kind::classify("Admin-1 capital", 20_000), Kind::Region);
        assert_eq!(Kind::classify("Admin-1 region capital", 20_000), Kind::Region);
    }

    #[test]
    fn an_ordinary_place_is_split_by_population() {
        // The source calls both of these `Populated place`, so this split is ours.
        assert_eq!(Kind::classify("Populated place", 250_000), Kind::City);
        assert_eq!(Kind::classify("Populated place", 4_000), Kind::Town);
        assert_eq!(Kind::classify("Populated place", 100_000), Kind::City);
    }

    #[test]
    fn repeated_regions_are_stored_once() {
        // The whole reason for interning. Sixty places in one Land should not
        // carry sixty copies of its name.
        let many: Vec<Place> = (0..60)
            .map(|i| place(&format!("Town {i}"), "Baden-Württemberg", "Germany"))
            .collect();
        let blob = encode(&many);
        assert_eq!(
            u16::from_le_bytes(blob[..2].try_into().unwrap()),
            2,
            "region and country, once each",
        );
        let needle = "Baden-Württemberg".as_bytes();
        let copies = blob.windows(needle.len()).filter(|w| *w == needle).count();
        assert_eq!(copies, 1, "the region name is stored once, not sixty times");
    }

    #[test]
    fn a_place_with_no_region_says_so_rather_than_pointing_at_one() {
        // Label 0 is a real name. An absent region has to be distinguishable
        // from the first one in the table, or every unregioned city in the world
        // would claim to be in Uruguay.
        let blob = encode(&[place("Somewhere", "", "")]);
        assert_eq!(u16::from_le_bytes(blob[..2].try_into().unwrap()), 0);
        // header, then count, x, y, packed, population
        let at = 2 + 4 + 2 + 2 + 1 + 2;
        assert_eq!(u16::from_le_bytes(blob[at..at + 2].try_into().unwrap()), NO_LABEL);
    }

    #[test]
    fn rank_and_kind_share_a_byte_without_overwriting_each_other() {
        for rank in 0..=15u8 {
            for kind in [Kind::Country, Kind::Region, Kind::City, Kind::Town] {
                let mut p = place("X", "", "");
                p.rank = rank;
                p.kind = kind;
                let blob = encode(&[p]);
                let packed = blob[2 + 4 + 4];
                assert_eq!(packed >> 4, rank, "rank {rank}");
                assert_eq!(packed & 0x0F, kind.tag(), "kind {kind:?}");
            }
        }
    }

    #[test]
    fn a_rank_outside_four_bits_is_clamped_rather_than_wrapped() {
        // LABELRANK is 1 to 10, but a source is a source. Wrapping would turn the
        // least prominent place on the map into the most.
        let mut p = place("X", "", "");
        p.rank = 200;
        let blob = encode(&[p]);
        assert_eq!(blob[2 + 4 + 4] >> 4, 15);
    }

    #[test]
    fn a_population_beyond_the_field_saturates() {
        let mut p = place("X", "", "");
        p.population = 4_000_000_000;
        let blob = encode(&[p]);
        let at = 2 + 4 + 4 + 1;
        assert_eq!(u16::from_le_bytes(blob[at..at + 2].try_into().unwrap()), u16::MAX);
    }

    #[test]
    fn names_stay_utf8_when_cut() {
        // Cutting mid-sequence would put bytes in the file that are not UTF-8 and
        // leave the reader to decide what to do about it.
        let long = "ü".repeat(200); // 400 bytes
        assert!(truncate(&long).len() <= 255);
        assert!(std::str::from_utf8(truncate(&long).as_bytes()).is_ok());
        assert_eq!(truncate("Düsseldorf"), "Düsseldorf");
    }

    #[test]
    fn an_empty_table_encodes_to_a_readable_header() {
        let blob = encode(&[]);
        assert_eq!(u16::from_le_bytes(blob[..2].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(blob[2..6].try_into().unwrap()), 0);
        assert_eq!(blob.len(), 6);
    }
}

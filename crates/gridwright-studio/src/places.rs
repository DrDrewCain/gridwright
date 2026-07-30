//! City and region names, drawn only where the loaded network actually is.
//!
//! **Scoped to the file, not to the globe.** A basemap that names every city it
//! knows about turns into a wall of type the moment anyone zooms out, and none of
//! it is about the network on screen. So the table ships whole — it is one flat
//! list, filtered at draw time — and nothing is labelled until a network with real
//! coordinates is loaded, at which point the names that appear are the ones inside
//! that network's own extent. A study of eight substations in Baden-Württemberg
//! gets Stuttgart, Karlsruhe and Heilbronn, and does not get Lisbon.
//!
//! Two things follow from Overbye (NAPS 2019) on geographic grid displays, where
//! the finding is that a detailed background "runs the risk of background
//! camouflaging the electric grid information of interest":
//!
//! - Labels are drawn *under* the network, in the dimmest ink the theme has that
//!   is still readable, and never in a colour the network uses to mean something.
//! - There is a hard cap on how many appear, and the cap is enforced by
//!   prominence, so the ones that survive are the ones a reader orients by.
//!
//! Placement is greedy and collision-checked: most prominent first, and a label
//! that would overlap one already placed is dropped rather than nudged. Nudging
//! looks tidier in a still frame and is much worse to pan with, because a label
//! that moves as the camera moves reads as a different label.

use eframe::egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Stroke, Vec2};

/// The table, built by `gridwright-mapgen` from Natural Earth's populated places.
///
/// One file, not one per level: a place has no shape, so there is nothing for a
/// pyramid to simplify.
const TABLE: &[u8] = include_bytes!("map/places.bin");

/// Absent region or country. Zero is a real label, so this cannot be zero.
const NO_LABEL: u16 = u16::MAX;

/// What a place is, which decides how it is drawn. Tags match the generator's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Country,
    Region,
    City,
    Town,
}

impl Kind {
    fn from_tag(t: u8) -> Kind {
        match t {
            0 => Kind::Country,
            1 => Kind::Region,
            2 => Kind::City,
            _ => Kind::Town,
        }
    }

    /// Radius of the mark, in points. A capital reads larger than a town, and
    /// nothing here approaches the size of a substation symbol.
    fn dot(self) -> f32 {
        match self {
            Kind::Country => 2.6,
            Kind::Region => 2.0,
            Kind::City => 1.7,
            Kind::Town => 1.3,
        }
    }
}

#[derive(Clone)]
pub struct Place {
    /// Mercator, matching `layout::project_one`.
    pub at: Pos2,
    pub name: String,
    /// State, province or Land. Empty where the source has none.
    pub region: String,
    pub country: String,
    /// Two-letter country code, for joining to a network file's own codes.
    pub iso: String,
    pub kind: Kind,
    /// Lower is more prominent, 1 to 10.
    pub rank: u8,
    pub population: u32,
}

pub struct Places {
    /// Sorted by prominence at load, so every later query keeps that order for
    /// free and the greedy placement below is simply a scan.
    all: Vec<Place>,
}

impl Default for Places {
    fn default() -> Self {
        Self::load()
    }
}

impl Places {
    pub fn load() -> Self {
        let mut all = decode(TABLE);
        // Rank first, population as the tie-break. Natural Earth's LABELRANK is
        // coarse -- most German cities share one -- so without the population the
        // order inside a rank would be file order, and which of Stuttgart and
        // Pforzheim got the room would be an accident of the source.
        all.sort_by(|a, b| {
            a.rank
                .cmp(&b.rank)
                .then(b.population.cmp(&a.population))
                .then(a.name.cmp(&b.name))
        });
        Self { all }
    }

    pub fn len(&self) -> usize {
        self.all.len()
    }

    pub fn is_empty(&self) -> bool {
        self.all.is_empty()
    }

    /// The most prominent places inside `extent`, in Mercator.
    ///
    /// A scan, not an index. The table is 7,342 entries and a rejection is two
    /// comparisons, which is less work than a frame of the diagram it sits under;
    /// an index here would be structure for its own sake.
    pub fn within(&self, extent: Rect, limit: usize) -> Vec<&Place> {
        self.all
            .iter()
            .filter(|p| extent.contains(p.at))
            .take(limit)
            .collect()
    }

    /// Every distinct region name inside `extent`, most prominent first.
    ///
    /// What a reader wants in a caption: a network spanning Schleswig-Holstein,
    /// Saxony-Anhalt and Bavaria is described by those three names far better than
    /// by the twenty cities inside them.
    pub fn regions(&self, extent: Rect) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for p in self.all.iter().filter(|p| extent.contains(p.at)) {
            if !p.region.is_empty() && !out.contains(&p.region.as_str()) {
                out.push(&p.region);
            }
        }
        out
    }

    /// The country a two-letter code names, if the gazetteer knows it.
    ///
    /// Transmission data identifies a country by its ISO code and a reader wants a
    /// name. This is the join, and it comes from the gazetteer rather than from a
    /// table written out by hand here -- sixty codes typed from memory is sixty
    /// chances to mislabel somebody's country.
    pub fn country_named(&self, iso: &str) -> Option<&str> {
        if iso.is_empty() {
            return None;
        }
        self.all
            .iter()
            .find(|p| p.iso.eq_ignore_ascii_case(iso) && !p.country.is_empty())
            .map(|p| p.country.as_str())
    }

    /// Every distinct country inside `extent`, most prominent first.
    pub fn countries(&self, extent: Rect) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for p in self.all.iter().filter(|p| extent.contains(p.at)) {
            if !p.country.is_empty() && !out.contains(&p.country.as_str()) {
                out.push(&p.country);
            }
        }
        out
    }
}

/// Where labels have already been put, so the next one can be refused.
///
/// A uniform grid rather than a list of rectangles. The list is what everyone
/// writes first and it compares every candidate against every placement, which is
/// quadratic in the number of labels; the grid asks only the cells a candidate
/// covers and is flat in the count.
struct Taken {
    cell: f32,
    at: std::collections::HashMap<(i32, i32), Vec<Rect>>,
}

impl Taken {
    fn new(cell: f32) -> Self {
        Self {
            cell: cell.max(1.0),
            at: std::collections::HashMap::new(),
        }
    }

    fn cells(&self, r: Rect) -> impl Iterator<Item = (i32, i32)> + '_ {
        let (x0, x1) = ((r.min.x / self.cell) as i32, (r.max.x / self.cell) as i32);
        let (y0, y1) = ((r.min.y / self.cell) as i32, (r.max.y / self.cell) as i32);
        (y0..=y1).flat_map(move |y| (x0..=x1).map(move |x| (x, y)))
    }

    /// Reserve `r` if nothing overlaps it. Answers whether it was free.
    fn claim(&mut self, r: Rect) -> bool {
        let cells: Vec<(i32, i32)> = self.cells(r).collect();
        for c in &cells {
            if let Some(here) = self.at.get(c) {
                if here.iter().any(|o| o.intersects(r)) {
                    return false;
                }
            }
        }
        for c in cells {
            self.at.entry(c).or_default().push(r);
        }
        true
    }
}

/// How the labels are toned, supplied by the caller so the palette stays in the
/// theme.
#[derive(Clone, Copy)]
pub struct Tone {
    pub name: Color32,
    pub mark: Color32,
    /// Painted behind the text so a name crossing a coastline stays readable.
    pub halo: Color32,
}

/// Draw the places inside `extent`, and answer how many got room.
///
/// `extent` is in **Mercator**, because that is the space the gazetteer is in and
/// the space a caller can describe a region of the world in.
///
/// `frame` and `project` are taken separately, and deliberately, rather than as
/// one composed mapping. Every position here is Mercator and the canvas's screen
/// transform expects the layout's normalised space, so a caller handed a single
/// `Fn(Pos2) -> Pos2` will pass the screen transform straight in and put every
/// label thousands of units off screen -- which is exactly what happened, and
/// looked identical to a table that had failed to load. Composing the two here
/// means it cannot be got wrong out there.
/// `reserved` is screen space the network has already claimed. **The map yields to
/// the network, never the other way round**: a city name landing on a substation's
/// name is two labels in one place and a reader cannot tell which is which -- and
/// on a German grid the collision is often literal, because the substation is
/// named after the city. Passed in rather than discovered, because the network is
/// drawn after this and cannot be asked yet.
pub fn draw(
    painter: &Painter,
    places: &Places,
    extent: Rect,
    limit: usize,
    frame: crate::layout::Frame,
    project: impl Fn(Pos2) -> Pos2,
    reserved: &[Rect],
    tone: Tone,
) -> usize {
    // Room for roughly the tallest label, so a grid cell holds a handful.
    let mut taken = Taken::new(24.0);
    for r in reserved {
        taken.claim(*r);
    }
    let font = FontId::monospace(9.5);
    let mut drawn = 0usize;

    for p in places.within(extent, limit) {
        let screen = project(frame.apply(p.at));
        if !painter.clip_rect().contains(screen) {
            continue;
        }

        // Measured before claiming, because the claim has to cover the text.
        let galley = painter.layout_no_wrap(p.name.clone(), font.clone(), tone.name);
        let offset = Vec2::new(p.kind.dot() + 3.0, 0.0);
        let anchor = screen + offset;
        let box_ = Align2::LEFT_CENTER
            .anchor_size(anchor, galley.size())
            .expand(1.5);
        // The mark is part of what must not be overlapped: two dots touching read
        // as one place with two names.
        if !taken.claim(box_.union(Rect::from_center_size(screen, Vec2::splat(6.0)))) {
            continue;
        }

        // A halo rather than a filled plate. A plate punches a hole in the
        // coastline under it and the map stops being continuous; a halo lets the
        // line show through while keeping the glyph edges legible.
        for d in [
            Vec2::new(1.0, 0.0),
            Vec2::new(-1.0, 0.0),
            Vec2::new(0.0, 1.0),
            Vec2::new(0.0, -1.0),
        ] {
            painter.galley(box_.min + Vec2::splat(1.5) + d, galley.clone(), tone.halo);
        }
        painter.galley(box_.min + Vec2::splat(1.5), galley.clone(), tone.name);

        let r = p.kind.dot();
        match p.kind {
            // A capital is a ring, so it is distinguishable from a large city at
            // a glance rather than by comparing two dot sizes.
            Kind::Country | Kind::Region => {
                painter.circle_stroke(screen, r, Stroke::new(1.0, tone.mark));
            }
            _ => {
                painter.circle_filled(screen, r, tone.mark);
            }
        }
        drawn += 1;
    }
    drawn
}

/// Read the table. A short read stops early rather than panicking.
fn decode(blob: &[u8]) -> Vec<Place> {
    let mut at = 0usize;
    let Some(n_labels) = u16b(blob, &mut at) else {
        return Vec::new();
    };
    let mut labels: Vec<String> = Vec::with_capacity(n_labels as usize);
    for _ in 0..n_labels {
        let Some(text) = string(blob, &mut at) else {
            return Vec::new();
        };
        labels.push(text);
    }

    let Some(n) = u32b(blob, &mut at) else {
        return Vec::new();
    };
    let name_of = |i: u16| -> String {
        if i == NO_LABEL {
            String::new()
        } else {
            labels.get(i as usize).cloned().unwrap_or_default()
        }
    };

    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let (Some(x), Some(y), Some(packed), Some(pop), Some(region), Some(country), Some(iso)) = (
            i16b(blob, &mut at),
            i16b(blob, &mut at),
            u8b(blob, &mut at),
            u16b(blob, &mut at),
            u16b(blob, &mut at),
            u16b(blob, &mut at),
            u16b(blob, &mut at),
        ) else {
            break;
        };
        let Some(name) = string(blob, &mut at) else {
            break;
        };
        out.push(Place {
            at: crate::layout::project_one(
                x as f64 / 32767.0 * 180.0,
                y as f64 / 32767.0 * 90.0,
            ),
            name,
            region: name_of(region),
            country: name_of(country),
            iso: name_of(iso),
            kind: Kind::from_tag(packed & 0x0F),
            rank: packed >> 4,
            population: pop as u32 * 1000,
        });
    }
    out
}

fn string(b: &[u8], at: &mut usize) -> Option<String> {
    let len = u8b(b, at)? as usize;
    let raw = b.get(*at..*at + len)?;
    *at += len;
    // Lossy rather than fatal. The generator writes UTF-8 on a character
    // boundary, and if that ever broke, one mangled label beats no map.
    Some(String::from_utf8_lossy(raw).into_owned())
}

fn u8b(b: &[u8], at: &mut usize) -> Option<u8> {
    let v = b.get(*at).copied();
    *at += 1;
    v
}

fn u16b(b: &[u8], at: &mut usize) -> Option<u16> {
    let v = b.get(*at..*at + 2)?;
    *at += 2;
    Some(u16::from_le_bytes(v.try_into().ok()?))
}

fn u32b(b: &[u8], at: &mut usize) -> Option<u32> {
    let v = b.get(*at..*at + 4)?;
    *at += 4;
    Some(u32::from_le_bytes(v.try_into().ok()?))
}

fn i16b(b: &[u8], at: &mut usize) -> Option<i16> {
    let v = b.get(*at..*at + 2)?;
    *at += 2;
    Some(i16::from_le_bytes(v.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::pos2;

    /// Mercator box around a lon/lat window, for asking what is inside it.
    fn window(west: f64, east: f64, south: f64, north: f64) -> Rect {
        Rect::from_two_pos(
            crate::layout::project_one(west, north),
            crate::layout::project_one(east, south),
        )
    }

    #[test]
    fn the_demo_networks_extent_finds_its_own_cities() {
        // **The end-to-end check the wiring needs.** Every piece can be right --
        // the table decodes, the frame inverts, the query is confined -- and the
        // canvas can still label nothing, because the extent handed to it was
        // computed in the wrong space. So this walks the real path: lay the demo
        // network out, take the bus positions back into Mercator through its own
        // frame, and ask what is inside.
        const SAMPLE: &[u8] = include_bytes!("../../../examples/demo-grid.json");
        let loaded = gridwright_worker::load(Some("demo-grid.json"), SAMPLE)
            .expect("the demo network loads");
        let placed = crate::layout::layout(&loaded.network);
        assert_eq!(placed.kind, crate::layout::Origin::Geographic);

        let mut extent: Option<Rect> = None;
        for p in &placed.pos {
            let m = placed.frame.invert(*p);
            extent = Some(match extent {
                Some(r) => r.union(Rect::from_min_max(m, m)),
                None => Rect::from_min_max(m, m),
            });
        }
        let extent = extent.expect("the demo network has buses");
        let extent = extent.expand2(extent.size() * 0.25);

        let places = Places::load();
        let names: Vec<&str> = places
            .within(extent, 60)
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert!(
            !names.is_empty(),
            "the demo network's extent {extent:?} names nothing",
        );
        // The demo runs from the Elbe mouth to the Swabian Jura, so these are the
        // cities a reader would expect to see beside it.
        for want in ["Hamburg", "Stuttgart"] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
    }

    #[test]
    fn the_table_decodes_and_carries_the_whole_source() {
        let p = Places::load();
        assert!(p.len() > 7000, "only {} places", p.len());
    }

    #[test]
    fn a_named_city_lands_where_it_belongs() {
        // Quantisation is about 600 m, and these are hundreds of kilometres
        // apart, so a swapped or shifted field would be unmissable here.
        let p = Places::load();
        for (name, lon, lat) in [
            ("Hamburg", 9.993, 53.551),
            ("Berlin", 13.405, 52.52),
            ("Stuttgart", 9.18, 48.78),
        ] {
            let found = p
                .all
                .iter()
                .find(|q| q.name == name)
                .unwrap_or_else(|| panic!("{name} is not in the table"));
            let want = crate::layout::project_one(lon, lat);
            assert!(
                (found.at - want).length() < 0.01,
                "{name} at {:?} rather than {want:?}",
                found.at,
            );
        }
    }

    #[test]
    fn a_place_carries_the_region_and_country_it_is_in() {
        // The interning is the part that can go wrong silently: an off-by-one in
        // the label table would put every city in its neighbour's state.
        let p = Places::load();
        let hamburg = p.all.iter().find(|q| q.name == "Hamburg").unwrap();
        assert_eq!(hamburg.country, "Germany");
        let stuttgart = p.all.iter().find(|q| q.name == "Stuttgart").unwrap();
        assert_eq!(stuttgart.country, "Germany");
        assert_eq!(stuttgart.region, "Baden-Württemberg");
    }

    #[test]
    fn a_country_code_resolves_to_a_country_name() {
        // The join a per-country filter needs: network files say DE, readers want
        // Germany. Taken from the gazetteer rather than from a hand-written table,
        // so it cannot drift from the names shown on the map.
        let p = Places::load();
        for (code, want) in [
            ("DE", "Germany"),
            ("FR", "France"),
            ("ES", "Spain"),
            ("NO", "Norway"),
        ] {
            assert_eq!(p.country_named(code), Some(want), "{code}");
        }
        assert_eq!(p.country_named(""), None);
        assert_eq!(p.country_named("ZZ"), None);
    }

    #[test]
    fn capitals_are_classified_as_capitals() {
        let p = Places::load();
        assert_eq!(
            p.all.iter().find(|q| q.name == "Berlin").unwrap().kind,
            Kind::Country
        );
    }

    #[test]
    fn a_query_is_confined_to_its_extent() {
        // **The requirement this module exists for.** Detail appears where the
        // loaded network is and nowhere else.
        let p = Places::load();
        let south_west_germany = window(8.0, 10.5, 47.5, 49.5);
        let names: Vec<&str> = p
            .within(south_west_germany, 40)
            .iter()
            .map(|q| q.name.as_str())
            .collect();
        assert!(names.contains(&"Stuttgart"), "got {names:?}");
        for elsewhere in ["Berlin", "Hamburg", "Lisbon", "Tokyo"] {
            assert!(!names.contains(&elsewhere), "{elsewhere} is not in the window");
        }
    }

    #[test]
    fn a_query_answers_the_most_prominent_first() {
        // The cap has to bite on the least useful names, not on whichever ones
        // the source happened to list last.
        let p = Places::load();
        let germany = window(5.5, 15.5, 47.0, 55.5);
        let top = p.within(germany, 3);
        assert!(
            top.iter().any(|q| q.name == "Berlin"),
            "three most prominent German places were {:?}",
            top.iter().map(|q| &q.name).collect::<Vec<_>>(),
        );
        for w in top.windows(2) {
            assert!(w[0].rank <= w[1].rank, "prominence is out of order");
        }
    }

    #[test]
    fn a_limit_is_respected() {
        let p = Places::load();
        let whole_world = window(-180.0, 180.0, -85.0, 85.0);
        assert_eq!(p.within(whole_world, 12).len(), 12);
    }

    #[test]
    fn regions_and_countries_come_from_the_extent() {
        let p = Places::load();
        let north = window(8.5, 11.5, 53.0, 54.5);
        let regions = p.regions(north);
        assert!(!regions.is_empty(), "no regions in northern Germany");
        assert!(p.countries(north).contains(&"Germany"));
        // Distinct, or a caption listing twenty cities' states would repeat
        // Schleswig-Holstein eleven times.
        let mut sorted = regions.clone();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), before, "{regions:?} repeats");
    }

    #[test]
    fn reserved_space_is_claimed_before_any_label() {
        // Overlapping reservations are expected -- two adjacent substations
        // overlap constantly -- so claiming them must not depend on them being
        // disjoint. What matters is that afterwards none of that space is free.
        let mut taken = Taken::new(24.0);
        let a = Rect::from_min_size(pos2(50.0, 50.0), Vec2::new(60.0, 20.0));
        let b = Rect::from_min_size(pos2(80.0, 55.0), Vec2::new(60.0, 20.0));
        taken.claim(a);
        taken.claim(b);
        assert!(!taken.claim(a), "reserved space came back free");
        assert!(
            !taken.claim(Rect::from_min_size(pos2(100.0, 60.0), Vec2::new(10.0, 5.0))),
            "space inside the second reservation came back free",
        );
        assert!(taken.claim(Rect::from_min_size(pos2(400.0, 400.0), Vec2::splat(10.0))));
    }

    #[test]
    fn a_claimed_box_refuses_an_overlapping_one() {
        let mut taken = Taken::new(24.0);
        let a = Rect::from_min_size(pos2(10.0, 10.0), Vec2::new(60.0, 12.0));
        assert!(taken.claim(a));
        assert!(!taken.claim(a), "the same box was claimed twice");
        assert!(
            !taken.claim(Rect::from_min_size(pos2(40.0, 12.0), Vec2::new(60.0, 12.0))),
            "an overlapping box was allowed"
        );
        assert!(
            taken.claim(Rect::from_min_size(pos2(200.0, 200.0), Vec2::new(60.0, 12.0))),
            "a box nowhere near the first was refused"
        );
    }

    #[test]
    fn a_box_spanning_several_cells_is_still_seen() {
        // The bug a grid invites: claiming only the cell of the top-left corner,
        // so a long label collides with nothing past its first 24 points.
        let mut taken = Taken::new(24.0);
        assert!(taken.claim(Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(200.0, 10.0))));
        assert!(
            !taken.claim(Rect::from_min_size(pos2(150.0, 0.0), Vec2::new(40.0, 10.0))),
            "an overlap 150 points along was missed"
        );
    }

    #[test]
    fn a_truncated_table_stops_rather_than_panicking() {
        assert!(decode(&[]).is_empty());
        assert!(decode(&[0, 0]).is_empty());
        assert!(decode(&[0, 0, 9, 9, 9, 9]).is_empty());
        // A label count that overruns the blob.
        assert!(decode(&[9, 0, 3, b'a']).is_empty());
    }

    #[test]
    fn an_absent_region_reads_as_absent_rather_than_as_the_first_label() {
        // Zero is a real label, so the sentinel cannot be zero. If it were, every
        // place with no region would claim to be in whichever one came first.
        let mut blob = Vec::new();
        blob.extend(1u16.to_le_bytes());
        blob.push(7);
        blob.extend(b"Bavaria");
        blob.extend(1u32.to_le_bytes());
        blob.extend(0i16.to_le_bytes());
        blob.extend(0i16.to_le_bytes());
        blob.push(0x32); // rank 3, kind 2 = City
        blob.extend(0u16.to_le_bytes());
        blob.extend(NO_LABEL.to_le_bytes());
        blob.extend(NO_LABEL.to_le_bytes());
        blob.extend(NO_LABEL.to_le_bytes());
        blob.push(2);
        blob.extend(b"Xx");

        let got = decode(&blob);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "Xx");
        assert!(got[0].region.is_empty(), "got {:?}", got[0].region);
        assert!(got[0].country.is_empty());
        assert_eq!(got[0].rank, 3);
        assert_eq!(got[0].kind, Kind::City);
    }
}

//! The shell: a summary panel on the left, the network on the right.

use eframe::egui::{self, Pos2};
use gridwright_net::Network;
use gridwright_worker::{Failure, Loaded, Solved};

use crate::backend::{DefaultSolver, SolveBackend};
use crate::layout::layout;
use crate::view::NetworkView;

pub struct StudioApp {
    loaded: Option<Loaded>,
    /// Bus positions, computed once per load rather than per frame. The
    /// relaxation in [`crate::layout`] is O(n²) and the answer does not depend
    /// on the camera.
    positions: Vec<Pos2>,
    /// Whether those positions are a projection of geography or an invention.
    /// Shown to the reader, because the picture cannot tell them apart.
    origin: crate::layout::Origin,
    /// How the projection was fitted into the unit box, so the basemap can
    /// follow it. Only meaningful when `origin` is geographic.
    frame: crate::layout::Frame,
    view: NetworkView,
    backend: Box<dyn SolveBackend>,
    outcome: Option<Result<Solved, Failure>>,
    /// Peak unserved energy per bus over the horizon, for the view to mark.
    /// Reduced once when a result arrives; see `NetworkView::ui`.
    peak_shed: Vec<f64>,
    /// One price per bus, for the canvas ramp. Empty until a solve returns.
    bus_price: Vec<f64>,
    /// One utilisation per line, as a fraction of its rating. NaN where the
    /// line has no rating to be a fraction of.
    line_load: Vec<f64>,
    /// Signed flow per line at the chosen instant, for drawing direction.
    ///
    /// Separate from `line_load`, which is a magnitude reduced by peak. A peak
    /// magnitude has no direction to report -- the hour a corridor worked
    /// hardest may not be an hour it flowed the way it usually does -- so
    /// direction is taken at the instant on screen and nowhere else.
    line_flow: Vec<f64>,
    /// Which snapshot the canvas is showing, or the whole horizon at once.
    instant: Instant,
    palette: crate::palette::Palette,
    /// A scrub requested by a chart this frame, applied once at the end of it.
    ///
    /// A `Cell` because charts are drawn from `&self` -- they run while the
    /// solve result is borrowed -- and this is the one thing they need to write.
    scrub_to: std::cell::Cell<Option<Instant>>,
    /// Which embedded case is loaded, if the network came from the list.
    ///
    /// Recorded rather than recovered by matching the loaded network's name
    /// against the list. The readers derive that name from the file stem or from
    /// what the file calls itself, so it is not the list's file name and matching
    /// on it would leave the picker showing nothing as selected -- or worse,
    /// marking the wrong row when two cases share a stem.
    from_sample: Option<usize>,
    /// A country to isolate, applied at the end of the frame.
    ///
    /// Deferred like the others: the region list is drawn while `self.loaded` is
    /// borrowed to read the countries out of, and isolating one needs the network
    /// again to know what to hide.
    pending_only_region: Option<String>,
    /// A case chosen from the picker this frame, opened once at the end of it.
    ///
    /// Deferred for the same reason as `scrub_to`, and more sharply: the picker is
    /// drawn while `self.loaded` is borrowed to read the current name from, and
    /// opening a network replaces it.
    pending_sample: Option<usize>,
    /// How long the last solve took, in seconds, as reported by the backend.
    ///
    /// Asked of the backend rather than measured here, because only it knows
    /// whether it solved on a thread or inline inside one frame. See
    /// [`SolveBackend::took`].
    solve_took: Option<f64>,
    /// The last thing that went wrong while opening a file. Kept until the next
    /// load rather than shown for a few frames: a person who dropped the wrong
    /// file may not be looking at the screen when it lands.
    load_error: Option<String>,
}

/// A network to open when there is nothing else to open.
///
/// It used to be IEEE 14-bus, and that was the wrong choice for a *demo* even
/// though it is the right choice for a test case. Opening it showed almost
/// nothing this interface can do: MATPOWER carries no coordinates, so the
/// layout fell back to the spring embedder; its `baseKV` is 1.0 throughout
/// because the case is written in per unit, so voltage colouring stayed off;
/// it has one snapshot, so the timeline hid itself; and it has no congestion,
/// so every bus priced identically and the ramp was flat. Four features, all
/// silently inert, on the one file most people will ever open.
///
/// This is a small north-south German corridor instead: eight substations at
/// real coordinates across 380, 220 and 110 kV, with offshore wind in the north
/// and gas in the south, over a day at hourly resolution. The corridor between
/// them is deliberately tight, so cheap northern wind cannot always reach
/// southern load and the nodal prices actually separate -- which is the output
/// this engine exists to produce and was, until now, demonstrated by a picture
/// of one flat number.


/// Rows below which a freshly opened network is solved without being asked.
///
/// From the measured in-wasm ladder: 432 rows solves in 3.9 ms and 2,592 in
/// 150 ms. Two thousand sits inside "the answer was already there when I
/// looked", which is the only regime where solving unasked is a courtesy
/// rather than a hijacking of the main thread.
const AUTO_SOLVE_ROWS: usize = 2_000;

impl StudioApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Before anything draws. The default egui palette is a fine developer
        // theme and the wrong thing to ship: two greys a shade apart, and a
        // blue accent that would compete with amber for the only channel this
        // interface uses to say something is wrong.
        crate::theme::apply(&cc.egui_ctx);

        Self {
            loaded: None,
            positions: Vec::new(),
            origin: crate::layout::Origin::Invented,
            frame: crate::layout::Frame::identity(),
            view: NetworkView::default(),
            // The context is taken here rather than at solve time because the
            // native backend needs it to wake the UI from another thread, and
            // `CreationContext` is the one place it is handed to us.
            backend: Box::new(new_solver(&cc.egui_ctx)),
            outcome: None,
            peak_shed: Vec::new(),
            bus_price: Vec::new(),
            line_load: Vec::new(),
            line_flow: Vec::new(),
            instant: Instant::Horizon,
            palette: crate::palette::Palette::default(),
            scrub_to: std::cell::Cell::new(None),
            from_sample: None,
            pending_only_region: None,
            pending_sample: None,
            solve_took: None,
            load_error: None,
        }
    }

    /// Read any supported format from bytes.
    ///
    /// Bytes rather than a path, all the way down, because that is the only
    /// form both targets have: a browser hands over a `File`'s contents and
    /// never a filesystem location.
    pub fn open_bytes(&mut self, name: Option<&str>, bytes: &[u8]) {
        match gridwright_worker::load(name, bytes) {
            Ok(loaded) => self.adopt(loaded),
            Err(f) => self.load_error = Some(format!("{}: {}", f.kind, f.message)),
        }
    }

    /// Everything that happens once a network has been read, however it was.
    fn adopt(&mut self, loaded: gridwright_worker::Loaded) {
        // Cleared here rather than in every caller. A dropped file is not one of
        // the embedded cases, and `open_sample` sets this again straight after.
        self.from_sample = None;
        let placed = layout(&loaded.network);
        self.positions = placed.pos;
        self.origin = placed.kind;
        self.frame = placed.frame;
        self.view.reset();
        self.outcome = None;
        self.peak_shed.clear();
        self.bus_price.clear();
        self.line_load.clear();
        self.line_flow.clear();
        self.load_error = None;
        // A new file has a new horizon, and an instant chosen against the old
        // one is a position in a timeline that no longer exists. Reset rather
        // than clamp: silently landing on the last hour of a shorter year looks
        // like the scrubber moved itself.
        self.instant = Instant::Horizon;
        self.solve_took = None;

        // Solve immediately when it is cheap enough to be imperceptible.
        // Asking someone to press a button to find out something that takes ten
        // milliseconds is friction with no purpose, and the answer is what they
        // opened the file for.
        //
        // The threshold is deliberately far below what the backend will accept:
        // this is "so fast the user will not notice", not "as much as we can
        // get away with". Anything larger stays explicit, because a solve you
        // did not ask for and then have to wait for is worse than a button.
        let rows = gridwright_build::Lopf::row_counts(&loaded.network).total();
        if rows <= AUTO_SOLVE_ROWS && self.backend.is_ready() {
            self.backend.submit(&loaded.network);
        }

        self.loaded = Some(loaded);
    }

    /// Take a network that was read elsewhere.
    ///
    /// The native binary reads directories, which `open_bytes` cannot: a PyPSA
    /// network is a directory of CSV files and there is no single blob to hand
    /// over. Everything after the read is the same, so this is the shared tail
    /// of both paths rather than a second way of loading.
    pub fn open_network(&mut self, network: gridwright_net::Network, name: &str) {
        self.adopt(gridwright_worker::Loaded {
            name: name.into(),
            notes: Vec::new(),
            network,
        });
    }

    /// Open the bundled IEEE 14-bus case.
    ///
    /// Public because the browser entry point reaches for it when the page is
    /// asked for `#demo`, and because it is the same thing the empty state's
    /// button does — one code path, so the two cannot drift.
    /// Open one of the embedded cases.
    ///
    /// Out of range is ignored rather than clamped. The index comes from a list
    /// the caller was handed, so a bad one is a bug in the caller, and silently
    /// opening a different network than the reader asked for would hide it.
    pub fn open_sample(&mut self, which: usize) {
        if let Some(s) = crate::samples::ALL.get(which) {
            self.open_bytes(Some(s.name), s.bytes);
            // After, because `open_bytes` clears this: a network arriving by any
            // other route is not one of these, and the picker must not claim it is.
            if self.loaded.is_some() {
                self.from_sample = Some(which);
            }
        }
    }

    fn network(&self) -> Option<&Network> {
        self.loaded.as_ref().map(|l| &l.network)
    }

    /// Drag and drop is the one loader that works identically on both targets:
    /// natively egui reports a path, in a browser it reports the bytes, and the
    /// only difference here is which of the two is present.
    fn take_dropped(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        let Some(file) = dropped.into_iter().next_back() else {
            return;
        };

        let name = if file.name.is_empty() {
            file.path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
        } else {
            Some(file.name.clone())
        };

        if let Some(bytes) = file.bytes.clone() {
            self.open_bytes(name.as_deref(), &bytes);
            return;
        }

        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = &file.path {
            match std::fs::read(path) {
                Ok(bytes) => self.open_bytes(name.as_deref(), &bytes),
                Err(e) => self.load_error = Some(format!("read: {e}")),
            }
            return;
        }

        self.load_error = Some("read: the dropped item carried neither bytes nor a path".into());
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        use crate::theme;

        // The wordmark, set as two weights of one word rather than as a logo.
        // "grid" is the subject and "wright" is the claim — a wright builds
        // things — so the weight break falls where the meaning does.
        ui.add_space(theme::UNIT);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("grid")
                    .size(15.0)
                    .color(theme::INK_STRONG)
                    .strong(),
            );
            ui.add_space(-6.0);
            ui.label(egui::RichText::new("wright").size(15.0).color(theme::INK));
        });
        ui.add_space(theme::UNIT * 2.0);

        match &self.loaded {
            None => {
                // An empty state is an instruction, not an apology. Name the
                // formats, because "a network file" is not a thing anyone has
                // on disk — `case14_ieee.m` is.
                ui.label(theme::eyebrow("no network"));
                ui.add_space(theme::UNIT);
                ui.label("Drop a file onto the canvas,");
                ui.add_space(theme::UNIT);
                // An empty state that can only wait is a dead end. The cases are
                // embedded rather than fetched so they work offline, from a
                // file:// URL, and on the first paint — and because a browser
                // tab has no working directory to open them from.
                //
                // Named, not described. The button used to read "open the IEEE
                // 14-bus case" and opened the demo grid, which is the kind of
                // wrong that survives because nobody re-reads a button.
                let first = &crate::samples::ALL[crate::samples::DEFAULT];
                if ui.button(format!("or open {}", first.name)).clicked() {
                    self.open_sample(crate::samples::DEFAULT);
                }
                ui.add_space(theme::UNIT);
                ui.label(
                    egui::RichText::new(format!(
                        "{} cases are built in — ⌘K to pick one",
                        crate::samples::ALL.len()
                    ))
                    .size(11.0)
                    .color(theme::INK_DIM),
                );
                ui.add_space(theme::UNIT * 2.0);
                ui.label(theme::eyebrow("reads"));
                ui.add_space(theme::UNIT);
                for line in [
                    "MATPOWER  .m",
                    "PSS/E     .raw .rawx",
                    "CGMES     .xml .zip",
                    "UCTE      .uct",
                    "PyPSA     .nc  csv",
                ] {
                    ui.label(
                        egui::RichText::new(line)
                            .monospace()
                            .size(11.0)
                            .color(theme::INK_DIM),
                    );
                }
            }
            Some(loaded) => {
                ui.label(theme::eyebrow("network"));
                ui.add_space(theme::UNIT);
                // A picker rather than a label. The name was the one piece of the
                // panel that looked like it should be clickable and was not, and
                // the only route to a second case was the palette — which a
                // reader has to already know exists.
                //
                // `selected_text` is whatever is loaded, including a dropped file
                // that is not in the list at all. Showing the nearest entry
                // instead would claim the reader had opened something they had not.
                egui::ComboBox::from_id_salt("case")
                    .selected_text(
                        egui::RichText::new(&loaded.name)
                            .size(13.0)
                            .color(theme::INK_STRONG)
                            .strong(),
                    )
                    .width(ui.available_width() - theme::UNIT * 2.0)
                    .show_ui(ui, |ui| {
                        for (i, s) in crate::samples::ALL.iter().enumerate() {
                            let here = self.from_sample == Some(i);
                            // Label over file name, with the size and the note
                            // beneath. A list of file names is unreadable and a
                            // list of sizes does not say why anyone would pick one.
                            let row = ui.selectable_label(
                                here,
                                egui::RichText::new(format!(
                                    "{}\n{} buses · {}{}",
                                    s.label,
                                    s.buses,
                                    s.note,
                                    if s.located { "" } else { " · no positions" },
                                ))
                                .size(11.5),
                            );
                            if row.clicked() && !here {
                                self.pending_sample = Some(i);
                            }
                        }
                    });

                // What the case is a model of, for the cases that came from the
                // list. It answers the question a reader asks straight after
                // opening one — *where is this?* — which the file itself never
                // does: four of them are portions of American Electric Power's
                // system in the early 1960s and not one bus in them carries a
                // coordinate, so the honest answer is words rather than a map.
                if let Some(s) = self.from_sample.and_then(|i| crate::samples::ALL.get(i)) {
                    ui.add_space(theme::UNIT);
                    ui.label(
                        egui::RichText::new(s.abstracts)
                            .size(11.0)
                            .color(theme::INK_DIM),
                    );
                    if !s.located {
                        // Said plainly, because `bus1` looks like a name the file
                        // chose and it is not: MATPOWER carries no bus names at
                        // all, and these are numbers this reader wrote a prefix
                        // onto. A reader who thinks the file named them will trust
                        // the arrangement too.
                        ui.add_space(theme::UNIT);
                        ui.label(
                            egui::RichText::new(NO_GEOGRAPHY)
                            .size(11.0)
                            .color(theme::INK_DIM),
                        );
                    }
                }
            }
        }

        self.regions(ui);

        if let Some(err) = &self.load_error {
            ui.add_space(theme::UNIT);
            ui.label(egui::RichText::new(err).color(theme::TRIP));
        }

        let Some(net) = self.network() else { return };

        // Above the summary, because it is about what the user just clicked and
        // the summary is about the file. A selection that appears below a
        // fixed block of statistics is a selection nobody notices.
        if self.inspector(ui) {
            ui.add_space(theme::UNIT * 3.0);
        }

        ui.add_space(theme::UNIT * 3.0);
        ui.label(theme::eyebrow("composition"));
        ui.add_space(theme::UNIT);
        egui::Grid::new("summary").num_columns(2).show(ui, |ui| {
            count(ui, "Buses", net.buses.len());
            count(ui, "Lines", net.lines.len());
            count(ui, "Links", net.links.len());
            count(ui, "Generators", net.generators.len());
            count(ui, "Loads", net.loads.len());
            count(ui, "Storage", net.storage.len());
            count(ui, "Snapshots", net.n_snapshots());
            // Reported because they change what the model *is*, not just how
            // big it is: an AC line may not span two synchronous areas, and
            // each area carries its own angle reference.
            count(ui, "Synchronous areas", net.synchronous_areas().len());
            if !net.investment_periods.is_empty() {
                count(ui, "Investment periods", net.investment_periods.len());
            }
            if !net.scenarios.is_empty() {
                count(ui, "Scenarios", net.scenarios.len());
            }
            if !net.contingencies.is_empty() {
                count(ui, "Contingencies", net.contingencies.len());
            }
        });

        // Every format carries more than a linear model can hold, and the
        // readers record what they dropped. Showing that is the point of it
        // existing; hiding it decides on the user's behalf what they did not
        // need to know.
        if let Some(loaded) = &self.loaded
            && !loaded.notes.is_empty()
        {
            ui.separator();
            ui.label(theme::eyebrow("reader notes"));
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .show(ui, |ui| {
                    for note in &loaded.notes {
                        ui.label(note);
                    }
                });
        }

        ui.separator();
        self.results_area(ui);
    }

    /// What is attached to the selected bus, and what the solve said about it.
    ///
    /// Returns whether anything was drawn, so the caller can space around it
    /// without leaving a hole when nothing is selected.
    /// Countries in the loaded network, each switchable.
    ///
    /// Only when there is more than one, which is the honest test: a filter with a
    /// single entry is a control that does nothing, and every IEEE case has exactly
    /// one country because MATPOWER has no column for it.
    ///
    /// The list is what makes a continental model usable. 7,893 substations across
    /// sixty countries is a question about scale, and asking anything about Portugal
    /// inside it means being able to put the other fifty-nine away.
    fn regions(&mut self, ui: &mut egui::Ui) {
        let Some(net) = self.network() else { return };
        let counts = crate::NetworkView::regions(net);
        if counts.len() < 2 {
            return;
        }
        // Names resolved before the list is drawn. The gazetteer lives on the view
        // and switching a country off mutates the view, so reading one while
        // holding the other is a borrow this cannot have.
        //
        // Each code is looked up **against its own buses' extent**, widened a
        // little, so a code that does not mean what ISO says shows the code rather
        // than a country on the other side of the world. The extract writes PA for
        // the Palestinian territories and NI for Northern Ireland, and a blind
        // lookup called them Panama and Nicaragua.
        let extents = crate::NetworkView::region_extents(net, &self.positions, self.frame);
        let regions: Vec<(String, usize, Option<String>)> = counts
            .into_iter()
            .map(|(code, buses)| {
                let name = extents
                    .iter()
                    .find(|(c, _, _)| *c == code)
                    .and_then(|(_, box_, _)| {
                        let pad = (box_.size() * 0.25).max(egui::Vec2::splat(0.02));
                        self.view
                            .places()
                            .country_named_within(&code, box_.expand2(pad))
                    })
                    .map(str::to_string);
                (code, buses, name)
            })
            .collect();

        ui.add_space(crate::theme::UNIT * 2.0);
        ui.horizontal(|ui| {
            ui.label(crate::theme::eyebrow("regions"));
            // Only offered when it would do something. A live "show all" beside a
            // list where everything is already shown is a button that reads as
            // broken the first time somebody presses it.
            if self.view.any_region_hidden() {
                ui.add_space(crate::theme::UNIT);
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("all").size(10.0).color(crate::theme::INK),
                    ))
                    .on_hover_text("show every country again")
                    .clicked()
                {
                    self.view.show_all_regions();
                }
            }
        });
        ui.add_space(crate::theme::UNIT);

        // Scrolled, and capped in height. Sixty countries down the panel would push
        // the composition, the results and the dispatch stack off the bottom of the
        // window -- the filter is not the most important thing here.
        egui::ScrollArea::vertical()
            .max_height(190.0)
            .id_salt("regions")
            .show(ui, |ui| {
                for (code, buses, named) in &regions {
                    // The country's name where the gazetteer knows the code, and the
                    // code itself where it does not. Never a guess: an unrecognised
                    // code shown as some nearby country's name would be a label
                    // that is confidently wrong.
                    let label = match named.as_deref() {
                        Some(name) => format!("{name}  ·  {buses}"),
                        None => format!("{code}  ·  {buses}"),
                    };
                    let was = !self.view.region_hidden(code);
                    let mut shown = was;
                    let text = egui::RichText::new(label).size(11.0).color(if was {
                        crate::theme::INK
                    } else {
                        crate::theme::INK_DIM
                    });
                    let row = ui.add(egui::Checkbox::new(&mut shown, text));
                    if row.changed() {
                        self.view.set_region_hidden(code, !shown);
                    }
                    // Alt-click to isolate. The common thing a reader wants is one
                    // country, and reaching it by switching off fifty-nine others
                    // is not a route anybody would take.
                    if row.clicked() && ui.input(|i| i.modifiers.alt) {
                        self.pending_only_region = Some(code.to_string());
                    }
                    if named.is_none() {
                        row.on_hover_text(format!(
                            "{code}: the file's own code — the gazetteer has no \
                             country of that code anywhere near these buses"
                        ));
                    } else {
                        row.on_hover_text("alt-click to show only this one");
                    }
                }
            });
    }

    fn inspector(&self, ui: &mut egui::Ui) -> bool {
        use crate::theme;

        let Some(net) = self.network() else {
            return false;
        };
        if let Some(e) = self.view.selected_line() {
            return self.corridor(ui, net, e);
        }
        let Some(b) = self.view.selected().filter(|&b| b < net.buses.len()) else {
            return false;
        };
        let bus = &net.buses[b];

        ui.add_space(theme::UNIT * 3.0);
        let solved = self.outcome.as_ref().and_then(|o| o.as_ref().ok());

        ui.label(theme::eyebrow("selected bus"));
        ui.add_space(theme::UNIT);
        ui.label(
            egui::RichText::new(&bus.name)
                .size(13.0)
                .color(theme::INK_STRONG)
                .strong(),
        );

        // The price first, because it is the answer this engine exists to
        // produce and the reason to click a bus at all. A dual on a nodal
        // balance row *is* the marginal cost of energy there.
        if let Some(Ok(solved)) = &self.outcome
            && let Some(series) = solved.prices.get(b)
            && let Some(first) = series.first()
        {
            let (lo, hi) = series
                .iter()
                .fold((f64::MAX, f64::MIN), |(l, h), &v| (l.min(v), h.max(v)));
            ui.add_space(theme::UNIT);
            ui.horizontal(|ui| {
                ui.label(theme::number(format!("{first:.2}")));
                ui.label(
                    egui::RichText::new("/MWh")
                        .size(11.0)
                        .color(theme::INK_DIM),
                );
            });
            // A range only when there is one. On a single snapshot the low and
            // high are the same number and printing both is noise.
            if hi - lo > 1e-9 {
                ui.label(
                    egui::RichText::new(format!("{lo:.2} to {hi:.2} over the horizon"))
                        .size(11.0)
                        .color(theme::INK_DIM),
                );
            }
        }

        // The price over the whole horizon, under the number for right now.
        //
        // A single price is a fact; the series is the *behaviour*, and on a
        // congested network they are different stories -- a bus that averages
        // 80 per MWh because it sits at 78 all day is a different bus from one
        // that sits at 2 all night and 210 for three evening hours, and the
        // panel could not tell them apart.
        if let Some(series) = solved.and_then(|s| s.prices.get(b)).filter(|s| s.len() > 1) {
            ui.add_space(theme::UNIT);
            let rect = self.chart_rect(ui, 44.0, series.len());
            let ax = crate::chart::Axes::fit(rect, series);
            let p = ui.painter();
            crate::chart::frame(p, &ax);
            crate::chart::line(p, &ax, series, theme::INK_STRONG);
            // Where the scrubber is, so the number above and the shape below
            // are visibly the same instant rather than two unrelated readings.
            if let Instant::At(t) = self.instant {
                crate::chart::marker(p, &ax, t, series.len());
            }
            crate::chart::bounds(p, &ax, " /MWh");

            // The same numbers sorted downward: how many hours the bus spent
            // above each price. This is the standard chart of the field, and it
            // answers a question the time series cannot -- "how often was it
            // expensive" rather than "when". A flat-topped curve with a cliff
            // is a bus with two regimes; a smooth slope is one that is
            // continuously marginal.
            //
            // Drawn beside rather than instead. Small multiples of the same
            // data under different transforms outperform one interactive plot
            // for analysis, which is the most replicated finding in this
            // literature.
            let sorted = crate::chart::duration(series);
            if sorted.len() > 1 {
                ui.add_space(theme::UNIT);
                let (rect, _) = ui.allocate_exact_size(
                    egui::Vec2::new(ui.available_width(), 34.0),
                    egui::Sense::hover(),
                );
                // Sharing the price chart's axis on purpose: the two are the
                // same quantity, and letting the duration curve refit its own
                // bounds would draw the identical range at a different height
                // and invite a comparison that means nothing.
                let dax = crate::chart::Axes::like(&ax, rect);
                let p = ui.painter();
                crate::chart::frame(p, &dax);
                crate::chart::line(p, &dax, &sorted, theme::INK);
                p.text(
                    dax.rect.left_bottom() + egui::vec2(2.0, -1.0),
                    egui::Align2::LEFT_BOTTOM,
                    "hours above",
                    egui::FontId::proportional(9.0),
                    theme::INK_DIM,
                );
            }
        }

        // Attachments, named rather than counted. "3 generators" tells you
        // nothing you cannot see on the canvas; their names and sizes do.
        //
        // Once solved, a machine shows what it ran at against what it could
        // have run at. That ratio is the question an operator has about a
        // generator -- a plant sitting at 6 of 60 MW is being told by the
        // market it is not worth running, and the nameplate alone never says so.
        let mut rows = 0;
        egui::Grid::new("inspector").num_columns(2).show(ui, |ui| {
            for (g_i, g) in net.generators.iter().enumerate().filter(|(_, g)| g.bus == b) {
                let peak = solved
                    .and_then(|s| s.dispatch.get(g_i))
                    .map(|series| series.iter().fold(0.0_f64, |m, v| m.max(*v)));
                let size = match peak {
                    Some(p) => format!("{p:.0} / {:.0} MW", g.p_nom),
                    None => format!("{:.0} MW", g.p_nom),
                };
                attached(ui, &g.name, size);
                rows += 1;
            }
            for l in net.loads.iter().filter(|l| l.bus == b) {
                attached(ui, &l.name, format!("{:.0} MW", l.p_set));
                rows += 1;
            }
            for st in net.storage.iter().filter(|s| s.bus == b) {
                // Power *and* energy. A battery described only by its megawatts
                // is half described -- 180 MW for six hours and 180 MW for
                // fifteen minutes are different assets and the same number.
                attached(
                    ui,
                    &st.name,
                    format!("{:.0} MW · {:.0} MWh", st.p_nom, st.p_nom * st.max_hours),
                );
                rows += 1;
            }
        });

        // The balance at this bus, which is the quantity the whole model is
        // built around: everything injected here equals everything withdrawn,
        // and the dual of that equality is the price shown at the top of this
        // block. Naming the two sides makes the price legible as a consequence
        // rather than as a number that arrived from somewhere.
        //
        // Only once solved. Before that, dispatch is unknown and the honest
        // sum would be "up to `p_nom`, against `p_set`", which is two different
        // kinds of number and not a balance.
        if let Some(solved) = solved {
            let at = self.instant;
            let made: f64 = net
                .generators
                .iter()
                .enumerate()
                .filter(|(_, g)| g.bus == b)
                .filter_map(|(i, _)| solved.dispatch.get(i))
                .map(|series| at.mean(series))
                .sum();
            let taken: f64 = net.loads.iter().filter(|l| l.bus == b).map(|l| l.p_set).sum();

            if made != 0.0 || taken != 0.0 {
                ui.add_space(theme::UNIT);
                ui.separator();
                egui::Grid::new("balance").num_columns(2).show(ui, |ui| {
                    // `+ 0.0` normalises negative zero. A bus with no load
                    // formatted as "-0 MW" reads as a tiny negative quantity
                    // rather than as nothing, which is exactly the confusion a
                    // signed balance must not create.
                    reading(ui, "Generated", format!("{:.0} MW", made + 0.0));
                    reading(ui, "Consumed", format!("{:.0} MW", taken + 0.0));
                    // The remainder is what the network moved. Positive means
                    // this bus exported and negative means it imported, which
                    // is the sign convention a corridor label uses too:
                    // positive is flow away from the end you are standing on.
                    reading(ui, "Net to network", format!("{:+.0} MW", made - taken));
                });
            }
        }

        // Storage gets its own chart, because state of charge is the quantity
        // that makes one legible: charge and discharge are two series that only
        // mean something together, and their integral is what an operator
        // actually reasons about.
        if let Some(solved) = solved {
            for (i, st) in net.storage.iter().enumerate().filter(|(_, s)| s.bus == b) {
                let Some(soc) = solved.soc.get(i).filter(|s| s.len() > 1) else {
                    continue;
                };
                ui.add_space(theme::UNIT);
                let rect = self.chart_rect(ui, 38.0, soc.len());
                // From zero to the unit's *capacity*, not to whatever it
                // happened to reach. A state of charge that peaks at 40% should
                // look four tenths full; fitted to its own maximum it would
                // look full, which is the opposite of the fact.
                let full = st.p_nom * st.max_hours;
                let ax = crate::chart::Axes::from_zero(rect, &[full.max(1.0)]);
                let p = ui.painter();
                crate::chart::frame(p, &ax);
                crate::chart::line(p, &ax, soc, theme::INK_STRONG);
                if let Instant::At(t) = self.instant {
                    crate::chart::marker(p, &ax, t, soc.len());
                }
                crate::chart::bounds(p, &ax, " MWh");
            }
        }

        // Unserved energy last, and only when there is some. It belongs under
        // the loads it happened to, and a line reading "0 MW shed" on every
        // healthy bus would train the reader to stop seeing it.
        if let Some(shed) = self.peak_shed.get(b).copied().filter(|&v| v > 0.0) {
            ui.label(
                egui::RichText::new(format!("{shed:.1} MW unserved at peak"))
                    .size(11.0)
                    .color(theme::TRIP),
            );
        }
        if rows == 0 {
            ui.label(
                egui::RichText::new("nothing attached")
                    .size(11.0)
                    .color(theme::INK_DIM),
            );
        }

        true
    }

    /// What a corridor is, and what it carried.
    ///
    /// The counterpart to the bus block. A corridor's interesting quantity is
    /// signed -- flow has a direction, and the hour it reverses is usually the
    /// hour something changed -- which is why the chart here always shows its
    /// zero and the bus chart does not.
    fn corridor(&self, ui: &mut egui::Ui, net: &gridwright_net::Network, e: usize) -> bool {
        use crate::theme;

        let Some(line) = net.lines.get(e) else {
            return false;
        };
        let ends = |b: usize| {
            net.buses
                .get(b)
                .map(|x| x.name.as_str())
                .unwrap_or("?")
                .to_string()
        };

        ui.add_space(theme::UNIT * 3.0);
        ui.label(theme::eyebrow("selected corridor"));
        ui.add_space(theme::UNIT);
        ui.label(
            egui::RichText::new(&line.name)
                .size(13.0)
                .color(theme::INK_STRONG)
                .strong(),
        );
        ui.label(
            egui::RichText::new(format!("{} — {}", ends(line.bus0), ends(line.bus1)))
                .size(11.0)
                .color(theme::INK_DIM),
        );

        let solved = self.outcome.as_ref().and_then(|o| o.as_ref().ok());
        if let Some(series) = solved.and_then(|s| s.flows.get(e)).filter(|s| s.len() > 1) {
            ui.add_space(theme::UNIT);
            let rect = self.chart_rect(ui, 44.0, series.len());
            let ax = crate::chart::Axes::fit(rect, series);
            let p = ui.painter();
            crate::chart::frame(p, &ax);
            // The rating drawn as a line across the chart, so "at its limit" is
            // something you see rather than something you compute. Both signs,
            // because a corridor is equally constrained running backwards.
            crate::chart::threshold(p, &ax, line.s_nom, theme::ALARM);
            crate::chart::threshold(p, &ax, -line.s_nom, theme::ALARM);
            crate::chart::line(p, &ax, series, theme::INK_STRONG);
            if let Instant::At(t) = self.instant {
                crate::chart::marker(p, &ax, t, series.len());
            }
            crate::chart::bounds(p, &ax, " MW");
        }

        egui::Grid::new("corridor").num_columns(2).show(ui, |ui| {
            reading(ui, "Rating", format!("{:.0} MW", line.s_nom));
            if let Some(used) = self.line_load.get(e).copied().filter(|v| v.is_finite()) {
                reading(ui, "Peak loading", format!("{:.0}%", used * 100.0));
            }
            reading(
                ui,
                "Voltage",
                match net.buses.get(line.bus0).map(|b| b.v_nom).unwrap_or(0.0) {
                    kv if kv >= 1.0 => format!("{kv:.0} kV"),
                    _ => "—".into(),
                },
            );
        });

        true
    }

    fn results_area(&mut self, ui: &mut egui::Ui) {
        ui.label(crate::theme::eyebrow("results"));
        ui.add_space(crate::theme::UNIT);

        let ready = self.backend.is_ready();
        let busy = self.backend.is_busy();
        let can_solve = ready && !busy && self.loaded.is_some();

        if ui
            .add_enabled(can_solve, egui::Button::new("Solve"))
            .clicked()
            && let Some(net) = self.loaded.as_ref().map(|l| &l.network)
        {
            self.backend.submit(net);
            self.outcome = None;
            self.peak_shed.clear();
        }

        if !ready {
            ui.label("Solving is not wired up on this target yet.");
        } else if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Solving…");
            });
        }

        match &self.outcome {
            None => {}
            Some(Err(f)) => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("{}: {}", f.kind, f.message),
                );
            }
            Some(Ok(solved)) => {
                // Before the numbers, not after. A caveat printed underneath a
                // table has already lost -- the reader took the number and
                // stopped.
                let trust = Trust::of(&solved.status);
                if let Some(caveat) = trust.caveat() {
                    ui.add_space(crate::theme::UNIT);
                    egui::Frame::new()
                        .fill(crate::theme::SLATE_FIELD)
                        .stroke(egui::Stroke::new(1.0, trust.color()))
                        .inner_margin(egui::Margin::same((crate::theme::UNIT * 1.5) as i8))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(caveat).size(11.0).color(trust.color()),
                            );
                        });
                    ui.add_space(crate::theme::UNIT * 1.5);
                }

                egui::Grid::new("results").num_columns(2).show(ui, |ui| {
                    reading(ui, "Status", solved.status.clone());
                    // Absent unless optimal, and shown as absent rather than as
                    // zero: a cost read off a non-optimal answer is a wrong
                    // number that looks like a right one.
                    reading(
                        ui,
                        "Objective",
                        match solved.objective {
                            Some(v) => format!("{v:.2}"),
                            None => "—".into(),
                        },
                    );
                    reading(ui, "Total shed", format!("{:.3}", solved.total_shed));
                    if let Some(t) = self.solve_took {
                        // Three scales, because this number spans four orders
                        // of magnitude. The 14-bus case solves in a fraction of
                        // a millisecond and a thousand-bus one takes seconds,
                        // and a single format makes one of those two read as
                        // zero -- which is a measurement reported as an absence.
                        let ms = t * 1000.0;
                        reading(
                            ui,
                            "Took",
                            match ms {
                                // Browsers coarsen `performance.now()` as a
                                // side-channel defence -- Chrome to about a
                                // tenth of a millisecond without cross-origin
                                // isolation -- and the 14-bus case solves
                                // faster than that. Saying so beats printing a
                                // zero, which claims a measurement of nothing
                                // rather than nothing measurable.
                                _ if ms < 0.05 => "< 0.1 ms".to_string(),
                                _ if ms < 10.0 => format!("{ms:.1} ms"),
                                _ if ms < 1000.0 => format!("{ms:.0} ms"),
                                _ => format!("{t:.1} s"),
                            },
                        );
                    }
                    if !solved.built.is_empty() {
                        reading(ui, "Capacity built", thousands(solved.built.len()));
                    }
                });

                self.dispatch_stack(ui, solved);

                if solved.total_shed > 0.0 {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        "Buses with unserved energy are ringed on the canvas.",
                    );
                }
            }
        }
    }

    /// One colour per band of the dispatch stack, in the stack's own order.
    ///
    /// Carrier where the file named one, and the lightness ramp where it did
    /// not -- a hue nobody assigned is a hue that means nothing, and inventing
    /// one for "unknown" would make two unrelated fuels look related.
    ///
    /// Same-carrier units are separated by lightness within their hue, so two
    /// gas sets at one station are visibly two bands and still visibly gas.
    fn band_colors(&self) -> Vec<egui::Color32> {
        let Some(net) = self.network() else {
            return Vec::new();
        };
        let mut order: Vec<usize> = (0..net.generators.len()).collect();
        order.sort_by(|&a, &b| {
            net.generators[a]
                .marginal_cost
                .total_cmp(&net.generators[b].marginal_cost)
        });

        let carriers: Vec<&str> = order
            .iter()
            .map(|&g| net.generators[g].carrier.as_str())
            .chain(net.storage.iter().map(|_| "storage"))
            .collect();

        let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        let n = carriers.len().max(1);
        carriers
            .iter()
            .enumerate()
            .map(|(i, c)| match crate::theme::carrier_color(c).map(|(_, col)| col) {
                Some(base) => {
                    // Each repeat of a carrier steps a little lighter, so a
                    // second gas set is distinguishable from the first without
                    // leaving the hue that says both are gas.
                    let k = seen.entry(c).or_insert(0);
                    let shade = 1.0 - (*k as f32 * 0.18).min(0.45);
                    *k += 1;
                    base.gamma_multiply(shade)
                }
                None => crate::view::ramp(if n > 1 { i as f32 / (n - 1) as f32 } else { 1.0 }),
            })
            .collect()
    }

    /// Allocate a chart's rectangle, and let dragging inside it scrub time.
    /// Allocate a chart's rectangle, and let dragging inside it scrub time.
    ///
    /// The charts and the map are one instrument, not a picture beside a
    /// control. Seeing a spike in a price series and having to go find the
    /// same hour on the slider below is the interaction this removes -- you
    /// point at the spike and the network shows you that hour.
    ///
    /// Records the request on `scrub_to` rather than acting on it, and returns
    /// only the rectangle. Charts are drawn while the solve result is borrowed,
    /// so a method that reduced immediately could not be called from where the
    /// charts live. The request is applied once, at the end of the frame.
    fn chart_rect(&self, ui: &mut egui::Ui, height: f32, n: usize) -> egui::Rect {
        let (rect, response) = ui.allocate_exact_size(
            egui::Vec2::new(ui.available_width(), height),
            egui::Sense::click_and_drag(),
        );
        // Drag rather than click alone, so sweeping across the chart plays the
        // day through the network -- which is the closest thing to animation
        // worth having, because the reader is driving it.
        if (response.dragged() || response.clicked())
            && let Some(p) = response.interact_pointer_pos()
        {
            let ax = crate::chart::Axes::fit(rect, &[0.0, 1.0]);
            self.scrub_to.set(Some(Instant::At(ax.sample_at(p.x, n))));
        }
        if response.hovered() {
            ui.ctx()
                .set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        rect
    }

    /// What ran, hour by hour, cheapest at the bottom.
    ///
    /// The chart this domain reaches for first. The bands are ordered by
    /// marginal cost, so the *order* is the merit order and the shape of the
    /// stack is the story of the day: a wide cheap base that thins in the
    /// evening while an expensive band opens above it is a system leaning on
    /// its peakers, and that is legible here without reading a single number.
    fn dispatch_stack(&self, ui: &mut egui::Ui, solved: &Solved) {
        use crate::theme;

        let Some(net) = self.network() else { return };
        if solved.dispatch.is_empty() || net.n_snapshots() < 2 {
            return;
        }

        // Cheapest first, which puts the must-run renewables at the bottom
        // where they belong and the peakers at the top where their appearance
        // is the event worth seeing.
        let mut order: Vec<usize> = (0..net.generators.len().min(solved.dispatch.len())).collect();
        order.sort_by(|&a, &b| {
            net.generators[a]
                .marginal_cost
                .total_cmp(&net.generators[b].marginal_cost)
        });

        let mut series: Vec<&[f64]> = order.iter().map(|&g| solved.dispatch[g].as_slice()).collect();

        // Storage on top, and only its *discharge*. A battery delivering is
        // generation and belongs in the stack; a battery charging is load and
        // does not -- stacking a negative band would make the total stop being
        // the total. Charging is visible on the state-of-charge chart, which is
        // where it reads as what it is.
        let discharged: Vec<Vec<f64>> = solved
            .storage_power
            .iter()
            .map(|s| s.iter().map(|v| v.max(0.0)).collect())
            .collect();
        series.extend(discharged.iter().map(|v| v.as_slice()));

        let totals = crate::chart::stack_peak(&series);
        if totals.iter().all(|v| *v <= 0.0) {
            return;
        }

        ui.add_space(theme::UNIT * 2.0);
        ui.label(theme::eyebrow("dispatch"));
        ui.add_space(theme::UNIT);

        let rect = self.chart_rect(ui, 56.0, totals.len());
        // Fitted to the totals rather than to any one band, and from zero:
        // this is a stack of magnitudes, which is the case where a truncated
        // axis genuinely misleads.
        let ax = crate::chart::Axes::from_zero(rect, &totals);
        let p = ui.painter();
        crate::chart::frame(p, &ax);

        // Cheap bands recede and expensive ones step forward, on the same
        // lightness axis price uses on the busbars. One quantity, one channel,
        // whichever picture it appears in.
        // Coloured by carrier, not by position on a ramp.
        //
        // Nine bands of near-identical grey is not a chart, it is a gradient --
        // and this is the one surface where hue is free to mean something else,
        // because a panel chart shares no space with the canvas where hue means
        // voltage and alarm state. Which fuel is running is exactly the
        // categorical distinction hue is best at.
        let colors = self.band_colors();
        let bands: Vec<(&[f64], egui::Color32)> =
            series.iter().zip(&colors).map(|(s, c)| (*s, *c)).collect();
        crate::chart::stack(p, &ax, &bands);

        if let Instant::At(t) = self.instant {
            crate::chart::marker(p, &ax, t, totals.len());
        }
        crate::chart::bounds(p, &ax, " MW");

        // Named bottom to top in the same order as the bands, so the legend is
        // the merit order written out.
        ui.add_space(theme::UNIT * 0.5);
        let named: Vec<&str> = order
            .iter()
            .map(|&g| net.generators[g].name.as_str())
            .chain(net.storage.iter().map(|s| s.name.as_str()))
            .collect();
        for (i, name) in named.iter().enumerate().take(9) {
            let swatch = colors.get(i).copied().unwrap_or(theme::INK_DIM);
            ui.horizontal(|ui| {
                // A block rather than a rule, and outlined in the same hairline
                // that separates the bands, so a swatch reads as the band it
                // stands for rather than as a dash before a name.
                let (sw, _) = ui.allocate_exact_size(
                    egui::Vec2::new(11.0, 9.0),
                    egui::Sense::hover(),
                );
                let p = ui.painter();
                p.rect_filled(sw, 1.0, swatch);
                p.rect_stroke(
                    sw,
                    1.0,
                    egui::Stroke::new(1.0, theme::SLATE_WORK),
                    egui::StrokeKind::Inside,
                );
                ui.label(
                    egui::RichText::new(*name)
                        .size(10.0)
                        .color(theme::INK_DIM),
                );
            });
        }
    }

    /// The one instrument that is always on screen.
    ///
    /// A control-room annunciator tells you the state of the plant whether or
    /// not you asked, and this is the same idea: a lamp, what is loaded, and —
    /// once something has been solved — the anatomy of that solve.
    ///
    /// The anatomy bar is the part worth having. `phase one` is the simplex
    /// finding *a* feasible point; `phase two` is it finding the *best* one.
    /// Only the second is the question the user asked. On a real 1,354-bus
    /// network phase one is 87% of the iterations, which is a fact about this
    /// solver that no other interface would show you, and it is the argument
    /// for a warm start rendered as a picture rather than as a paragraph.
    fn status_strip(&mut self, ui: &mut egui::Ui) {
        use crate::theme;

        let frame = egui::Frame::new()
            .fill(theme::SLATE_DEEP)
            .stroke(egui::Stroke::new(1.0, theme::SLATE_LINE))
            .inner_margin(egui::Margin::symmetric(
                (theme::UNIT * 2.0) as i8,
                theme::UNIT as i8,
            ));

        egui::Panel::bottom("status")
            .frame(frame)
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (lamp, text) = self.state();
                    // A filled dot, not a coloured word: the lamp is readable
                    // at the edge of vision, which is the whole point of an
                    // annunciator.
                    let (rect, _) =
                        ui.allocate_exact_size(egui::Vec2::splat(8.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 3.5, lamp);
                    ui.label(egui::RichText::new(text).size(11.0).color(theme::INK));

                    if self.loaded.is_some() {
                        separator(ui);
                        // Whether the picture is a map. Nothing in the diagram
                        // distinguishes a projection from a relaxation -- both
                        // are dots joined by lines, and both look equally
                        // authoritative -- so a reader who assumes the wrong one
                        // will draw conclusions about distance and geography
                        // that the picture does not support.
                        ui.label(
                            egui::RichText::new(self.origin.label())
                                .size(11.0)
                                .color(match self.origin {
                                    crate::layout::Origin::Geographic => theme::INK,
                                    _ => theme::INK_DIM,
                                }),
                        );
                    }

                    if let Some(net) = self.network() {
                        separator(ui);
                        ui.label(
                            egui::RichText::new(format!(
                                "{} buses · {} lines · {} snapshots",
                                thousands(net.buses.len()),
                                thousands(net.lines.len()),
                                thousands(net.n_snapshots()),
                            ))
                            .monospace()
                            .size(11.0)
                            .color(theme::INK_DIM),
                        );
                    }

                    if let Some(Ok(solved)) = &self.outcome {
                        self.anatomy(ui, solved);
                    }
                });
            });
    }

    /// The timeline, when there is one.
    ///
    /// Absent on a single-snapshot network, which most test cases are. A
    /// scrubber over one position is a control that cannot be moved, and a
    /// disabled control teaches a reader that the feature is broken rather than
    /// that their file has no time axis in it.
    fn timeline(&mut self, ui: &mut egui::Ui) {
        use crate::theme;

        let Some(n) = self.network().map(|n| n.n_snapshots()).filter(|&n| n > 1) else {
            return;
        };

        let frame = egui::Frame::new()
            .fill(theme::SLATE_DEEP)
            .stroke(egui::Stroke::new(1.0, theme::SLATE_LINE))
            .inner_margin(egui::Margin::symmetric(
                (theme::UNIT * 2.0) as i8,
                theme::UNIT as i8,
            ));

        let mut changed = false;
        egui::Panel::bottom("timeline")
            .frame(frame)
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // The horizon is a mode, not position zero on the track.
                    // "Everything at once" is a different question from "this
                    // hour", and putting it at one end of a slider would make
                    // stepping off it look like scrubbing.
                    let whole = matches!(self.instant, Instant::Horizon);
                    if ui.selectable_label(whole, "horizon").clicked() {
                        self.instant = Instant::Horizon;
                        changed = true;
                    }

                    separator(ui);

                    let mut t = match self.instant {
                        Instant::At(t) => t,
                        Instant::Horizon => 0,
                    };
                    let slider = egui::Slider::new(&mut t, 0..=(n - 1))
                        .show_value(false)
                        .handle_shape(egui::style::HandleShape::Rect { aspect_ratio: 0.4 });
                    let r = ui.add(slider);
                    self.timeline_track(ui, r.rect, n);
                    if r.changed() {
                        self.instant = Instant::At(t);
                        changed = true;
                    }

                    ui.label(
                        egui::RichText::new(self.instant.label(n))
                            .monospace()
                            .size(11.0)
                            .color(if whole { theme::INK_DIM } else { theme::INK }),
                    );

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.label(
                                egui::RichText::new("← → step · shift for a day")
                                    .size(10.0)
                                    .color(theme::INK_DIM),
                            );
                        },
                    );
                });
            });

        if self.step_keys(ui.ctx(), n) || changed {
            self.reduce();
        }
    }

    /// Jump to the hour that best answers a question.
    ///
    /// A year has 8,760 hours and almost all of them are unremarkable. Finding
    /// the three that are not, by dragging, is the kind of search a tool should
    /// do rather than ask for -- and it is exactly the "dynamically extract it"
    /// argument that justified the palette in the first place.
    fn go_to(&mut self, what: Interesting) {
        let Some(Ok(solved)) = &self.outcome else {
            return;
        };
        let n = self.network().map(|n| n.n_snapshots()).unwrap_or(0);
        if n < 2 {
            return;
        }

        let score = |t: usize| -> f64 {
            match what {
                // The spread across buses in one hour. Congestion *is* price
                // divergence -- if every bus prices the same, nothing was
                // binding -- so the widest hour is the most congested one.
                Interesting::Spread => {
                    let (lo, hi) = solved
                        .prices
                        .iter()
                        .filter_map(|s| s.get(t))
                        .filter(|v| v.is_finite())
                        .fold((f64::MAX, f64::MIN), |(l, h), &v| (l.min(v), h.max(v)));
                    if lo > hi { 0.0 } else { hi - lo }
                }
                // Unserved energy first, and only the highest price if there
                // was none anywhere. A system that failed to serve its load has
                // a worst hour that is not up for debate; one that never failed
                // has a worst hour defined by what it had to pay.
                Interesting::Stress => {
                    let shed: f64 = solved.shed.iter().filter_map(|s| s.get(t)).sum();
                    if shed > 0.0 {
                        1e9 + shed
                    } else {
                        solved
                            .prices
                            .iter()
                            .filter_map(|s| s.get(t))
                            .filter(|v| v.is_finite())
                            .fold(0.0_f64, |m, &v| m.max(v))
                    }
                }
            }
        };

        // Ties go to the earliest hour, which is what `max_by` on a forward
        // scan gives -- a flat day should land on hour one rather than on
        // whichever hour the comparison happened to prefer.
        if let Some(t) = (0..n).max_by(|&a, &b| score(a).total_cmp(&score(b))) {
            self.instant = Instant::At(t);
            self.reduce();
        }
    }

    /// The shape of the day, drawn behind the slider.
    /// The shape of the day, drawn behind the slider.
    ///
    /// An empty track tells a reader nothing about where to drag. This one
    /// carries the system price envelope, so the expensive hours are visible
    /// before you scrub to them -- the evening peak shows as a bulge and you
    /// aim at it rather than hunting.
    ///
    /// Highest and lowest across all buses rather than a mean, because the
    /// *spread* is the interesting quantity: an hour where every bus prices
    /// the same is an uncongested hour, and one where they diverge is not, and
    /// a mean hides exactly that.
    fn timeline_track(&self, ui: &egui::Ui, rect: egui::Rect, n: usize) {
        use crate::theme;

        let Some(Ok(solved)) = &self.outcome else {
            return;
        };
        if solved.prices.is_empty() || n < 2 {
            return;
        }

        let mut hi = vec![f64::MIN; n];
        let mut lo = vec![f64::MAX; n];
        for series in &solved.prices {
            for (t, v) in series.iter().enumerate().take(n) {
                if v.is_finite() {
                    hi[t] = hi[t].max(*v);
                    lo[t] = lo[t].min(*v);
                }
            }
        }
        if hi.contains(&f64::MIN) {
            return;
        }

        // Inset vertically so the slider's own handle and rail stay readable on
        // top of it. This is a backdrop, not a chart competing with the control.
        let band = rect.shrink2(egui::vec2(0.0, 3.0));
        let ax = crate::chart::Axes::fit(band, &hi);
        let p = ui.painter();
        crate::chart::band(p, &ax, &lo, &hi, theme::SLATE_RAISED);
        crate::chart::line(p, &ax, &hi, theme::INK_DIM);
    }

    /// Arrow keys walk the horizon one snapshot at a time.
    /// Arrow keys walk the horizon one snapshot at a time.
    ///
    /// Scrubbing a slider finds a region; stepping finds an hour. Both are
    /// needed, and a person comparing two adjacent snapshots cannot do it by
    /// dragging -- the pointer moves several snapshots per pixel on a year.
    ///
    /// Returns whether anything moved.
    fn step_keys(&mut self, ctx: &egui::Context, n: usize) -> bool {
        use egui::Key;

        if ctx.memory(|m| m.focused()).is_some() {
            return false;
        }
        let (back, fwd, first, last, stride) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::Comma),
                i.key_pressed(Key::Period),
                i.modifiers.shift,
            )
        });

        // Two step sizes, because one is not enough on a real horizon.
        //
        // ArcGIS keeps the *step interval* as a term distinct from the position
        // and the addressable range, and it is the one this was missing:
        // walking a year of hourly snapshots one at a time is 8,760 keypresses,
        // and dragging the slider moves several snapshots per pixel. Shift
        // steps by a day where there is one, so the same key answers both "the
        // next hour" and "this hour tomorrow".
        let step = if !stride {
            1
        } else {
            // A day, unless the horizon is too short for that to mean anything,
            // in which case a tenth of it -- which keeps a large stride useful
            // on a representative-week or typical-day model.
            match n {
                0..=48 => (n / 10).max(1),
                _ => 24,
            }
        };

        let t = match self.instant {
            Instant::At(t) => t,
            // Stepping off the horizon lands at the start rather than
            // somewhere in the middle, so the first press is predictable.
            Instant::Horizon if back || fwd || first || last => 0,
            Instant::Horizon => return false,
        };

        let next = match (back, fwd, first, last) {
            (_, _, true, _) => 0,
            (_, _, _, true) => n - 1,
            // Saturating rather than wrapping. Walking off the end of a year
            // and arriving in January is a jump the reader did not ask for.
            (true, false, ..) => t.saturating_sub(step),
            (false, true, ..) => (t + step).min(n - 1),
            _ => return false,
        };

        let moved = self.instant != Instant::At(next);
        self.instant = Instant::At(next);
        moved
    }

    /// Offer the palette, and do whatever it was told.
    fn run_palette(&mut self, ctx: &egui::Context) {
        use crate::palette::Action;

        let (buses, lines) = match self.network() {
            Some(n) => (
                n.buses.iter().map(|b| b.name.clone()).collect(),
                n.lines.iter().map(|l| l.name.clone()).collect(),
            ),
            None => (Vec::new(), Vec::new()),
        };

        let Some(action) = self.palette.ui(
            ctx,
            &crate::palette::Names {
                buses: &buses,
                lines: &lines,
            },
        ) else {
            return;
        };
        match action {
            // Selecting *and* moving the camera, not one or the other. A
            // selection the reader cannot see is a panel describing something
            // off screen, and a camera move with no selection loses the thing
            // they searched for the moment they pan.
            Action::GoTo(b) => self.view.reveal(b),
            Action::GoToLine(e) => self.view.reveal_line(e),
            Action::Solve => {
                if let Some(net) = self.loaded.as_ref().map(|l| &l.network) {
                    self.backend.submit(net);
                    self.outcome = None;
                    self.reduce();
                }
            }
            Action::Fit => self.view.refit(),
            Action::OpenSample(i) => self.open_sample(i),
            Action::Horizon => {
                self.instant = Instant::Horizon;
                self.reduce();
            }
            Action::MostCongested => self.go_to(Interesting::Spread),
            Action::WorstHour => self.go_to(Interesting::Stress),
        }
    }

    /// Lamp colour and one word for the current state.
    fn state(&self) -> (egui::Color32, &'static str) {
        use crate::theme;
        if self.backend.is_busy() {
            return (theme::ALARM, "solving");
        }
        match &self.outcome {
            Some(Err(_)) => (theme::TRIP, "failed"),
            Some(Ok(s)) if s.total_shed > 0.0 => (theme::TRIP, "unserved energy"),
            Some(Ok(_)) => (theme::LIVE, "solved"),
            None if self.loaded.is_some() => (theme::OFF, "not solved"),
            None => (theme::OFF, "no network"),
        }
    }

    /// Where the last solve's iterations went, as a bar rather than a sentence.
    fn anatomy(&self, ui: &mut egui::Ui, solved: &Solved) {
        use crate::theme;
        let (Some(total), Some(p1)) = (solved.iterations, solved.phase_one_iterations) else {
            return;
        };
        if total == 0 {
            return;
        }

        separator(ui);
        let share = (p1 as f32 / total as f32).clamp(0.0, 1.0);
        let (rect, response) =
            ui.allocate_exact_size(egui::Vec2::new(120.0, 8.0), egui::Sense::hover());
        let p = ui.painter();

        // Both halves are painted, because this is a whole divided in two and
        // not a fill level. Drawn as a fill, the unpainted remainder reads as
        // work not yet done -- and the remainder here is phase two, the part
        // that actually answered the question. The wasted half should be the
        // one that looks spent.
        let mut phase_one = rect;
        phase_one.set_width(rect.width() * share);
        let mut phase_two = rect;
        phase_two.set_left(phase_one.right());

        // Amber for phase one because it is, in the sense that matters, wasted
        // work: a warm start would have skipped it.
        p.rect_filled(phase_one, 1.0, theme::ALARM.gamma_multiply(0.75));
        p.rect_filled(phase_two, 1.0, theme::INK_DIM);

        ui.label(
            egui::RichText::new(format!("{:.0}% phase one", share * 100.0))
                .monospace()
                .size(11.0)
                .color(theme::INK_DIM),
        );
        response.on_hover_text(format!(
            "{} of {} simplex iterations were spent finding a feasible point \
             rather than an optimal one. A warm start would reuse the previous \
             basis and skip most of it.",
            thousands(p1),
            thousands(total),
        ));
    }

    /// Reduce the solve's per-bus, per-snapshot series to one number per bus,
    /// which is what the canvas can draw.
    ///
    /// Shed is reduced by peak rather than by total: the view marks *where* the
    /// system failed, and a bus that sheds heavily for one hour is as much a
    /// failure of that bus as one that sheds lightly all year.
    fn absorb(&mut self, outcome: Result<Solved, Failure>) {
        self.outcome = Some(outcome);
        self.reduce();
    }

    /// Collapse the solve's per-snapshot series to the one number per component
    /// that the canvas can draw.
    ///
    /// Re-run whenever the instant changes rather than folded into drawing,
    /// because it is a pass over the whole horizon and the picture it produces
    /// does not change between frames.
    fn reduce(&mut self) {
        let reduced = match (&self.outcome, &self.loaded) {
            (Some(Ok(solved)), Some(loaded)) => {
                reduce(solved, &loaded.network, self.instant)
            }
            _ => Reduced::default(),
        };
        self.peak_shed = reduced.shed;
        self.bus_price = reduced.price;
        self.line_load = reduced.load;
        self.line_flow = reduced.flow;
    }
}

impl eframe::App for StudioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.take_dropped(ui.ctx());

        if let Some(outcome) = self.backend.take_result() {
            self.solve_took = self.backend.took();
            self.absorb(outcome);
        }

        egui::Panel::left("summary")
            .resizable(true)
            .default_size(300.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .animated(false)
                    .show(ui, |ui| self.side_panel(ui));
            });

        // Before the panels, so a keystroke aimed at the palette is not first
        // eaten by a text field behind it.
        self.run_palette(ui.ctx());

        self.status_strip(ui);
        self.timeline(ui);

        // No margins: the canvas should meet the panel edge, and an inset would
        // show as a border around a view the user is panning.
        //
        // The fill is the one deliberate inversion in the layout. The work
        // surface is *lighter* than the furniture around it, so the network is
        // the brightest thing on screen and the panel recedes. A hairline on
        // the shared edge keeps the two from bleeding into one another.
        let canvas = egui::Frame::new()
            .fill(crate::theme::SLATE_WORK)
            .stroke(egui::Stroke::new(1.0, crate::theme::SLATE_LINE));
        egui::CentralPanel::default().frame(canvas).show(ui, |ui| {
            let net = self.loaded.as_ref().map(|l| &l.network);
            self.view.ui(
                ui,
                net,
                &self.positions,
                crate::view::Overlay {
                    peak_shed: &self.peak_shed,
                    prices: &self.bus_price,
                    loading: &self.line_load,
                    flow: &self.line_flow,
                },
                // The basemap draws only under a projection. A coastline under
                // invented coordinates is a map of somewhere that does not
                // exist.
                matches!(self.origin, crate::layout::Origin::Geographic)
                    .then_some(self.frame),
            );
        });

        // Before the scrub, because opening a network resets the instant and
        // applying a scrub into the old horizon first would be a position in a
        // timeline that is about to stop existing.
        if let Some(code) = self.pending_only_region.take()
            && let Some(net) = self.loaded.as_ref().map(|l| &l.network)
        {
            self.view.only_region(net, &code);
        }

        if let Some(i) = self.pending_sample.take() {
            self.open_sample(i);
        }

        // Applied here, after every panel has drawn, because a chart cannot
        // reduce while the result it is drawing from is borrowed.
        if let Some(at) = self.scrub_to.take()
            && self.instant != at
        {
            self.instant = at;
            self.reduce();
        }
    }
}

/// Said plainly under an unlocated case.
///
/// Because `bus1` looks like a name the file chose, and it is not: MATPOWER carries
/// no bus names at all, and these are numbers with a prefix this reader wrote on.
/// A person who believes the file named them will trust the arrangement too.
const NO_GEOGRAPHY: &str = "buses are numbered, not named; the arrangement is the topology relaxed";

/// A label and its count.
///
/// The number is monospace and right-aligned so a column of them shares a
/// decimal position; proportional digits in a summary make the eye re-scan
/// every row to find where the value starts.
fn count(ui: &mut egui::Ui, label: &str, n: usize) {
    ui.label(egui::RichText::new(label).color(crate::theme::INK));
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.label(crate::theme::number(thousands(n)));
    });
    ui.end_row();
}

/// A label and a solved value, in the same shape as the composition counts.
///
/// Split from `count` rather than generalised over it: these are readings off
/// an answer and those are facts about a file, and the two blocks happening to
/// share a layout is not a reason to make one function decide which it is.
fn reading(ui: &mut egui::Ui, label: &str, value: String) {
    ui.label(egui::RichText::new(label).color(crate::theme::INK));
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.label(crate::theme::number(value));
    });
    ui.end_row();
}

/// Group digits, because `13659` and `1354` are hard to tell apart at a glance
/// and `13,659` and `1,354` are not.
fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(not(target_arch = "wasm32"))]
fn new_solver(ctx: &egui::Context) -> DefaultSolver {
    DefaultSolver::new(ctx.clone())
}

#[cfg(target_arch = "wasm32")]
fn new_solver(_ctx: &egui::Context) -> DefaultSolver {
    DefaultSolver::new()
}

/// A dim vertical tick between groups in the status strip.
///
/// A full-height `ui.separator()` is too loud for a 22px strip; this is a
/// middot's worth of separation, which is all the eye needs.
fn separator(ui: &mut egui::Ui) {
    ui.add_space(crate::theme::UNIT * 2.0);
    ui.label(
        eframe::egui::RichText::new("·")
            .size(11.0)
            .color(crate::theme::SLATE_LINE),
    );
    ui.add_space(crate::theme::UNIT);
}

/// One attached component: what it is on the left, how big on the right.
fn attached(ui: &mut egui::Ui, name: &str, size: String) {
    ui.label(
        eframe::egui::RichText::new(name)
            .size(11.0)
            .color(crate::theme::INK),
    );
    ui.with_layout(
        eframe::egui::Layout::right_to_left(eframe::egui::Align::Center),
        |ui| {
            ui.label(
                eframe::egui::RichText::new(size)
                    .monospace()
                    .size(11.0)
                    .color(crate::theme::INK_DIM),
            );
        },
    );
    ui.end_row();
}

/// One number per component, ready to draw.
#[derive(Debug, Default, PartialEq)]
struct Reduced {
    /// Per bus: unserved energy.
    shed: Vec<f64>,
    /// Per bus: nodal price.
    price: Vec<f64>,
    /// Per line: flow as a fraction of rating, NaN where unrated.
    load: Vec<f64>,
    /// Per line: signed flow at the instant, for the direction chevrons.
    flow: Vec<f64>,
}

/// Collapse a solve's per-snapshot series at the chosen instant.
///
/// A free function rather than a method so it can be tested without a window.
/// The choice of reduction per quantity is the whole content of it, and getting
/// one of them wrong produces a picture that is plausible and false.
fn reduce(solved: &Solved, net: &gridwright_net::Network, at: Instant) -> Reduced {
    Reduced {
        // Shed reduces by peak. It is an *event*: one bad hour is the story,
        // and a mean would divide the failure by the length of the year until
        // it disappeared.
        shed: solved.shed.iter().map(|s| at.peak(s)).collect(),

        // Price reduces by mean, because it is a *condition* rather than an
        // event. One congested hour should not repaint a bus that is ordinary
        // for the rest of the year.
        price: solved.prices.iter().map(|s| at.mean(s)).collect(),

        // Corridors reduce by peak, like shed and unlike price. A line that
        // binds for one hour of the year is a constrained line, and averaging
        // that away hides the hour the network was short of transfer capacity.
        //
        // NaN rather than zero where a line has no rating: zero would draw an
        // unrated line as idle, which is a claim about a quantity the model
        // never had.
        load: solved
            .flows
            .iter()
            .zip(&net.lines)
            .map(|(series, line)| {
                let rating = line.s_nom.abs();
                if rating > 0.0 {
                    at.peak_abs(series) / rating
                } else {
                    f64::NAN
                }
            })
            .collect(),

        // Signed, and reduced by *mean* at both settings rather than by peak
        // like the magnitude above. At an instant the mean of one sample is
        // that sample; over the horizon it is the net direction across the day,
        // which is a real and useful quantity. A peak's direction, by contrast,
        // is whichever way the corridor happened to be flowing in its single
        // busiest hour, which is not a fact about the day.
        flow: solved.flows.iter().map(|s| at.mean(s)).collect(),
    }
}

/// What makes an hour worth jumping to.
#[derive(Debug, Clone, Copy)]
enum Interesting {
    /// Prices diverged most across the network: the most congested hour.
    Spread,
    /// The system was under most stress: unserved energy if any, else price.
    Stress,
}

/// Which part of the horizon the canvas is showing.
///
/// A year of hourly snapshots reduced to one number per bus is the only thing
/// the diagram can draw, and until now that reduction was always over the whole
/// horizon. That answers "was this network ever short of capacity" and cannot
/// answer "what did it look like at 18:00 on the coldest day", which is the
/// question a congested hour actually raises.
///
/// The literature is unambiguous that scrubbing is the right primitive here.
/// Amini et al. (2015) found that a good 2D temporal view "relies significantly
/// on scrubbing the timeline", and that much of a 3D space-time view's measured
/// advantage was a proxy for having any temporal interaction at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instant {
    /// Every snapshot at once, reduced.
    Horizon,
    /// One snapshot, shown as it is.
    At(usize),
}

impl Instant {
    /// The largest value, or the value at this instant.
    fn peak(self, series: &[f64]) -> f64 {
        match self {
            Instant::Horizon => series.iter().copied().fold(0.0_f64, f64::max),
            Instant::At(t) => series.get(t).copied().unwrap_or(0.0),
        }
    }

    /// The largest magnitude, for quantities that are signed.
    ///
    /// Flow has a direction, and a corridor running at its rating in the
    /// negative direction is exactly as constrained as one running at its
    /// rating in the positive direction.
    fn peak_abs(self, series: &[f64]) -> f64 {
        match self {
            Instant::Horizon => series.iter().fold(0.0_f64, |m, v| m.max(v.abs())),
            Instant::At(t) => series.get(t).copied().unwrap_or(0.0).abs(),
        }
    }

    /// The mean over the horizon, or the value at this instant.
    pub(crate) fn mean(self, series: &[f64]) -> f64 {
        match self {
            Instant::Horizon => match series.len() {
                0 => 0.0,
                n => series.iter().sum::<f64>() / n as f64,
            },
            Instant::At(t) => series.get(t).copied().unwrap_or(0.0),
        }
    }

    /// A short phrase naming what is on screen.
    fn label(self, of: usize) -> String {
        match self {
            Instant::Horizon => format!("all {of}"),
            // One-based, and with the total, because "snapshot 4000" tells a
            // reader nothing about where in the year they are and "4000 / 8760"
            // tells them roughly mid-May.
            Instant::At(t) => format!("{} / {of}", t + 1),
        }
    }
}

#[cfg(test)]
mod instant_tests {
    use super::Instant;

    const SERIES: [f64; 4] = [10.0, -30.0, 20.0, 0.0];

    #[test]
    fn the_horizon_reduces_across_every_snapshot() {
        assert_eq!(Instant::Horizon.peak(&SERIES), 20.0);
        assert_eq!(Instant::Horizon.peak_abs(&SERIES), 30.0);
        assert_eq!(Instant::Horizon.mean(&SERIES), 0.0);
    }

    #[test]
    fn an_instant_reduces_to_the_value_there() {
        // All three reductions collapse to the same thing at a point, except
        // that magnitude drops the sign. That is the property that makes the
        // scrubber honest: what is drawn at snapshot t is the number at
        // snapshot t, not a window around it.
        assert_eq!(Instant::At(1).peak(&SERIES), -30.0);
        assert_eq!(Instant::At(1).mean(&SERIES), -30.0);
        assert_eq!(Instant::At(1).peak_abs(&SERIES), 30.0);
    }

    #[test]
    fn magnitude_is_taken_before_the_maximum_not_after() {
        // Flow is signed, and a corridor at its rating in the negative
        // direction is exactly as constrained as one at its rating in the
        // positive direction. Folding with `max` before `abs` would report this
        // series as 20% loaded when it reached 30.
        assert_eq!(Instant::Horizon.peak_abs(&SERIES), 30.0);
        assert_ne!(Instant::Horizon.peak(&SERIES), 30.0);
    }

    #[test]
    fn an_instant_past_the_end_reads_as_nothing_rather_than_panicking() {
        // The horizon length comes from the network and the series length from
        // the solve, and a stale result paired with a freshly loaded file is a
        // real sequence rather than a hypothetical one.
        assert_eq!(Instant::At(99).peak(&SERIES), 0.0);
        assert_eq!(Instant::At(99).mean(&SERIES), 0.0);
        assert_eq!(Instant::At(99).peak_abs(&SERIES), 0.0);
    }

    #[test]
    fn an_empty_series_has_a_mean_rather_than_a_division_by_zero() {
        assert_eq!(Instant::Horizon.mean(&[]), 0.0);
    }
}

#[cfg(test)]
mod reduce_tests {
    use super::*;
    use gridwright_net::{Bus, Line, Network, Snapshots};

    /// One line rated 100, two buses, three snapshots.
    fn scene() -> (Network, Solved) {
        let mut net = Network::new(Snapshots::hourly(3));
        for name in ["a", "b"] {
            net.buses.push(Bus {
                name: name.into(),
                ..Default::default()
            });
        }
        net.lines.push(Line {
            name: "a-b".into(),
            bus0: 0,
            bus1: 1,
            s_nom: 100.0,
            ..Default::default()
        });
        let solved = Solved {
            status: "Optimal".into(),
            objective: Some(1.0),
            total_shed: 5.0,
            iterations: None,
            phase_one_iterations: None,
            prices: vec![vec![10.0, 40.0, 10.0], vec![10.0, 10.0, 10.0]],
            dispatch: Vec::new(),
            flows: vec![vec![50.0, -100.0, 0.0]],
            soc: Vec::new(),
            storage_power: Vec::new(),
            shed: vec![vec![0.0, 5.0, 0.0], vec![0.0, 0.0, 0.0]],
            built: Vec::new(),
        };
        (net, solved)
    }

    #[test]
    fn the_horizon_hides_the_hour_the_instant_reveals() {
        // The entire point of the scrubber. Over the whole horizon bus `a`
        // averages 20/MWh and the corridor reads fully loaded; at snapshot 1
        // the price is 40 and the shed is real. Neither picture is wrong and
        // neither one can be read off the other.
        let (net, solved) = scene();

        let whole = reduce(&solved, &net, Instant::Horizon);
        assert_eq!(whole.price[0], 20.0);
        assert_eq!(whole.shed[0], 5.0);

        let peak_hour = reduce(&solved, &net, Instant::At(1));
        assert_eq!(peak_hour.price[0], 40.0);
        assert_eq!(peak_hour.shed[0], 5.0);

        let quiet_hour = reduce(&solved, &net, Instant::At(2));
        assert_eq!(quiet_hour.price[0], 10.0);
        assert_eq!(quiet_hour.shed[0], 0.0);
    }

    #[test]
    fn a_corridor_at_its_rating_backwards_reads_as_fully_loaded() {
        let (net, solved) = scene();
        // -100 on a line rated 100 is a line at its limit. Reducing without
        // taking magnitude first would report it as 50% loaded over the
        // horizon and as *negative* at the instant.
        assert_eq!(reduce(&solved, &net, Instant::Horizon).load[0], 1.0);
        assert_eq!(reduce(&solved, &net, Instant::At(1)).load[0], 1.0);
        assert_eq!(reduce(&solved, &net, Instant::At(0)).load[0], 0.5);
    }

    #[test]
    fn an_unrated_line_is_not_reported_as_idle() {
        let (mut net, solved) = scene();
        net.lines[0].s_nom = 0.0;
        assert!(reduce(&solved, &net, Instant::Horizon).load[0].is_nan());
    }

    #[test]
    fn a_result_from_a_longer_horizon_does_not_panic_on_a_shorter_one() {
        // A stale solve paired with a freshly loaded file: the instant is
        // valid for the network and past the end of the result.
        let (net, solved) = scene();
        let r = reduce(&solved, &net, Instant::At(50));
        assert_eq!(r.price[0], 0.0);
        assert_eq!(r.shed[0], 0.0);
    }

    #[test]
    fn a_result_with_fewer_lines_than_the_network_stops_at_the_shorter_one() {
        // `zip` is load-bearing here rather than incidental: indexing would
        // panic on the mismatch, which a dropped file can produce.
        let (mut net, solved) = scene();
        net.lines.push(Line {
            name: "extra".into(),
            s_nom: 10.0,
            ..Default::default()
        });
        assert_eq!(reduce(&solved, &net, Instant::Horizon).load.len(), 1);
    }
}

/// How much a result can be relied on, from its status alone.
///
/// The failure this exists to prevent is documented and real: a published
/// workflow wrote a results file for a run that returned `Status: ok`,
/// `Termination: suboptimal`, `Objective: 3.14e+37`. A wrong answer with a
/// beautiful chart on top of it is the worst thing this tool could ship, so a
/// result that is not proven optimal has to *look* different rather than merely
/// say so in a field nobody reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trust {
    /// Proven optimal. Every number derived from it means what it says.
    Proven,
    /// A feasible point that was not proven optimal -- a hit iteration or time
    /// limit, or a branch-and-bound that stopped on its node count. The
    /// dispatch is real; the cost is an upper bound and the prices are duals of
    /// a relaxation rather than of the answer.
    Unproven,
    /// No answer at all. Anything drawn from it would be invented.
    None,
}

impl Trust {
    fn of(status: &str) -> Self {
        // Matched on the string because that is what crosses the worker
        // boundary; the enum behind it lives in `gridwright-solve` and is not a
        // dependency here. Unknown statuses are untrusted rather than trusted,
        // because a solver this does not recognise has told us something we
        // cannot interpret, and the safe reading of "I do not understand this"
        // is not "it is fine".
        match status {
            "Optimal" => Trust::Proven,
            "Limit" => Trust::Unproven,
            "Infeasible" | "Unbounded" => Trust::None,
            _ => Trust::Unproven,
        }
    }

    /// What to say about it, in the reader's terms rather than the solver's.
    fn caveat(self) -> Option<&'static str> {
        match self {
            Trust::Proven => None,
            Trust::Unproven => Some(
                "Stopped before proving optimality. The dispatch is feasible; the cost is an \
                 upper bound and the prices are duals of a relaxation.",
            ),
            Trust::None => Some("No feasible answer. Nothing below is a result."),
        }
    }

    fn color(self) -> egui::Color32 {
        match self {
            Trust::Proven => crate::theme::LIVE,
            Trust::Unproven => crate::theme::ALARM,
            Trust::None => crate::theme::TRIP,
        }
    }
}

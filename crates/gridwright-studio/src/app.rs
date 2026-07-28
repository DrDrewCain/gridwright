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
    view: NetworkView,
    backend: Box<dyn SolveBackend>,
    outcome: Option<Result<Solved, Failure>>,
    /// Peak unserved energy per bus over the horizon, for the view to mark.
    /// Reduced once when a result arrives; see `NetworkView::ui`.
    peak_shed: Vec<f64>,
    /// The last thing that went wrong while opening a file. Kept until the next
    /// load rather than shown for a few frames: a person who dropped the wrong
    /// file may not be looking at the screen when it lands.
    load_error: Option<String>,
}

/// A network to open when there is nothing else to open.
///
/// IEEE 14-bus: small enough to read at a glance, real enough to be worth
/// solving, and the case every power-systems engineer already knows — so the
/// first thing the studio shows is something the user can check against their
/// own expectations rather than something they have to take on trust.
const SAMPLE: &[u8] = include_bytes!("../../../examples/pglib/case14_ieee.m");
const SAMPLE_NAME: &str = "case14_ieee.m";

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
            view: NetworkView::default(),
            // The context is taken here rather than at solve time because the
            // native backend needs it to wake the UI from another thread, and
            // `CreationContext` is the one place it is handed to us.
            backend: Box::new(new_solver(&cc.egui_ctx)),
            outcome: None,
            peak_shed: Vec::new(),
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
            Ok(loaded) => {
                self.positions = layout(&loaded.network);
                self.view.reset();
                self.outcome = None;
                self.peak_shed.clear();
                self.load_error = None;

                // Solve immediately when it is cheap enough to be
                // imperceptible. Asking someone to press a button to find out
                // something that takes ten milliseconds is friction with no
                // purpose, and the answer is what they opened the file for.
                //
                // The threshold is deliberately far below what the backend will
                // accept: this is "so fast the user will not notice", not "as
                // much as we can get away with". Anything larger stays explicit,
                // because a solve you did not ask for and then have to wait for
                // is worse than a button.
                let rows = gridwright_build::Lopf::row_counts(&loaded.network).total();
                if rows <= AUTO_SOLVE_ROWS && self.backend.is_ready() {
                    self.backend.submit(&loaded.network);
                }

                self.loaded = Some(loaded);
            }
            Err(f) => self.load_error = Some(format!("{}: {}", f.kind, f.message)),
        }
    }

    /// Open the bundled IEEE 14-bus case.
    ///
    /// Public because the browser entry point reaches for it when the page is
    /// asked for `#demo`, and because it is the same thing the empty state's
    /// button does — one code path, so the two cannot drift.
    pub fn open_sample(&mut self) {
        self.open_bytes(Some(SAMPLE_NAME), SAMPLE);
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
                // An empty state that can only wait is a dead end. The sample
                // is embedded rather than fetched so it works offline, from a
                // file:// URL, and on the first paint — and because a browser
                // tab has no working directory to open it from.
                if ui.button("or open the IEEE 14-bus case").clicked() {
                    self.open_sample();
                }
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
                ui.label(
                    egui::RichText::new(&loaded.name)
                        .size(13.0)
                        .color(theme::INK_STRONG)
                        .strong(),
                );
            }
        }

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
            ui.label(egui::RichText::new("Reader notes").strong());
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
    fn inspector(&self, ui: &mut egui::Ui) -> bool {
        use crate::theme;

        let Some(net) = self.network() else {
            return false;
        };
        let Some(b) = self.view.selected().filter(|&b| b < net.buses.len()) else {
            return false;
        };
        let bus = &net.buses[b];

        ui.add_space(theme::UNIT * 3.0);
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

        // Attachments, named rather than counted. "3 generators" tells you
        // nothing you cannot see on the canvas; their names and sizes do.
        let mut rows = 0;
        egui::Grid::new("inspector").num_columns(2).show(ui, |ui| {
            for g in net.generators.iter().filter(|g| g.bus == b) {
                attached(ui, &g.name, format!("{:.0} MW", g.p_nom));
                rows += 1;
            }
            for l in net.loads.iter().filter(|l| l.bus == b) {
                attached(ui, &l.name, format!("{:.0} MW", l.p_set));
                rows += 1;
            }
            for st in net.storage.iter().filter(|s| s.bus == b) {
                attached(ui, &st.name, format!("{:.0} MW", st.p_nom));
                rows += 1;
            }
        });
        if rows == 0 {
            ui.label(
                egui::RichText::new("nothing attached")
                    .size(11.0)
                    .color(theme::INK_DIM),
            );
        }

        true
    }

    fn results_area(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Results").strong());

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
                egui::Grid::new("results").num_columns(2).show(ui, |ui| {
                    ui.label("Status");
                    ui.label(&solved.status);
                    ui.end_row();

                    ui.label("Objective");
                    // Absent unless optimal, and shown as absent rather than as
                    // zero: a cost read off a non-optimal answer is a wrong
                    // number that looks like a right one.
                    match solved.objective {
                        Some(v) => ui.label(format!("{v:.2}")),
                        None => ui.label("—"),
                    };
                    ui.end_row();

                    ui.label("Total shed");
                    ui.label(format!("{:.3}", solved.total_shed));
                    ui.end_row();

                    if !solved.built.is_empty() {
                        ui.label("Capacity built");
                        ui.label(format!("{} components", solved.built.len()));
                        ui.end_row();
                    }
                });

                if solved.total_shed > 0.0 {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        "Buses with unserved energy are ringed on the canvas.",
                    );
                }
            }
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
        let share = p1 as f32 / total as f32;
        let (rect, response) =
            ui.allocate_exact_size(egui::Vec2::new(120.0, 8.0), egui::Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 1.0, theme::SLATE_FIELD);
        let mut feasible = rect;
        feasible.set_width(rect.width() * share);
        // Amber for phase one because it is, in the sense that matters, wasted
        // work: a warm start would have skipped it.
        p.rect_filled(feasible, 1.0, theme::ALARM.gamma_multiply(0.75));

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

    /// Reduce the per-bus, per-snapshot shed to one number per bus.
    ///
    /// Peak rather than total: the view marks *where* the system failed, and a
    /// bus that sheds heavily for one hour is as much a failure of that bus as
    /// one that sheds lightly all year.
    fn absorb(&mut self, outcome: Result<Solved, Failure>) {
        self.peak_shed = match &outcome {
            Ok(solved) => solved
                .shed
                .iter()
                .map(|per_snapshot| per_snapshot.iter().copied().fold(0.0_f64, f64::max))
                .collect(),
            Err(_) => Vec::new(),
        };
        self.outcome = Some(outcome);
    }
}

impl eframe::App for StudioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.take_dropped(ui.ctx());

        if let Some(outcome) = self.backend.take_result() {
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

        self.status_strip(ui);

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
            self.view.ui(ui, net, &self.positions, &self.peak_shed);
        });
    }
}

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

/// Group digits, because `13659` and `1354` are hard to tell apart at a glance
/// and `13,659` and `1,354` are not.
fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
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

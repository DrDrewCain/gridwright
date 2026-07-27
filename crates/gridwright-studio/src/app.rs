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

impl StudioApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
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
                self.loaded = Some(loaded);
                self.view.reset();
                self.outcome = None;
                self.peak_shed.clear();
                self.load_error = None;
            }
            Err(f) => self.load_error = Some(format!("{}: {}", f.kind, f.message)),
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
        ui.heading("gridwright studio");
        ui.add_space(4.0);

        match &self.loaded {
            None => {
                ui.label("No network loaded.");
                ui.label("Drop a network file onto the canvas.");
            }
            Some(loaded) => {
                ui.label(egui::RichText::new(&loaded.name).strong());
            }
        }

        if let Some(err) = &self.load_error {
            ui.colored_label(ui.visuals().error_fg_color, err);
        }

        let Some(net) = self.network() else { return };

        ui.separator();
        ui.label(egui::RichText::new("Network").strong());
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
                egui::ScrollArea::vertical().show(ui, |ui| self.side_panel(ui));
            });

        // `no_frame` rather than the default margins: the canvas paints its own
        // background and should meet the panel edge, and an inset would show as
        // a border around a view the user is panning.
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let net = self.loaded.as_ref().map(|l| &l.network);
            self.view.ui(ui, net, &self.positions, &self.peak_shed);
        });
    }
}

fn count(ui: &mut egui::Ui, label: &str, n: usize) {
    ui.label(label);
    ui.label(n.to_string());
    ui.end_row();
}

#[cfg(not(target_arch = "wasm32"))]
fn new_solver(ctx: &egui::Context) -> DefaultSolver {
    DefaultSolver::new(ctx.clone())
}

#[cfg(target_arch = "wasm32")]
fn new_solver(_ctx: &egui::Context) -> DefaultSolver {
    DefaultSolver::new()
}

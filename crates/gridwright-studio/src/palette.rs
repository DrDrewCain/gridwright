//! Type a name, go there.
//!
//! The argument for this is made best from inside the field. Overbye, IREP
//! 2007, on a 43,000-bus model whose 7,100-bus one-line diagram could not
//! answer the question he had: *"the one-line did not show the desired
//! information, [but] the one-line, along with the underlying power system
//! model, did contain the necessary information. The trick was to provide a way
//! of dynamically extracting it."* And on why a fixed set of views cannot
//! substitute: *"it can be quite difficult to design a priori a single display,
//! or even a set of displays, that contains all the information needed."*
//!
//! A palette is the cheapest possible form of that. Every bus in the file is
//! reachable in three keystrokes whether or not it is on screen, whether or not
//! its label survived decluttering, and whether or not anybody thought to build
//! a view that shows it.

use eframe::egui;

use crate::fuzzy;
use crate::theme;

/// What the reader chose.
pub enum Action {
    /// Select this bus and bring the camera to it.
    GoTo(usize),
    /// Select this corridor and bring the camera to it.
    GoToLine(usize),
    Solve,
    Fit,
    OpenSample,
    /// Show the whole horizon rather than one snapshot.
    Horizon,
    /// Go to the hour the system was under most stress.
    WorstHour,
    /// Go to the hour prices diverged most across the network.
    MostCongested,
}

/// A command, as opposed to a place.
struct Command {
    label: &'static str,
    /// Shown right-aligned. VS Code documents doing this, and it is the single
    /// best discoverability feature a palette has: it teaches the shortcut
    /// every time the reader takes the slow path. Neither Figma nor Adobe does
    /// it, and both are worse for it.
    keys: &'static str,
    make: fn() -> Action,
}

const COMMANDS: [Command; 6] = [
    Command {
        label: "Solve",
        keys: "",
        make: || Action::Solve,
    },
    Command {
        label: "Fit to window",
        keys: "F",
        make: || Action::Fit,
    },
    Command {
        label: "Go to the most congested hour",
        keys: "",
        make: || Action::MostCongested,
    },
    Command {
        label: "Go to the worst hour",
        keys: "",
        make: || Action::WorstHour,
    },
    Command {
        label: "Show whole horizon",
        keys: "",
        make: || Action::Horizon,
    },
    Command {
        label: "Open the sample case",
        keys: "",
        make: || Action::OpenSample,
    },
];

/// How many results are worth showing.
///
/// A list longer than the eye can take in is a list nobody reads to the end of,
/// and the matcher's whole job is that the right answer is near the top.
const SHOWN: usize = 12;

#[derive(Default)]
pub struct Palette {
    open: bool,
    query: String,
    /// Which result is armed for Enter.
    cursor: usize,
    /// Set for one frame after opening, to take focus from whatever had it.
    just_opened: bool,
}

impl Palette {
    /// Open or close on the keyboard, and report what was chosen.
    ///
    /// `names` is every component the palette can navigate to.
    pub fn ui(&mut self, ctx: &egui::Context, names: &Names<'_>) -> Option<Action> {
        self.take_shortcut(ctx);
        if !self.open {
            return None;
        }

        let hits = self.rank(names);
        self.cursor = self.cursor.min(hits.len().saturating_sub(1));

        let mut chosen = None;
        let mut close = false;

        egui::Modal::new(egui::Id::new("palette")).show(ctx, |ui| {
            ui.set_width(420.0);

            let field = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("go to a bus, or run a command")
                    .desired_width(f32::INFINITY)
                    .font(egui::FontId::proportional(14.0)),
            );
            if self.just_opened {
                field.request_focus();
                self.just_opened = false;
            }

            // Arrows move the cursor rather than the text caret. Read before
            // the rows are drawn so the armed row is right on the frame the key
            // was pressed, not one frame later.
            let (up, down, enter, esc) = ui.input(|i| {
                (
                    i.key_pressed(egui::Key::ArrowUp),
                    i.key_pressed(egui::Key::ArrowDown),
                    i.key_pressed(egui::Key::Enter),
                    i.key_pressed(egui::Key::Escape),
                )
            });
            if down && !hits.is_empty() {
                self.cursor = (self.cursor + 1).min(hits.len() - 1);
            }
            if up {
                self.cursor = self.cursor.saturating_sub(1);
            }
            if esc {
                close = true;
            }

            if hits.is_empty() {
                ui.add_space(theme::UNIT * 2.0);
                ui.label(
                    egui::RichText::new("nothing by that name")
                        .size(12.0)
                        .color(theme::INK_DIM),
                );
            }

            ui.add_space(theme::UNIT);
            for (row, hit) in hits.iter().enumerate() {
                let armed = row == self.cursor;
                if (enter && armed) || self.row(ui, hit, armed) {
                    chosen = Some(hit.action());
                    close = true;
                }
            }
        });

        if close {
            self.open = false;
            self.query.clear();
            self.cursor = 0;
        }
        chosen
    }

    /// One result. Returns whether it was clicked.
    fn row(&self, ui: &mut egui::Ui, hit: &Hit, armed: bool) -> bool {
        let bg = if armed {
            theme::SLATE_RAISED
        } else {
            egui::Color32::TRANSPARENT
        };
        let r = egui::Frame::new()
            .fill(bg)
            .inner_margin(egui::Margin::symmetric(
                (theme::UNIT * 1.5) as i8,
                (theme::UNIT * 0.75) as i8,
            ))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(hit.label())
                            .size(12.0)
                            .color(if armed { theme::INK_STRONG } else { theme::INK }),
                    );
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.label(
                                egui::RichText::new(hit.aside())
                                    .monospace()
                                    .size(10.0)
                                    .color(theme::INK_DIM),
                            );
                        },
                    );
                });
            });
        ui.interact(
            r.response.rect,
            egui::Id::new(("palette-row", hit.label())),
            egui::Sense::click(),
        )
        .clicked()
    }

    /// Everything matching, best first.
    fn rank(&self, names: &Names<'_>) -> Vec<Hit> {
        let q = self.query.trim();

        // Commands first when the query is empty, because a reader who opened
        // the palette with no idea what to type should see what it can do
        // rather than the first twelve buses in file order.
        let mut hits: Vec<(i32, Hit)> = Vec::new();
        for (i, c) in COMMANDS.iter().enumerate() {
            if let Some(s) = fuzzy::score(q, c.label) {
                // Commands are a short, fixed list and buses are thousands, so
                // an unweighted merge buries them. The offset keeps them near
                // the top without letting them outrank an exact bus name.
                hits.push((s + 6 - i as i32, Hit::Command(i)));
            }
        }
        for (b, name) in names.buses.iter().enumerate() {
            if let Some(s) = fuzzy::score(q, name) {
                hits.push((s, Hit::Bus(b, name.clone())));
            }
        }
        // Corridors too. Half the network is lines, and a reader who knows a
        // circuit by name had no way to reach it -- the only route was to find
        // it on the canvas, which is exactly the search the palette exists to
        // replace.
        for (e, name) in names.lines.iter().enumerate() {
            if let Some(s) = fuzzy::score(q, name) {
                // Slightly below an equally good bus match. A name that fits
                // both is far more often the substation being looked for, and
                // the corridor is one row further down rather than absent.
                hits.push((s - 1, Hit::Line(e, name.clone())));
            }
        }

        // Stable by score, then by index, so a tie resolves the same way on
        // every keystroke. A list that reshuffles under the cursor while the
        // reader is aiming at a row is worse than a list that ranks imperfectly.
        hits.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        hits.into_iter().take(SHOWN).map(|(_, h)| h).collect()
    }

    /// Cmd-K, and Ctrl-Shift-P for the muscle memory of everyone who has used
    /// an editor.
    ///
    /// Not Cmd-F, which is where Adobe put Photoshop's search and has been
    /// regretted ever since -- a reader will expect that to find something in
    /// the view. Not Cmd-slash either: Figma's own documentation is split-brained
    /// about whether that is still the binding, and on several keyboard layouts
    /// the slash itself needs a modifier.
    fn take_shortcut(&mut self, ctx: &egui::Context) {
        let hit = ctx.input_mut(|i| {
            i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND,
                egui::Key::K,
            )) || i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::P,
            ))
        });
        if hit {
            self.open = !self.open;
            self.just_opened = self.open;
            self.query.clear();
            self.cursor = 0;
        }
    }
}

/// A row in the list: a place or a command.
enum Hit {
    Bus(usize, String),
    Line(usize, String),
    Command(usize),
}

/// Everything the palette can navigate to, in index order.
pub struct Names<'a> {
    pub buses: &'a [String],
    pub lines: &'a [String],
}

impl Hit {
    fn label(&self) -> &str {
        match self {
            Hit::Bus(_, name) | Hit::Line(_, name) => name,
            Hit::Command(i) => COMMANDS[*i].label,
        }
    }

    /// The right-hand column: what kind of thing this is, or its shortcut.
    fn aside(&self) -> &str {
        match self {
            Hit::Bus(..) => "bus",
            Hit::Line(..) => "line",
            Hit::Command(i) => COMMANDS[*i].keys,
        }
    }

    fn action(&self) -> Action {
        match self {
            Hit::Bus(b, _) => Action::GoTo(*b),
            Hit::Line(e, _) => Action::GoToLine(*e),
            Hit::Command(i) => (COMMANDS[*i].make)(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    fn labels(p: &Palette, buses: &[String], lines: &[String]) -> Vec<String> {
        p.rank(&Names { buses, lines })
            .iter()
            .map(|h| h.label().to_string())
            .collect()
    }

    #[test]
    fn an_empty_query_offers_the_commands_first() {
        // Someone who opened the palette with no idea what to type should see
        // what it can do, not the first twelve buses in file order.
        let p = Palette::default();
        let got = labels(&p, &strs(&["bus1", "bus2"]), &[]);
        assert_eq!(got[0], "Solve");
        assert!(got.contains(&"bus1".to_string()));
    }

    #[test]
    fn typing_a_bus_name_puts_that_bus_first() {
        let p = Palette {
            query: "bus2".into(),
            ..Default::default()
        };
        let got = labels(&p, &strs(&["bus1", "bus2", "bus20", "substation2"]), &[]);
        assert_eq!(got[0], "bus2");
    }

    #[test]
    fn a_command_is_reachable_by_name() {
        let p = Palette {
            query: "fit".into(),
            ..Default::default()
        };
        assert_eq!(labels(&p, &strs(&["bus1"]), &[])[0], "Fit to window");
    }

    #[test]
    fn a_corridor_is_reachable_by_name() {
        let p = Palette {
            query: "north".into(),
            ..Default::default()
        };
        let got = labels(&p, &strs(&["bus1"]), &strs(&["north-south"]));
        assert_eq!(got[0], "north-south");
    }

    #[test]
    fn a_bus_outranks_a_corridor_of_the_same_name() {
        // A name that fits both is far more often the substation.
        let p = Palette {
            query: "alpha".into(),
            ..Default::default()
        };
        let got = labels(&p, &strs(&["alpha"]), &strs(&["alpha"]));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], "alpha");
    }

    #[test]
    fn the_list_is_capped() {
        let many: Vec<String> = (0..500).map(|i| format!("bus{i}")).collect();
        let n = Names { buses: &many, lines: &[] };
        assert_eq!(Palette::default().rank(&n).len(), SHOWN);
    }

    #[test]
    fn nothing_matching_ranks_nothing() {
        let p = Palette {
            query: "zzzzz".into(),
            ..Default::default()
        };
        let n = Names { buses: &strs(&["bus1", "bus2"]), lines: &[] };
        assert!(p.rank(&n).is_empty());
    }

    #[test]
    fn ranking_is_stable_across_calls() {
        // A list that reshuffles between frames while the reader is aiming at a
        // row is worse than one that ranks imperfectly.
        let p = Palette {
            query: "bus".into(),
            ..Default::default()
        };
        let b = strs(&["bus1", "bus2", "bus3", "abus", "busbar"]);
        assert_eq!(labels(&p, &b, &[]), labels(&p, &b, &[]));
    }
}

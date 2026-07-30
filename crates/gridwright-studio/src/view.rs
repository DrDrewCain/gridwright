//! The network drawing: pan, zoom, buses as nodes, lines and links as edges.
//!
//! Everything here is `egui::Painter` primitives — line segments, circles, a
//! little text. No scene graph, no 3D engine, no plotting library. A power
//! system diagram is a few thousand line segments and a few thousand circles,
//! which is well inside what an immediate-mode painter emits in a frame, and
//! anything heavier would be paying for a retained scene that changes every time
//! the camera moves anyway.

use eframe::egui::{
    Align2, Color32, FontId, Painter, Pos2, Rect, Sense, Stroke, Ui, Vec2, pos2, vec2,
};
use gridwright_net::Network;

/// Screen points per model unit at the widest sensible view, and at the closest.
///
/// [`crate::layout`] hands back a roughly unit-sized box, so these are bounds on
/// "the whole network fits in a thumbnail" and "one substation fills the pane".
const MIN_ZOOM: f32 = 20.0;

/// How many named places the canvas will consider in one frame.
///
/// A cap on *candidates*, not on what appears: the collision pass drops whatever
/// has no room, so a tight view shows a handful and a regional one shows most of
/// this. It exists because past a few dozen names the map stops being a reference
/// and becomes the thing a reader reads instead of the network.
const PLACE_LIMIT: usize = 60;
const MAX_ZOOM: f32 = 40_000.0;

/// How close the pointer must be to a bus, in screen points, to pick it.
///
/// Generous relative to the drawn radius on purpose: at a zoom where the whole
/// network is visible the nodes are a couple of points across, and requiring a
/// hit on the circle itself would make hovering a test of aim.
const PICK_RADIUS: f32 = 9.0;


/// AC corridors and transport corridors are different objects, not different
/// ratings of one, so they are told apart before anything else on the canvas.
/// A transport link is controllable — HVDC, or a modelled exchange — and where
/// power goes on it is a decision rather than a consequence of impedance.
const AC_COLOR: Color32 = Color32::from_rgb(0x6a, 0x74, 0x82);
const TRANSPORT_COLOR: Color32 = Color32::from_rgb(0x3f, 0x93, 0x8c);
const LINK_COLOR: Color32 = Color32::from_rgb(0x86, 0x6c, 0xa8);

/// What the last solve found, in the shape the canvas draws it.
///
/// Reduced by the caller rather than here: each field is a reduction over every
/// snapshot, and redoing that per frame would make the cost of drawing scale
/// with the length of the horizon, for a picture that does not change between
/// frames. Grouped because they arrive together, are empty together, and are
/// the only reason the drawing functions need to know a solve happened.
#[derive(Clone, Copy, Default)]
pub struct Overlay<'a> {
    /// Per bus: the worst unserved energy at any snapshot.
    pub peak_shed: &'a [f64],
    /// Per bus: the mean nodal price over the horizon.
    pub prices: &'a [f64],
    /// Per line: peak flow as a fraction of rating, NaN where unrated.
    pub loading: &'a [f64],
    /// Per line: signed flow at the chosen instant. Positive is from `bus0`
    /// toward `bus1`, which is the convention every one-line label uses --
    /// positive is away from the end you are standing on.
    pub flow: &'a [f64],
}

pub struct NetworkView {
    /// Screen points per model unit.
    zoom: f32,
    /// The model-space point pinned to the middle of the pane.
    centre: Pos2,
    /// Cleared when a network is loaded, so the first frame with real content
    /// frames it. Deferred to draw time because fitting needs the pane size,
    /// and nothing knows that until there is a pane.
    needs_fit: bool,
    /// The circuit the user has chosen. Mutually exclusive with `selected`:
    /// the inspector describes one thing, and two selections would make "the
    /// selection" ambiguous in a panel with room for one answer.
    selected_line: Option<usize>,
    /// The bus the user has chosen, which outlives the pointer leaving it.
    ///
    /// Distinct from hover on purpose: hover answers "what is under my cursor"
    /// and vanishes, selection answers "what am I working on" and does not. An
    /// inspector driven by hover cannot be read, because reading it means
    /// moving the pointer off the thing it describes.
    selected: Option<usize>,
    /// Where the camera is heading, when it was told rather than dragged.
    goal: Option<(Pos2, f32)>,
    /// The bus under the pointer last frame, so the cursor can stop offering to
    /// pan over something that is offering to be clicked.
    hovered: Option<usize>,
    /// Whether the next fit jumps rather than travels. True for a new network.
    snap_next_fit: bool,
    /// The map under the network. Levels are decoded on first use.
    /// Countries the reader has switched off, by the code the network file uses.
    ///
    /// Off rather than on, so the default is everything and a network with no
    /// country codes at all is unaffected. A continental model is the only place
    /// this matters: 7,893 substations across sixty countries is a question about
    /// scale, and answering a question about Portugal inside it means being able
    /// to put the other fifty-nine away.
    hidden: std::collections::HashSet<String>,
    basemap: crate::basemap::Basemap,
    places: crate::places::Places,
    /// Which map layers the reader wants.
    pub layers: crate::basemap::Show,
    /// A bus to bring the camera to on the next frame that has a layout.
    reveal_next: Option<usize>,
    /// A corridor to bring the camera to, likewise.
    reveal_line_next: Option<usize>,
    /// Whether a corridor was under the pointer when the edges were drawn this
    /// frame. Set by `draw_edges`, read by `draw_buses` a few lines later.
    circuit_under_pointer: bool,
}

impl Default for NetworkView {
    fn default() -> Self {
        Self {
            zoom: 400.0,
            centre: Pos2::ZERO,
            needs_fit: true,
            selected_line: None,
            selected: None,
            goal: None,
            hovered: None,
            snap_next_fit: true,
            hidden: std::collections::HashSet::new(),
            basemap: crate::basemap::Basemap::default(),
            places: crate::places::Places::load(),
            layers: crate::basemap::Show::default(),
            reveal_next: None,
            reveal_line_next: None,
            circuit_under_pointer: false,
        }
    }
}

impl NetworkView {
    /// Refit at the next opportunity. Called when the network changes under it.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Select a bus and bring the camera to it.
    ///
    /// Both, not either. A selection the reader cannot see is an inspector
    /// describing something off screen; a camera move with no selection loses
    /// the thing they searched for the moment they pan away from it.
    ///
    /// The camera *glides* rather than cutting, because this is the case the
    /// easing exists for: the reader did not steer this move and has no idea
    /// where on the network they are about to end up. Watching it travel is
    /// what tells them.
    pub fn reveal(&mut self, bus: usize) {
        self.selected = Some(bus);
        self.selected_line = None;
        self.reveal_next = Some(bus);
    }

    /// Which circuit is selected, for whoever draws the inspector.
    pub fn selected_line(&self) -> Option<usize> {
        self.selected_line
    }

    /// Refit at the next frame, travelling rather than cutting.
    pub fn refit(&mut self) {
        self.needs_fit = true;
    }

    /// Select a corridor and bring the camera to its midpoint.
    pub fn reveal_line(&mut self, line: usize) {
        self.selected_line = Some(line);
        self.selected = None;
        self.reveal_line_next = Some(line);
    }

    /// Which bus is selected, for whoever draws the inspector.
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// The overlay carries what the last solve found, and is empty until there
    /// is one.
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        net: Option<&Network>,
        layout: &[Pos2],
        overlay: Overlay<'_>,
        geo: Option<crate::layout::Frame>,
    ) {
        let (response, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        let rect = response.rect;
        let painter = painter.with_clip_rect(rect);

        painter.rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);

        let Some(net) = net.filter(|_| !layout.is_empty()) else {
            self.draw_empty(&painter, rect);
            return;
        };

        if rect.width() < 1.0 || rect.height() < 1.0 {
            return;
        }

        if self.needs_fit {
            self.fit(rect, layout);
            self.needs_fit = false;
        }

        // Deferred to here because centring on a bus needs its position, and the
        // palette that asked for it has no layout to look in.
        if let Some(b) = self.reveal_next.take()
            && let Some(&p) = layout.get(b)
        {
            self.goal = Some((p, self.zoom.max(220.0)));
        }
        if let Some(e) = self.reveal_line_next.take()
            && let Some(line) = net.lines.get(e)
            && let (Some(&a), Some(&b)) = (layout.get(line.bus0), layout.get(line.bus1))
        {
            // Framed at the *midpoint*: one end on screen puts the other off it,
            // which is the half a reader asking about a corridor wants to see.
            let mid = a + (b - a) * 0.5;
            let span = a.distance(b).max(1e-4);
            let fits = (rect.width().min(rect.height()) * 0.55) / span;
            self.goal = Some((mid, fits.clamp(MIN_ZOOM, MAX_ZOOM)));
        }

        self.handle_camera(ui, &response, rect, net.buses.len());

        // Under everything, and only when the positions are a projection. A
        // coastline beneath a spring embedding would place substations on a map
        // they have no relationship to, which is a far worse lie than no map at
        // all -- the whole point of the origin label in the status strip is that
        // those two pictures are indistinguishable, and this would make one of
        // them look authoritative.
        if let Some(frame) = geo {
            // The camera is read out before the call, because `draw` now needs
            // `&mut self.basemap` to cache the level it decodes and the closure
            // would otherwise hold a shared borrow of the whole view.
            let (zoom, centre) = (self.zoom, self.centre);
            let visible = self.visible(rect);
            let to_screen = move |p: eframe::egui::Pos2| rect.center() + (p - centre) * zoom;
            let layers = self.layers;
            self.basemap.draw(
                &painter,
                visible,
                frame,
                to_screen,
                crate::basemap::Tone {
                    // Land is *lighter* than the canvas and the sea *is* the
                    // canvas. Painting the sea instead would put the brighter
                    // tone on the larger area, and the canvas is already the
                    // brightest surface in the window by design.
                    land: crate::theme::SLATE_RAISED,
                    sea: crate::theme::SLATE_WORK,
                    coast: crate::theme::SLATE_LINE,
                    border: crate::theme::SLATE_LINE.gamma_multiply(0.65),
                    river: crate::theme::SLATE_LINE.gamma_multiply(0.9),
                    urban: crate::theme::SLATE_LINE.gamma_multiply(0.5),
                },
                layers,
            );

            // Names, after the map and before the network. **Only inside the
            // loaded network's own extent**, which is the whole point: a basemap
            // that labels every city it knows about is a wall of type, and none of
            // it is about the network on screen.
            //
            // The extent is the buses' own bounding box taken back into Mercator
            // and widened by a quarter, so the places that frame the study appear
            // without the ones a hundred kilometres past it.
            let mut extent: Option<eframe::egui::Rect> = None;
            for p in layout.iter() {
                let m = frame.invert(*p);
                extent = Some(match extent {
                    Some(r) => r.union(eframe::egui::Rect::from_min_max(m, m)),
                    None => eframe::egui::Rect::from_min_max(m, m),
                });
            }
            if let Some(extent) = extent {
                let pad = (extent.size() * 0.25).max(eframe::egui::Vec2::splat(0.004));
                let reserved = self.bus_footprints(&painter, rect, net, layout);
                crate::places::draw(
                    &painter,
                    &self.places,
                    extent.expand2(pad),
                    // A cap, not a zoom rule. Past a few dozen names the map stops
                    // being a reference and starts being the thing you read
                    // instead of the network, and the collision pass already drops
                    // whatever has no room.
                    PLACE_LIMIT,
                    frame,
                    to_screen,
                    &reserved,
                    crate::places::Tone {
                        // Dimmer than any network ink, and nothing the network uses
                        // to mean something. Overbye (NAPS 2019): a detailed
                        // background risks "camouflaging the electric grid
                        // information of interest".
                        name: crate::theme::INK_DIM,
                        mark: crate::theme::INK_DIM.gamma_multiply(0.8),
                        halo: crate::theme::SLATE_WORK,
                    },
                );
            }
        }

        // Recorded before the network is drawn, so a click that turns out to have
        // been meant for a country name can be undone.
        let selected_before = self.selected;

        let on_circuit = self.draw_edges(
            &painter,
            rect,
            net,
            layout,
            overlay,
            response.hover_pos(),
        );
        self.circuit_under_pointer = on_circuit.is_some();
        let on_bus = self.draw_buses(ui, &painter, net, layout, overlay, &response);

        // Country names last, over everything, as a watermark.
        //
        // **Under the network was the obvious place and it was wrong.** Drawn there
        // the names were legible over open sea and buried in exactly the dense
        // regions a reader needs them -- half a word showing through a mat of
        // corridors, which reads as a label that has been cut off rather than one
        // that is behind something.
        //
        // So: on top, and quiet enough not to compete. Large, letter-spaced, in a
        // fraction of the dimmest ink, with a dark halo so it survives crossing a
        // bright 380 kV corridor. That is the register a country name belongs in on
        // any map -- present when looked for, ignorable otherwise -- and it keeps
        // faith with the finding this basemap is built around, which is about a
        // background competing with the grid rather than about it being visible.
        if let Some(frame) = geo
            && let Some(code) =
                self.draw_regions(ui, &painter, rect, net, layout, frame)
        {
            // The bus this click also landed on is given back. At a zoom where the
            // busbars are points a country name covers hundreds of them, so the
            // click that isolated Spain had already selected whichever one happened
            // to be under the word -- and the inspector then described a substation
            // the reader had not asked about.
            self.selected = selected_before;
            // Clicking a country shows only that country; clicking the one already
            // on its own shows everything again. **Decided from the country
            // clicked, not from whether anything is hidden**, which is what makes
            // it switch focus rather than flip-flop: the first version read
            // "something is hidden" and so clicking France while Spain was isolated
            // showed the whole continent, and moving between two countries took two
            // clicks with everything flashing up in between.
            if self.only_shown(net, &code) {
                self.show_all_regions();
            } else {
                self.only_region(net, &code);
            }
        }

        // A bus wins a tie. The pointer sits within picking distance of both
        // whenever it is near a tap point, and the bus is the thing that can be
        // selected -- offering a readout for the circuit there would contradict
        // the cursor.
        if let Some((c, at)) = on_circuit.filter(|_| !on_bus) {
            self.circuit_readout(ui, &painter, net, overlay.loading, c, at);
            // A corridor is selectable now, not merely hoverable. Hover answers
            // "what is under my cursor" and vanishes when you move to read the
            // panel; a corridor's flow over a day is exactly the thing you
            // cannot read while keeping the pointer on it.
            if response.clicked()
                && let Circuit::Line(e) = c
            {
                self.selected_line = Some(e);
                self.selected = None;
            }
        }

        // The selected corridor stays marked once the pointer leaves it.
        if let Some(e) = self.selected_line
            && let Some(line) = net.lines.get(e)
            && let Some((a, b)) = self.segment(rect, self.visible(rect), layout, line.bus0, line.bus1)
        {
            let taps = TapSlots::build(net, layout);
            let (a, b) = taps.place(a, b, line.bus0, line.bus1, Circuit::Line(e), self.bar_half());
            painter.add(eframe::egui::Shape::line(
                self.tapped(a, b),
                Stroke::new(3.0, crate::theme::INK_STRONG),
            ));
        }
        self.draw_key(&painter, rect, net, overlay.prices);
        self.draw_keys_hint(&painter, rect);
    }

    fn handle_camera(
        &mut self,
        ui: &Ui,
        response: &eframe::egui::Response,
        rect: Rect,
        buses: usize,
    ) {
        self.keys(ui, rect, buses);
        self.glide(ui, rect);

        if response.dragged() {
            // Direct manipulation is never animated. A camera that eases behind
            // the pointer during a drag feels like lag, not like motion; the
            // easing is reserved for movement the user commanded but did not
            // steer, which is fit and the keyboard zoom.
            self.centre -= response.drag_delta() / self.zoom;
            self.goal = None;
        }
        if response.dragged() {
            ui.ctx().set_cursor_icon(eframe::egui::CursorIcon::Grabbing);
        } else if response.hovered() && self.hovered.is_none() {
            ui.ctx().set_cursor_icon(eframe::egui::CursorIcon::Grab);
        }

        if !response.hovered() {
            return;
        }

        // Scroll zooms rather than scrolls. There is nothing to scroll in a map
        // view, and every map a person has used behaves this way; pinch is
        // folded in through `zoom_delta` so a trackpad works too.
        let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
        let factor = pinch * (scroll * 0.004).exp();
        if (factor - 1.0).abs() < 1e-4 {
            return;
        }

        let anchor = ui.input(|i| i.pointer.hover_pos()).unwrap_or(rect.center());
        self.zoom_about(anchor, factor, rect);
        self.goal = None;
    }

    /// Zoom by `factor`, holding the model point under `anchor` still.
    ///
    /// Holding that point is what makes zooming feel like moving a camera
    /// rather than resizing a picture.
    fn zoom_about(&mut self, anchor: Pos2, factor: f32, rect: Rect) {
        let before = self.model_of(rect, anchor);
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let after = self.model_of(rect, anchor);
        self.centre += before - after;
    }

    /// The keyboard, for the operators who will not reach for a trackpad.
    ///
    /// This is a tool people keep open for hours. Every camera move having to
    /// go through a pointer is the difference between an instrument and a demo.
    fn keys(&mut self, ui: &Ui, rect: Rect, buses: usize) {
        use eframe::egui::Key;

        // Only when the canvas owns the keyboard. Otherwise `f` typed into a
        // future filter box would fling the camera across the network.
        if ui.memory(|m| m.focused()).is_some() {
            return;
        }

        let (fit, clear, zoom_in, zoom_out, tab, back) = ui.input(|i| {
            (
                i.key_pressed(Key::F) || i.key_pressed(Key::Home),
                i.key_pressed(Key::Escape),
                i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals),
                i.key_pressed(Key::Minus),
                i.key_pressed(Key::Tab) && !i.modifiers.shift,
                i.key_pressed(Key::Tab) && i.modifiers.shift,
            )
        });

        // Tab walks the buses, shift-tab walks back.
        //
        // Borrowed from Figma, where tab and shift-tab select the next and
        // previous sibling and complete a four-direction keyboard walk of the
        // document. There is no hierarchy here yet to walk up and down, but the
        // sideways half is useful on its own: it is the only way to reach a bus
        // whose label lost a collision, short of knowing its name well enough
        // to type it.
        //
        // Nothing happens with no network loaded, and the first press selects
        // the first bus rather than doing nothing.
        if (tab || back) && buses > 0 {
            let next = match (self.selected, tab) {
                (None, _) => 0,
                (Some(b), true) => (b + 1) % buses,
                (Some(b), false) => (b + buses - 1) % buses,
            };
            self.reveal(next);
        }

        if fit {
            self.needs_fit = true;
        }
        if clear {
            self.selected = None;
            self.selected_line = None;
        }
        // A fixed step rather than the scroll factor, so a held key walks the
        // zoom at a predictable rate and each press is undone by one press of
        // the other key.
        if zoom_in != zoom_out {
            let step = if zoom_in { 1.0 / 0.8 } else { 0.8 };
            self.zoom_about(rect.center(), step, rect);
            self.goal = None;
        }
    }

    /// Ease the camera toward a commanded position.
    ///
    /// Framing a network is a change of place, and cutting between two places
    /// with no motion between them makes the reader re-find where they were.
    /// Watching the camera travel costs a fifth of a second and answers it.
    fn glide(&mut self, ui: &Ui, rect: Rect) {
        let Some((centre, zoom)) = self.goal else {
            return;
        };

        // Exponential decay, framerate-independent: the fraction of the
        // remaining distance covered per second is constant, so the motion is
        // the same on a 60Hz panel and a 120Hz one.
        let dt = ui.input(|i| i.stable_dt).min(0.1);
        let t = 1.0 - (-12.0 * dt).exp();

        // Interpolated in log space, because zoom is multiplicative -- a linear
        // walk from 40 to 400 spends most of its time already zoomed out.
        self.zoom = (self.zoom.ln() + (zoom.ln() - self.zoom.ln()) * t).exp();
        self.centre += (centre - self.centre) * t;

        let close = (self.zoom / zoom).ln().abs() < 0.002
            && (centre - self.centre).length() * self.zoom < 0.5;
        if close {
            self.centre = centre;
            self.zoom = zoom;
            self.goal = None;
        } else {
            // Nothing else is asking for frames, so the animation has to.
            ui.ctx().request_repaint();
        }
        let _ = rect;
    }

    /// What the keyboard does, where the keyboard is used.
    ///
    /// Shortcuts nobody is told about are shortcuts nobody has. Putting this in
    /// a menu or a help dialog would mean a reader has to already suspect the
    /// keys exist before they can find out that they do.
    ///
    /// Opposite corner from the price key so the two do not stack, and in the
    /// dimmest ink in the palette: it is a thing you notice once and then stop
    /// seeing, which is the correct life cycle for an instruction.
    fn draw_keys_hint(&self, painter: &eframe::egui::Painter, rect: Rect) {
        painter.text(
            rect.right_bottom() + vec2(-12.0, -12.0),
            Align2::RIGHT_BOTTOM,
            "scroll zoom · drag pan · tab next bus · F fit · esc clear",
            FontId::proportional(10.0),
            crate::theme::INK_DIM,
        );
    }

    /// The canvas with nothing on it.
    ///
    /// An empty screen is an invitation to act, so it names the action and
    /// then the way out for a reader who has no file to hand. It points at the
    /// panel rather than reprinting what is on it -- the formats are listed
    /// there with their extensions, and two copies of a list is how one of them
    /// goes stale.
    fn draw_empty(&self, painter: &eframe::egui::Painter, rect: Rect) {
        use crate::theme;

        // A dashed boundary inside the pane, so the drop target is a place
        // rather than the whole window. Dashes because a solid rule here would
        // read as a panel edge, and there is no panel.
        let target = Rect::from_center_size(
            rect.center(),
            vec2(rect.width().min(420.0), rect.height().min(200.0)),
        );
        painter.add(eframe::egui::Shape::dashed_line(
            &[
                target.left_top(),
                target.right_top(),
                target.right_bottom(),
                target.left_bottom(),
                target.left_top(),
            ],
            Stroke::new(1.0, theme::SLATE_LINE),
            6.0,
            5.0,
        ));

        painter.text(
            target.center() - vec2(0.0, 10.0),
            Align2::CENTER_CENTER,
            "Drop a network here",
            FontId::proportional(15.0),
            theme::INK,
        );
        painter.text(
            target.center() + vec2(0.0, 12.0),
            Align2::CENTER_CENTER,
            "or open the sample case on the left",
            FontId::proportional(11.0),
            theme::INK_DIM,
        );
    }

    /// Model-space bounds of what is on screen, so everything outside can be
    /// dropped before a shape is built.
    ///
    /// Egui pays per emitted shape whether or not it lands in the clip rect,
    /// and at a zoom that shows one substation of a national model, that is
    /// most of the network.
    fn visible(&self, rect: Rect) -> Rect {
        Rect::from_min_max(self.model_of(rect, rect.min), self.model_of(rect, rect.max))
    }

    fn draw_edges(
        &self,
        painter: &eframe::egui::Painter,
        rect: Rect,
        net: &Network,
        layout: &[Pos2],
        overlay: Overlay<'_>,
        pointer: Option<Pos2>,
    ) -> Option<(Circuit, Pos2)> {
        let visible = self.visible(rect);
        let mut near: Option<(Circuit, Pos2, f32)> = None;
        let mut consider = |c: Circuit, path: &[Pos2]| {
            if let Some(ptr) = pointer
                && let Some((at, d)) = nearest_on(path, ptr)
                && d <= PICK_RADIUS
                && near.is_none_or(|(_, _, best)| d < best)
            {
                near = Some((c, at, d));
            }
        };
        // Width carries rating, so the corridors that matter read first. Square
        // root rather than linear because transfer capacities span three orders
        // of magnitude within one network, and a linear map turns everything
        // below the largest interconnector into a hairline.
        let max_s_nom = net
            .lines
            .iter()
            .map(|l| l.s_nom.abs())
            .fold(0.0_f64, f64::max)
            .max(1.0);

        // Where each circuit lands along each busbar.
        //
        // A busbar has length because more than one thing connects to it, and a
        // diagram where every circuit meets the bar at its midpoint throws that
        // away — the bar stops being a conductor and goes back to being a dot
        // wearing a rectangle. Taps are spread along the bar and ordered by the
        // direction of their far end, so circuits leaving to the left land on
        // the left of the bar and nothing needs to cross the bar to get where
        // it is going.
        let taps = TapSlots::build(net, layout);
        let by_voltage = voltages_are_stated(net);

        // **The lowest voltage worth drawing at this zoom.**
        //
        // Semantic zoom on the network itself, and the same rule every published
        // grid map follows: at continental scale ENTSO-E's own map shows the
        // 380 kV backbone and nothing else, because ten thousand corridors in a
        // thousand-pixel box is one texture rather than ten thousand lines. The
        // European extract at fit zoom was exactly that -- the busbars had already
        // been reduced to points and the *lines* became the mat.
        //
        // The threshold is chosen by how much room there is, not by a bus count,
        // for the same reason the label rule is: eight thousand buses zoomed into
        // one country has all the room it needs. It comes from the same spacing
        // estimate the busbars use, so the two simplify together and a reader
        // never sees points joined by nothing.
        let floor_kv = self.voltage_floor(rect, layout, net);

        for (e, line) in net.lines.iter().enumerate() {
            // Kept whatever its voltage if the reader is pointing at it or has
            // selected it. A corridor that vanishes while being inspected is the
            // one case where decluttering contradicts the interface.
            // Either end hidden and the corridor goes nowhere, so it goes too.
            if !self.shows(net, line.bus0) || !self.shows(net, line.bus1) {
                continue;
            }
            let mine = self.selected_line == Some(e);
            if !mine && by_voltage && line_kv(net, line) < floor_kv {
                continue;
            }
            let Some((a, b)) = self.segment(rect, visible, layout, line.bus0, line.bus1) else {
                continue;
            };
            let (a, b) = taps.place(a, b, line.bus0, line.bus1, Circuit::Line(e), self.bar_half());
            let width = 0.8 + 2.4 * (line.s_nom.abs() / max_s_nom).sqrt() as f32;
            // Voltage where the network states it, kind where it does not.
            //
            // Both are identity rather than state: what this corridor *is*, not
            // what it is doing. State stays on the lightness axis below, and
            // the alarm hues stay on the buses -- a red busbar and a 220 kV
            // corridor are different shapes in different places, and the one
            // that means trouble is also ringed.
            let color = if line.is_transport() {
                TRANSPORT_COLOR
            } else if by_voltage {
                voltage_color(line_kv(net, line))
            } else {
                AC_COLOR
            };
            // Loading brightens the corridor, the same way price brightens a
            // busbar. An idle line is still drawn -- it is part of the network
            // whether or not it carried anything -- but it sits back.
            let use_ = overlay.loading.get(e).copied().unwrap_or(f64::NAN);
            let color = if use_.is_nan() {
                color
            } else {
                lerp_color(
                    color.gamma_multiply(0.55),
                    crate::theme::INK_STRONG,
                    use_.clamp(0.0, 1.0) as f32,
                )
            };
            let path = self.tapped(a, b);
            consider(Circuit::Line(e), &path);
            painter.add(eframe::egui::Shape::line(
                path.clone(),
                Stroke::new(width, color),
            ));

            // Which way the power is going.
            //
            // A static chevron, not a moving one. The experiment on this is
            // unambiguous: encoding flow magnitude as animation speed showed no
            // clear advantage and raised measured workload, with the moving
            // arrows themselves named as the likely cause. Direction is a fact
            // that does not change between frames, so it is drawn as one.
            //
            // Magnitude is already carried by the corridor's brightness, so the
            // chevron carries direction alone -- one channel, one meaning.
            if let Some(&f) = overlay.flow.get(e)
                && f.abs() > 1e-6
            {
                chevron(painter, &path, f > 0.0, width, color);
            }
            // A corridor at its rating is where the price separation across
            // this network comes from, so it is marked with the tick a diagram
            // uses for a constraint rather than with a fourth colour.
            if use_ >= BINDING {
                self.binding_tick(painter, &path, width);
            }
        }

        let max_p_nom = net
            .links
            .iter()
            .map(|l| l.p_nom.abs())
            .fold(0.0_f64, f64::max)
            .max(1.0);

        for (e, link) in net.links.iter().enumerate() {
            let Some((a, b)) = self.segment(rect, visible, layout, link.bus0, link.bus1) else {
                continue;
            };
            let (a, b) = taps.place(a, b, link.bus0, link.bus1, Circuit::Link(e), self.bar_half());
            let width = 0.8 + 2.0 * (link.p_nom.abs() / max_p_nom).sqrt() as f32;
            let path = self.tapped(a, b);
            consider(Circuit::Link(e), &path);
            painter.add(eframe::egui::Shape::line(path, Stroke::new(width, LINK_COLOR)));
        }

        near.map(|(c, at, _)| (c, at))
    }

    /// Two short strokes across a corridor at its limit.
    ///
    /// The tick is borrowed from the way a schematic marks a constraint, and it
    /// is a shape rather than a colour on purpose: the palette already spends
    /// its hues on state, and a binding constraint is not a fault. It is the
    /// model doing exactly what it was asked to.
    fn binding_tick(&self, painter: &eframe::egui::Painter, path: &[Pos2], width: f32) {
        // Placed on the longest leg rather than at the path midpoint, because
        // the midpoint of a tapped route can land on a corner, where a
        // perpendicular tick has no single direction to be perpendicular to.
        let Some((a, b)) = path
            .windows(2)
            .map(|w| (w[0], w[1]))
            .max_by(|x, y| {
                (x.0 - x.1)
                    .length()
                    .total_cmp(&(y.0 - y.1).length())
            })
        else {
            return;
        };
        let along = (b - a).normalized();
        let across = vec2(-along.y, along.x) * (width + 3.0);
        let mid = a + (b - a) * 0.5;
        let stroke = Stroke::new(1.2, crate::theme::INK_STRONG);
        for offset in [-2.5, 2.5] {
            let c = mid + along * offset;
            painter.line_segment([c - across, c + across], stroke);
        }
    }

    /// Screen endpoints, or `None` when the edge cannot be seen or its endpoints
    /// are out of range.
    ///
    /// Bus references are indices into a `Vec`, and `Network::validate` is what
    /// checks they are in range — the view may be handed something that was
    /// never validated, so it declines to index rather than panicking on a file
    /// somebody dragged in.
    /// A circuit routed as a tap onto two busbars, rather than as a chord
    /// between two points.
    ///
    /// This is the difference between a diagram of a power system and a graph
    /// with bars for vertices. On a real single-line diagram nothing meets a
    /// busbar at an angle: a circuit runs, then turns and drops onto the bar
    /// perpendicular. The bar is a conductor with physical extent, and a
    /// connection lands *on* it — the right angle is what says so.
    ///
    /// So each end gets a short vertical stub, and the diagonal runs between
    /// the stub ends. Three segments instead of one, and the whole picture
    /// stops reading as a node-link graph.
    ///
    /// The stub goes up from the lower bus and down from the higher one, so a
    /// circuit always leaves a bar on the side facing its far end and never
    /// crosses back over its own busbar.
    /// Half the drawn length of a busbar, in screen points.
    ///
    /// One definition, because the bar, its taps, its label offset and its hit
    /// target all have to agree, and they were each computing it separately.
    pub(crate) fn bar_half(&self) -> f32 {
        (self.zoom * 0.022).clamp(6.0, 30.0)
    }

    fn tapped(&self, a: Pos2, b: Pos2) -> Vec<Pos2> {
        // Tied to the bar's own size rather than to zoom directly, so the tap
        // always reads as a drop onto *that* bar. Long enough to be
        // unmistakably a right angle: a three-pixel stub is just a kinked line.
        let stub = (self.bar_half() * 1.15).clamp(9.0, 34.0);
        let dir = if b.y >= a.y { 1.0 } else { -1.0 };
        let a_stub = pos2(a.x, a.y + stub * dir);
        let b_stub = pos2(b.x, b.y - stub * dir);
        vec![a, a_stub, b_stub, b]
    }

    fn segment(
        &self,
        rect: Rect,
        visible: Rect,
        layout: &[Pos2],
        i: usize,
        j: usize,
    ) -> Option<(Pos2, Pos2)> {
        let p = *layout.get(i)?;
        let q = *layout.get(j)?;
        // Bounding-box overlap rather than a real segment-rectangle test: it
        // admits a few segments that miss the pane, which the clip rect then
        // discards, and it costs four comparisons instead of a clip routine.
        if !visible.intersects(Rect::from_two_pos(p, q)) {
            return None;
        }
        Some((self.screen_of(rect, p), self.screen_of(rect, q)))
    }

    #[allow(clippy::too_many_arguments)]
    /// The price ramp, spelled out.
    ///
    /// An unlabelled colour ramp is decoration: the reader can see that two
    /// buses differ but not by how much, or even in which direction. The ends
    /// carry numbers so the encoding is readable without being explained.
    fn draw_key(
        &self,
        painter: &eframe::egui::Painter,
        rect: Rect,
        net: &Network,
        prices: &[f64],
    ) {
        use crate::theme;

        let font = eframe::egui::FontId::proportional(10.0);
        let mut y = rect.bottom() - 12.0;

        // Carriers present, above the corridor kinds. Generators are coloured
        // by fuel on the canvas now, and a colour with no key is a colour a
        // reader has to guess at -- which is exactly the failure the ENTSO-E
        // map ships, where two voltage bands share a class and the legend never
        // says so.
        let mut fuels: Vec<(&'static str, Color32)> = Vec::new();
        for g in &net.generators {
            if let Some((family, c)) = crate::theme::carrier_color(&g.carrier)
                && !fuels.iter().any(|(seen, _)| *seen == family)
            {
                fuels.push((family, c));
            }
        }
        // Alphabetical, so the key does not reshuffle when a file is reordered.
        fuels.sort_by(|a, b| a.0.cmp(b.0));

        // Corridor kinds, and only the ones this network has. A legend row for
        // a category with no members teaches a distinction the reader will
        // never see, and on a single-area MATPOWER case that is two of the
        // three -- most of the legend would be about nothing.
        //
        // When the network states voltages, the AC row is replaced by the bands
        // actually present. Built from `VOLTAGE_SCALE` itself rather than from a
        // parallel list of labels: ENTSO-E's grid map hardcodes its legend in
        // CSS separately from its style, and has shipped a bug for years where
        // two voltage bands share a class and cannot be told apart on screen.
        let by_voltage = voltages_are_stated(net);
        let mut kinds: Vec<(bool, Color32, String)> = Vec::new();
        if by_voltage {
            for (i, (_, color)) in VOLTAGE_SCALE.iter().enumerate() {
                // Labelled with the voltages this network actually runs at, not
                // with the band's lower bound. A 380 kV network whose legend
                // reads "310 kV" is a legend describing the palette rather than
                // the grid, and a reader checking it against what they know
                // about their own system finds a number that is simply not in
                // it.
                let mut present: Vec<i64> = net
                    .lines
                    .iter()
                    .filter(|l| !l.is_transport())
                    .map(|l| line_kv(net, l))
                    .filter(|kv| voltage_color(*kv) == *color)
                    .map(|kv| kv.round() as i64)
                    .collect();
                present.sort_unstable();
                present.dedup();
                if present.is_empty() {
                    continue;
                }
                let label = if i == 0 {
                    "unknown kV".to_string()
                } else {
                    // Several levels can land in one band -- 132 and 150 kV do,
                    // and both are common in the same network -- so the row
                    // names all of them rather than picking one.
                    format!(
                        "{} kV",
                        present
                            .iter()
                            .map(|kv| kv.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                kinds.push((true, *color, label));
            }
        } else {
            kinds.push((
                net.lines.iter().any(|l| !l.is_transport()),
                AC_COLOR,
                "ac line".into(),
            ));
        }
        kinds.push((
            net.lines.iter().any(|l| l.is_transport()),
            TRANSPORT_COLOR,
            "transport".into(),
        ));
        kinds.push((!net.links.is_empty(), LINK_COLOR, "link".into()));
        for (name, color) in fuels {
            kinds.push((true, color, name.to_string()));
        }
        // Bottom up, so adding a kind pushes the stack away from the edge
        // rather than shifting every row already on screen.
        for (present, color, name) in kinds.into_iter().rev() {
            if !present {
                continue;
            }
            painter.line_segment(
                [pos2(rect.left() + 12.0, y), pos2(rect.left() + 30.0, y)],
                Stroke::new(2.0, color),
            );
            painter.text(
                pos2(rect.left() + 36.0, y),
                Align2::LEFT_CENTER,
                &name,
                font.clone(),
                theme::INK_DIM,
            );
            y -= 14.0;
        }
        let floor = y - 2.0;

        if prices.is_empty() {
            return;
        }
        let (lo, hi) = prices
            .iter()
            .fold((f64::MAX, f64::MIN), |(l, h), &v| (l.min(v), h.max(v)));

        if hi - lo < 1e-9 {
            // A network with no congestion has one price everywhere, and a ramp
            // across a range of zero would imply variation that is not there.
            painter.text(
                pos2(rect.left() + 12.0, floor),
                Align2::LEFT_BOTTOM,
                format!("{lo:.2} /MWh at every bus"),
                font,
                theme::INK_DIM,
            );
            return;
        }

        // Stacked upward from whatever the corridor legend left free: caption,
        // then the ramp, then its end values. The numbers go under the ramp so
        // they sit against the swatch they label rather than across a gap from
        // it.
        // Above the price ramp: corridor loading, on the same lightness axis.
        //
        // Two encodings share that axis -- price on the busbars, utilisation on
        // the corridors -- and until now only one of them was explained. A
        // reader could reasonably conclude the bright lines were expensive.
        let key_w = 108.0;
        let load_bar = Rect::from_min_size(pos2(rect.left() + 12.0, floor - 56.0), vec2(key_w, 5.0));
        let steps = 24;
        for i in 0..steps {
            let t = i as f32 / (steps - 1) as f32;
            let cell = Rect::from_min_size(
                load_bar.min + vec2(load_bar.width() * t, 0.0),
                vec2(load_bar.width() / steps as f32 + 1.0, load_bar.height()),
            );
            painter.rect_filled(
                cell,
                0.0,
                lerp_color(AC_COLOR.gamma_multiply(0.55), theme::INK_STRONG, t),
            );
        }
        painter.text(
            load_bar.left_top() + vec2(0.0, -3.0),
            Align2::LEFT_BOTTOM,
            "corridor loading",
            font.clone(),
            theme::INK_DIM,
        );
        painter.text(
            load_bar.left_bottom() + vec2(0.0, 2.0),
            Align2::LEFT_TOP,
            "idle",
            font.clone(),
            theme::INK_DIM,
        );
        painter.text(
            load_bar.right_bottom() + vec2(0.0, 2.0),
            Align2::RIGHT_TOP,
            "at rating",
            font.clone(),
            theme::INK_DIM,
        );

        let bar = Rect::from_min_size(pos2(rect.left() + 12.0, floor - 22.0), vec2(key_w, 5.0));
        // Painted in steps rather than as a gradient mesh: at this size the
        // banding is invisible, and it keeps the paint list to plain rects.
        let steps = 24;
        for i in 0..steps {
            let t = i as f32 / (steps - 1) as f32;
            let cell = Rect::from_min_size(
                bar.min + vec2(bar.width() * t, 0.0),
                vec2(bar.width() / steps as f32 + 1.0, bar.height()),
            );
            painter.rect_filled(cell, 0.0, lerp_color(theme::INK_DIM, theme::INK_STRONG, t));
        }

        painter.text(
            bar.left_bottom() + vec2(0.0, 2.0),
            Align2::LEFT_TOP,
            format!("{lo:.0}"),
            font.clone(),
            theme::INK_DIM,
        );
        painter.text(
            bar.right_bottom() + vec2(0.0, 2.0),
            Align2::RIGHT_TOP,
            format!("{hi:.0} /MWh"),
            font.clone(),
            theme::INK_DIM,
        );
        painter.text(
            bar.left_top() + vec2(0.0, -3.0),
            Align2::LEFT_BOTTOM,
            "mean nodal price",
            font,
            theme::INK_DIM,
        );
    }

    fn draw_buses(
        &mut self,
        ui: &Ui,
        painter: &eframe::egui::Painter,
        net: &Network,
        layout: &[Pos2],
        overlay: Overlay<'_>,
        response: &eframe::egui::Response,
    ) -> bool {
        // Whether a corridor already claimed this click. Read from the field
        // rather than passed, because it is state this frame produced a moment
        // ago and threading it back in as an argument was the eighth one.
        let on_circuit = self.circuit_under_pointer;
        let rect = response.rect;
        let visible = self.visible(rect);
        let Overlay {
            peak_shed,
            prices,
            loading: _,
            flow: _,
        } = overlay;
        // A bus is drawn as a bar, not as a dot.
        //
        // This is the one primitive that decides whether the canvas reads as a
        // power system or as a generic node graph, and it is not a stylistic
        // preference: in every single-line diagram ever drawn, a busbar is a
        // bar. Circles say "vertex". Bars say "busbar", and an engineer reads
        // the second without being told.
        //
        let (half, thickness) = self.bar_metrics();
        // Symbols scale with the bar but stop growing sooner. A generator ring
        // as tall as the busbar is wide stops reading as a machine hanging off
        // a conductor and starts reading as a lollipop.
        let glyph = (half * 0.34).clamp(2.5, 9.0);
        let pointer = response.hover_pos();

        // What is attached to each bus, computed once. Asking
        // `generators.iter().any(...)` inside the bus loop is quadratic, which
        // is invisible at fourteen buses and is not at thirteen thousand.
        let mut gens = vec![0usize; net.buses.len()];
        let mut loads = vec![0usize; net.buses.len()];
        for g in &net.generators {
            if let Some(c) = gens.get_mut(g.bus) {
                *c += 1;
            }
        }
        for l in &net.loads {
            if let Some(c) = loads.get_mut(l.bus) {
                *c += 1;
            }
        }
        let has_gen: Vec<bool> = gens.iter().map(|&c| c > 0).collect();

        // Carrier colours per bus, in the order the fan draws them. Collected
        // once rather than searched per bus per frame, which is quadratic and
        // invisible at eight buses and is not at thirteen thousand.
        let mut at_bus: Vec<Vec<Option<Color32>>> = vec![Vec::new(); net.buses.len()];
        for g in &net.generators {
            if let Some(v) = at_bus.get_mut(g.bus) {
                v.push(crate::theme::carrier_color(&g.carrier).map(|(_, c)| c));
            }
        }
        let has_load: Vec<bool> = loads.iter().map(|&c| c > 0).collect();

        // The threshold for drawing individual machines rather than one symbol
        // standing for all of them. Tied to the busbar's on-screen half-width,
        // because that is literally how much room there is to fan them across.
        let detail = half >= 16.0;

        // **And the threshold below which a bar stops being a bar.**
        //
        // A busbar is drawn as a bar for a good reason, stated above, and that
        // reason has a precondition: the bar has to be legible *as* a bar. On the
        // 7,893-bus European network at continental zoom every bus is a 12-point
        // bar with about two points between it and its neighbour, and eight
        // thousand of them tile into a solid white mat. Nothing about that reads
        // as a power system; the shape that says "busbar" only says it when there
        // is space around it.
        //
        // So below that point each bus becomes a point. This is semantic zoom, the
        // same rule the symbol count above follows: what to draw is a question
        // about how much room there is, not about the model.
        //
        // Spacing is estimated from the visible count over the visible area rather
        // than measured pairwise -- a nearest-neighbour distance over eight
        // thousand buses every frame is the one thing this loop cannot afford, and
        // the estimate is within a factor of two of the truth for anything that is
        // not a single dense cluster.
        let shown = layout
            .iter()
            .enumerate()
            .filter(|(b, p)| visible.contains(**p) && self.shows(net, *b))
            .count()
            .max(1);
        let spacing = (rect.area() / shown as f32).sqrt();
        let crowded = spacing < half * 2.0;
        // Never smaller than a pixel, or the network vanishes instead of
        // simplifying.
        let dot = (spacing * 0.22).clamp(1.0, thickness);
        let mut has_store = vec![false; net.buses.len()];
        for st in &net.storage {
            if let Some(f) = has_store.get_mut(st.bus) {
                *f = true;
            }
        }

        // The spread across the network, so the ramp uses its whole range. An
        // absolute scale would render a healthy system as one flat colour,
        // since prices in a network without congestion are all nearly equal --
        // and the interesting thing about nodal prices is precisely where they
        // stop being equal.
        let price_span = (!prices.is_empty()).then(|| {
            prices
                .iter()
                .fold((f64::MAX, f64::MIN), |(l, h), &v| (l.min(v), h.max(v)))
        });

        let mut best: Option<(usize, f32)> = None;

        for (b, _bus) in net.buses.iter().enumerate() {
            if !self.shows(net, b) {
                continue;
            }
            let Some(&p) = layout.get(b) else { continue };
            if !visible.expand(half / self.zoom).contains(p) {
                continue;
            }
            let s = self.screen_of(rect, p);
            let shed = peak_shed.get(b).copied().unwrap_or(0.0) > 0.0;

            // Neutral unless it has something to report. Colouring every bus by
            // country was decorative: on a single-area case it painted all of
            // them the same arbitrary hue, which is a colour carrying no
            // information on a screen where colour is supposed to mean
            // something.
            let ink = if shed {
                crate::theme::TRIP
            } else {
                crate::theme::INK
            };

            // Price raises the bar's lightness rather than introducing a hue.
            //
            // Deliberate: hue in this interface means state -- amber is stale,
            // red is unserved energy -- and a price heatmap in a fourth colour
            // would compete with those for the channel that carries meaning.
            // Lightness is the free axis, and it is also the one that survives
            // every form of colour vision deficiency, which a blue-to-red ramp
            // does not. Expensive buses glow; cheap ones recede.
            let ink = match price_span.zip(prices.get(b)) {
                Some(((lo, hi), &v)) if !shed => {
                    let t = ((v - lo) / (hi - lo).max(1e-9)).clamp(0.0, 1.0) as f32;
                    lerp_color(crate::theme::INK_DIM, crate::theme::INK_STRONG, t)
                }
                _ => ink,
            };

            // A short bar rather than a circle even when crowded: at two points
            // across, a rectangle and a circle are the same handful of pixels, and
            // the rectangle keeps the primitive and the picking geometry the same
            // shape at every zoom.
            let bar = if crowded {
                Rect::from_center_size(s, vec2(dot * 2.0, dot))
            } else {
                Rect::from_center_size(s, vec2(half * 2.0, thickness))
            };
            painter.rect_filled(bar, 0.0, ink);

            // Injection above the bar, withdrawal below — the convention a
            // one-line diagram uses, and it means a glance tells you where
            // power enters and where it leaves without reading a legend.
            //
            // How *many* symbols is a question of zoom, not of the model. This
            // is semantic zoom, which the control-room study named as one of
            // three critical needs: zoomed out, one generator symbol says
            // "generation here" and eight say "unreadable mat"; zoomed in, the
            // count is the useful part, because a substation with six machines
            // is a different place from one with one.
            if has_gen[b] && !crowded {
                // Machines take their carrier's colour, where the file named
                // one. A generator symbol is its own shape class, so this
                // collides with nothing: hue on a corridor means voltage, hue
                // on a busbar means alarm state, and hue on a ring with a sine
                // in it means what is burning. Falls back to the bus ink when
                // the carrier is unknown, rather than inventing a hue.
                let n = if detail { gens[b].min(4) } else { 1 };
                for k in 0..n {
                    let dx = fan(k, n, glyph);
                    let c = at_bus[b]
                        .get(k)
                        .copied()
                        .flatten()
                        .unwrap_or(ink);
                    generator(painter, s + vec2(dx, -thickness * 0.5), glyph, c);
                }
            }
            if has_store[b] && !crowded {
                let x = if has_gen[b] { glyph * 2.6 } else { 0.0 };
                storage(painter, s + vec2(x, -thickness * 0.5), glyph, ink);
            }
            if has_load[b] && !crowded {
                // Loads keep the bus ink rather than taking a carrier colour.
                // A load has no fuel -- it is demand, and what it is *for* is a
                // different question the model does not answer. Colouring it by
                // the bus's carrier would say "electricity" on every load in an
                // electricity network, which is a colour carrying no
                // information.
                let n = if detail { loads[b].min(4) } else { 1 };
                for k in 0..n {
                    let dx = fan(k, n, glyph);
                    load(painter, s + vec2(dx, thickness * 0.5), glyph, ink);
                }
            }

            // Where the system failed, which the domain model treats as the
            // useful half of an infeasible answer rather than a footnote.
            if shed {
                painter.rect_stroke(
                    bar.expand(3.0),
                    1.0,
                    Stroke::new(1.5, crate::theme::TRIP),
                    eframe::egui::StrokeKind::Outside,
                );
            }

            if let Some(ptr) = pointer {
                let d = s.distance(ptr);
                if d <= PICK_RADIUS.max(half) && best.is_none_or(|(_, bd)| d < bd) {
                    best = Some((b, d));
                }
            }
        }

        // Names, decluttered.
        //
        // The old rule was "label everything under two hundred buses", which is
        // a claim about the file rather than about the picture: a hundred and
        // eighteen buses zoomed out is a solid mat of overlapping text, and one
        // bus zoomed in has room for a paragraph. What decides legibility is
        // whether two labels land on top of each other, so that is what is
        // tested.
        //
        // This is what every map renderer calls declutter, and the ordering is
        // the part that matters: labels are placed in a fixed sequence, so the
        // same bus wins the same collision on every frame. Sorting by anything
        // that changes with the camera would make names flicker in and out as
        // the reader pans, which is worse than losing them consistently.
        // **And the order is by what the bus is, not by where it sits in the file.**
        // File order is arbitrary, so on a dense case the labels that survived
        // were whichever happened to be listed first -- 162 buses came out as a
        // wall of text naming no particular thing. Significance is a property of
        // the network rather than of the camera, so it keeps the no-flicker
        // property above: two buses hold the same relative priority however the
        // reader pans.
        let order = self.label_order(net, layout, visible);

        // Every visible busbar is claimed before any label is placed, so a name
        // never lands on top of a *different* substation. Without this the
        // densest cases put labels across their neighbours' bars, which reads as
        // the wrong name attached to the wrong thing -- worse than no name.
        let mut placed: Vec<Rect> = order
            .iter()
            .filter_map(|b| layout.get(*b))
            .map(|p| {
                Rect::from_center_size(self.screen_of(rect, *p), vec2(half * 2.0, thickness))
                    .expand(2.0)
            })
            .collect();
        let bars = placed.len();

        for b in order {
            let bus = &net.buses[b];
            let Some(&p) = layout.get(b) else { continue };
            let s = self.screen_of(rect, p);
            let galley = painter.layout_no_wrap(
                bus.name.clone(),
                FontId::proportional(10.0),
                crate::theme::INK_DIM,
            );

            // The selected bus is labelled whatever else is in the way. It is
            // the one the reader asked about, and losing its name to a
            // collision with a neighbour they did not ask about is the one
            // failure this rule must not have.
            let mine = self.selected == Some(b);
            // Its own bar is skipped, or every label would collide with the bus
            // it belongs to.
            let own = Rect::from_center_size(s, vec2(half * 2.0, thickness)).expand(2.0);
            let free = |placed: &[Rect], box_: Rect| {
                placed
                    .iter()
                    .enumerate()
                    .all(|(i, r)| !r.intersects(box_) || (i < bars && *r == own))
            };

            // Below first, then above. **Two candidate positions rather than one**,
            // which is what a map renderer does and for the reason a dense case
            // makes obvious: with only one, a busbar three rows down blocks a name
            // that had a perfectly good gap over its own head. Reserving the bars
            // took the 118-bus case from a wall of text to twelve names, and the
            // second position brings back the ones that were never really in
            // anybody's way.
            //
            // Below is tried first so the common case stays where a reader has
            // learned to look, and the order is fixed so a label does not swap
            // sides as neighbours come and go.
            let offsets = [
                vec2(0.0, thickness * 0.5 + 19.0),
                vec2(0.0, -(thickness * 0.5 + 19.0)),
            ];
            // Enough margin that two labels read as two things. A gap of a pixel
            // is not a gap at ten point.
            let candidate = |off: Vec2| {
                Rect::from_center_size(s + off, galley.size()).expand2(vec2(5.0, 2.0))
            };
            let Some(box_) = offsets
                .into_iter()
                .map(candidate)
                .find(|b| mine || free(&placed, *b))
            else {
                continue;
            };
            placed.push(box_);

            // A knocked-out background rather than a halo stroke. Edges pass
            // behind labels constantly in a meshed network, and text with a
            // line through it is unreadable in a way no amount of contrast
            // fixes.
            painter.rect_filled(box_, 2.0, crate::theme::SLATE_WORK);
            painter.galley(
                box_.min + vec2(5.0, 2.0),
                galley,
                if mine {
                    crate::theme::INK_STRONG
                } else {
                    crate::theme::INK_DIM
                },
            );
        }

        // A click takes the bus under the pointer, or clears when there is
        // none. Clearing on empty canvas matters: without it the only way to
        // deselect is to select something else, and there is no gesture for
        // "nothing".
        if response.clicked() {
            if let Some((b, _)) = best {
                self.selected = Some(b);
                self.selected_line = None;
            } else if !on_circuit {
                // Clearing on empty canvas clears *both*, or a stale corridor
                // stays in the panel describing something the reader has
                // visibly deselected. Guarded on `on_circuit` because a click
                // that landed on a corridor reaches here too, having already
                // selected it -- without the guard it would be cleared in the
                // same frame it was chosen.
                self.selected = None;
                self.selected_line = None;
            }
        }
        // Recorded for the cursor: over a bus the pointer should promise a
        // click, not a pan.
        self.hovered = best.map(|(b, _)| b);
        if self.hovered.is_some() && !response.dragged() {
            ui.ctx()
                .set_cursor_icon(eframe::egui::CursorIcon::PointingHand);
        }

        // The selection is drawn whether or not the pointer is still on it, and
        // in a different shape from the hover mark rather than a slightly
        // larger version of it. Two rings a pixel apart in size are one ring as
        // far as a reader is concerned: with the pointer anywhere on the canvas
        // there was no way to tell which bus the panel was describing.
        //
        // Corner brackets, because they are what a viewfinder uses to say "this
        // one" -- they leave the object itself uncovered and they do not close,
        // so they cannot be mistaken for a boundary the object has.
        if let Some(sel) = self.selected.filter(|&b| b < net.buses.len())
            && let Some(&p) = layout.get(sel)
            && visible.contains(p)
        {
            let s = self.screen_of(rect, p);
            let bar = Rect::from_center_size(s, vec2(half * 2.0, thickness)).expand(6.0);
            brackets(painter, bar, (half * 0.45).clamp(4.0, 10.0));
        }

        let Some((picked, _)) = best else {
            return false;
        };
        let bus = &net.buses[picked];
        let s = self.screen_of(rect, layout[picked]);
        let (half, thickness) = self.bar_metrics();
        // Hover is a closed outline and one pixel thin: present, but plainly a
        // lighter mark than the brackets that say what is selected.
        painter.rect_stroke(
            Rect::from_center_size(s, vec2(half * 2.0, thickness)).expand(3.0),
            1.0,
            Stroke::new(1.0, crate::theme::INK),
            eframe::egui::StrokeKind::Outside,
        );

        let label = format!("{} · {} · {}", bus.name, bus.country, bus.carrier);
        callout(ui, painter, s + vec2(half + 10.0, 0.0), label);
        true
    }

    /// What a corridor is rated for and what it carried.
    ///
    /// The rating alone is on the diagram already, as the stroke width. The
    /// number worth surfacing on hover is the one the picture can only
    /// approximate: how close to that rating the solve actually pushed it.
    fn circuit_readout(
        &self,
        ui: &Ui,
        painter: &eframe::egui::Painter,
        net: &Network,
        loading: &[f64],
        c: Circuit,
        at: Pos2,
    ) {
        let label = match c {
            Circuit::Line(e) => {
                let Some(line) = net.lines.get(e) else { return };
                let used = loading.get(e).copied().unwrap_or(f64::NAN);
                let kind = if line.is_transport() { "transport" } else { "ac" };
                if used.is_nan() {
                    format!("{} · {kind} · {:.0} MW", line.name, line.s_nom)
                } else {
                    format!(
                        "{} · {kind} · {:.0} of {:.0} MW · {:.0}%",
                        line.name,
                        used * line.s_nom,
                        line.s_nom,
                        used * 100.0,
                    )
                }
            }
            Circuit::Link(e) => {
                let Some(link) = net.links.get(e) else { return };
                format!("{} · link · {:.0} MW", link.name, link.p_nom)
            }
        };
        painter.circle_filled(at, 2.5, crate::theme::INK_STRONG);
        callout(ui, painter, at + vec2(10.0, 6.0), label);
    }

    /// Visible buses, most worth naming first.
    ///
    /// Declutter needs an order, and the order decides which names a reader gets
    /// when there is not room for all of them. Three properties are wanted, and
    /// they pull against each other:
    ///
    /// - **Stable under panning.** Anything camera-dependent makes names blink in
    ///   and out as the reader moves, which is worse than losing them
    ///   consistently. So the rank comes from the network, never from the screen.
    /// - **Stable under a re-ordered file.** Two readers with the same network
    ///   written out in a different order should see the same labels, so file
    ///   position is only ever the last tie-break.
    /// - **About the thing being looked at.** A bus with a power station on it and
    ///   six circuits into it is what a reader orients by; a load-only stub at the
    ///   end of a spur is not, however early it appears in the file.
    ///
    /// Generation is weighted above demand deliberately. Both matter, but a bus
    /// with plant on it is where a dispatch decision happens, and on the IEEE
    /// cases the loads outnumber the generators three to one — ranking them
    /// equally names the spurs and loses the stations.
    fn label_order(&self, net: &Network, layout: &[Pos2], visible: Rect) -> Vec<usize> {
        let mut weight = vec![0.0f64; net.buses.len()];
        for g in &net.generators {
            if let Some(w) = weight.get_mut(g.bus) {
                *w += g.p_nom * 2.0;
            }
        }
        for l in &net.loads {
            if let Some(w) = weight.get_mut(l.bus) {
                *w += l.p_set.abs();
            }
        }
        // Degree, scaled so it breaks ties among buses of similar size rather
        // than competing with capacity. A junction with no plant and no load is
        // still a landmark, which is why it is added rather than only used as a
        // tie-break.
        for line in &net.lines {
            for end in [line.bus0, line.bus1] {
                if let Some(w) = weight.get_mut(end) {
                    *w += 25.0;
                }
            }
        }

        let mut order: Vec<usize> = (0..net.buses.len())
            .filter(|b| {
                self.shows(net, *b) && layout.get(*b).is_some_and(|p| visible.contains(*p))
            })
            .collect();
        order.sort_by(|a, b| {
            // The selected bus first, so it is never the one that loses a
            // collision -- the reader asked about it by name.
            let picked = |i: &usize| self.selected == Some(*i);
            picked(b)
                .cmp(&picked(a))
                .then_with(|| {
                    weight[*b]
                        .partial_cmp(&weight[*a])
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                // Name before index, so a re-exported file with the same buses in
                // a different order labels the same ones.
                .then_with(|| net.buses[*a].name.cmp(&net.buses[*b].name))
                .then(a.cmp(b))
        });
        order
    }

    /// Country names on the map, and the one the reader clicked.
    ///
    /// **Asked for because a list of names in a panel is the wrong place for them.**
    /// A reader looking at a continental network wants to know which country they
    /// are pointing at, and a name in a side panel makes them work that out from
    /// bus counts. On the map the answer is where the question is.
    ///
    /// Clicking one isolates it, and clicking it again brings the rest back — so
    /// the countries *are* the regions, switchable in place, and the panel list is
    /// the index rather than the only route.
    ///
    /// Drawn under the network and above the coastline, in the dimmest ink that is
    /// still readable at size. A country label competing with a busbar for
    /// attention would be the background camouflaging the foreground, which is the
    /// finding this whole basemap is built around.
    fn draw_regions(
        &self,
        ui: &mut eframe::egui::Ui,
        painter: &eframe::egui::Painter,
        rect: Rect,
        net: &Network,
        layout: &[Pos2],
        frame: crate::layout::Frame,
    ) -> Option<String> {
        let extents = Self::region_extents(net, layout, frame);
        if extents.len() < 2 {
            return None;
        }

        let mut hit = None;
        // Placed largest-first so a big country wins a collision with a small one
        // it encloses, rather than the alphabetical order the extents come in.
        let mut order: Vec<usize> = (0..extents.len()).collect();
        order.sort_by(|a, b| {
            extents[*b]
                .1
                .area()
                .partial_cmp(&extents[*a].1.area())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut taken: Vec<Rect> = Vec::new();

        for i in order {
            let (code, box_, centre) = &extents[i];
            let on_screen = Rect::from_min_max(
                self.screen_of(rect, frame.apply(box_.min)),
                self.screen_of(rect, frame.apply(box_.max)),
            );
            // Sized to the country, so a label reads as belonging to the area it
            // covers rather than floating at a fixed size over anything.
            let size = (on_screen.width().min(on_screen.height() * 2.0) * 0.11).clamp(9.0, 34.0);
            // Skip a country with no room for its own name. On a continental view
            // that is Luxembourg and Malta; zoom in and they appear.
            if on_screen.width() < size * 2.5 || !rect.intersects(on_screen) {
                continue;
            }
            // **And skip a country the reader is inside.** A country name is for
            // orientation at a scale where individual substations are not the
            // subject; once the country is wider than the pane, the reader is
            // looking at part of it and the word is in the way of the thing they
            // came for. Dropping it here is also what lets the label take a click
            // outright below, because it is only ever present at a zoom where a
            // busbar is not what is being aimed at.
            if on_screen.width() > rect.width() * 1.6 {
                continue;
            }

            let hidden = self.hidden.contains(code);
            // The name where the gazetteer can vouch for the code against these
            // buses, and the code itself where it cannot -- never a guess.
            let pad = (box_.size() * 0.25).max(Vec2::splat(0.02));
            let label = self
                .places
                .country_named_within(code, box_.expand2(pad))
                .unwrap_or(code);

            let at = self.screen_of(rect, frame.apply(*centre));
            // Letter-spaced, which is what says "region" rather than "thing".
            // Every atlas does it and it is the cheapest way to keep a large word
            // from reading as a label attached to whatever it happens to overlap.
            let spaced: String = label
                .to_uppercase()
                .chars()
                .flat_map(|c| [c, ' '])
                .collect();
            let galley = painter.layout_no_wrap(
                spaced.trim_end().to_string(),
                FontId::proportional(size),
                crate::theme::INK_DIM,
            );
            let where_ = Rect::from_center_size(at, galley.size()).expand(4.0);
            if !rect.contains(where_.center()) || taken.iter().any(|t| t.intersects(where_)) {
                continue;
            }
            taken.push(where_);

            // **A real interaction, not a look at the raw pointer state.** Reading
            // `primary_clicked` inside a draw call reports the same click on every
            // layout pass egui runs for the frame, so an action derived from the
            // current state applies twice and undoes itself. A `Response` is
            // delivered once however many passes there were.
            //
            // The label takes the click outright, rather than yielding to a busbar
            // under the pointer. Two attempts at yielding both failed, and in
            // opposite directions: requiring an empty spot made the names
            // unclickable across central Europe, where every pointer position is
            // within picking distance of some bus; conditioning it on the view
            // being crowded then broke the way back out, because isolating one
            // country *un*-crowds the view and the name stopped answering. The rule
            // that works is the one the skip above earns -- a country name only
            // exists at a zoom where the reader is looking at the country rather
            // than inside it, and at that zoom a two-pixel dot is not what they are
            // aiming at.
            let response = ui.interact(
                where_,
                eframe::egui::Id::new(("region", code.as_str())),
                eframe::egui::Sense::click(),
            );
            let over = response.hovered();
            if response.clicked() {
                hit = Some(code.clone());
            }
            // A hidden country keeps its name and loses everything else, which is
            // what makes it possible to switch back on. Struck through, because a
            // dimmer label alone reads as a country that is merely far away.
            let ink = if over {
                crate::theme::INK_STRONG
            } else if hidden {
                crate::theme::INK_DIM.gamma_multiply(0.5)
            } else {
                crate::theme::INK_DIM.gamma_multiply(0.42)
            };
            // A halo, not a plate. A filled plate behind a word this large would
            // punch a hole in the network under it, which is the one thing a label
            // drawn on top must not do.
            let origin = where_.min + Vec2::splat(4.0);
            for d in [
                Vec2::new(1.5, 0.0),
                Vec2::new(-1.5, 0.0),
                Vec2::new(0.0, 1.5),
                Vec2::new(0.0, -1.5),
            ] {
                painter.galley(
                    origin + d,
                    galley.clone(),
                    crate::theme::SLATE_DEEP.gamma_multiply(0.75),
                );
            }
            painter.galley(origin, galley, ink);
            if hidden {
                let y = where_.center().y;
                painter.line_segment(
                    [pos2(where_.left() + 4.0, y), pos2(where_.right() - 4.0, y)],
                    Stroke::new(1.0, ink),
                );
            }
            if over {
                ui.ctx()
                    .set_cursor_icon(eframe::egui::CursorIcon::PointingHand);
                response.on_hover_text(if self.only_shown(net, code) {
                    format!("{label} only — click to show every country again")
                } else {
                    format!("click to show only {label}")
                });
            }
        }
        hit
    }

    /// Whether a bus is drawn at all.
    ///
    /// One predicate, used by the bars, the labels, the corridors and the density
    /// estimate that decides how much detail any of them get. If they disagreed the
    /// reader would see corridors ending in nothing, or a network thinned to a
    /// backbone because of buses that are not on screen.
    fn shows(&self, net: &Network, b: usize) -> bool {
        self.hidden.is_empty()
            || net
                .buses
                .get(b)
                .is_none_or(|bus| !self.hidden.contains(&bus.country))
    }

    /// Countries in this network, each with the box its buses occupy.
    ///
    /// In **Mercator**, so it can be asked of the gazetteer, which is the only
    /// thing that can turn a code into a name — and which must be asked about the
    /// right part of the world, because a file's codes are only mostly ISO. See
    /// `Places::country_named_within`.
    pub fn region_extents(
        net: &Network,
        layout: &[Pos2],
        frame: crate::layout::Frame,
    ) -> Vec<(String, Rect, Pos2)> {
        let mut boxes: std::collections::HashMap<&str, (Rect, Pos2, f32)> =
            std::collections::HashMap::new();
        for (b, bus) in net.buses.iter().enumerate() {
            let code = bus.country.trim();
            if code.is_empty() {
                continue;
            }
            let Some(&p) = layout.get(b) else { continue };
            let m = frame.invert(p);
            let e = boxes
                .entry(code)
                .or_insert((Rect::from_min_max(m, m), Pos2::ZERO, 0.0));
            e.0 = e.0.union(Rect::from_min_max(m, m));
            // A running sum, turned into a mean below. The *centroid* rather than
            // the box centre, because a country like Norway has its buses down one
            // edge of its bounding box and the centre of that box is the sea.
            e.1 += m.to_vec2();
            e.2 += 1.0;
        }
        let mut out: Vec<(String, Rect, Pos2)> = boxes
            .into_iter()
            .map(|(code, (b, sum, n))| {
                (code.to_string(), b, (sum.to_vec2() / n.max(1.0)).to_pos2())
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Countries in this network, with how many buses each has.
    ///
    /// Ordered by bus count, largest first: on a continental model the answer a
    /// reader is looking for is almost always one of the big grids, and an
    /// alphabetical list puts Albania above Germany.
    pub fn regions(net: &Network) -> Vec<(String, usize)> {
        let mut counted: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for bus in &net.buses {
            if !bus.country.trim().is_empty() {
                *counted.entry(bus.country.as_str()).or_default() += 1;
            }
        }
        let mut out: Vec<(String, usize)> = counted
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        // Count first, then the code, so the order is stable when two countries
        // have the same number of buses.
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }

    /// The gazetteer, for a caller that needs to name a country code.
    pub fn places(&self) -> &crate::places::Places {
        &self.places
    }

    pub fn region_hidden(&self, code: &str) -> bool {
        self.hidden.contains(code)
    }

    pub fn set_region_hidden(&mut self, code: &str, hide: bool) {
        if hide {
            self.hidden.insert(code.to_string());
        } else {
            self.hidden.remove(code);
        }
    }

    /// Show every country again.
    pub fn show_all_regions(&mut self) {
        self.hidden.clear();
    }

    /// Show only this country.
    ///
    /// The buses with no country code go too. They are kept visible by every other
    /// path through this filter -- switching one country off should not blank a
    /// bus that never claimed to be anywhere -- but *isolating* a country and
    /// leaving 1,195 uncoded nodes scattered over the rest of the continent is not
    /// isolation, it is a network with the recognisable parts removed.
    ///
    /// The empty string is never offered as a row, so this is the only way it can
    /// be set, and `show_all_regions` clears it with everything else.
    pub fn only_region(&mut self, net: &Network, code: &str) {
        self.hidden = Self::regions(net)
            .into_iter()
            .map(|(c, _)| c)
            .filter(|c| c != code)
            .collect();
        if !code.is_empty() {
            self.hidden.insert(String::new());
        }
    }

    pub fn any_region_hidden(&self) -> bool {
        !self.hidden.is_empty()
    }

    /// Whether this country is the only one being shown.
    ///
    /// Asked of the country rather than of the filter, which is the difference
    /// between a control that switches focus and one that flip-flops. The first
    /// version decided what to do from whether *anything* was hidden, so clicking
    /// France while Spain was isolated meant "something is hidden, show
    /// everything" -- and the reader had to click twice to move between two
    /// countries, with the whole continent flashing up in between.
    pub fn only_shown(&self, net: &Network, code: &str) -> bool {
        !self.hidden.is_empty()
            && !self.hidden.contains(code)
            && Self::regions(net)
                .iter()
                .all(|(c, _)| c == code || self.hidden.contains(c))
    }

    /// The lowest line voltage worth drawing, given how much room there is.
    ///
    /// Zero when everything fits, which is the common case and every case that is
    /// not a continental model. Above that it climbs through the voltage levels
    /// the network actually states, so the picture thins to a backbone rather than
    /// to an arbitrary subset -- there is no point hiding 219 kV and keeping
    /// 220 kV.
    ///
    /// Returns a *voltage*, not a count, because the reader can be told a voltage.
    fn voltage_floor(&self, rect: Rect, layout: &[Pos2], net: &Network) -> f64 {
        let visible = self.visible(rect);
        let shown = layout
            .iter()
            .enumerate()
            .filter(|(b, p)| visible.contains(**p) && self.shows(net, *b))
            .count();
        // Roughly one corridor per two hundred and fifty square points. A budget,
        // not a limit -- see the floor below it.
        let room = (rect.area() / 250.0) as usize;
        if shown <= room.max(1) {
            return 0.0;
        }

        // The levels this network states, ascending, so the floor lands on a real
        // one. Collected each call rather than cached: it is one pass over the
        // buses against the ten thousand segments this decides for.
        let mut levels: Vec<f64> = net
            .buses
            .iter()
            .map(|b| b.v_nom)
            .filter(|v| *v > 0.0)
            .collect();
        levels.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        levels.dedup();
        if levels.len() < 2 {
            return 0.0;
        }

        // **A floor on how much may be hidden, and it is the part that matters.**
        //
        // Raising the floor until the count fits sounds right and is wrong, because
        // the levels are not evenly populated. The European extract has 4,261 lines
        // at 380 kV and above and 462 at 500 kV and above, and a budget landing
        // between those two numbers jumps straight over the 380 kV backbone --
        // which left Russia's 750 kV and Egypt's 500 kV on screen and the whole of
        // western Europe blank. Technically the highest voltages present. Exactly
        // the wrong picture.
        //
        // So the budget yields to keeping a sixth of the network. Better a denser
        // picture than a picture of somewhere else.
        let keep = (net.lines.len() / 6).max(1);
        let mut chosen = 0.0;
        for &floor in &levels {
            let above = net
                .lines
                .iter()
                .filter(|l| {
                    line_kv(net, l) >= floor
                        && self.shows(net, l.bus0)
                        && self.shows(net, l.bus1)
                })
                .count();
            if above < keep {
                break;
            }
            chosen = floor;
            if above <= room {
                break;
            }
        }
        chosen
    }

    /// Half-length and thickness of a bus bar at the current zoom.
    ///
    /// One definition, because four places need both and they have to agree: the
    /// bar itself, the hover outline, the selection brackets, and the footprint
    /// the basemap's labels keep out of. The thickness in particular was written
    /// out three times.
    fn bar_metrics(&self) -> (f32, f32) {
        let half = self.bar_half();
        (half, (half * 0.30).clamp(3.0, 8.0))
    }

    /// Screen space each bus occupies, symbol and name together.
    ///
    /// For the basemap's labels to keep out of, so a city name never lands on a
    /// substation's. Deliberately **conservative**: the height covers the
    /// generator and load glyphs above and below the bar plus the name beneath
    /// them, and the width is the wider of the bar and the name, without asking
    /// which glyphs this particular bus actually has. Reserving more than is
    /// strictly needed costs a city label that would have fitted; reserving less
    /// costs two names in one place, and only one of those is recoverable by
    /// panning.
    fn bus_footprints(
        &self,
        painter: &eframe::egui::Painter,
        rect: Rect,
        net: &Network,
        layout: &[Pos2],
    ) -> Vec<Rect> {
        let visible = self.visible(rect);
        let (half, thickness) = self.bar_metrics();
        let mut out = Vec::with_capacity(net.buses.len());
        for (b, bus) in net.buses.iter().enumerate() {
            if !self.shows(net, b) {
                continue;
            }
            let Some(&p) = layout.get(b) else { continue };
            if !visible.contains(p) {
                continue;
            }
            let s = self.screen_of(rect, p);
            let name = painter.layout_no_wrap(
                bus.name.clone(),
                FontId::proportional(10.0),
                crate::theme::INK_DIM,
            );
            // The bar, the glyph rows either side of it, and the name below.
            let width = (half * 2.0).max(name.size().x) + 6.0;
            let top = -thickness * 0.5 - 14.0;
            let bottom = thickness * 0.5 + 19.0 + name.size().y * 0.5 + 2.0;
            out.push(Rect::from_min_max(
                s + vec2(-width * 0.5, top),
                s + vec2(width * 0.5, bottom),
            ));
        }
        out
    }

    fn screen_of(&self, rect: Rect, p: Pos2) -> Pos2 {
        rect.center() + (p - self.centre) * self.zoom
    }

    fn model_of(&self, rect: Rect, s: Pos2) -> Pos2 {
        self.centre + (s - rect.center()) / self.zoom
    }

    fn fit(&mut self, rect: Rect, layout: &[Pos2]) {
        let (mut lo, mut hi) = (Vec2::splat(f32::MAX), Vec2::splat(f32::MIN));
        for p in layout {
            lo = lo.min(p.to_vec2());
            hi = hi.max(p.to_vec2());
        }
        let span = hi - lo;
        let centre = pos2((lo.x + hi.x) * 0.5, (lo.y + hi.y) * 0.5);
        // Margin enough that a bus on the boundary is not cut in half by the
        // edge it was fitted to, and that its label -- which hangs below the
        // bar and is not in `layout` -- still lands inside the pane.
        let zoom = (0.88 * (rect.width() / span.x.max(1e-3)).min(rect.height() / span.y.max(1e-3)))
            .clamp(MIN_ZOOM, MAX_ZOOM);

        // A network arriving snaps; a refit glides. There is nothing on screen
        // to stay oriented to when a file opens, so motion there would be an
        // intro rather than a continuity cue -- and the reader is made to wait
        // for their own data.
        if self.snap_next_fit {
            self.snap_next_fit = false;
            self.centre = centre;
            self.zoom = zoom;
            self.goal = None;
        } else {
            self.goal = Some((centre, zoom));
        }
    }
}

/// Identifies one circuit, so a bus can order the things attached to it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Circuit {
    Line(usize),
    Link(usize),
}

/// Which point along each busbar each circuit lands on.
///
/// Built once per frame from the layout. The cost is linear in circuits and the
/// alternative is every circuit landing on top of every other at the middle of
/// the bar, which is what a node-link graph looks like and what a single-line
/// diagram specifically does not.
struct TapSlots {
    /// `(circuit, bus) -> (slot index, slots on that bus)`.
    slot: std::collections::HashMap<(Circuit, usize), (usize, usize)>,
}

impl TapSlots {
    fn build(net: &Network, layout: &[Pos2]) -> Self {
        // Per bus, everything attached to it and the direction it leaves in.
        let mut at: Vec<Vec<(f32, Circuit)>> = vec![Vec::new(); net.buses.len()];
        let mut note = |bus: usize, other: usize, c: Circuit| {
            if let (Some(p), Some(q)) = (layout.get(bus), layout.get(other))
                && let Some(slot) = at.get_mut(bus)
            {
                slot.push((q.x - p.x, c));
            }
        };
        for (e, l) in net.lines.iter().enumerate() {
            note(l.bus0, l.bus1, Circuit::Line(e));
            note(l.bus1, l.bus0, Circuit::Line(e));
        }
        for (e, l) in net.links.iter().enumerate() {
            note(l.bus0, l.bus1, Circuit::Link(e));
            note(l.bus1, l.bus0, Circuit::Link(e));
        }

        let mut slot = std::collections::HashMap::new();
        for (bus, mut circuits) in at.into_iter().enumerate() {
            // By horizontal direction of the far end, so the landing order
            // along the bar matches the order the circuits fan out in. Sorting
            // by anything else would make circuits cross each other on the
            // approach for no reason a reader could see.
            circuits.sort_by(|a, b| a.0.total_cmp(&b.0));
            let n = circuits.len();
            for (i, (_, c)) in circuits.into_iter().enumerate() {
                slot.insert((c, bus), (i, n));
            }
        }
        Self { slot }
    }

    /// Move each end of a circuit from the bus centre to its own tap point.
    fn place(
        &self,
        a: Pos2,
        b: Pos2,
        bus_a: usize,
        bus_b: usize,
        c: Circuit,
        half: f32,
    ) -> (Pos2, Pos2) {
        (
            pos2(a.x + self.offset(c, bus_a, half), a.y),
            pos2(b.x + self.offset(c, bus_b, half), b.y),
        )
    }

    fn offset(&self, c: Circuit, bus: usize, half: f32) -> f32 {
        let Some(&(i, n)) = self.slot.get(&(c, bus)) else {
            return 0.0;
        };
        if n <= 1 {
            return 0.0;
        }
        // Inset from the ends so a tap never sits exactly on the bar's tip,
        // where it would read as the line simply continuing past it.
        let span = half * 1.5;
        -span * 0.5 + span * (i as f32 + 0.5) / n as f32
    }
}

/// A point on the lightness ramp, from receding to prominent.
///
/// Shared with the charts on purpose: price on a busbar and merit order in a
/// dispatch stack are both "how expensive", and one quantity should not change
/// its encoding between two pictures in the same window.
pub fn ramp(t: f32) -> Color32 {
    lerp_color(crate::theme::INK_DIM, crate::theme::INK_STRONG, t.clamp(0.0, 1.0))
}

/// Blend two colours, for the price ramp.
fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(
        f(a.r(), b.r()),
        f(a.g(), b.g()),
        f(a.b(), b.b()),
    )
}

/// A generator: a circle on a stem, with a sine inside it for AC.
///
/// This is the IEC symbol, and it is worth the four extra shapes. A filled dot
/// says "something is here"; a circle with a sine in it says "a rotating
/// machine feeding alternating current is here", which is the same amount of
/// ink telling a power engineer something they can act on.
fn generator(painter: &Painter, bar_top: Pos2, r: f32, ink: Color32) {
    let centre = bar_top - vec2(0.0, r + 5.0);
    painter.line_segment(
        [bar_top, bar_top - vec2(0.0, 5.0)],
        Stroke::new(1.2, ink),
    );
    painter.circle_stroke(centre, r, Stroke::new(1.2, ink));

    // Below about five pixels of radius the sine is three pixels of noise
    // inside a ring, so the symbol falls back to a filled disc -- still a
    // generator by position, just no longer claiming to show its waveform.
    if r < 5.0 {
        painter.circle_filled(centre, r * 0.55, ink);
        return;
    }
    let w = r * 0.62;
    let pts: Vec<Pos2> = (0..=12)
        .map(|i| {
            let t = i as f32 / 12.0;
            centre + vec2(-w + 2.0 * w * t, -(t * std::f32::consts::TAU).sin() * r * 0.34)
        })
        .collect();
    painter.add(eframe::egui::Shape::line(pts, Stroke::new(1.1, ink)));
}

/// A load: a solid arrowhead on a stem, pointing away from the bar.
///
/// The arrow is the whole point of the symbol -- it is the one thing on the
/// diagram that states a direction, and withdrawal is the only quantity here
/// whose direction is fixed rather than solved for.
fn load(painter: &Painter, bar_bottom: Pos2, r: f32, ink: Color32) {
    let tip = bar_bottom + vec2(0.0, r * 2.2 + 4.0);
    painter.line_segment(
        [bar_bottom, tip - vec2(0.0, r * 1.5)],
        Stroke::new(1.2, ink),
    );
    painter.add(eframe::egui::Shape::convex_polygon(
        vec![
            tip,
            tip - vec2(r * 0.75, r * 1.5),
            tip + vec2(r * 0.75, -r * 1.5),
        ],
        ink,
        Stroke::NONE,
    ));
}

/// Storage: the battery symbol, a long plate and a short one.
///
/// Drawn on the injection side even though storage withdraws too. It is the
/// only component on the diagram that does both, and the convention has no
/// place for that -- putting it above at least groups it with the other things
/// that can be dispatched.
fn storage(painter: &Painter, bar_top: Pos2, r: f32, ink: Color32) {
    let stroke = Stroke::new(1.2, ink);
    let stem = bar_top - vec2(0.0, 4.0);
    painter.line_segment([bar_top, stem], stroke);
    for (i, w) in [r * 1.1, r * 0.55].into_iter().enumerate() {
        let y = stem.y - i as f32 * 3.5;
        painter.line_segment([pos2(stem.x - w, y), pos2(stem.x + w, y)], stroke);
    }
}

/// How close to its rating a corridor has to run before it counts as binding.
///
/// Not 1.0: a simplex answer sits on the constraint to within its tolerance,
/// not on it exactly, and a diagram that marks 0.99999 as slack would fail to
/// mark most of the lines the solver was actually held back by.
const BINDING: f64 = 0.995;

#[cfg(test)]
mod tests {
    use super::*;
    use gridwright_net::{Bus, Line, Snapshots};

    /// Three buses in a row: `left — middle — right`.
    fn row() -> (Network, Vec<Pos2>) {
        let mut net = Network::new(Snapshots::hourly(1));
        for name in ["left", "middle", "right"] {
            net.buses.push(Bus {
                name: name.into(),
                ..Default::default()
            });
        }
        net.lines.push(Line {
            name: "a".into(),
            bus0: 1,
            bus1: 0,
            ..Default::default()
        });
        net.lines.push(Line {
            name: "b".into(),
            bus0: 1,
            bus1: 2,
            ..Default::default()
        });
        (net, vec![pos2(-1.0, 0.0), pos2(0.0, 0.0), pos2(1.0, 0.0)])
    }

    #[test]
    fn taps_land_in_the_direction_their_circuit_leaves() {
        let (net, layout) = row();
        let taps = TapSlots::build(&net, &layout);

        // The line to the left bus taps the left of the middle bar and the one
        // to the right taps the right. This is the whole reason slots are
        // sorted: get it backwards and the two circuits cross on the approach.
        let to_left = taps.offset(Circuit::Line(0), 1, 20.0);
        let to_right = taps.offset(Circuit::Line(1), 1, 20.0);
        assert!(to_left < 0.0, "westbound circuit tapped at {to_left}");
        assert!(to_right > 0.0, "eastbound circuit tapped at {to_right}");
    }

    #[test]
    fn a_lone_circuit_taps_the_middle_of_its_bar() {
        let (net, layout) = row();
        let taps = TapSlots::build(&net, &layout);
        // The left bus has one circuit on it, so there is nothing to spread.
        assert_eq!(taps.offset(Circuit::Line(0), 0, 20.0), 0.0);
    }

    #[test]
    fn taps_stay_on_the_bar_they_belong_to() {
        let (net, layout) = row();
        let taps = TapSlots::build(&net, &layout);
        let half = 20.0;
        for c in [Circuit::Line(0), Circuit::Line(1)] {
            for bus in 0..net.buses.len() {
                let d = taps.offset(c, bus, half);
                assert!(
                    d.abs() <= half,
                    "circuit {c:?} tapped bus {bus} at {d}, past the end of a bar of half-width {half}",
                );
            }
        }
    }

    #[test]
    fn an_unattached_circuit_offsets_to_the_centre() {
        let (net, layout) = row();
        let taps = TapSlots::build(&net, &layout);
        // A view may be handed a network that never went through `validate`,
        // and an out-of-range reference must not panic its way onto the canvas.
        assert_eq!(taps.offset(Circuit::Line(99), 0, 20.0), 0.0);
        assert_eq!(taps.offset(Circuit::Line(0), 99, 20.0), 0.0);
    }

    #[test]
    fn the_ramp_ends_where_its_endpoints_are() {
        let (a, b) = (crate::theme::INK_DIM, crate::theme::INK_STRONG);
        assert_eq!(lerp_color(a, b, 0.0), a);
        assert_eq!(lerp_color(a, b, 1.0), b);
    }

    #[test]
    fn the_ramp_only_gets_lighter() {
        // The claim the price encoding rests on: brighter means more expensive,
        // with no dip in between that would read as a cheaper bus.
        let (a, b) = (crate::theme::INK_DIM, crate::theme::INK_STRONG);
        let lum = |c: Color32| c.r() as u32 + c.g() as u32 + c.b() as u32;
        let mut last = 0;
        for i in 0..=20 {
            let now = lum(lerp_color(a, b, i as f32 / 20.0));
            assert!(now >= last, "step {i} darkened: {last} then {now}");
            last = now;
        }
    }
}

/// A label painted directly rather than as an egui tooltip.
///
/// The canvas is one allocated widget, so a real tooltip would attach to the
/// pane and appear wherever egui likes rather than beside the thing the pointer
/// is actually over.
fn callout(ui: &Ui, painter: &Painter, anchor: Pos2, label: String) {
    let color = ui.visuals().strong_text_color();
    let galley = painter.layout_no_wrap(label, FontId::proportional(12.0), color);
    painter.rect_filled(
        Rect::from_min_size(anchor, galley.size()).expand(3.0),
        3.0,
        ui.visuals().panel_fill,
    );
    painter.galley(anchor, galley, color);
}

/// The closest point on a polyline to `p`, and how far away it is.
///
/// Used for picking circuits, which are routed as three-segment taps rather
/// than as straight chords -- distance to the chord would miss the pointer on
/// the stubs, which is exactly where a reader points when two circuits run
/// alongside each other between the same pair of buses.
fn nearest_on(path: &[Pos2], p: Pos2) -> Option<(Pos2, f32)> {
    path.windows(2)
        .map(|w| {
            let (a, b) = (w[0], w[1]);
            let seg = b - a;
            let len2 = seg.length_sq();
            let t = if len2 <= f32::EPSILON {
                0.0
            } else {
                ((p - a).dot(seg) / len2).clamp(0.0, 1.0)
            };
            let at = a + seg * t;
            (at, at.distance(p))
        })
        .min_by(|x, y| x.1.total_cmp(&y.1))
}

#[cfg(test)]
mod pick_tests {
    use super::*;

    #[test]
    fn nearest_lands_on_the_leg_the_pointer_is_beside() {
        // A tapped route: down off one bar, across, up onto the next.
        let path = [
            pos2(0.0, 0.0),
            pos2(0.0, 10.0),
            pos2(100.0, 10.0),
            pos2(100.0, 20.0),
        ];
        let (at, d) = nearest_on(&path, pos2(50.0, 14.0)).unwrap();
        assert_eq!(at, pos2(50.0, 10.0));
        assert!((d - 4.0).abs() < 1e-4, "distance was {d}");
    }

    #[test]
    fn nearest_finds_the_stubs_not_just_the_chord() {
        // The point of picking against the route rather than the chord: a
        // pointer beside a vertical stub is nowhere near the straight line
        // between the two bus centres.
        let path = [pos2(0.0, 0.0), pos2(0.0, 40.0), pos2(200.0, 40.0)];
        let (at, d) = nearest_on(&path, pos2(3.0, 20.0)).unwrap();
        assert_eq!(at, pos2(0.0, 20.0));
        assert!((d - 3.0).abs() < 1e-4, "distance was {d}");
    }

    #[test]
    fn nearest_clamps_to_the_ends() {
        let path = [pos2(0.0, 0.0), pos2(10.0, 0.0)];
        let (at, _) = nearest_on(&path, pos2(-50.0, 0.0)).unwrap();
        assert_eq!(at, pos2(0.0, 0.0));
    }

    #[test]
    fn a_degenerate_path_picks_nothing_rather_than_dividing_by_zero() {
        assert!(nearest_on(&[], pos2(0.0, 0.0)).is_none());
        assert!(nearest_on(&[pos2(1.0, 1.0)], pos2(0.0, 0.0)).is_none());
        // Two coincident points: a zero-length segment, which the projection
        // would divide by.
        let (at, d) = nearest_on(&[pos2(4.0, 0.0), pos2(4.0, 0.0)], pos2(0.0, 0.0)).unwrap();
        assert_eq!(at, pos2(4.0, 0.0));
        assert!((d - 4.0).abs() < 1e-4);
    }
}

/// Four corner brackets around a rect: the viewfinder mark for "this one".
fn brackets(painter: &Painter, r: Rect, arm: f32) {
    let stroke = Stroke::new(1.5, crate::theme::INK_STRONG);
    // Never longer than half a side, or opposite arms meet and the brackets
    // close into the outline they exist not to be.
    let arm = arm.min(r.width() * 0.45).min(r.height() * 0.45);
    for (corner, dx, dy) in [
        (r.left_top(), 1.0, 1.0),
        (r.right_top(), -1.0, 1.0),
        (r.left_bottom(), 1.0, -1.0),
        (r.right_bottom(), -1.0, -1.0),
    ] {
        painter.line_segment([corner, corner + vec2(arm * dx, 0.0)], stroke);
        painter.line_segment([corner, corner + vec2(0.0, arm * dy)], stroke);
    }
}

/// Voltage bands, adapted from OpenInfraMap's scale.
///
/// Adopted rather than invented, because OIM's is the most considered one in
/// the field: TenneT's ArcGIS service colours by voltage and leaves every class
/// at the same width, Swissgrid's KML draws the entire Swiss grid in two greys,
/// and ENTSO-E's flagship map has shipped a bug for years where two bands share
/// a CSS class. Being able to say "close to OpenInfraMap" is worth more to a
/// reader than a palette designed here.
///
/// **One band is deliberately changed, and the reason is a rule worth stating.**
/// Swissgrid, on redrawing their national map: *"Red is a signal colour and is
/// no longer used when the grid is in its normal state in the new
/// representation."* OIM's 220 kV band is `#C73030`, a red — drawn on perfectly
/// healthy corridors, where it competes with the red that means unserved
/// energy. A healthy 220 kV line and a bus that failed to serve its load must
/// not be the same colour, and of the two, the failure is the one that has to
/// win. So 220 kV takes a warm brown-orange instead, keeping its place in the
/// ramp without spending the alarm hue.
///
/// The 25 kV band moved too, and the test is what found it: OIM's `#55B555` is
/// close enough to this theme's "solved" green that a healthy 25 kV corridor and
/// a lamp reporting a good answer read as the same colour. Deepened to a forest
/// green, which keeps it distinct from both that lamp and the 10 kV blue above
/// it.
///
/// Between them these two changes also keep the palette inside NUREG-0700's
/// guidance to avoid displays requiring red-green comparisons, which the
/// original scale's 25 kV green against 220 kV red would have needed.
///
/// Each entry is the lower bound of a band in kV, and the colour from there up.
const VOLTAGE_SCALE: [(f64, Color32); 8] = [
    (0.0, Color32::from_rgb(0x7A, 0x7A, 0x85)),
    (10.0, Color32::from_rgb(0x6E, 0x97, 0xB8)),
    (25.0, Color32::from_rgb(0x3E, 0x8A, 0x3E)),
    (52.0, Color32::from_rgb(0xB5, 0x9F, 0x10)),
    (132.0, Color32::from_rgb(0xB5, 0x5D, 0x00)),
    (220.0, Color32::from_rgb(0xA8, 0x62, 0x38)),
    (310.0, Color32::from_rgb(0xB5, 0x4E, 0xB2)),
    (550.0, Color32::from_rgb(0x00, 0xC1, 0xCF)),
];

/// How close two colours may be before a reader cannot tell them apart.
///
/// Crude on purpose: a sum of channel differences rather than a perceptual
/// distance, because the thing being guarded against is a palette that grows a
/// near-duplicate of the alarm colour, and that shows up plainly even in a
/// coarse metric. A real colour-difference formula would be more accurate and
/// would not catch anything this does not.
#[cfg(test)]
fn channel_distance(a: Color32, b: Color32) -> i32 {
    (a.r() as i32 - b.r() as i32).abs()
        + (a.g() as i32 - b.g() as i32).abs()
        + (a.b() as i32 - b.b() as i32).abs()
}

/// Below this a stated voltage is not a voltage.
///
/// MATPOWER cases routinely carry `baseKV` of 1.0 because they are written in
/// per unit and never needed a real number. Colouring a network by a voltage
/// nobody stated would invent eight bands out of one placeholder.
const MIN_REAL_KV: f64 = 1.0;

/// The band a corridor at this voltage falls in.
fn voltage_color(kv: f64) -> Color32 {
    VOLTAGE_SCALE
        .iter()
        .rev()
        .find(|(from, _)| kv >= *from)
        .map(|(_, c)| *c)
        .unwrap_or(VOLTAGE_SCALE[0].1)
}

/// Whether this network says enough about voltage to colour by it.
///
/// Two distinct real levels, not one. A single-voltage network coloured by
/// voltage is a network drawn in one arbitrary hue, which is exactly the
/// mistake the country colouring made before it was removed: a colour that
/// varies with nothing is a colour carrying no information on a screen where
/// colour is supposed to mean something.
fn voltages_are_stated(net: &Network) -> bool {
    let mut seen: Option<f64> = None;
    for b in &net.buses {
        if b.v_nom < MIN_REAL_KV {
            continue;
        }
        match seen {
            None => seen = Some(b.v_nom),
            Some(v) if (v - b.v_nom).abs() > 1e-6 => return true,
            _ => {}
        }
    }
    false
}

/// The voltage a corridor runs at.
///
/// The higher of its two ends. A line between a 380 kV and a 220 kV bus is a
/// transformer in everything but name, and drawing it at the lower level would
/// hide the higher network it is part of.
fn line_kv(net: &Network, line: &gridwright_net::Line) -> f64 {
    let at = |b: usize| net.buses.get(b).map(|b| b.v_nom).unwrap_or(0.0);
    at(line.bus0).max(at(line.bus1))
}

#[cfg(test)]
mod region_tests {
    use super::*;
    use gridwright_net::{Bus, Snapshots};

    fn net_of(countries: &[&str]) -> Network {
        let mut net = Network::new(Snapshots::hourly(1));
        for (i, c) in countries.iter().enumerate() {
            net.buses.push(Bus {
                name: format!("b{i}"),
                country: (*c).to_string(),
                v_nom: 380.0,
                ..Bus::default()
            });
        }
        net
    }

    #[test]
    fn regions_are_counted_and_ordered_by_size() {
        // Largest first, because on a continental model the grid a reader wants is
        // almost always one of the big ones and an alphabetical list puts Albania
        // above Germany.
        let net = net_of(&["DE", "FR", "DE", "ES", "DE", "FR"]);
        assert_eq!(
            NetworkView::regions(&net),
            vec![
                ("DE".to_string(), 3),
                ("FR".to_string(), 2),
                ("ES".to_string(), 1)
            ]
        );
    }

    #[test]
    fn a_bus_with_no_country_is_not_a_region() {
        // MATPOWER has no column for it, so every IEEE case would otherwise offer
        // a filter with one blank entry in it.
        let net = net_of(&["DE", "", "  ", "FR"]);
        let regions = NetworkView::regions(&net);
        assert_eq!(regions.len(), 2);
        assert!(regions.iter().all(|(c, _)| !c.trim().is_empty()));
    }

    #[test]
    fn ties_are_broken_so_the_list_does_not_reshuffle() {
        // Two countries with the same bus count must come out in the same order
        // every frame, or the list moves under the reader's cursor.
        let net = net_of(&["FR", "DE", "ES", "AT"]);
        let once = NetworkView::regions(&net);
        assert_eq!(once, NetworkView::regions(&net));
        assert_eq!(
            once.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>(),
            vec!["AT", "DE", "ES", "FR"],
        );
    }

    #[test]
    fn everything_shows_until_something_is_hidden() {
        let net = net_of(&["DE", "FR"]);
        let mut view = NetworkView::default();
        assert!(!view.any_region_hidden());
        assert!((0..net.buses.len()).all(|b| view.shows(&net, b)));

        view.set_region_hidden("FR", true);
        assert!(view.any_region_hidden());
        assert!(view.shows(&net, 0));
        assert!(!view.shows(&net, 1));
    }

    #[test]
    fn isolating_one_country_hides_the_others_and_nothing_else() {
        let net = net_of(&["DE", "FR", "ES"]);
        let mut view = NetworkView::default();
        view.only_region(&net, "FR");
        assert!(!view.shows(&net, 0));
        assert!(view.shows(&net, 1));
        assert!(!view.shows(&net, 2));

        // And it is reversible in one action, or a reader who isolated a country
        // on a sixty-country model has to click fifty-nine times to get back.
        view.show_all_regions();
        assert!(!view.any_region_hidden());
        assert!((0..net.buses.len()).all(|b| view.shows(&net, b)));
    }

    #[test]
    fn isolating_a_country_also_puts_away_the_buses_with_no_country() {
        // Otherwise isolating Spain leaves every uncoded node in the extract -- 1,195
        // of them, scattered across the continent -- on screen, which is a network
        // with the recognisable parts removed rather than one country shown.
        let net = net_of(&["ES", "FR", "", ""]);
        let mut view = NetworkView::default();
        view.only_region(&net, "ES");
        assert!(view.shows(&net, 0), "the isolated country is hidden");
        assert!(!view.shows(&net, 1));
        assert!(!view.shows(&net, 2), "an uncoded bus survived isolation");
        assert!(!view.shows(&net, 3));

        view.show_all_regions();
        assert!((0..net.buses.len()).all(|b| view.shows(&net, b)));
    }

    /// What a click on a country's name does, as the canvas decides it.
    fn click(view: &mut NetworkView, net: &Network, code: &str) {
        if view.only_shown(net, code) {
            view.show_all_regions();
        } else {
            view.only_region(net, code);
        }
    }

    #[test]
    fn the_first_click_on_a_country_shows_that_country() {
        // **Reported: the first click showed the whole grid and the second one
        // finally narrowed it.** The rule used to be "if anything is hidden, show
        // everything", read fresh on every click, so any earlier interaction that
        // had hidden something turned the next click into a reset.
        let net = net_of(&["ES", "FR", "DE"]);
        let mut view = NetworkView::default();
        click(&mut view, &net, "ES");
        assert!(view.shows(&net, 0), "clicking Spain did not show Spain");
        assert!(!view.shows(&net, 1));
        assert!(!view.shows(&net, 2));
    }

    #[test]
    fn clicking_another_country_moves_to_it_rather_than_resetting() {
        // The failure the report was actually about: with Spain isolated, clicking
        // France showed the whole continent, and a second click on France was
        // needed to get there. Moving between two countries is one click.
        let net = net_of(&["ES", "FR", "DE"]);
        let mut view = NetworkView::default();
        click(&mut view, &net, "ES");
        click(&mut view, &net, "FR");
        assert!(!view.shows(&net, 0), "Spain is still shown");
        assert!(view.shows(&net, 1), "clicking France did not show France");
        assert!(!view.shows(&net, 2));
    }

    #[test]
    fn clicking_the_country_already_on_its_own_shows_everything() {
        // The way back out, using the same gesture, so a reader who isolated
        // Portugal does not have to go and find the panel to escape it.
        let net = net_of(&["ES", "FR", "DE"]);
        let mut view = NetworkView::default();
        click(&mut view, &net, "ES");
        assert!(view.any_region_hidden());
        click(&mut view, &net, "ES");
        assert!(!view.any_region_hidden());
        assert!((0..net.buses.len()).all(|b| view.shows(&net, b)));
    }

    #[test]
    fn only_shown_is_about_the_country_asked_about() {
        let net = net_of(&["ES", "FR", "DE"]);
        let mut view = NetworkView::default();
        assert!(!view.only_shown(&net, "ES"), "nothing is isolated yet");
        view.only_region(&net, "ES");
        assert!(view.only_shown(&net, "ES"));
        assert!(!view.only_shown(&net, "FR"));
        // Two of three shown is not one on its own.
        view.show_all_regions();
        view.set_region_hidden("DE", true);
        assert!(!view.only_shown(&net, "ES"));
        assert!(!view.only_shown(&net, "FR"));
    }

    #[test]
    fn applying_a_click_twice_in_one_frame_would_undo_it() {
        // Why the hit test uses a `Response` rather than reading the raw pointer:
        // egui may lay a frame out more than once, a draw call that reads
        // `primary_clicked` sees the same click on each pass, and this is what that
        // costs. The test documents the hazard rather than the fix, so anyone
        // tempted back to raw input can see what it does.
        let net = net_of(&["ES", "FR"]);
        let mut view = NetworkView::default();
        click(&mut view, &net, "ES");
        click(&mut view, &net, "ES");
        assert!(
            !view.any_region_hidden(),
            "two applications of one click must be visible as a no-op, which is \
             exactly why the click may only be applied once",
        );
    }

    #[test]
    fn hiding_a_country_that_is_not_in_the_network_changes_nothing_visible() {
        // The filter is keyed by the code the file uses, and a stale code left over
        // from a previous network must not blank the current one.
        let net = net_of(&["DE", "FR"]);
        let mut view = NetworkView::default();
        view.set_region_hidden("ZZ", true);
        assert!((0..net.buses.len()).all(|b| view.shows(&net, b)));
    }

    #[test]
    fn a_country_with_no_code_is_never_hidden() {
        // Otherwise switching off the blank entry that `regions` refuses to offer
        // would still be reachable, and would hide every uncoded bus at once.
        let net = net_of(&["", "DE"]);
        let mut view = NetworkView::default();
        view.set_region_hidden("DE", true);
        assert!(view.shows(&net, 0), "an uncoded bus was hidden");
        assert!(!view.shows(&net, 1));
    }
}

#[cfg(test)]
mod voltage_tests {
    use super::*;
    use gridwright_net::{Bus, Snapshots};

    fn net_at(kv: &[f64]) -> Network {
        let mut net = Network::new(Snapshots::hourly(1));
        for (i, v) in kv.iter().enumerate() {
            net.buses.push(Bus {
                name: format!("b{i}"),
                v_nom: *v,
                ..Default::default()
            });
        }
        net
    }

    #[test]
    fn no_voltage_band_is_mistakable_for_an_alarm() {
        // Swissgrid's rule: red is a signal colour and is not used when the
        // grid is in its normal state. A healthy corridor and a bus that failed
        // to serve its load must not be the same colour, and the failure is the
        // one that has to win.
        //
        // This is a real regression risk rather than a hypothetical: the scale
        // this was adapted from puts #C73030 on 220 kV, which is a red, and
        // 220 kV is the single most common transmission voltage in Europe.
        for (kv, color) in VOLTAGE_SCALE {
            for (name, state) in [
                ("trip", crate::theme::TRIP),
                ("alarm", crate::theme::ALARM),
                ("live", crate::theme::LIVE),
            ] {
                let d = channel_distance(color, state);
                assert!(
                    d > 90,
                    "the {kv} kV band is within {d} of the {name} colour",
                );
            }
        }
    }

    #[test]
    fn bands_start_at_their_lower_bound() {
        // Exactly on a boundary belongs to the band it opens, not the one
        // below. 220.0 kV is a 220 kV line.
        assert_eq!(voltage_color(220.0), VOLTAGE_SCALE[5].1);
        assert_eq!(voltage_color(219.9), VOLTAGE_SCALE[4].1);
        assert_eq!(voltage_color(1000.0), VOLTAGE_SCALE[7].1);
        assert_eq!(voltage_color(0.0), VOLTAGE_SCALE[0].1);
    }

    #[test]
    fn a_per_unit_case_is_not_a_network_of_one_volt_lines() {
        // MATPOWER cases routinely carry baseKV of 1.0 because they are written
        // in per unit. Reading that as a real voltage would colour the whole
        // network from one placeholder.
        assert!(!voltages_are_stated(&net_at(&[1.0, 1.0, 1.0])));
        assert!(!voltages_are_stated(&net_at(&[0.0, 0.0])));
    }

    #[test]
    fn one_stated_level_is_not_worth_a_colour_axis() {
        // A network drawn entirely in one hue is a hue carrying no information,
        // which is the mistake the country colouring made before it was removed.
        assert!(!voltages_are_stated(&net_at(&[380.0, 380.0, 380.0])));
        assert!(voltages_are_stated(&net_at(&[380.0, 220.0])));
    }

    #[test]
    fn a_level_stated_on_only_some_buses_still_counts() {
        // Real files are partial. Two real levels among placeholders is enough
        // to be worth drawing.
        assert!(voltages_are_stated(&net_at(&[0.0, 380.0, 110.0, 1.0])));
    }

    #[test]
    fn a_corridor_takes_the_higher_of_its_two_ends() {
        // A line between 380 and 220 is a transformer in all but name, and
        // drawing it at the lower level would hide the higher network it is
        // part of.
        let net = net_at(&[380.0, 220.0]);
        let line = gridwright_net::Line {
            bus0: 1,
            bus1: 0,
            ..Default::default()
        };
        assert_eq!(line_kv(&net, &line), 380.0);
    }

    #[test]
    fn a_corridor_pointing_off_the_end_of_the_bus_list_has_no_voltage() {
        // The view is handed networks that never went through `validate`.
        let net = net_at(&[380.0]);
        let line = gridwright_net::Line {
            bus0: 0,
            bus1: 99,
            ..Default::default()
        };
        assert_eq!(line_kv(&net, &line), 380.0);
        let orphan = gridwright_net::Line {
            bus0: 98,
            bus1: 99,
            ..Default::default()
        };
        assert_eq!(line_kv(&net, &orphan), 0.0);
    }
}

/// A single arrowhead partway along a corridor, pointing the way power flows.
///
/// Placed at forty percent rather than the midpoint, so on a pair of parallel
/// circuits between the same substations the two chevrons do not land on top of
/// each other. Drawn on the longest leg for the same reason the binding tick is:
/// the midpoint of a tapped route can fall on a corner, where an arrow has no
/// single direction to point along.
fn chevron(painter: &Painter, path: &[Pos2], forward: bool, width: f32, color: Color32) {
    let Some((a, b)) = path
        .windows(2)
        .map(|w| (w[0], w[1]))
        .max_by(|x, y| (x.0 - x.1).length().total_cmp(&(y.0 - y.1).length()))
    else {
        return;
    };
    let (from, to) = if forward { (a, b) } else { (b, a) };
    let along = (to - from).normalized();
    if !along.is_finite() {
        return;
    }
    let across = vec2(-along.y, along.x);
    let at = from + (to - from) * 0.4;
    let size = (width * 2.2).clamp(4.0, 9.0);

    // An open chevron rather than a filled triangle: filled reads as a
    // component on the line -- a load arrow is a filled triangle in this very
    // diagram -- where two strokes read as an annotation about it.
    let stroke = Stroke::new((width * 0.8).clamp(1.0, 2.0), color);
    for side in [-1.0, 1.0] {
        painter.line_segment(
            [at, at - along * size + across * size * 0.6 * side],
            stroke,
        );
    }
}

/// Where the `k`th of `n` fanned symbols sits, relative to the bar's centre.
///
/// Centred as a group, so one symbol stays exactly on the bar's midline and the
/// arrangement stays symmetric as more appear. An off-centre single symbol is
/// the tell that a fan was bolted onto a design that assumed one.
fn fan(k: usize, n: usize, glyph: f32) -> f32 {
    if n <= 1 {
        return 0.0;
    }
    let pitch = glyph * 2.6;
    (k as f32 - (n as f32 - 1.0) * 0.5) * pitch
}

#[cfg(test)]
mod fan_tests {
    use super::fan;

    #[test]
    fn a_lone_symbol_sits_on_the_midline() {
        // The tell that a fan was bolted onto a design that assumed one symbol
        // is that the single case drifts off centre.
        assert_eq!(fan(0, 1, 6.0), 0.0);
    }

    #[test]
    fn a_fan_is_centred_as_a_group() {
        for n in 2..=4 {
            let sum: f32 = (0..n).map(|k| fan(k, n, 6.0)).sum();
            assert!(sum.abs() < 1e-4, "n = {n} fanned off centre by {sum}");
        }
    }

    #[test]
    fn symbols_do_not_overlap_each_other() {
        // The pitch has to clear the glyph, or "four generators" draws as one
        // smudge and the detail was worse than the summary.
        let glyph = 6.0;
        for n in 2..=4 {
            let gap = fan(1, n, glyph) - fan(0, n, glyph);
            assert!(gap >= glyph * 2.0, "n = {n} packed them {gap} apart");
        }
    }
}

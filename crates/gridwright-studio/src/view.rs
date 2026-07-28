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
}

impl Default for NetworkView {
    fn default() -> Self {
        Self {
            zoom: 400.0,
            centre: Pos2::ZERO,
            needs_fit: true,
            selected: None,
            goal: None,
            hovered: None,
            snap_next_fit: true,
        }
    }
}

impl NetworkView {
    /// Refit at the next opportunity. Called when the network changes under it.
    pub fn reset(&mut self) {
        *self = Self::default();
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

        self.handle_camera(ui, &response, rect);

        let on_circuit = self.draw_edges(
            &painter,
            rect,
            net,
            layout,
            overlay.loading,
            response.hover_pos(),
        );
        let on_bus = self.draw_buses(ui, &painter, net, layout, overlay, &response);

        // A bus wins a tie. The pointer sits within picking distance of both
        // whenever it is near a tap point, and the bus is the thing that can be
        // selected -- offering a readout for the circuit there would contradict
        // the cursor.
        if let Some((c, at)) = on_circuit.filter(|_| !on_bus) {
            self.circuit_readout(ui, &painter, net, overlay.loading, c, at);
        }
        self.draw_key(&painter, rect, net, overlay.prices);
        self.draw_keys_hint(&painter, rect);
    }

    fn handle_camera(&mut self, ui: &Ui, response: &eframe::egui::Response, rect: Rect) {
        self.keys(ui, rect);
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
    fn keys(&mut self, ui: &Ui, rect: Rect) {
        use eframe::egui::Key;

        // Only when the canvas owns the keyboard. Otherwise `f` typed into a
        // future filter box would fling the camera across the network.
        if ui.memory(|m| m.focused()).is_some() {
            return;
        }

        let (fit, clear, zoom_in, zoom_out) = ui.input(|i| {
            (
                i.key_pressed(Key::F) || i.key_pressed(Key::Home),
                i.key_pressed(Key::Escape),
                i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals),
                i.key_pressed(Key::Minus),
            )
        });

        if fit {
            self.needs_fit = true;
        }
        if clear {
            self.selected = None;
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
            "scroll zoom · drag pan · F fit · esc clear",
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
        loading: &[f64],
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

        for (e, line) in net.lines.iter().enumerate() {
            let Some((a, b)) = self.segment(rect, visible, layout, line.bus0, line.bus1) else {
                continue;
            };
            let (a, b) = taps.place(a, b, line.bus0, line.bus1, Circuit::Line(e), self.bar_half());
            let width = 0.8 + 2.4 * (line.s_nom.abs() / max_s_nom).sqrt() as f32;
            let color = if line.is_transport() {
                TRANSPORT_COLOR
            } else {
                AC_COLOR
            };
            // Loading brightens the corridor, the same way price brightens a
            // busbar. An idle line is still drawn -- it is part of the network
            // whether or not it carried anything -- but it sits back.
            let use_ = loading.get(e).copied().unwrap_or(f64::NAN);
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

        // Corridor kinds, and only the ones this network has. A legend row for
        // a category with no members teaches a distinction the reader will
        // never see, and on a single-area MATPOWER case that is two of the
        // three -- most of the legend would be about nothing.
        let kinds = [
            (
                net.lines.iter().any(|l| !l.is_transport()),
                AC_COLOR,
                "ac line",
            ),
            (
                net.lines.iter().any(|l| l.is_transport()),
                TRANSPORT_COLOR,
                "transport",
            ),
            (!net.links.is_empty(), LINK_COLOR, "link"),
        ];
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
                name,
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
        let bar = Rect::from_min_size(pos2(rect.left() + 12.0, floor - 22.0), vec2(108.0, 5.0));
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
        let rect = response.rect;
        let visible = self.visible(rect);
        let Overlay {
            peak_shed,
            prices,
            loading: _,
        } = overlay;
        // A bus is drawn as a bar, not as a dot.
        //
        // This is the one primitive that decides whether the canvas reads as a
        // power system or as a generic node graph, and it is not a stylistic
        // preference: in every single-line diagram ever drawn, a busbar is a
        // bar. Circles say "vertex". Bars say "busbar", and an engineer reads
        // the second without being told.
        //
        // Half-length follows zoom weakly. Fixed screen size turns a national
        // model into a solid mat at low zoom; fixed model size draws bars the
        // width of the pane at high zoom.
        let half = (self.zoom * 0.022).clamp(6.0, 30.0);
        let thickness = (half * 0.30).clamp(3.0, 8.0);
        // Symbols scale with the bar but stop growing sooner. A generator ring
        // as tall as the busbar is wide stops reading as a machine hanging off
        // a conductor and starts reading as a lollipop.
        let glyph = (half * 0.34).clamp(2.5, 9.0);
        let pointer = response.hover_pos();

        // What is attached to each bus, computed once. Asking
        // `generators.iter().any(...)` inside the bus loop is quadratic, which
        // is invisible at fourteen buses and is not at thirteen thousand.
        let mut has_gen = vec![false; net.buses.len()];
        let mut has_load = vec![false; net.buses.len()];
        for g in &net.generators {
            if let Some(f) = has_gen.get_mut(g.bus) {
                *f = true;
            }
        }
        for l in &net.loads {
            if let Some(f) = has_load.get_mut(l.bus) {
                *f = true;
            }
        }
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

            let bar = Rect::from_center_size(s, vec2(half * 2.0, thickness));
            painter.rect_filled(bar, 0.0, ink);

            // Injection above the bar, withdrawal below — the convention a
            // one-line diagram uses, and it means a glance tells you where
            // power enters and where it leaves without reading a legend.
            if has_gen[b] {
                generator(painter, s - vec2(0.0, thickness * 0.5), glyph, ink);
            }
            if has_store[b] {
                let x = if has_gen[b] { glyph * 2.6 } else { 0.0 };
                storage(painter, s + vec2(x, -thickness * 0.5), glyph, ink);
            }
            if has_load[b] {
                load(painter, s + vec2(0.0, thickness * 0.5), glyph, ink);
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

        // Names, once the bars are far enough apart to carry them. Below this
        // they overlap into an unreadable mat, and an unreadable label is worse
        // than none because it still costs the pixels.
        if net.buses.len() <= 200 || half >= 14.0 {
            for (b, bus) in net.buses.iter().enumerate() {
                let Some(&p) = layout.get(b) else { continue };
                if !visible.contains(p) {
                    continue;
                }
                let s = self.screen_of(rect, p);
                let at = s + vec2(0.0, thickness * 0.5 + 19.0);
                // A knocked-out background rather than a halo stroke. Edges
                // pass behind labels constantly in a meshed network, and text
                // with a line through it is unreadable in a way that no amount
                // of contrast fixes.
                let galley = painter.layout_no_wrap(
                    bus.name.clone(),
                    FontId::proportional(10.0),
                    crate::theme::INK_DIM,
                );
                let box_ = Rect::from_center_size(at, galley.size()).expand2(vec2(3.0, 1.0));
                painter.rect_filled(box_, 2.0, crate::theme::SLATE_WORK);
                painter.galley(box_.min + vec2(3.0, 1.0), galley, crate::theme::INK_DIM);
            }
        }

        // A click takes the bus under the pointer, or clears when there is
        // none. Clearing on empty canvas matters: without it the only way to
        // deselect is to select something else, and there is no gesture for
        // "nothing".
        if response.clicked() {
            self.selected = best.map(|(b, _)| b);
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
        let half = (self.zoom * 0.022).clamp(6.0, 30.0);
        let thickness = (half * 0.30).clamp(3.0, 8.0);
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

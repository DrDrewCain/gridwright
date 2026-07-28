//! The network drawing: pan, zoom, buses as nodes, lines and links as edges.
//!
//! Everything here is `egui::Painter` primitives — line segments, circles, a
//! little text. No scene graph, no 3D engine, no plotting library. A power
//! system diagram is a few thousand line segments and a few thousand circles,
//! which is well inside what an immediate-mode painter emits in a frame, and
//! anything heavier would be paying for a retained scene that changes every time
//! the camera moves anyway.

use eframe::egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Ui, Vec2, pos2, vec2};
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

/// Distinct rather than pretty, and fixed rather than generated.
///
/// Country is the axis this engine reports cross-border flows on, so it is the
/// grouping worth seeing at a glance. A generated hue-per-string would give two
/// neighbouring countries near-identical colours often enough to matter; a short
/// hand-picked ramp collides less in the cases anyone looks at, and collides
/// visibly rather than subtly when it does.
const COUNTRY_COLORS: [Color32; 10] = [
    Color32::from_rgb(0x4c, 0x9f, 0xe0),
    Color32::from_rgb(0xe0, 0x8b, 0x4c),
    Color32::from_rgb(0x6d, 0xc2, 0x6d),
    Color32::from_rgb(0xd0, 0x62, 0x9a),
    Color32::from_rgb(0xc9, 0xb4, 0x3f),
    Color32::from_rgb(0x8f, 0x7d, 0xd6),
    Color32::from_rgb(0x4f, 0xc0, 0xb8),
    Color32::from_rgb(0xd9, 0x5f, 0x5f),
    Color32::from_rgb(0x9a, 0xa5, 0xb1),
    Color32::from_rgb(0xa8, 0x7c, 0x52),
];

/// AC corridors and transport corridors are different objects, not different
/// ratings of one, so they are told apart before anything else on the canvas.
/// A transport link is controllable — HVDC, or a modelled exchange — and where
/// power goes on it is a decision rather than a consequence of impedance.
const AC_COLOR: Color32 = Color32::from_rgb(0x6a, 0x74, 0x82);
const TRANSPORT_COLOR: Color32 = Color32::from_rgb(0x3f, 0x93, 0x8c);
const LINK_COLOR: Color32 = Color32::from_rgb(0x86, 0x6c, 0xa8);
const SHED_COLOR: Color32 = Color32::from_rgb(0xff, 0x5c, 0x4d);

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
}

impl Default for NetworkView {
    fn default() -> Self {
        Self {
            zoom: 400.0,
            centre: Pos2::ZERO,
            needs_fit: true,
            selected: None,
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

    /// `peak_shed` is per bus, empty when nothing has been solved. Precomputed
    /// by the caller rather than derived here: it is a reduction over every
    /// snapshot, and doing that per frame would make the cost of drawing scale
    /// with the length of the horizon for a picture that does not change.
    pub fn ui(&mut self, ui: &mut Ui, net: Option<&Network>, layout: &[Pos2], peak_shed: &[f64]) {
        let (response, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        let rect = response.rect;
        let painter = painter.with_clip_rect(rect);

        painter.rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);

        let Some(net) = net.filter(|_| !layout.is_empty()) else {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Drop a network file here",
                FontId::proportional(15.0),
                ui.visuals().weak_text_color(),
            );
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

        // Model-space bounds of what is on screen, so everything outside can be
        // dropped before a shape is built. Egui pays per emitted shape whether
        // or not it lands in the clip rect, and at a zoom that shows one
        // substation of a national model that is most of the network.
        let visible =
            Rect::from_min_max(self.model_of(rect, rect.min), self.model_of(rect, rect.max));

        self.draw_edges(&painter, rect, visible, net, layout);
        self.draw_buses(
            ui, &painter, rect, visible, net, layout, peak_shed, &response,
        );
    }

    fn handle_camera(&mut self, ui: &Ui, response: &eframe::egui::Response, rect: Rect) {
        if response.dragged() {
            self.centre -= response.drag_delta() / self.zoom;
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
        let before = self.model_of(rect, anchor);
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let after = self.model_of(rect, anchor);
        // Hold the model point under the cursor still, which is what makes
        // zooming feel like moving a camera rather than resizing a picture.
        self.centre += before - after;
    }

    fn draw_edges(
        &self,
        painter: &eframe::egui::Painter,
        rect: Rect,
        visible: Rect,
        net: &Network,
        layout: &[Pos2],
    ) {
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
            painter.add(eframe::egui::Shape::line(
                self.tapped(a, b),
                Stroke::new(width, color),
            ));
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
            painter.add(eframe::egui::Shape::line(
                self.tapped(a, b),
                Stroke::new(width, LINK_COLOR),
            ));
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
    fn draw_buses(
        &mut self,
        ui: &Ui,
        painter: &eframe::egui::Painter,
        rect: Rect,
        visible: Rect,
        net: &Network,
        layout: &[Pos2],
        peak_shed: &[f64],
        response: &eframe::egui::Response,
    ) {
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
                SHED_COLOR
            } else {
                crate::theme::INK
            };

            let bar = Rect::from_center_size(s, vec2(half * 2.0, thickness));
            painter.rect_filled(bar, 0.0, ink);

            // Injection above the bar, withdrawal below — the convention a
            // one-line diagram uses, and it means a glance tells you where
            // power enters and where it leaves without reading a legend.
            if has_gen[b] {
                painter.circle_filled(
                    s + vec2(0.0, -(thickness * 0.5 + 4.0)),
                    (thickness * 0.9).max(2.0),
                    ink,
                );
            }
            if has_load[b] {
                let base = s + vec2(0.0, thickness * 0.5);
                painter.line_segment(
                    [base, base + vec2(0.0, 5.0)],
                    Stroke::new(thickness * 0.7, ink),
                );
            }

            // Where the system failed, which the domain model treats as the
            // useful half of an infeasible answer rather than a footnote.
            if shed {
                painter.rect_stroke(
                    bar.expand(3.0),
                    1.0,
                    Stroke::new(1.5, SHED_COLOR),
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

        // The selection is drawn whether or not the pointer is still on it.
        if let Some(sel) = self.selected.filter(|&b| b < net.buses.len())
            && let Some(&p) = layout.get(sel)
            && visible.contains(p)
        {
            let s = self.screen_of(rect, p);
            let bar = Rect::from_center_size(s, vec2(half * 2.0, thickness));
            painter.rect_stroke(
                bar.expand(5.0),
                1.0,
                Stroke::new(1.5, crate::theme::INK_STRONG),
                eframe::egui::StrokeKind::Outside,
            );
        }

        let Some((picked, _)) = best else {
            return;
        };
        let bus = &net.buses[picked];
        let s = self.screen_of(rect, layout[picked]);
        let half = (self.zoom * 0.022).clamp(6.0, 30.0);
        let thickness = (half * 0.30).clamp(3.0, 8.0);
        painter.rect_stroke(
            Rect::from_center_size(s, vec2(half * 2.0, thickness)).expand(4.0),
            1.0,
            Stroke::new(1.5, crate::theme::INK_STRONG),
            eframe::egui::StrokeKind::Outside,
        );

        // A label painted directly rather than an egui tooltip, because the
        // canvas is one allocated widget and a tooltip would attach to the pane
        // rather than to the node the pointer is actually over.
        let label = format!("{} · {} · {}", bus.name, bus.country, bus.carrier);
        let anchor = s + vec2(half + 10.0, 0.0);
        let color = ui.visuals().strong_text_color();
        let galley = painter.layout_no_wrap(label, FontId::proportional(12.0), color);
        painter.rect_filled(
            Rect::from_min_size(anchor, galley.size()).expand(3.0),
            3.0,
            ui.visuals().panel_fill,
        );
        painter.galley(anchor, galley, color);
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
        self.centre = pos2((lo.x + hi.x) * 0.5, (lo.y + hi.y) * 0.5);
        // A tenth of the pane in margin, so nodes on the boundary are not cut
        // in half by the edge they were fitted to.
        self.zoom = (0.82 * (rect.width() / span.x.max(1e-3)).min(rect.height() / span.y.max(1e-3)))
            .clamp(MIN_ZOOM, MAX_ZOOM);
    }
}

/// FNV-1a over the country code.
///
/// Any stable hash would do. What matters is that it does not depend on process
/// state: `DefaultHasher` is seeded per process in some std versions, which
/// would repaint the same network in different colours on each launch.
pub fn country_color(country: &str) -> Color32 {
    let mut h: u32 = 0x811c_9dc5;
    for byte in country.as_bytes() {
        h ^= u32::from(*byte);
        h = h.wrapping_mul(0x0100_0193);
    }
    COUNTRY_COLORS[h as usize % COUNTRY_COLORS.len()]
}

/// Identifies one circuit, so a bus can order the things attached to it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
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

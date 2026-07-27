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
}

impl Default for NetworkView {
    fn default() -> Self {
        Self {
            zoom: 400.0,
            centre: Pos2::ZERO,
            needs_fit: true,
        }
    }
}

impl NetworkView {
    /// Refit at the next opportunity. Called when the network changes under it.
    pub fn reset(&mut self) {
        *self = Self::default();
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

        for line in &net.lines {
            let Some((a, b)) = self.segment(rect, visible, layout, line.bus0, line.bus1) else {
                continue;
            };
            let width = 0.8 + 2.4 * (line.s_nom.abs() / max_s_nom).sqrt() as f32;
            let color = if line.is_transport() {
                TRANSPORT_COLOR
            } else {
                AC_COLOR
            };
            painter.line_segment([a, b], Stroke::new(width, color));
        }

        let max_p_nom = net
            .links
            .iter()
            .map(|l| l.p_nom.abs())
            .fold(0.0_f64, f64::max)
            .max(1.0);

        for link in &net.links {
            let Some((a, b)) = self.segment(rect, visible, layout, link.bus0, link.bus1) else {
                continue;
            };
            let width = 0.8 + 2.0 * (link.p_nom.abs() / max_p_nom).sqrt() as f32;
            painter.line_segment([a, b], Stroke::new(width, LINK_COLOR));
        }
    }

    /// Screen endpoints, or `None` when the edge cannot be seen or its endpoints
    /// are out of range.
    ///
    /// Bus references are indices into a `Vec`, and `Network::validate` is what
    /// checks they are in range — the view may be handed something that was
    /// never validated, so it declines to index rather than panicking on a file
    /// somebody dragged in.
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
        &self,
        ui: &Ui,
        painter: &eframe::egui::Painter,
        rect: Rect,
        visible: Rect,
        net: &Network,
        layout: &[Pos2],
        peak_shed: &[f64],
        response: &eframe::egui::Response,
    ) {
        // Radius follows zoom weakly rather than not at all: fixed screen size
        // makes a zoomed-out national model a solid mat of overlapping dots,
        // and fixed model size makes a zoomed-in one draw circles the size of
        // the pane.
        let radius = (self.zoom * 0.006).clamp(1.5, 9.0);
        let pointer = response.hover_pos();

        let mut best: Option<(usize, f32)> = None;

        for (b, bus) in net.buses.iter().enumerate() {
            let Some(&p) = layout.get(b) else { continue };
            if !visible.expand(radius / self.zoom).contains(p) {
                continue;
            }
            let s = self.screen_of(rect, p);

            painter.circle_filled(s, radius, country_color(&bus.country));

            // Where the system failed, which the domain model treats as the
            // useful half of an infeasible answer rather than a footnote.
            if peak_shed.get(b).copied().unwrap_or(0.0) > 0.0 {
                painter.circle_stroke(s, radius + 2.0, Stroke::new(1.6, SHED_COLOR));
            }

            if let Some(ptr) = pointer {
                let d = s.distance(ptr);
                if d <= PICK_RADIUS && best.is_none_or(|(_, bd)| d < bd) {
                    best = Some((b, d));
                }
            }
        }

        let Some((picked, _)) = best else {
            return;
        };
        let bus = &net.buses[picked];
        let s = self.screen_of(rect, layout[picked]);
        painter.circle_stroke(
            s,
            radius + 3.0,
            Stroke::new(1.5, ui.visuals().strong_text_color()),
        );

        // A label painted directly rather than an egui tooltip, because the
        // canvas is one allocated widget and a tooltip would attach to the pane
        // rather than to the node the pointer is actually over.
        let label = format!("{} · {} · {}", bus.name, bus.country, bus.carrier);
        let anchor = s + vec2(radius + 6.0, 0.0);
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
        self.zoom = (0.9 * (rect.width() / span.x.max(1e-3)).min(rect.height() / span.y.max(1e-3)))
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

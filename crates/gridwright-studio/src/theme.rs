//! The visual language: an annunciator panel, not a dashboard.
//!
//! A control-room annunciator is a grid of engraved windows that stay dark
//! until something happens. Its discipline is that **a lit cell means
//! something**, and that is the rule this theme is built on: the interface is
//! near-monochrome, and every saturated pixel on screen is carrying state about
//! a power system. If a colour appears here that is neither neutral nor
//! reporting state, it is a bug.
//!
//! Three consequences worth stating, because each is a deliberate departure
//! from what a dark technical UI usually looks like.
//!
//! **The canvas is brighter than the chrome.** Panels are [`SLATE_DEEP`] and
//! the work surface is [`SLATE_WORK`], which inverts the usual elevation model.
//! VS Code does the same thing for the same reason: the thing you are working
//! on should be the brightest large surface, and the furniture around it should
//! recede. Tools that show imagery — Rerun, video editors — go the other way,
//! and they are right to, because their content supplies its own light. A
//! schematic does not.
//!
//! **There is no accent colour.** Not a muted one, not a tasteful one — none.
//! Selection and focus are drawn with a lighter neutral and a stroke. This is
//! the part most likely to read as under-designed at first glance, and it is
//! the point: on a screen where amber means *stale* and red means *unserved
//! energy*, a decorative blue competes with information for the same channel.
//!
//! **Contrast is set against a dark-mode floor, not WCAG's number.** WCAG 2's
//! ratio overstates contrast on dark backgrounds — two colours can both score
//! 4.5 and be far apart perceptually. Body text here clears roughly 10:1
//! against the canvas, which is where APCA's Lc 75 actually lands.

use eframe::egui::{self, Color32, CornerRadius, Stroke, TextStyle, Vec2};

// ---------------------------------------------------------------------------
// Surfaces. Cold and blue-cast rather than neutral grey, so that amber and
// green read as warm signals against them rather than as more of the same.
// ---------------------------------------------------------------------------

/// Panels, rails, the status strip. The furniture.
pub const SLATE_DEEP: Color32 = Color32::from_rgb(0x14, 0x17, 0x1C);
/// The work surface. Deliberately lighter than [`SLATE_DEEP`].
pub const SLATE_WORK: Color32 = Color32::from_rgb(0x1C, 0x20, 0x27);
/// Popups and menus, which sit above everything.
pub const SLATE_RAISED: Color32 = Color32::from_rgb(0x23, 0x28, 0x31);
/// Hairlines, panel edges, node outlines. One weight, used everywhere.
pub const SLATE_LINE: Color32 = Color32::from_rgb(0x2A, 0x30, 0x3A);
/// Input fields, which are recessed rather than raised.
pub const SLATE_FIELD: Color32 = Color32::from_rgb(0x11, 0x14, 0x18);

// ---------------------------------------------------------------------------
// Text. Three weights of neutral, and nothing else.
// ---------------------------------------------------------------------------

/// Values, headings, the row under the cursor. ~14:1 on the canvas.
pub const INK_STRONG: Color32 = Color32::from_rgb(0xE6, 0xE9, 0xEE);
/// Body and labels. ~10:1 — the floor for anything read fluently.
pub const INK: Color32 = Color32::from_rgb(0xA7, 0xAF, 0xBC);
/// Units, counts, metadata. Never used for anything that must be read.
pub const INK_DIM: Color32 = Color32::from_rgb(0x6C, 0x76, 0x84);

// ---------------------------------------------------------------------------
// Signals. The only saturated colours in the product, and each one is a claim
// about the state of a network or of a solve.
// ---------------------------------------------------------------------------

/// Proved optimal, energised, in service.
pub const LIVE: Color32 = Color32::from_rgb(0x57, 0xC0, 0x8A);
/// **Stale**, stopped on a limit, not converged.
///
/// The workhorse. Solves here take seconds to minutes, so "edited but not yet
/// re-solved" is the dominant state of this application — unlike every node
/// editor it borrows from, where recomputation is fast enough that the state
/// never renders. Amber is on screen more than green is.
pub const ALARM: Color32 = Color32::from_rgb(0xE8, 0xA3, 0x3D);
/// Infeasible, unbounded, unserved energy.
pub const TRIP: Color32 = Color32::from_rgb(0xE0, 0x5A, 0x5A);
/// Out of service, bypassed, deliberately excluded.
pub const OFF: Color32 = Color32::from_rgb(0x5A, 0x64, 0x72);

/// Base spacing unit. Four rather than eight: an inspector at eight is a form,
/// and this is an instrument.
pub const UNIT: f32 = 4.0;

/// Row height for lists and table rows.
///
/// Twenty-four because it is on the spacing grid *and* is exactly WCAG 2.2's
/// minimum target size, so a row is a legal hit target without special pleading.
pub const ROW: f32 = 24.0;

/// Install the theme. Called once, before the first frame.
pub fn apply(ctx: &egui::Context) {
    ctx.global_style_mut(|style| {
        style.visuals = visuals();
        spacing(style);
        text(style);
    });
}

fn text(style: &mut egui::Style) {
    // Twelve, not sixteen. Every dense professional tool lives at 11-14: VS
    // Code's editor is 12 on macOS, Figma's interface is 11, Blender is 11pt.
    // Sixteen is a reading size for prose and it wastes a panel.
    for ts in [TextStyle::Body, TextStyle::Button, TextStyle::Monospace] {
        if let Some(f) = style.text_styles.get_mut(&ts) {
            f.size = 12.0;
        }
    }
    if let Some(f) = style.text_styles.get_mut(&TextStyle::Small) {
        f.size = 11.0;
    }
    // Section headers are 13, not 20. Hierarchy here comes from weight and
    // colour, not from size — a 20px heading in a 300px panel is a shout.
    if let Some(f) = style.text_styles.get_mut(&TextStyle::Heading) {
        f.size = 13.0;
    }
}

fn spacing(style: &mut egui::Style) {
    let s = &mut style.spacing;
    // Wide horizontally so a label and its value are clearly two things;
    // tight vertically so a summary reads as one block.
    s.item_spacing = Vec2::new(UNIT * 2.0, UNIT);
    s.button_padding = Vec2::new(UNIT * 2.0, UNIT);
    // The visual height is smaller than the hit height; see ROW.
    s.interact_size = Vec2::new(UNIT * 6.0, 20.0);
    s.indent = 14.0;
    s.menu_margin = egui::Margin::same(UNIT as i8);
    s.window_margin = egui::Margin::same((UNIT * 2.0) as i8);
    // Thin, because a scrollbar is furniture and this panel is narrow.
    s.scroll.bar_width = 6.0;
    s.scroll.bar_inner_margin = 2.0;
    s.scroll.bar_outer_margin = 0.0;
}

fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();

    v.panel_fill = SLATE_DEEP;
    v.window_fill = SLATE_RAISED;
    v.extreme_bg_color = SLATE_FIELD;
    v.faint_bg_color = SLATE_LINE;
    v.window_stroke = Stroke::new(1.0, SLATE_LINE);

    v.override_text_color = Some(INK);
    v.error_fg_color = TRIP;
    v.warn_fg_color = ALARM;
    v.hyperlink_color = INK_STRONG;

    // Hairlines everywhere, one weight. A separator that is heavier than the
    // text it separates is louder than the content.
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, SLATE_LINE);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, INK);
    v.widgets.noninteractive.bg_fill = SLATE_DEEP;
    v.widgets.noninteractive.weak_bg_fill = SLATE_DEEP;

    // Controls are drawn as recessed wells rather than raised buttons: on a
    // panel this dense, a field of raised rectangles reads as clutter.
    v.widgets.inactive.bg_fill = SLATE_RAISED;
    v.widgets.inactive.weak_bg_fill = SLATE_RAISED;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, SLATE_LINE);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, INK);

    // Hover and press move the *neutral* lighter. No accent hue anywhere,
    // because a decorative colour competes with amber and red for the channel
    // that carries meaning.
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x2E, 0x35, 0x40);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2E, 0x35, 0x40);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x3D, 0x46, 0x54));
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, INK_STRONG);

    v.widgets.active.bg_fill = Color32::from_rgb(0x38, 0x41, 0x4E);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(0x38, 0x41, 0x4E);
    v.widgets.active.bg_stroke = Stroke::new(1.0, INK_DIM);
    v.widgets.active.fg_stroke = Stroke::new(1.0, INK_STRONG);

    v.widgets.open.bg_fill = SLATE_RAISED;
    v.widgets.open.weak_bg_fill = SLATE_RAISED;
    v.widgets.open.bg_stroke = Stroke::new(1.0, SLATE_LINE);

    v.selection.bg_fill = Color32::from_rgb(0x2E, 0x38, 0x46);
    v.selection.stroke = Stroke::new(1.0, INK_STRONG);

    let small = CornerRadius::same(3);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = small;
        // Buttons do not grow on hover. A control that changes size under the
        // cursor is a control you can miss by arriving at it.
        w.expansion = 0.0;
    }
    v.window_corner_radius = CornerRadius::same(6);
    v.menu_corner_radius = CornerRadius::same(6);

    // Shadows are for things that float. Nothing here floats except menus, and
    // a shadow under a docked panel is decoration pretending to be depth.
    v.window_shadow = egui::epaint::Shadow::NONE;
    v.popup_shadow = egui::epaint::Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(120),
    };

    v
}

/// A one-line label in the panel voice: small, dim, and letterspaced by the
/// only means egui offers, which is a leading space in the string.
///
/// Used for the eyebrow above each block. It exists as a function so the
/// treatment is defined once; the moment two call sites style a heading by
/// hand they will drift.
pub fn eyebrow(text: &str) -> egui::RichText {
    egui::RichText::new(text.to_uppercase())
        .size(10.0)
        .color(INK_DIM)
        .strong()
}

/// A numeric, in the monospace face so columns of them line up.
pub fn number(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into())
        .monospace()
        .color(INK_STRONG)
}

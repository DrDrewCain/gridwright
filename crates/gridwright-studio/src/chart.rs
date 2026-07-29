//! Small charts, drawn with the same painter as everything else.
//!
//! Hand-drawn rather than taken from `egui_plot`, and the reason is the visual
//! language. A plotting crate arrives with its own opinions about grid lines,
//! axis furniture, hover behaviour and default colours, and reconciling those
//! with a theme this specific costs more than the drawing does — the whole of
//! a line series is a `Shape::line` over a mapped slice.
//!
//! These are also *small*. Nothing here is a general plotting library: no
//! zooming, no panning, no legends, no second axis. They are the size that fits
//! in a side panel beside the thing they describe, which is the size the
//! evidence says works — small multiples beat one large interactive plot for
//! analysis, in three separate replications.

use eframe::egui::{self, Color32, Pos2, Rect, Stroke, pos2, vec2};

use crate::theme;

/// The frame a series is drawn in: a rectangle and the value range it spans.
pub struct Axes {
    pub rect: Rect,
    lo: f64,
    hi: f64,
}

impl Axes {
    /// Fit to the data, with the zero line included when the data crosses it.
    ///
    /// Not always zero-based. A price series that runs from 78 to 92 per MWh
    /// drawn against a zero baseline is a flat line at the top of the frame,
    /// and the variation — which is the entire content — disappears. Truncated
    /// axes mislead when the reader is comparing *magnitudes* between bars;
    /// these are time series, where the shape is the point.
    pub fn fit(rect: Rect, series: &[f64]) -> Self {
        let (mut lo, mut hi) = series
            .iter()
            .filter(|v| v.is_finite())
            .fold((f64::MAX, f64::MIN), |(l, h), &v| (l.min(v), h.max(v)));

        if lo > hi {
            // Nothing finite in the series.
            lo = 0.0;
            hi = 1.0;
        }
        // Signed data always shows its zero, because the sign is the story.
        if lo < 0.0 {
            hi = hi.max(0.0);
        }
        // A flat series still needs a band to sit in, or every point maps to
        // the same pixel and the line vanishes.
        if (hi - lo).abs() < 1e-9 {
            let pad = hi.abs().max(1.0) * 0.1;
            lo -= pad;
            hi += pad;
        }
        Self { rect, lo, hi }
    }

    /// A second frame sharing another's value range.
    ///
    /// For drawing two views of the same quantity side by side. Refitting each
    /// independently would draw the identical range at a different height and
    /// invite a comparison between two charts that are not comparable.
    pub fn like(other: &Axes, rect: Rect) -> Self {
        Self {
            rect,
            lo: other.lo,
            hi: other.hi,
        }
    }

    /// Where a value sits vertically.
    fn y(&self, v: f64) -> f32 {
        let t = ((v - self.lo) / (self.hi - self.lo)).clamp(0.0, 1.0) as f32;
        self.rect.bottom() - t * self.rect.height()
    }

    /// Where sample `i` of `n` sits horizontally.
    ///
    /// Samples land at the *centre* of their slot rather than at the edges, so
    /// a 24-hour series reads as twenty-four hours rather than as twenty-three
    /// intervals. It also means the first and last points are not clipped by
    /// the frame.
    fn x(&self, i: usize, n: usize) -> f32 {
        let t = if n <= 1 {
            0.5
        } else {
            (i as f32 + 0.5) / n as f32
        };
        self.rect.left() + t * self.rect.width()
    }

    pub fn range(&self) -> (f64, f64) {
        (self.lo, self.hi)
    }
}

/// The frame: a baseline, and a zero line when zero is inside the data.
pub fn frame(painter: &egui::Painter, ax: &Axes) {
    painter.rect_filled(ax.rect, 0.0, theme::SLATE_FIELD);

    let (lo, hi) = ax.range();
    if lo < 0.0 && hi > 0.0 {
        let y = ax.y(0.0);
        painter.line_segment(
            [pos2(ax.rect.left(), y), pos2(ax.rect.right(), y)],
            Stroke::new(1.0, theme::SLATE_LINE),
        );
    }
}

/// A line series.
pub fn line(painter: &egui::Painter, ax: &Axes, series: &[f64], color: Color32) {
    if series.len() < 2 {
        // A single sample is a point, not a line, and a `Shape::line` of one
        // vertex draws nothing at all rather than something small.
        if let Some(&v) = series.first() {
            painter.circle_filled(pos2(ax.x(0, 1), ax.y(v)), 2.0, color);
        }
        return;
    }
    let pts: Vec<Pos2> = series
        .iter()
        .enumerate()
        .map(|(i, &v)| pos2(ax.x(i, series.len()), ax.y(v)))
        .collect();
    painter.add(egui::Shape::line(pts, Stroke::new(1.4, color)));
}

/// A marker at one sample, for showing where the scrubber is.
pub fn marker(painter: &egui::Painter, ax: &Axes, i: usize, n: usize) {
    let x = ax.x(i, n);
    painter.line_segment(
        [pos2(x, ax.rect.top()), pos2(x, ax.rect.bottom())],
        Stroke::new(1.0, theme::INK_DIM),
    );
}

/// The value range, written at the corners.
///
/// Two numbers rather than a tick ladder. At this size a ladder is more ink
/// than data, and the question a sparkline answers is "what shape, between what
/// bounds" — which two numbers answer completely.
pub fn bounds(painter: &egui::Painter, ax: &Axes, unit: &str) {
    let (lo, hi) = ax.range();
    let font = egui::FontId::proportional(9.0);
    painter.text(
        ax.rect.right_top() + vec2(-2.0, 1.0),
        egui::Align2::RIGHT_TOP,
        format!("{hi:.0}{unit}"),
        font.clone(),
        theme::INK_DIM,
    );
    painter.text(
        ax.rect.right_bottom() + vec2(-2.0, -1.0),
        egui::Align2::RIGHT_BOTTOM,
        format!("{lo:.0}"),
        font,
        theme::INK_DIM,
    );
}

/// Sort descending — a duration curve.
///
/// The standard chart of this field: how many hours the quantity spent above
/// each level, which is the question a duration curve answers and a time series
/// cannot. Returns a new vector rather than sorting in place, because the
/// caller almost always wants both.
pub fn duration(series: &[f64]) -> Vec<f64> {
    let mut out: Vec<f64> = series.iter().copied().filter(|v| v.is_finite()).collect();
    out.sort_by(|a, b| b.total_cmp(a));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ax(series: &[f64]) -> Axes {
        Axes::fit(
            Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 50.0)),
            series,
        )
    }

    #[test]
    fn a_narrow_band_is_not_flattened_against_zero() {
        // A price series from 78 to 92 drawn against a zero baseline is a flat
        // line at the top of the frame, and the variation is the whole content.
        let a = ax(&[78.0, 85.0, 92.0]);
        assert_eq!(a.range(), (78.0, 92.0));
        // The extremes reach the edges rather than clustering in a tenth of it.
        assert_eq!(a.y(92.0), a.rect.top());
        assert_eq!(a.y(78.0), a.rect.bottom());
    }

    #[test]
    fn signed_data_always_shows_its_zero() {
        // Flow has a direction, and a chart of it that omits zero hides where
        // the direction reversed.
        let (lo, hi) = ax(&[-40.0, -10.0, -5.0]).range();
        assert!(lo < 0.0 && hi >= 0.0, "got {lo} to {hi}");
    }

    #[test]
    fn a_flat_series_still_gets_a_band() {
        // Without this every sample maps to one pixel and the line disappears.
        let (lo, hi) = ax(&[50.0, 50.0, 50.0]).range();
        assert!(hi > lo, "a flat series collapsed to {lo}..{hi}");
    }

    #[test]
    fn an_all_nan_series_does_not_produce_nan_geometry() {
        // Unrated lines carry NaN loading, and a NaN axis makes every later
        // coordinate NaN, which egui draws as nothing with no error.
        let a = ax(&[f64::NAN, f64::NAN]);
        assert!(a.y(0.0).is_finite());
        let (lo, hi) = a.range();
        assert!(lo.is_finite() && hi.is_finite());
    }

    #[test]
    fn an_empty_series_does_not_panic() {
        let (lo, hi) = ax(&[]).range();
        assert!(lo.is_finite() && hi.is_finite());
    }

    #[test]
    fn samples_sit_in_the_middle_of_their_slot() {
        // So a 24-hour series reads as 24 hours rather than 23 intervals, and
        // the end points are not clipped by the frame.
        let a = ax(&[0.0, 1.0]);
        assert!(a.x(0, 24) > a.rect.left());
        assert!(a.x(23, 24) < a.rect.right());
    }

    #[test]
    fn a_shared_axis_keeps_the_other_range() {
        // Two views of one quantity have to share a scale, or the reader
        // compares two heights that mean different things.
        let a = ax(&[10.0, 90.0]);
        let b = Axes::like(&a, Rect::from_min_size(pos2(0.0, 60.0), vec2(100.0, 20.0)));
        assert_eq!(a.range(), b.range());
        assert_ne!(a.rect, b.rect);
    }

    #[test]
    fn a_duration_curve_is_the_series_sorted_downward() {
        assert_eq!(duration(&[3.0, 1.0, 2.0]), vec![3.0, 2.0, 1.0]);
    }

    #[test]
    fn a_duration_curve_drops_the_values_that_are_not_numbers() {
        // NaN in a comparison sort is how you get a silently scrambled series.
        assert_eq!(duration(&[3.0, f64::NAN, 1.0]), vec![3.0, 1.0]);
    }
}

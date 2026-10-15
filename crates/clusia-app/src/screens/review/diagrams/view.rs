//! Where a diagram sits in its canvas: the zoom, the pan and the clamps. It is plain data so it
//! is tested without a window; `canvas` writes the result into a `UiTransform`.
//!
//! The diagram's content is a node of its own size that `UiTransform` scales around its centre,
//! so `translation` moves that centre back: a content point `q` is drawn at `pan + zoom × q`.

use bevy::math::{Rect, Vec2};

pub const ZOOM_MIN: f32 = 0.25;
pub const ZOOM_MAX: f32 = 2.0;
/// What **−** and **+** multiply the zoom by.
pub const BUTTON_STEP: f32 = 1.25;
/// Space `fit` keeps free at the top (the header) and the bottom (the legend).
pub const TOP_INSET: f32 = 56.0;
pub const BOTTOM_INSET: f32 = 56.0;
/// How much of the content stays on screen when it is panned away.
const MIN_VISIBLE: f32 = 80.0;
/// Space `reveal` leaves between the revealed box and the edge of the canvas.
const REVEAL_MARGIN: f32 = 24.0;
/// What one line of a mouse wheel counts as, in pixels.
const LINE_PIXELS: f32 = 20.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasView {
    pub zoom: f32,
    /// Where the content's top-left corner is drawn, in canvas pixels.
    pub pan: Vec2,
    /// The area the content is shown in.
    pub viewport: Vec2,
    /// The content's size at 100 %.
    pub content: Vec2,
}

impl CanvasView {
    pub fn new(content: Vec2) -> Self {
        Self {
            zoom: 1.0,
            pan: Vec2::ZERO,
            viewport: Vec2::ZERO,
            content,
        }
    }

    /// Fits the whole content between the header and the legend, never above 100 %, centred.
    pub fn fit(&mut self) {
        if self.viewport.x <= 0.0 || self.viewport.y <= 0.0 || self.content.min_element() <= 0.0 {
            return;
        }
        let room = Vec2::new(
            self.viewport.x,
            (self.viewport.y - TOP_INSET - BOTTOM_INSET).max(self.viewport.y * 0.5),
        );
        self.zoom = (room.x / self.content.x)
            .min(room.y / self.content.y)
            .clamp(ZOOM_MIN, 1.0);
        let free = room - self.content * self.zoom;
        let top = if room.y < self.viewport.y {
            TOP_INSET
        } else {
            0.0
        };
        self.pan = Vec2::new(free.x / 2.0, top + free.y / 2.0);
        self.clamp_pan();
    }

    /// Zooms by `factor` with the point under `cursor` (in canvas pixels) staying put.
    pub fn zoom_at(&mut self, cursor: Vec2, factor: f32) {
        let zoom = (self.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        if (zoom - self.zoom).abs() < f32::EPSILON {
            return;
        }
        let ratio = zoom / self.zoom;
        self.pan = cursor - (cursor - self.pan) * ratio;
        self.zoom = zoom;
        self.clamp_pan();
    }

    pub fn pan_by(&mut self, delta: Vec2) {
        self.pan += delta;
        self.clamp_pan();
    }

    pub fn set_viewport(&mut self, viewport: Vec2) {
        self.viewport = viewport;
        self.clamp_pan();
    }

    /// Moves the content the least that brings `rect` (in content pixels) into the canvas, with a
    /// margin, clear of the header and legend bands `fit` keeps free. A box taller or wider than
    /// the canvas shows its top-left corner.
    pub fn reveal(&mut self, rect: Rect) {
        for axis in 0..2 {
            let (top, bottom) = if axis == 1 {
                (TOP_INSET, BOTTOM_INSET)
            } else {
                (0.0, 0.0)
            };
            let start = REVEAL_MARGIN.max(top);
            let end = self.viewport[axis] - REVEAL_MARGIN.max(bottom);
            let far = self.pan[axis] + rect.max[axis] * self.zoom;
            if far > end {
                self.pan[axis] -= far - end;
            }
            let near = self.pan[axis] + rect.min[axis] * self.zoom;
            if near < start {
                self.pan[axis] += start - near;
            }
        }
        self.clamp_pan();
    }

    /// The `UiTransform` translation that draws the content as `pan` and `zoom` say.
    pub fn translation(&self) -> Vec2 {
        self.pan - self.content / 2.0 * (1.0 - self.zoom)
    }

    pub fn percent(&self) -> u32 {
        (self.zoom * 100.0).round() as u32
    }

    fn clamp_pan(&mut self) {
        let size = self.content * self.zoom;
        for axis in 0..2 {
            let keep = MIN_VISIBLE.min(size[axis]);
            let lo = keep - size[axis];
            let hi = (self.viewport[axis] - keep).max(lo);
            self.pan[axis] = self.pan[axis].clamp(lo, hi);
        }
    }
}

/// A scroll amount in pixels: a line of a mouse wheel counts as 20.
pub fn wheel_pixels(line: bool, amount: f32) -> f32 {
    if line { amount * LINE_PIXELS } else { amount }
}

/// The zoom factor of ⌘ + scroll over `pixels`: scrolling up zooms in, 100 px by about 28 %.
pub fn wheel_factor(pixels: f32) -> f32 {
    (pixels * 0.0025).exp()
}

/// The zoom factor of a trackpad pinch of `delta` (positive spreads the fingers).
pub fn pinch_factor(delta: f32) -> f32 {
    (1.0 + delta).clamp(0.5, 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(content: (f32, f32), viewport: (f32, f32)) -> CanvasView {
        let mut v = CanvasView::new(Vec2::new(content.0, content.1));
        v.set_viewport(Vec2::new(viewport.0, viewport.1));
        v
    }

    /// Where the content point `q` is drawn.
    fn drawn(v: &CanvasView, q: Vec2) -> Vec2 {
        let centre = v.content / 2.0;
        centre + v.zoom * (q - centre) + v.translation()
    }

    #[test]
    fn a_small_diagram_is_fitted_at_100_percent_and_centred_between_the_insets() {
        let mut v = view((400.0, 200.0), (1000.0, 600.0));
        v.fit();
        assert_eq!(v.zoom, 1.0, "never above 100 %");
        assert_eq!(v.percent(), 100);
        let room = 600.0 - TOP_INSET - BOTTOM_INSET;
        assert_eq!(v.pan, Vec2::new(300.0, TOP_INSET + (room - 200.0) / 2.0));
    }

    #[test]
    fn a_big_diagram_is_fitted_down_to_what_the_canvas_holds() {
        let mut v = view((2000.0, 1000.0), (1000.0, 600.0));
        v.fit();
        let room_y = 600.0 - TOP_INSET - BOTTOM_INSET;
        assert!((v.zoom - (1000.0f32 / 2000.0).min(room_y / 1000.0)).abs() < 1e-6);
        assert!(v.zoom < 0.5 + 1e-6);
        let size = v.content * v.zoom;
        assert!(v.pan.x >= 0.0 && v.pan.x + size.x <= 1000.0 + 1e-3);
        assert!(v.pan.y >= TOP_INSET - 1e-3 && v.pan.y + size.y <= 600.0 - BOTTOM_INSET + 1e-3);
    }

    #[test]
    fn fit_never_goes_under_the_smallest_zoom() {
        let mut v = view((20_000.0, 10_000.0), (1000.0, 600.0));
        v.fit();
        assert_eq!(v.zoom, ZOOM_MIN);
    }

    #[test]
    fn fit_without_a_viewport_does_nothing() {
        let mut v = CanvasView::new(Vec2::new(400.0, 200.0));
        let before = v;
        v.fit();
        assert_eq!(v, before);
    }

    #[test]
    fn the_translation_draws_the_content_where_pan_and_zoom_say() {
        let mut v = view((400.0, 200.0), (1000.0, 600.0));
        v.pan = Vec2::new(120.0, 80.0);
        v.zoom = 0.5;
        assert_eq!(drawn(&v, Vec2::ZERO), Vec2::new(120.0, 80.0));
        assert_eq!(drawn(&v, Vec2::new(400.0, 200.0)), Vec2::new(320.0, 180.0));
        v.zoom = 1.0;
        assert_eq!(v.translation(), v.pan);
    }

    #[test]
    fn zooming_keeps_the_point_under_the_cursor() {
        let mut v = view((800.0, 500.0), (1000.0, 600.0));
        v.pan = Vec2::new(100.0, 60.0);
        let cursor = Vec2::new(420.0, 300.0);
        let q = (cursor - v.pan) / v.zoom;
        v.zoom_at(cursor, 1.25);
        assert!((v.zoom - 1.25).abs() < 1e-6);
        assert!((drawn(&v, q) - cursor).length() < 1e-3);
        v.zoom_at(cursor, 0.5);
        assert!((drawn(&v, q) - cursor).length() < 1e-3);
    }

    #[test]
    fn zoom_stays_between_25_and_200_percent() {
        let mut v = view((800.0, 500.0), (1000.0, 600.0));
        for _ in 0..20 {
            v.zoom_at(Vec2::new(500.0, 300.0), 1.25);
        }
        assert_eq!((v.zoom, v.percent()), (ZOOM_MAX, 200));
        for _ in 0..40 {
            v.zoom_at(Vec2::new(500.0, 300.0), 0.8);
        }
        assert_eq!((v.zoom, v.percent()), (ZOOM_MIN, 25));
        let pan = v.pan;
        v.zoom_at(Vec2::new(500.0, 300.0), 0.8);
        assert_eq!(
            v.pan, pan,
            "a zoom that clamps to the same value moves nothing"
        );
    }

    #[test]
    fn the_content_cannot_be_panned_out_of_reach() {
        let mut v = view((800.0, 500.0), (1000.0, 600.0));
        v.pan_by(Vec2::new(-10_000.0, -10_000.0));
        assert_eq!(v.pan, Vec2::new(80.0 - 800.0, 80.0 - 500.0));
        v.pan_by(Vec2::new(10_000.0, 10_000.0));
        assert_eq!(v.pan, Vec2::new(1000.0 - 80.0, 600.0 - 80.0));
        let mut tiny = view((30.0, 20.0), (1000.0, 600.0));
        tiny.pan_by(Vec2::new(-5_000.0, -5_000.0));
        assert_eq!(
            tiny.pan,
            Vec2::new(0.0, 0.0),
            "a content smaller than the margin stays whole"
        );
    }

    #[test]
    fn a_smaller_viewport_pulls_the_content_back() {
        let mut v = view((800.0, 500.0), (1000.0, 600.0));
        v.pan = Vec2::new(900.0, 500.0);
        v.set_viewport(Vec2::new(400.0, 300.0));
        assert_eq!(v.pan, Vec2::new(320.0, 220.0));
    }

    #[test]
    fn reveal_moves_the_least_that_shows_a_box() {
        let mut v = view((2000.0, 1000.0), (800.0, 500.0));
        v.pan = Vec2::ZERO;
        v.reveal(Rect::new(1500.0, 100.0, 1700.0, 200.0));
        assert_eq!(v.pan.x, 800.0 - REVEAL_MARGIN - 1700.0);
        assert_eq!(v.pan.y, 0.0, "the box was already on screen vertically");
        let mut w = view((2000.0, 1000.0), (800.0, 500.0));
        w.pan = Vec2::new(-1000.0, -500.0);
        w.reveal(Rect::new(40.0, 40.0, 240.0, 128.0));
        assert_eq!(w.pan, Vec2::new(REVEAL_MARGIN - 40.0, TOP_INSET - 40.0));
    }

    #[test]
    fn reveal_keeps_a_box_clear_of_the_header_and_the_legend() {
        let mut v = view((2000.0, 1000.0), (800.0, 500.0));
        v.pan = Vec2::ZERO;
        v.reveal(Rect::new(100.0, 430.0, 300.0, 480.0));
        assert_eq!(v.pan.y, 500.0 - BOTTOM_INSET - 480.0, "above the legend");
        v.pan = Vec2::new(0.0, -300.0);
        v.reveal(Rect::new(100.0, 300.0, 300.0, 340.0));
        assert_eq!(v.pan.y, TOP_INSET - 300.0, "under the header");
    }

    #[test]
    fn a_box_bigger_than_the_canvas_shows_its_corner() {
        let mut v = view((3000.0, 3000.0), (800.0, 500.0));
        v.reveal(Rect::new(100.0, 100.0, 2000.0, 2000.0));
        assert_eq!(v.pan, Vec2::new(REVEAL_MARGIN - 100.0, TOP_INSET - 100.0));
    }

    #[test]
    fn reveal_follows_the_zoom() {
        let mut v = view((2000.0, 1000.0), (800.0, 500.0));
        v.zoom = 0.5;
        v.pan = Vec2::ZERO;
        v.reveal(Rect::new(1400.0, 0.0, 1600.0, 100.0));
        assert_eq!(v.pan.x, 800.0 - REVEAL_MARGIN - 1600.0 * 0.5);
    }

    #[test]
    fn the_wheel_pans_in_pixels_and_zooms_smoothly() {
        assert_eq!(wheel_pixels(true, 2.0), 40.0);
        assert_eq!(wheel_pixels(false, 7.0), 7.0);
        assert!((wheel_factor(0.0) - 1.0).abs() < 1e-6);
        assert!(wheel_factor(100.0) > 1.2 && wheel_factor(100.0) < 1.3);
        assert!((wheel_factor(100.0) * wheel_factor(-100.0) - 1.0).abs() < 1e-5);
        assert_eq!(pinch_factor(0.1), 1.1);
        assert_eq!(pinch_factor(-5.0), 0.5);
        assert_eq!(pinch_factor(5.0), 2.0);
    }
}

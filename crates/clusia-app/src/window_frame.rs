//! Where the window opens: filling the visible area of a display the first time, then where the
//! user left it, remembered per display. All geometry is in points with the origin at the
//! top-left of the primary display; the platform layer converts from and to the system's own.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value, json};

/// The smallest window (`app::window`'s resize constraints).
pub const MIN_WIDTH: f64 = 960.0;
pub const MIN_HEIGHT: f64 = 640.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    fn overlap(&self, other: &Rect) -> f64 {
        let w = (self.x + self.w).min(other.x + other.w) - self.x.max(other.x);
        let h = (self.y + self.h).min(other.y + other.h) - self.y.max(other.y);
        if w > 0.0 && h > 0.0 { w * h } else { 0.0 }
    }

    /// This frame moved and shrunk, if need be, to lie inside `area`.
    fn inside(&self, area: &Rect) -> Rect {
        let w = self.w.min(area.w);
        let h = self.h.min(area.h);
        Rect {
            x: self.x.clamp(area.x, area.x + area.w - w),
            y: self.y.clamp(area.y, area.y + area.h - h),
            w,
            h,
        }
    }
}

/// A display and the part of it a window may use (without the menu bar and the Dock).
#[derive(Debug, Clone, PartialEq)]
pub struct Display {
    pub id: String,
    pub visible: Rect,
    /// Device pixels per point.
    pub scale: f64,
}

/// The last frame the window had on each display, and the display it was last on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Remembered {
    frames: BTreeMap<String, Rect>,
    last: Option<String>,
}

impl Remembered {
    /// What `path` holds; nothing when it is missing or unreadable.
    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let Ok(Value::Object(root)) = serde_json::from_str::<Value>(&text) else {
            return Self::default();
        };
        let number = |v: &Value, key: &str| v.get(key).and_then(Value::as_f64);
        let frames = root
            .get("frames")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(id, v)| {
                        let r = Rect::new(
                            number(v, "x")?,
                            number(v, "y")?,
                            number(v, "w")?,
                            number(v, "h")?,
                        );
                        (r.w >= MIN_WIDTH && r.h >= MIN_HEIGHT).then(|| (id.clone(), r))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let last = root.get("last").and_then(Value::as_str).map(str::to_owned);
        Self { frames, last }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let frames: serde_json::Map<String, Value> = self
            .frames
            .iter()
            .map(|(id, r)| (id.clone(), json!({"x": r.x, "y": r.y, "w": r.w, "h": r.h})))
            .collect();
        let text = json!({"frames": frames, "last": self.last});
        std::fs::write(path, serde_json::to_string_pretty(&text)?)
    }

    /// The display `frame` overlaps most.
    pub fn display_of<'a>(frame: &Rect, displays: &'a [Display]) -> Option<&'a Display> {
        displays
            .iter()
            .map(|d| (d, d.visible.overlap(frame)))
            .filter(|(_, area)| *area > 0.0)
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(d, _)| d)
    }

    /// Notes that the window now sits at `frame`, on the display it overlaps most. A frame on no
    /// display, or smaller than the window may be, is not worth remembering.
    pub fn record(&mut self, frame: Rect, displays: &[Display]) {
        if frame.w < MIN_WIDTH || frame.h < MIN_HEIGHT {
            return;
        }
        if let Some(display) = Self::display_of(&frame, displays) {
            self.frames.insert(display.id.clone(), frame);
            self.last = Some(display.id.clone());
        }
    }

    /// Where the window opens: its last frame on the display it was last on, else on another
    /// display it was remembered on, else filling the first display (the primary one).
    /// `None` without displays.
    pub fn placement(&self, displays: &[Display]) -> Option<Rect> {
        let main = displays.first()?;
        let remembered = |id: &str| {
            let display = displays.iter().find(|d| d.id == id)?;
            let frame = self.frames.get(id)?;
            Some(frame.inside(&display.visible))
        };
        self.last
            .as_deref()
            .and_then(remembered)
            .or_else(|| displays.iter().find_map(|d| remembered(&d.id)))
            .or(Some(main.visible))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: &str, x: f64, y: f64, w: f64, h: f64) -> Display {
        Display {
            id: id.into(),
            visible: Rect::new(x, y, w, h),
            scale: 2.0,
        }
    }

    /// A laptop panel under its menu bar and a monitor to its right.
    fn desk() -> Vec<Display> {
        vec![
            display("laptop", 0.0, 25.0, 1512.0, 920.0),
            display("monitor", 1512.0, 0.0, 2560.0, 1415.0),
        ]
    }

    #[test]
    fn the_first_launch_fills_the_visible_area_of_the_main_display() {
        let at = Remembered::default().placement(&desk());
        assert_eq!(at, Some(Rect::new(0.0, 25.0, 1512.0, 920.0)));
        assert_eq!(Remembered::default().placement(&[]), None);
    }

    #[test]
    fn the_window_returns_to_the_display_and_place_it_was_left_on() {
        let mut saved = Remembered::default();
        let left = Rect::new(1700.0, 100.0, 1400.0, 900.0);
        saved.record(left, &desk());
        assert_eq!(saved.placement(&desk()), Some(left));
    }

    #[test]
    fn each_display_keeps_its_own_frame() {
        let mut saved = Remembered::default();
        let on_laptop = Rect::new(40.0, 60.0, 1100.0, 700.0);
        let on_monitor = Rect::new(1800.0, 80.0, 1600.0, 1000.0);
        saved.record(on_laptop, &desk());
        saved.record(on_monitor, &desk());
        assert_eq!(saved.placement(&desk()), Some(on_monitor), "the last one");
        assert_eq!(
            saved.placement(&desk()[..1]),
            Some(on_laptop),
            "the monitor is gone: the laptop's own frame, not the monitor's"
        );
    }

    #[test]
    fn a_display_that_is_gone_falls_back_to_filling_the_screen() {
        let mut saved = Remembered::default();
        saved.record(Rect::new(1800.0, 80.0, 1600.0, 1000.0), &desk());
        let laptop_only = &desk()[..1];
        assert_eq!(
            saved.placement(laptop_only),
            Some(Rect::new(0.0, 25.0, 1512.0, 920.0))
        );
    }

    #[test]
    fn a_frame_larger_than_its_display_is_pulled_back_inside() {
        let mut saved = Remembered::default();
        saved.record(Rect::new(1800.0, 80.0, 2400.0, 1300.0), &desk());
        let smaller = vec![
            display("laptop", 0.0, 25.0, 1512.0, 920.0),
            display("monitor", 1512.0, 0.0, 1920.0, 1080.0),
        ];
        assert_eq!(
            saved.placement(&smaller),
            Some(Rect::new(1512.0, 0.0, 1920.0, 1080.0)),
            "shrunk to the display and kept in it"
        );
    }

    #[test]
    fn a_frame_on_no_display_or_too_small_is_not_remembered() {
        let mut saved = Remembered::default();
        saved.record(Rect::new(9000.0, 0.0, 1200.0, 800.0), &desk());
        saved.record(Rect::new(10.0, 40.0, 300.0, 200.0), &desk());
        assert_eq!(saved, Remembered::default());
    }

    #[test]
    fn the_frames_survive_a_restart_and_a_bad_file_reads_as_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("window.json");
        let mut saved = Remembered::default();
        saved.record(Rect::new(1700.0, 100.0, 1400.0, 900.0), &desk());
        saved.record(Rect::new(40.0, 60.0, 1100.0, 700.0), &desk());
        saved.save(&file).unwrap();
        assert_eq!(Remembered::load(&file), saved);
        assert_eq!(
            Remembered::load(&dir.path().join("missing.json")),
            Remembered::default()
        );
        std::fs::write(&file, "{ not json").unwrap();
        assert_eq!(Remembered::load(&file), Remembered::default());
        std::fs::write(
            &file,
            r#"{"frames":{"laptop":{"x":0,"y":0,"w":10,"h":10}}}"#,
        )
        .unwrap();
        assert_eq!(Remembered::load(&file), Remembered::default(), "tiny frame");
    }
}

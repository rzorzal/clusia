//! Tells a real render from a black or flat frame. A locked screen renders every window black, and
//! a black PNG is still tens of kilobytes, so only the pixels can say it is wrong.

use std::path::Path;
use std::process::Command;

use image::RgbImage;

/// Real demo renders measure a spread of 21 or more and a top colour under 0.53 of the pixels.
/// A render whose luminance spread is below this (0–255 scale) shows no content.
const MIN_LUMINANCE_STDDEV: f32 = 6.0;
/// A render where one colour covers more than this share of the pixels shows no content.
const MAX_SINGLE_COLOUR_SHARE: f32 = 0.98;
/// Mean luminance under this is black rather than merely dark.
const BLACK_MEAN: f32 = 4.0;

/// What the pixels of a render add up to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    pub mean: f32,
    pub stddev: f32,
    /// The share of pixels that have the most common colour.
    pub top_share: f32,
}

/// Why a render cannot be a real screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    Black,
    Flat,
}

impl Fault {
    /// The message for the owner; `locked` is whether the session reports a locked screen.
    pub fn message(self, locked: bool) -> String {
        match (self, locked) {
            (Fault::Black, true) => "the screen looks locked: the render is black \
                (the session reports CGSSessionScreenIsLocked); unlock it and run again"
                .into(),
            (Fault::Black, false) => "the screen looks locked: the render is black \
                (the session does not report a lock, so check for a screensaver or a sleeping display)"
                .into(),
            (Fault::Flat, true) => "the screen looks locked: the render is a single flat colour \
                (the session reports CGSSessionScreenIsLocked); unlock it and run again"
                .into(),
            (Fault::Flat, false) => {
                "the render is a single flat colour, so nothing was drawn".into()
            }
        }
    }
}

pub fn stats(img: &RgbImage) -> Stats {
    let mut counts: std::collections::HashMap<[u8; 3], u32> = std::collections::HashMap::new();
    let (mut sum, mut sum_sq) = (0f64, 0f64);
    for p in img.pixels() {
        *counts.entry(p.0).or_default() += 1;
        let l = luma(p.0) as f64;
        sum += l;
        sum_sq += l * l;
    }
    let n = (img.width() as f64 * img.height() as f64).max(1.0);
    let mean = sum / n;
    let top = counts.values().copied().max().unwrap_or(0) as f64;
    Stats {
        mean: mean as f32,
        stddev: ((sum_sq / n - mean * mean).max(0.0)).sqrt() as f32,
        top_share: (top / n) as f32,
    }
}

fn luma([r, g, b]: [u8; 3]) -> f32 {
    0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32
}

pub fn verdict(s: Stats) -> Result<(), Fault> {
    if s.stddev >= MIN_LUMINANCE_STDDEV && s.top_share <= MAX_SINGLE_COLOUR_SHARE {
        return Ok(());
    }
    Err(if s.mean < BLACK_MEAN {
        Fault::Black
    } else {
        Fault::Flat
    })
}

/// Checks a saved PNG; the error is the full message, naming the lock when the session reports one.
pub fn check_png(path: &Path) -> Result<(), String> {
    let img = image::open(path)
        .map_err(|e| format!("{}: cannot read the render: {e}", path.display()))?
        .to_rgb8();
    verdict(stats(&img))
        .map_err(|fault| format!("{}: {}", path.display(), fault.message(screen_locked())))
}

/// Whether macOS reports the screen as locked. The key only exists in the console-user entry while
/// the lock is up.
pub fn screen_locked() -> bool {
    Command::new("ioreg")
        .args(["-n", "Root", "-d1", "-w0"])
        .output()
        .map(|o| reports_locked(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or(false)
}

fn reports_locked(ioreg: &str) -> bool {
    ioreg
        .split("\"CGSSessionScreenIsLocked\"")
        .nth(1)
        .is_some_and(|rest| rest.trim_start_matches([' ', '=']).starts_with("Yes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn flat(c: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(200, 120, Rgb(c))
    }

    /// A page-like frame: a background, a header band and rows of text-like strokes.
    fn page(bg: [u8; 3], ink: [u8; 3]) -> RgbImage {
        let mut img = flat(bg);
        for y in 0..120u32 {
            for x in 0..200u32 {
                if y < 16 || (y % 10 < 3 && (20..170).contains(&x) && y > 24) {
                    img.put_pixel(x, y, Rgb(ink));
                }
            }
        }
        img
    }

    #[test]
    fn a_black_frame_is_black() {
        assert_eq!(verdict(stats(&flat([0, 0, 0]))), Err(Fault::Black));
    }

    #[test]
    fn a_flat_colour_frame_is_flat() {
        assert_eq!(verdict(stats(&flat([240, 240, 235]))), Err(Fault::Flat));
        assert_eq!(verdict(stats(&flat([40, 60, 200]))), Err(Fault::Flat));
    }

    #[test]
    fn a_nearly_black_frame_with_noise_is_still_black() {
        let mut img = flat([0, 0, 0]);
        img.put_pixel(3, 3, Rgb([2, 2, 2]));
        assert_eq!(verdict(stats(&img)), Err(Fault::Black));
    }

    #[test]
    fn light_and_dark_pages_pass() {
        assert_eq!(verdict(stats(&page([246, 246, 242], [30, 30, 30]))), Ok(()));
        assert_eq!(verdict(stats(&page([22, 22, 26], [225, 225, 225]))), Ok(()));
    }

    /// A light frame whose first `ink` of every 1000 pixels (in row order) are dark.
    fn sparse(ink: usize) -> RgbImage {
        let mut img = flat([246, 246, 242]);
        for (i, p) in img.pixels_mut().enumerate() {
            if i % 1000 < ink {
                *p = Rgb([30, 30, 30]);
            }
        }
        img
    }

    #[test]
    fn a_sparse_page_passes_at_the_edge_of_the_single_colour_share() {
        assert_eq!(verdict(stats(&sparse(25))), Ok(()));
        assert_eq!(verdict(stats(&sparse(15))), Err(Fault::Flat));
    }

    #[test]
    fn the_message_says_the_screen_looks_locked_and_names_the_lock_when_known() {
        let known = Fault::Black.message(true);
        assert!(known.contains("the screen looks locked: the render is black"));
        assert!(known.contains("CGSSessionScreenIsLocked"));
        let unknown = Fault::Black.message(false);
        assert!(unknown.contains("the screen looks locked: the render is black"));
        assert!(!unknown.contains("reports CGSSessionScreenIsLocked"));
    }

    #[test]
    fn ioreg_output_tells_a_locked_session() {
        assert!(reports_locked(
            r#""IOConsoleUsers" = ({"kCGSSessionOnConsoleKey"=Yes,"CGSSessionScreenIsLocked"=Yes,"x"=1})"#
        ));
        assert!(!reports_locked(
            r#""IOConsoleUsers" = ({"kCGSSessionOnConsoleKey"=Yes,"x"=1})"#
        ));
        assert!(!reports_locked(r#""CGSSessionScreenIsLocked"=No"#));
    }

    #[test]
    fn a_saved_black_png_is_rejected_and_a_page_passes() {
        let dir = tempfile::tempdir().unwrap();
        let black = dir.path().join("black.png");
        flat([0, 0, 0]).save(&black).unwrap();
        let err = check_png(&black).unwrap_err();
        assert!(
            err.contains("the screen looks locked: the render is black"),
            "{err}"
        );
        let ok = dir.path().join("page.png");
        page([246, 246, 242], [30, 30, 30]).save(&ok).unwrap();
        assert_eq!(check_png(&ok), Ok(()));
    }
}

//! A long list of rows drawn as a window: only the rows in view, and a margin on each side, are
//! spawned; a spacer above and one below stand in for the rest. A row weighs what it measured
//! when it was last laid out at the same width, or a guess made from the code font's own
//! metrics, learned from rows that were laid out.

use std::ops::Range;

use unicode_width::UnicodeWidthChar;

use crate::ui::code::TAB;

/// How far past each edge of the view rows are still spawned, in logical pixels. A scroll
/// shorter than this finds its rows already there.
pub const MARGIN: f32 = 240.0;

/// The view's height while the body has no laid-out size yet.
pub const VIEW_GUESS: f32 = 800.0;

/// How close to an edge of the view the spawned rows may end before they are placed again, in
/// logical pixels.
pub const LOW: f32 = 80.0;

/// How far past the edge the view moves toward rows placed again reach, in logical pixels. A
/// steady scroll finds its rows there for `AHEAD - LOW` pixels, so it changes the rows on one
/// frame in that many pixels' worth of frames rather than on each.
pub const AHEAD: f32 = 720.0;

/// The rows of `heights` that a view `view` pixels high, scrolled to `scroll`, shows with
/// `MARGIN` on each side. A scroll past the end counts as the end, as the layout clamps it.
pub fn window(heights: &[f32], scroll: f32, view: f32) -> Range<usize> {
    span(heights, scroll, view, MARGIN, MARGIN)
}

/// The rows of `heights` from `above` pixels over the view's top edge to `below` pixels under
/// its bottom edge, the view as `window` takes it.
fn span(heights: &[f32], scroll: f32, view: f32, above: f32, below: f32) -> Range<usize> {
    let total: f32 = heights.iter().sum();
    let scroll = scroll.min(total - view).max(0.0);
    let (from, to) = (scroll - above, scroll + view + below);
    let mut top = 0.0;
    let mut start = None;
    let mut end = heights.len();
    for (i, h) in heights.iter().enumerate() {
        if top >= to {
            end = i;
            break;
        }
        if start.is_none() && top + h > from {
            start = Some(i);
        }
        top += h;
    }
    start.unwrap_or(end).min(end)..end
}

/// The rows to hold spawned for the view, while `held` are. `held` stay while they reach `LOW`
/// past both edges of the view. When the view nears one end of them, the rows reach `AHEAD` past
/// that edge and `LOW` past the other; past both ends, `MARGIN` past each. When the view shares
/// no row with `held` (a jump, or nothing held yet), the rows are only what the view needs and
/// `LOW` each side, so a jump spawns little and the scroll after it fills ahead.
pub fn page(heights: &[f32], scroll: f32, view: f32, held: Range<usize>) -> Range<usize> {
    let need = span(heights, scroll, view, LOW, LOW);
    if need.is_empty() {
        return need;
    }
    if need.end <= held.start || held.end <= need.start {
        return need;
    }
    match (held.start <= need.start, need.end <= held.end) {
        (true, true) => held,
        (true, false) => span(heights, scroll, view, LOW, AHEAD),
        (false, true) => span(heights, scroll, view, AHEAD, LOW),
        (false, false) => window(heights, scroll, view),
    }
}

/// Where row `index` starts: what the rows above it weigh.
pub fn top(heights: &[f32], index: usize) -> f32 {
    heights[..index.min(heights.len())].iter().sum()
}

/// The row the view's top edge at `scroll` falls in, and how far into it.
pub fn at(heights: &[f32], scroll: f32) -> (usize, f32) {
    let mut top = 0.0;
    for (i, h) in heights.iter().enumerate() {
        if scroll < top + h {
            return (i, scroll - top);
        }
        top += h;
    }
    (heights.len(), scroll - top)
}

/// A laid-out code text, as `Metrics::learn` reads it.
#[derive(Debug, Clone, Copy)]
pub struct Seen<'a> {
    pub text: &'a str,
    pub size: [f32; 2],
    pub lines: usize,
    pub boxed: f32,
    pub share: f32,
}

/// The code font as rows lay it out, measured from laid-out code: one character's advance and
/// one line's height, and how much narrower a row's code is than the row's share of the body.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub advance: f32,
    pub line: f32,
    pub gutter: f32,
}

impl Metrics {
    /// Learns the metrics from laid-out code texts. Each is seen as its text, its laid-out size
    /// and visual lines, the width of its box (logical pixels), and the width it shares out (a
    /// row's part of the body). Only a line of plain ASCII on one visual line tells the advance.
    pub fn learn<'a>(seen: impl IntoIterator<Item = Seen<'a>>) -> Option<Self> {
        let seen: Vec<Seen> = seen
            .into_iter()
            .filter(|s| s.size[0] > 0.0 && s.size[1] > 0.0 && s.lines > 0)
            .collect();
        let line = seen
            .iter()
            .map(|s| s.size[1] / s.lines as f32)
            .reduce(f32::min)?;
        let one = seen
            .iter()
            .filter(|s| {
                s.lines == 1
                    && s.text.len() >= 4
                    && s.text.is_ascii()
                    && !s.text.ends_with(char::is_whitespace)
            })
            .max_by_key(|s| s.text.len())?;
        Some(Self {
            advance: one.size[0] / one.text.len() as f32,
            line,
            gutter: one.share - one.boxed,
        })
    }

    /// How many characters a line of code holds in a row whose share of the body is `share`.
    pub fn columns(&self, share: f32) -> usize {
        ((share - self.gutter) / self.advance).floor().max(1.0) as usize
    }
}

/// The columns a character takes in monospaced code: two for a wide glyph (East Asian Width),
/// none for a combining mark, a tab as many as it shows as.
fn columns_of(c: char) -> usize {
    if c == '\t' {
        TAB.len()
    } else {
        c.width().unwrap_or(0)
    }
}

/// The visual lines `text` takes at `columns` columns a line, broken as the code is: at blanks
/// where it can, inside a word only when the word is wider than a whole line. Blanks at the end
/// of a line hang past its edge.
pub fn wrapped_lines(text: &str, columns: usize) -> usize {
    let columns = columns.max(1);
    let mut lines = 1;
    let mut used = 0;
    let mut chars = text.chars().peekable();
    while chars.peek().is_some() {
        let mut ink = 0;
        while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
            ink += columns_of(c);
        }
        let mut blank = 0;
        while let Some(c) = chars.next_if(|c| c.is_whitespace()) {
            blank += columns_of(c);
        }
        if ink == 0 || used + ink <= columns {
            used += ink + blank;
            continue;
        }
        if used > 0 {
            lines += 1;
        }
        lines += (ink.max(1) - 1) / columns;
        used = (ink - 1) % columns + 1 + blank;
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_the_view_and_a_margin_each_side() {
        let heights = vec![20.0; 100];
        // A 200 px view at 1,000 px: rows 50..60, and 12 rows of margin each side.
        assert_eq!(window(&heights, 1_000.0, 200.0), 38..72);
        assert_eq!(window(&heights, 0.0, 200.0), 0..22);
        assert_eq!(window(&heights, 1.0e6, 200.0), 78..100);
        assert_eq!(window(&[], 0.0, 200.0), 0..0);
    }

    #[test]
    fn a_window_is_kept_until_the_view_comes_near_its_edge() {
        let heights = vec![20.0; 1_000];
        // A 200 px view at 10,000 px shows rows 500..510.
        let held = 490..540;
        assert_eq!(page(&heights, 10_000.0, 200.0, held.clone()), held);
        // 600 px further the view's bottom edge is within `LOW` of the held rows' end: the window
        // reaches `AHEAD` past the bottom and `LOW` above.
        let ahead = page(&heights, 10_600.0, 200.0, held.clone());
        assert_eq!(ahead, 526..576, "{ahead:?}");
        // Back up past the start: `AHEAD` above, `LOW` below.
        let behind = page(&heights, 9_700.0, 200.0, held.clone());
        assert_eq!(behind, 449..499, "{behind:?}");
        // No row in common: only what the view needs.
        assert_eq!(page(&heights, 15_000.0, 200.0, held), 746..764);
        assert_eq!(page(&heights, 0.0, 200.0, 0..0), 0..14);
        // A view taller than the held rows on both sides: a margin each side.
        assert_eq!(page(&heights, 10_000.0, 200.0, 499..511), 488..522);
        assert_eq!(page(&[], 0.0, 200.0, 0..0), 0..0);
    }

    #[test]
    fn a_steady_scroll_places_the_window_on_few_frames() {
        let heights = vec![22.0; 1_000];
        let mut held = 0..0;
        let mut placed = 0;
        for frame in 0..240 {
            let scroll = 40.0 * frame as f32;
            let next = page(&heights, scroll, 800.0, held.clone());
            if next != held {
                placed += 1;
            }
            held = next;
            let (first, _) = at(&heights, scroll);
            let (last, _) = at(&heights, scroll + 800.0);
            assert!(
                held.contains(&first) && held.contains(&last),
                "{frame}: {held:?}"
            );
            assert!(held.len() <= 80, "{frame}: {} rows", held.len());
        }
        assert!(placed <= 20, "placed on {placed} of 240 frames");
    }

    #[test]
    fn tall_rows_take_their_height() {
        let mut heights = vec![20.0; 100];
        heights[10] = 2_000.0;
        // The view inside the tall row holds it alone, with what the margin reaches.
        let w = window(&heights, 1_000.0, 200.0);
        assert!(w.contains(&10) && w.len() <= 2, "{w:?}");
        assert_eq!(top(&heights, 11), 2_200.0);
        assert_eq!(at(&heights, 2_210.0), (11, 10.0));
    }

    #[test]
    fn a_line_wraps_at_blanks_and_inside_a_word_only_when_it_must() {
        assert_eq!(wrapped_lines("", 10), 1);
        assert_eq!(wrapped_lines("short", 10), 1);
        assert_eq!(wrapped_lines("aaaa bbbb cccc", 10), 2);
        // Blanks hang past the edge.
        assert_eq!(wrapped_lines("aaaaaaaaaa    ", 10), 1);
        assert_eq!(wrapped_lines("a ".repeat(10).trim_end(), 10), 2);
        // A word wider than a line breaks inside, on lines of its own.
        assert_eq!(wrapped_lines(&"x".repeat(25), 10), 3);
        assert_eq!(wrapped_lines(&format!("ab {}", "x".repeat(25)), 10), 4);
        // Wide glyphs take two columns, combining marks none.
        assert_eq!(wrapped_lines(&"漢".repeat(5), 10), 1);
        assert_eq!(wrapped_lines(&"漢".repeat(6), 10), 2);
        assert_eq!(wrapped_lines(&"e\u{301}".repeat(10), 10), 1);
        assert_eq!(wrapped_lines("\tabcdefg", 10), 2);
    }

    fn seen(text: &str, size: [f32; 2], lines: usize) -> Seen<'_> {
        Seen {
            text,
            size,
            lines,
            boxed: 900.0,
            share: 1_000.0,
        }
    }

    #[test]
    fn metrics_come_from_the_widest_one_line_ascii_text() {
        let m = Metrics::learn([
            seen("fn main() {", [77.0, 16.0], 1),
            seen("let x = 1;", [70.0, 16.0], 1),
            // Wrapped: it tells the line height only.
            seen(&"x".repeat(400), [900.0, 48.0], 3),
            seen("ünïcode text", [84.0, 16.0], 1),
        ])
        .expect("metrics");
        assert_eq!(m.line, 16.0);
        assert_eq!(m.advance, 7.0);
        assert_eq!(m.gutter, 100.0);
        assert_eq!(m.columns(1_000.0), 128);
        assert_eq!(Metrics::learn([seen("ab", [14.0, 16.0], 1)]), None);
        assert_eq!(
            Metrics::learn([seen(&"x".repeat(400), [900.0, 48.0], 3)]),
            None,
            "no line on its own tells the advance"
        );
    }
}

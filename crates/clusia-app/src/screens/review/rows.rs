//! A long list of rows drawn as a window: only the rows in view, and a margin on each side, are
//! spawned; a spacer above and one below stand in for the rest. A row weighs what it measured
//! when it was last laid out at the same width, or a guess made from the code font's own
//! metrics, learned from rows that were laid out.

use std::ops::Range;

/// How far past each edge of the view rows are still spawned, in logical pixels. A scroll
/// shorter than this finds its rows already there.
pub const MARGIN: f32 = 240.0;

/// The view's height while the body has no laid-out size yet.
pub const VIEW_GUESS: f32 = 800.0;

/// The rows of `heights` that a view `view` pixels high, scrolled to `scroll`, shows with
/// `MARGIN` on each side, widened to hold the row `pin`. A scroll past the end counts as the end,
/// as the layout clamps it.
pub fn window(heights: &[f32], scroll: f32, view: f32, pin: Option<usize>) -> Range<usize> {
    let total: f32 = heights.iter().sum();
    let scroll = scroll.min(total - view).max(0.0);
    let (from, to) = (scroll - MARGIN, scroll + view + MARGIN);
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
    let start = start.unwrap_or(end).min(end);
    match pin.filter(|p| *p < heights.len()) {
        Some(p) => start.min(p)..end.max(p + 1),
        None => start..end,
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

/// The visual lines `text` takes at `columns` characters a line, broken as the code is: at
/// blanks where it can, inside a word only when the word is wider than a whole line. Blanks at
/// the end of a line hang past its edge.
pub fn wrapped_lines(text: &str, columns: usize) -> usize {
    let columns = columns.max(1);
    let mut lines = 1;
    let mut used = 0;
    let mut chars = text.chars().peekable();
    while chars.peek().is_some() {
        let mut ink = 0;
        while chars.next_if(|c| !c.is_whitespace()).is_some() {
            ink += 1;
        }
        let mut blank = 0;
        while chars.next_if(|c| c.is_whitespace()).is_some() {
            blank += 1;
        }
        if used + ink <= columns {
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
        assert_eq!(window(&heights, 1_000.0, 200.0, None), 38..72);
        assert_eq!(window(&heights, 0.0, 200.0, None), 0..22);
        assert_eq!(window(&heights, 1.0e6, 200.0, None), 78..100);
        assert_eq!(window(&[], 0.0, 200.0, None), 0..0);
    }

    #[test]
    fn a_pinned_row_widens_the_window_to_reach_it() {
        let heights = vec![20.0; 100];
        assert_eq!(window(&heights, 1_000.0, 200.0, Some(5)), 5..72);
        assert_eq!(window(&heights, 0.0, 200.0, Some(90)), 0..91);
    }

    #[test]
    fn tall_rows_take_their_height() {
        let mut heights = vec![20.0; 100];
        heights[10] = 2_000.0;
        // The view inside the tall row holds it alone, with what the margin reaches.
        let w = window(&heights, 1_000.0, 200.0, None);
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

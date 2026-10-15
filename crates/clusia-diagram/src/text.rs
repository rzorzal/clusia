//! Breaking text into lines that fit a width.

use crate::geometry::{Align, Point, TextLines};
use crate::measure::{Measure, TextStyle};

/// Wraps `text` at `max_w`: at spaces, and inside a word that is wider than the line. A text
/// that needs more than `max_lines` lines is cut with `…` at the end of the last one.
pub(crate) fn wrap(
    measure: &dyn Measure,
    text: &str,
    style: TextStyle,
    max_w: f32,
    max_lines: usize,
) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if measure.width(word, style) > max_w {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            for c in word.chars() {
                let mut grown = current.clone();
                grown.push(c);
                if !current.is_empty() && measure.width(&grown, style) > max_w {
                    lines.push(std::mem::replace(&mut current, c.to_string()));
                } else {
                    current = grown;
                }
            }
            continue;
        }
        if current.is_empty() {
            current = word.to_string();
            continue;
        }
        let joined = format!("{current} {word}");
        if measure.width(&joined, style) <= max_w {
            current = joined;
        } else {
            lines.push(std::mem::replace(&mut current, word.to_string()));
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        let mut last = lines.pop().unwrap_or_default();
        last.truncate(last.trim_end().len());
        loop {
            let with_dots = format!("{last}…");
            if last.is_empty() || measure.width(&with_dots, style) <= max_w {
                lines.push(with_dots);
                break;
            }
            last.pop();
        }
    }
    lines
}

/// The widest of `lines` in px.
pub(crate) fn widest(measure: &dyn Measure, lines: &[String], style: TextStyle) -> f32 {
    lines
        .iter()
        .map(|l| measure.width(l, style))
        .fold(0.0, f32::max)
}

/// `lines` as a block at `at`; with no `width` the block is as wide as its widest line.
pub(crate) fn block(
    measure: &dyn Measure,
    lines: Vec<String>,
    style: TextStyle,
    at: Point,
    width: Option<f32>,
    align: Align,
) -> TextLines {
    let height = lines.len() as f32 * measure.line_height(style);
    let width = width.unwrap_or_else(|| widest(measure, &lines, style));
    TextLines {
        lines,
        style,
        at,
        width,
        height,
        align,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::measure::Fonts;

    #[test]
    fn words_wrap_at_the_width_and_empty_text_has_no_lines() {
        let f = Fonts;
        let lines = wrap(
            &f,
            "refresh the token before it expires",
            TextStyle::Title,
            120.0,
            4,
        );
        assert!(lines.len() >= 2);
        assert!(lines.iter().all(|l| f.width(l, TextStyle::Title) <= 120.0));
        assert_eq!(lines.join(" "), "refresh the token before it expires");
        assert!(wrap(&f, "   ", TextStyle::Title, 120.0, 4).is_empty());
        assert!(wrap(&f, "x", TextStyle::Title, 120.0, 0).is_empty());
    }

    #[test]
    fn a_word_with_no_spaces_is_broken_inside() {
        let f = Fonts;
        let word = "a".repeat(80);
        let lines = wrap(&f, &word, TextStyle::Title, 100.0, 20);
        assert!(lines.len() > 2);
        assert!(lines.iter().all(|l| f.width(l, TextStyle::Title) <= 100.0));
        assert_eq!(lines.concat(), word);
    }

    #[test]
    fn too_many_lines_end_with_an_ellipsis_that_fits() {
        let f = Fonts;
        let word = "word ".repeat(60);
        let lines = wrap(&f, &word, TextStyle::Title, 100.0, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].ends_with('…'));
        assert!(lines.iter().all(|l| f.width(l, TextStyle::Title) <= 100.0));
    }

    #[test]
    fn a_width_smaller_than_one_character_still_terminates() {
        let f = Fonts;
        let lines = wrap(&f, "WWW", TextStyle::Title, 1.0, 5);
        assert_eq!(lines.concat(), "WWW");
        let cut = wrap(&f, "WWWWWWWW", TextStyle::Title, 1.0, 2);
        assert_eq!(cut.len(), 2);
    }

    #[test]
    fn a_block_measures_its_lines() {
        let f = Fonts;
        let b = block(
            &f,
            vec!["HTTP client".into()],
            TextStyle::Title,
            Point { x: 1.0, y: 2.0 },
            None,
            Align::Left,
        );
        assert_eq!(b.height, 16.0);
        assert!((b.width - f.width("HTTP client", TextStyle::Title)).abs() < 1e-4);
        assert_eq!(b.rect().x, 1.0);
    }
}

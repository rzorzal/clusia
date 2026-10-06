//! Colored code: one `Text` per line with a `TextSpan` child per highlighted run.
//!
//! Every span is an entity, so callers keep the number of lines on screen bounded (the diff
//! shows 500 at a time).

use std::ops::Range;

use bevy::prelude::*;
use bevy::text::{TextBackgroundColor, TextSpan};
use clusia_highlight::{Class, Span};

use crate::fonts::UiFonts;
use crate::theme::Swatch;
use crate::ui::kit::{Ink, Mark, Type, font};

/// Tabs show as this many spaces.
pub const TAB: &str = "    ";

/// A run of code: its text, its color and an optional background (the changed part of a line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeSpan {
    pub text: String,
    pub ink: Swatch,
    pub mark: Option<Swatch>,
}

/// One line of code in JetBrains Mono at `size` points, never wrapped.
pub fn code_line(fonts: &UiFonts, size: f32, spans: Vec<CodeSpan>) -> impl Bundle {
    let face = font(fonts, Type::MONO.size(size));
    (
        Text::default(),
        face.clone(),
        TextLayout::no_wrap(),
        Children::spawn(SpawnWith(move |p: &mut ChildSpawner| {
            for s in spans {
                let mut span = p.spawn((
                    TextSpan::new(s.text),
                    face.clone(),
                    TextColor::default(),
                    Ink(s.ink),
                ));
                if let Some(mark) = s.mark {
                    span.insert((TextBackgroundColor(Color::NONE), Mark(mark)));
                }
            }
        })),
    )
}

/// Cuts `line` into colored runs from its highlight `spans` (byte offsets), splitting at the
/// `emphasis` range (byte offsets) whose runs get the given background. Gaps and offsets that
/// fall outside the line or inside a character become plain text; tabs become spaces; an empty
/// line is one space, so it keeps its height.
pub fn spans_for(
    line: &str,
    spans: &[Span],
    emphasis: Option<(Range<usize>, Swatch)>,
) -> Vec<CodeSpan> {
    // Byte boundaries where a run may start or end.
    let mut cuts: Vec<(usize, Class)> = Vec::new();
    let mut at = 0;
    for s in spans {
        let (start, end) = (s.start.min(line.len()), s.end.min(line.len()));
        if start < at
            || start >= end
            || !line.is_char_boundary(start)
            || !line.is_char_boundary(end)
        {
            continue;
        }
        if start > at {
            cuts.push((at, Class::Plain));
        }
        cuts.push((start, s.class));
        at = end;
    }
    if at < line.len() || cuts.is_empty() {
        cuts.push((at, Class::Plain));
    }
    let emphasis = emphasis.filter(|(r, _)| {
        r.start < r.end
            && r.end <= line.len()
            && line.is_char_boundary(r.start)
            && line.is_char_boundary(r.end)
    });
    let mut out: Vec<CodeSpan> = Vec::new();
    for (i, &(start, class)) in cuts.iter().enumerate() {
        let end = cuts.get(i + 1).map_or(line.len(), |c| c.0);
        let mut pieces = vec![(start, end, None)];
        if let Some((r, mark)) = &emphasis {
            pieces = [
                (start, end.min(r.start), None),
                (start.max(r.start), end.min(r.end), Some(*mark)),
                (start.max(r.end), end, None),
            ]
            .into_iter()
            .filter(|(a, b, _)| a < b)
            .collect();
        }
        for (a, b, mark) in pieces {
            let text = line[a..b].replace('\t', TAB);
            let ink = Swatch::Code(class);
            match out.last_mut() {
                Some(last) if last.ink == ink && last.mark == mark => last.text.push_str(&text),
                _ => out.push(CodeSpan { text, ink, mark }),
            }
        }
    }
    out.retain(|s| !s.text.is_empty());
    if out.is_empty() {
        out.push(CodeSpan {
            text: " ".into(),
            ink: Swatch::Code(Class::Plain),
            mark: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Snapshot;
    use crate::testing;
    use crate::theme::LIGHT;

    fn span(start: usize, end: usize, class: Class) -> Span {
        Span { start, end, class }
    }

    fn plain(text: &str, class: Class, mark: Option<Swatch>) -> CodeSpan {
        CodeSpan {
            text: text.into(),
            ink: Swatch::Code(class),
            mark,
        }
    }

    #[test]
    fn runs_follow_the_highlight() {
        let line = "let x = 60;";
        let spans = [
            span(0, 3, Class::Keyword),
            span(3, 8, Class::Plain),
            span(8, 10, Class::Number),
            span(10, 11, Class::Punctuation),
        ];
        assert_eq!(
            spans_for(line, &spans, None),
            [
                plain("let", Class::Keyword, None),
                plain(" x = ", Class::Plain, None),
                plain("60", Class::Number, None),
                plain(";", Class::Punctuation, None),
            ]
        );
    }

    #[test]
    fn emphasis_splits_runs_and_marks_them() {
        let line = "if token.expires_at > now() {";
        let spans = [
            span(0, 2, Class::Keyword),
            span(2, line.len(), Class::Plain),
        ];
        let strong = Swatch::AddedStrong;
        assert_eq!(
            spans_for(line, &spans, Some((22..27, strong))),
            [
                plain("if", Class::Keyword, None),
                plain(" token.expires_at > ", Class::Plain, None),
                plain("now()", Class::Plain, Some(strong)),
                plain(" {", Class::Plain, None),
            ]
        );
        let across = spans_for(line, &spans, Some((1..4, strong)));
        assert_eq!(across[0], plain("i", Class::Keyword, None));
        assert_eq!(across[1], plain("f", Class::Keyword, Some(strong)));
        assert_eq!(across[2], plain(" t", Class::Plain, Some(strong)));
    }

    #[test]
    fn bad_input_degrades_to_plain_text() {
        assert_eq!(spans_for("", &[], None), [plain(" ", Class::Plain, None)]);
        assert_eq!(
            spans_for("\tok", &[], None),
            [plain("    ok", Class::Plain, None)]
        );
        // Offsets past the end, inside "é", or overlapping are ignored.
        let line = "é = 1";
        let spans = [span(1, 3, Class::Keyword), span(0, 99, Class::String)];
        assert_eq!(
            spans_for(line, &spans, None),
            [plain("é = 1", Class::String, None)]
        );
        // A gap between spans is plain; a mark inside "é" is dropped.
        let spans = [span(0, 2, Class::Variable), span(5, 6, Class::Number)];
        assert_eq!(
            spans_for("é = 1", &spans, Some((1..2, Swatch::RemovedStrong))),
            [
                plain("é", Class::Variable, None),
                plain(" = ", Class::Plain, None),
                plain("1", Class::Number, None),
            ]
        );
    }

    #[test]
    fn code_lines_spawn_colored_spans() {
        let mut app = testing::app(Snapshot::default());
        let fonts = UiFonts::default();
        let spans = vec![
            plain("fn", Class::Keyword, None),
            plain(" refresh", Class::Function, Some(Swatch::AddedStrong)),
        ];
        let line = app.world_mut().spawn(code_line(&fonts, 13.0, spans)).id();
        app.update();
        let kids = app.world().get::<Children>(line).unwrap().to_vec();
        assert_eq!(kids.len(), 2);
        let w = app.world();
        assert_eq!(w.get::<TextSpan>(kids[1]).unwrap().0, " refresh");
        assert_eq!(
            w.get::<TextColor>(kids[0]).unwrap().0,
            LIGHT.code[Class::Keyword.index()]
        );
        assert!(w.get::<TextBackgroundColor>(kids[0]).is_none());
        assert_eq!(
            w.get::<TextBackgroundColor>(kids[1]).unwrap().0,
            LIGHT.added_strong
        );
        assert_eq!(
            w.get::<TextFont>(kids[1]).unwrap().font_size,
            FontSize::Px(13.0)
        );
        assert_eq!(
            w.get::<TextLayout>(line).unwrap().linebreak,
            LineBreak::NoWrap
        );
    }
}

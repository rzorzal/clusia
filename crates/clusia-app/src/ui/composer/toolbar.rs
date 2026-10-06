//! The composer's text edits: the toolbar actions as pure functions over (text, selection), the
//! images and GIFs found in a text, and the toolbar row itself.
//!
//! Every edit keeps all the text it was given: it wraps, prefixes or inserts, and returns the
//! selection to restore. Selections are byte ranges and are clamped to character boundaries
//! first, so a stale range can never split a character.

use std::ops::Range;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::observe;

use crate::fonts::UiFonts;
use crate::theme::Swatch;
use crate::ui::composer::{ComposerKey, on_toolbar_button};
use crate::ui::kit::{Clickable, Fill, HoverFill, Type, text};

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolbarAction {
    Bold,
    Italic,
    Code,
    Link,
    List,
    Quote,
    Suggest,
}

impl ToolbarAction {
    /// The button's face.
    pub fn glyph(self) -> &'static str {
        match self {
            ToolbarAction::Bold => "B",
            ToolbarAction::Italic => "I",
            ToolbarAction::Code => "<>",
            ToolbarAction::Link => "↗",
            ToolbarAction::List => "1.",
            ToolbarAction::Quote => "“",
            ToolbarAction::Suggest => "±",
        }
    }
}

/// `sel` clamped into `text` and moved back onto character boundaries, start before end.
fn clamp(text: &str, sel: Range<usize>) -> Range<usize> {
    let floor = |mut i: usize| {
        i = i.min(text.len());
        while !text.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let (a, b) = (floor(sel.start), floor(sel.end));
    a.min(b)..a.max(b)
}

/// Replaces `sel` with `s` and puts the cursor after it.
pub fn insert_at(text: &str, sel: Range<usize>, s: &str) -> (String, Range<usize>) {
    let sel = clamp(text, sel);
    let out = format!("{}{s}{}", &text[..sel.start], &text[sel.end..]);
    let at = sel.start + s.len();
    (out, at..at)
}

/// Puts `before`/`after` around the selection and selects the inner text again. Without a
/// selection the markers go in as a pair with the cursor between them.
fn wrap(text: &str, sel: Range<usize>, before: &str, after: &str) -> (String, Range<usize>) {
    let sel = clamp(text, sel);
    let out = format!(
        "{}{before}{}{after}{}",
        &text[..sel.start],
        &text[sel.clone()],
        &text[sel.end..]
    );
    let start = sel.start + before.len();
    (out, start..start + sel.len())
}

/// The byte range of the whole lines the selection touches (no trailing newline).
fn line_block(text: &str, sel: &Range<usize>) -> Range<usize> {
    let start = text[..sel.start].rfind('\n').map_or(0, |i| i + 1);
    let last = if sel.end > sel.start && text.as_bytes()[sel.end - 1] == b'\n' {
        sel.end - 1
    } else {
        sel.end
    };
    let end = text[last..].find('\n').map_or(text.len(), |i| last + i);
    start..end.max(start)
}

/// Puts `prefix(n)` in front of each non-blank line of the selected lines (of a single blank
/// line too), `n` counting from 1.
fn prefix_lines(
    text: &str,
    sel: Range<usize>,
    prefix: impl Fn(usize) -> String,
) -> (String, Range<usize>) {
    let sel = clamp(text, sel);
    let block = line_block(text, &sel);
    let lines: Vec<&str> = text[block.clone()].split('\n').collect();
    let single = lines.len() == 1;
    let mut n = 0;
    let body: Vec<String> = lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() && !single {
                (*l).to_string()
            } else {
                n += 1;
                format!("{}{l}", prefix(n))
            }
        })
        .collect();
    let body = body.join("\n");
    let out = format!("{}{body}{}", &text[..block.start], &text[block.end..]);
    let end = block.start + body.len();
    (out, block.start..end)
}

/// A `suggestion` block holding `source`, started on its own line, with `source` selected so it
/// can be edited at once.
pub fn insert_suggestion(text: &str, sel: Range<usize>, source: &str) -> (String, Range<usize>) {
    let sel = clamp(text, sel);
    let lead = if sel.start == 0 || text[..sel.start].ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let trail = if text[sel.end..].is_empty() || text[sel.end..].starts_with('\n') {
        ""
    } else {
        "\n"
    };
    let head = format!("{lead}```suggestion\n");
    let block = format!("{head}{source}\n```{trail}");
    let (out, _) = insert_at(text, sel.clone(), &block);
    let start = sel.start + head.len();
    (out, start..start + source.len())
}

/// Applies a toolbar action to `text` with the selection `sel`; returns the new text and the new
/// selection.
pub fn apply(action: ToolbarAction, text: &str, sel: Range<usize>) -> (String, Range<usize>) {
    let sel = clamp(text, sel);
    match action {
        ToolbarAction::Bold => wrap(text, sel, "**", "**"),
        ToolbarAction::Italic => wrap(text, sel, "_", "_"),
        ToolbarAction::Code => {
            if text[sel.clone()].contains('\n') {
                wrap(text, sel, "```\n", "\n```")
            } else {
                wrap(text, sel, "`", "`")
            }
        }
        ToolbarAction::Link => {
            let (out, inner) = wrap(text, sel.clone(), "[", "](url)");
            if sel.is_empty() {
                (out, inner)
            } else {
                let url = inner.end + 2;
                (out, url..url + 3)
            }
        }
        ToolbarAction::List => prefix_lines(text, sel, |n| format!("{n}. ")),
        ToolbarAction::Quote => prefix_lines(text, sel, |_| "> ".to_string()),
        ToolbarAction::Suggest => {
            let source = text[sel.clone()].to_string();
            insert_suggestion(text, sel, &source)
        }
    }
}

/// An image or GIF written in the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chip {
    /// The markdown (or `<img>` tag) exactly as written.
    pub raw: String,
    pub alt: String,
    pub url: String,
    pub gif: bool,
}

impl Chip {
    /// The name shown on the chip: the alt text, else the last part of the link.
    pub fn label(&self) -> String {
        if !self.alt.trim().is_empty() {
            return self.alt.trim().to_string();
        }
        let path = self.url.split(['?', '#']).next().unwrap_or("");
        path.rsplit('/').next().unwrap_or("image").to_string()
    }

    /// The line under the name.
    pub fn note(&self) -> &'static str {
        let host = self.url.split('/').nth(2).unwrap_or("");
        if self.gif {
            "by link · plays inline"
        } else if host == "github.com" || host.ends_with(".githubusercontent.com") {
            "uploaded to GitHub"
        } else {
            "image link"
        }
    }

    /// Whether the note says the image lives on GitHub (drawn green).
    pub fn on_github(&self) -> bool {
        self.note() == "uploaded to GitHub"
    }
}

fn is_gif(url: &str) -> bool {
    let no_query = url.split(['?', '#']).next().unwrap_or("");
    no_query.to_ascii_lowercase().ends_with(".gif")
        || url
            .split('/')
            .nth(2)
            .is_some_and(|h| h.starts_with("media") && h.ends_with("giphy.com"))
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let at = tag.find(&format!("{name}=\""))? + name.len() + 2;
    let len = tag[at..].find('"')?;
    Some(tag[at..at + len].to_string())
}

/// The images and GIFs in `text`, in order: `![alt](https://…)` and `<img … src="https://…">`.
pub fn chips_in(text: &str) -> Vec<Chip> {
    let mut found: Vec<(usize, Chip)> = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find("![") {
        let at = from + i;
        from = at + 2;
        let Some(close) = text[at + 2..].find("](") else {
            continue;
        };
        let alt = &text[at + 2..at + 2 + close];
        let url_at = at + 2 + close + 2;
        let Some(end) = text[url_at..].find(')') else {
            continue;
        };
        let url = &text[url_at..url_at + end];
        if alt.contains('\n') || !url.starts_with("https://") || url.contains(char::is_whitespace) {
            continue;
        }
        found.push((
            at,
            Chip {
                raw: text[at..url_at + end + 1].to_string(),
                alt: alt.to_string(),
                url: url.to_string(),
                gif: is_gif(url),
            },
        ));
        from = url_at + end + 1;
    }
    let mut from = 0;
    while let Some(i) = text[from..].find("<img") {
        let at = from + i;
        let Some(end) = text[at..].find('>') else {
            break;
        };
        let tag = &text[at..at + end + 1];
        from = at + end + 1;
        if let Some(url) = attr(tag, "src").filter(|u| u.starts_with("https://")) {
            found.push((
                at,
                Chip {
                    raw: tag.to_string(),
                    alt: attr(tag, "alt").unwrap_or_default(),
                    gif: is_gif(&url),
                    url,
                },
            ));
        }
    }
    found.sort_by_key(|(at, _)| *at);
    found.into_iter().map(|(_, c)| c).collect()
}

/// `text` without the first occurrence of `raw`, and the line break after it when it stood on
/// a line of its own.
pub fn remove_chip(text: &str, raw: &str) -> String {
    let Some(at) = text.find(raw) else {
        return text.to_string();
    };
    let end = at + raw.len();
    let alone = (at == 0 || text[..at].ends_with('\n'))
        && (end == text.len() || text[end..].starts_with('\n'));
    let end = if alone && end < text.len() {
        end + 1
    } else {
        end
    };
    format!("{}{}", &text[..at], &text[end..])
}

/// One toolbar button (26 px).
pub fn tool_button(fonts: &UiFonts, glyph: &str, mono: bool, bold: bool) -> impl Bundle {
    let style = Type {
        size: 13.0,
        weight: if bold { 700 } else { 500 },
        ink: Swatch::Muted,
        mono,
    };
    (
        Node {
            width: px(28),
            height: px(26),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        (
            bevy::ui_widgets::Button,
            Clickable,
            bevy::picking::hover::Hovered::default(),
            bevy::input_focus::tab_navigation::TabIndex(0),
        ),
        BackgroundColor::default(),
        Fill(Swatch::Clear),
        HoverFill(Swatch::Hover),
        children![text(fonts, glyph.to_string(), style)],
    )
}

/// The action buttons of the toolbar: `B I <> ↗` always; the list, quote and — for a comment on
/// a line — suggestion buttons unless `compact`.
pub fn action_buttons(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    key: &ComposerKey,
    compact: bool,
    suggest: bool,
) {
    let mut actions = vec![
        ToolbarAction::Bold,
        ToolbarAction::Italic,
        ToolbarAction::Code,
        ToolbarAction::Link,
    ];
    if !compact {
        actions.extend([ToolbarAction::List, ToolbarAction::Quote]);
        if suggest {
            actions.push(ToolbarAction::Suggest);
        }
    }
    for action in actions {
        p.spawn((
            tool_button(
                fonts,
                action.glyph(),
                action == ToolbarAction::Code,
                action == ToolbarAction::Bold,
            ),
            action,
            ToolbarFor(key.clone()),
            observe(on_toolbar_button),
        ));
    }
}

/// Which composer a toolbar button acts on.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ToolbarFor(pub ComposerKey);

#[cfg(test)]
mod tests {
    use super::*;

    fn run(action: ToolbarAction, text: &str, sel: Range<usize>) -> (String, String) {
        let (out, new) = apply(action, text, sel);
        (out.clone(), out[new].to_string())
    }

    #[test]
    fn toolbar_wraps_or_inserts() {
        assert_eq!(
            run(ToolbarAction::Bold, "slow exchange", 0..4),
            ("**slow** exchange".into(), "slow".into())
        );
        let (out, sel) = apply(ToolbarAction::Italic, "a b", 1..1);
        assert_eq!((out.as_str(), sel), ("a__ b", 2..2), "pair, cursor between");
        assert_eq!(
            run(ToolbarAction::Code, "call exchange now", 5..13),
            ("call `exchange` now".into(), "exchange".into())
        );
        assert_eq!(
            run(ToolbarAction::Code, "a\nb", 0..3),
            ("```\na\nb\n```".into(), "a\nb".into()),
            "several lines become a fence"
        );
        assert_eq!(
            run(ToolbarAction::Link, "see docs", 4..8),
            ("see [docs](url)".into(), "url".into()),
            "the placeholder is selected"
        );
        let (out, sel) = apply(ToolbarAction::Link, "", 0..0);
        assert_eq!((out.as_str(), sel), ("[](url)", 1..1));
        assert_eq!(
            apply(ToolbarAction::List, "one\ntwo\n\nthree", 0..14).0,
            "1. one\n2. two\n\n3. three"
        );
        assert_eq!(apply(ToolbarAction::List, "", 0..0).0, "1. ");
        assert_eq!(
            apply(ToolbarAction::Quote, "a\nb\nc", 2..3).0,
            "a\n> b\nc",
            "only the touched line"
        );
        assert_eq!(
            apply(ToolbarAction::Quote, "a\nb\n", 0..2).0,
            "> a\nb\n",
            "a selection that ends with a line break does not take the next line"
        );
        let (out, _) = insert_at("ab", 1..1, "🐢");
        assert_eq!(out, "a🐢b");
    }

    #[test]
    fn toolbar_never_drops_text() {
        let text = "héllo wörld\n🐢 slow\n\nend";
        let is_subsequence = |small: &str, big: &str| {
            let mut rest = big.chars();
            small.chars().all(|c| rest.any(|b| b == c))
        };
        let actions = [
            ToolbarAction::Bold,
            ToolbarAction::Italic,
            ToolbarAction::Code,
            ToolbarAction::Link,
            ToolbarAction::List,
            ToolbarAction::Quote,
            ToolbarAction::Suggest,
        ];
        for action in actions {
            for start in 0..=text.len() + 3 {
                for end in start..=text.len() + 3 {
                    let (out, sel) = apply(action, text, start..end);
                    assert!(is_subsequence(text, &out), "{action:?} {start}..{end}");
                    assert!(
                        sel.end <= out.len()
                            && out.is_char_boundary(sel.start)
                            && out.is_char_boundary(sel.end),
                        "{action:?} {start}..{end}: {sel:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn suggest_prefills_the_lines() {
        let (out, sel) = insert_suggestion("Could we:", 9..9, "let a = 1;\nlet b = 2;");
        assert_eq!(out, "Could we:\n```suggestion\nlet a = 1;\nlet b = 2;\n```");
        assert_eq!(&out[sel], "let a = 1;\nlet b = 2;", "the code is selected");
        let (out, _) = insert_suggestion("", 0..0, "x");
        assert_eq!(out, "```suggestion\nx\n```");
        let (out, _) = insert_suggestion("a\nb", 1..1, "x");
        assert_eq!(out, "a\n```suggestion\nx\n```\nb");
        assert_eq!(
            apply(ToolbarAction::Suggest, "keep me", 0..7).0,
            "```suggestion\nkeep me\n```",
            "without a source the selection is the suggestion"
        );
    }

    #[test]
    fn chips_follow_the_text() {
        let text = "Trace:\n![race-trace.png](https://github.com/user-attachments/assets/a1)\n![waiting](https://media.giphy.com/media/x/giphy.gif) and ![nope](http://insecure/x.png) <img width=\"500\" alt=\"shot\" src=\"https://github.com/user-attachments/assets/b2\" />";
        let chips = chips_in(text);
        let seen: Vec<(String, bool, &str)> =
            chips.iter().map(|c| (c.label(), c.gif, c.note())).collect();
        assert_eq!(
            seen,
            [
                ("race-trace.png".into(), false, "uploaded to GitHub"),
                ("waiting".into(), true, "by link · plays inline"),
                ("shot".into(), false, "uploaded to GitHub"),
            ],
            "http links and plain text are not chips"
        );
        assert_eq!(
            chips[0].url,
            "https://github.com/user-attachments/assets/a1"
        );
        let gone = remove_chip(text, &chips[0].raw);
        assert!(!gone.contains("race-trace"));
        assert!(gone.starts_with("Trace:\n![waiting]"), "its line goes too");
        assert_eq!(chips_in(&gone).len(), 2);
        assert_eq!(
            remove_chip("a ![x](https://h/i.png) b", "![x](https://h/i.png)"),
            "a  b"
        );
        assert_eq!(remove_chip("keep", "![gone](https://h/i.png)"), "keep");
        let bare = Chip {
            raw: String::new(),
            alt: String::new(),
            url: "https://example.com/a/pic.png?x=1".into(),
            gif: false,
        };
        assert_eq!(bare.label(), "pic.png");
    }
}

//! Lays parsed markdown out as nodes. Every word, code run, emoji and image is one node in a
//! wrapping row (`FlexEnd`, so images and emoji sit on the line's bottom and equal text shares
//! a baseline); a space is the right margin of the node before it. Images are empty slots
//! (`MdImage`) and emoji are sized boxes (`MdEmoji`) that systems fill in.

use std::collections::HashSet;

use bevy::clipboard::Clipboard;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::{FontSource, Strikethrough, Underline, UnderlineColor};
use bevy::ui_widgets::{Activate, Button as WidgetButton};
use clusia_highlight::highlight;

use crate::fonts::UiFonts;
use crate::platform_open::{OpenUrls, visit};
use crate::theme::{Swatch, Theme};
use crate::ui::code::{code_line, spans_for};
use crate::ui::emoji::EmojiImages;
use crate::ui::kit::{Clickable, Fill, Ink, Stroke, Type, Variant, button, font, text};
use crate::ui::markdown::parse::{Block, Inline, ListItem, Style};

/// The width of a space, in ems.
const SPACE: f32 = 0.28;
/// Emoji are a little larger than the text beside them: the art has padding.
const EMOJI_SCALE: f32 = 1.35;
/// A code block shows at most this many lines (each colored run is an entity).
const MAX_CODE_LINES: usize = 200;

/// How to draw a body.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderOpts {
    /// Size of code blocks and suggestions, in points.
    pub code_size: f32,
    /// The lines a `suggestion` block replaces; shown as the removed row of its mini diff.
    pub suggestion_base: Option<String>,
    /// Draw images from other sites (otherwise they become link chips).
    pub load_external: bool,
}

impl RenderOpts {
    /// The options for a screen: the user's code size and image setting.
    pub fn from_config(code_size: f32, config: &clusia_core::Config) -> Self {
        Self {
            code_size,
            load_external: config.media.load_external_images,
            ..Self::default()
        }
    }
}

impl Default for RenderOpts {
    fn default() -> Self {
        Self {
            code_size: 12.0,
            suggestion_base: None,
            load_external: false,
        }
    }
}

/// Every word of a link carries its URL.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct MdLink(pub String);

/// An empty slot for the image at this URL; `ui::media` fills it.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct MdImage(pub String);

/// A box for the emoji drawn from this Twemoji file.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct MdEmoji(pub String);

/// A button that copies this markdown source to the clipboard.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CopyMarkdown(pub String);

/// The root of a body built by `markdown`.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct MdBody;

/// When present, copied text is recorded here instead of reaching the clipboard (tests).
#[derive(Resource, Debug, Default, Clone, PartialEq, Eq)]
pub struct CopiedText(pub Vec<String>);

/// Whether the image at `url` lives outside GitHub and Giphy (and so is a link unless the
/// user turned external images on).
pub fn is_external(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return true;
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .and_then(|authority| authority.split(':').next())
        .unwrap_or("");
    !clusia_core::media::allowed_host(host, &[])
}

/// Draws `blocks` as one column inside `p`.
pub fn markdown(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    blocks: &[Block],
    opts: &RenderOpts,
) {
    p.spawn((
        Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(8),
            width: percent(100),
            min_width: px(0),
            ..default()
        },
        MdBody,
    ))
    .with_children(|c| {
        for b in blocks {
            block(c, fonts, b, opts, Type::BODY);
        }
    });
}

/// Draws the first `max_chars` characters of `blocks` as one row in the text style `base`:
/// inline formatting only.
pub fn markdown_line(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    blocks: &[Block],
    base: Type,
    opts: &RenderOpts,
    max_chars: usize,
) {
    let items = line_inlines(blocks, max_chars);
    inlines(p, fonts, &items, base, opts);
}

/// The inline content of `blocks` flattened into one line of at most `max_chars` characters
/// (a cut ends in `…`). Images become their alt text, line breaks become spaces, code blocks
/// their first line and links plain words (the line sits inside something that is itself a
/// button); tables and rules are left out.
pub fn line_inlines(blocks: &[Block], max_chars: usize) -> Vec<Inline> {
    let mut line = Line {
        out: Vec::new(),
        used: 0,
        max: max_chars,
        cut: false,
    };
    line.blocks(blocks);
    line.finish()
}

struct Line {
    out: Vec<Inline>,
    used: usize,
    max: usize,
    /// Something was left out.
    cut: bool,
}

impl Line {
    fn blocks(&mut self, blocks: &[Block]) {
        for b in blocks {
            if self.cut {
                return;
            }
            match b {
                Block::Paragraph(items) | Block::Heading(_, items) => self.items(items),
                Block::List { items, .. } => {
                    for item in items {
                        self.blocks(&item.blocks);
                    }
                }
                Block::Quote(inner) => self.blocks(inner),
                Block::Code { text, .. } | Block::Suggestion(text) => {
                    let first = text.lines().next().unwrap_or("").to_string();
                    self.items(&[Inline::Code {
                        text: first,
                        space_after: false,
                    }]);
                }
                Block::Table { .. } | Block::Rule => {}
            }
        }
    }

    fn items(&mut self, items: &[Inline]) {
        if let Some(last) = self.out.last_mut() {
            set_space(last, true);
        }
        for item in items {
            if self.cut {
                return;
            }
            if self.used >= self.max {
                self.cut = true;
                return;
            }
            let mut item = match item {
                Inline::Image {
                    alt, space_after, ..
                } => Inline::Word {
                    text: if alt.is_empty() {
                        "image".into()
                    } else {
                        alt.clone()
                    },
                    style: Style::default(),
                    space_after: *space_after,
                },
                Inline::Break => {
                    if let Some(last) = self.out.last_mut() {
                        set_space(last, true);
                    }
                    continue;
                }
                other => other.clone(),
            };
            if let Inline::Word { style, .. } = &mut item {
                style.link = None;
            }
            let room = self.max - self.used;
            if let Inline::Word { text, .. } | Inline::Code { text, .. } = &mut item {
                let len = text.chars().count();
                if len > room {
                    let kept: String = text.chars().take(room).collect();
                    *text = format!("{}…", kept.trim_end());
                    set_space(&mut item, false);
                    self.out.push(item);
                    self.cut = true;
                    return;
                }
                self.used += len + 1;
            } else {
                self.used += 2;
            }
            self.out.push(item);
        }
    }

    fn finish(mut self) -> Vec<Inline> {
        if self.cut
            && let Some(
                Inline::Word {
                    text, space_after, ..
                }
                | Inline::Code {
                    text, space_after, ..
                },
            ) = self.out.last_mut()
            && !text.ends_with('…')
        {
            text.push('…');
            *space_after = false;
        }
        self.out
    }
}

fn set_space(item: &mut Inline, on: bool) {
    match item {
        Inline::Word { space_after, .. }
        | Inline::Code { space_after, .. }
        | Inline::Emoji { space_after, .. }
        | Inline::Image { space_after, .. } => *space_after = on,
        Inline::Break => {}
    }
}

/// A **Copy markdown** button for the comment whose source is `source`.
pub fn copy_button(fonts: &UiFonts, source: &str) -> impl Bundle {
    (
        button(fonts, "Copy markdown", Variant::Ghost),
        CopyMarkdown(source.to_string()),
    )
}

/// A small chip that opens `url` in the browser, standing in for media that cannot be shown:
/// `reason — open in browser`.
pub fn link_chip(p: &mut ChildSpawnerCommands, fonts: &UiFonts, url: &str, reason: &str) {
    let mut chip = p.spawn((
        Text::new(format!("{reason} — open in browser")),
        font(fonts, Type::META.ink(Swatch::Green)),
        TextColor::default(),
        Ink(Swatch::Green),
        TextLayout::new(Justify::Left, LineBreak::WordOrCharacter),
        Node {
            padding: UiRect::axes(px(8), px(4)),
            border: px(1).all(),
            border_radius: BorderRadius::all(px(6)),
            max_width: percent(100),
            ..default()
        },
        BackgroundColor::default(),
        Fill(Swatch::Chrome),
        BorderColor::default(),
        Stroke(Swatch::Line),
    ));
    chip.insert((link_parts(url), TabIndex(0)));
}

fn link_parts(url: &str) -> impl Bundle {
    (
        MdLink(url.to_string()),
        Clickable,
        Hovered::default(),
        WidgetButton,
        Underline,
        UnderlineColor(Color::NONE),
    )
}

fn block(c: &mut ChildSpawnerCommands, fonts: &UiFonts, b: &Block, opts: &RenderOpts, base: Type) {
    match b {
        Block::Paragraph(items) => inlines(c, fonts, items, base, opts),
        Block::Heading(level, items) => {
            let t = match level {
                1 => Type::TITLE,
                2 => Type::HEADING,
                _ => Type::STRONG,
            };
            inlines(c, fonts, items, t, opts);
        }
        Block::List { ordered, items } => list(c, fonts, *ordered, items, opts, base),
        Block::Quote(inner) => {
            c.spawn(Node {
                column_gap: px(10),
                width: percent(100),
                ..default()
            })
            .with_children(|q| {
                q.spawn((
                    Node {
                        width: px(3),
                        flex_shrink: 0.0,
                        border_radius: BorderRadius::all(px(2)),
                        ..default()
                    },
                    BackgroundColor::default(),
                    Fill(Swatch::Faint),
                ));
                q.spawn(Node {
                    flex_direction: FlexDirection::Column,
                    flex_grow: 1.0,
                    flex_basis: px(0),
                    min_width: px(0),
                    row_gap: px(6),
                    ..default()
                })
                .with_children(|col| {
                    for b in inner {
                        block(col, fonts, b, opts, base.ink(Swatch::Muted));
                    }
                });
            });
        }
        Block::Code { lang, text } => code_block(c, fonts, lang.as_deref(), text, opts),
        Block::Suggestion(text) => suggestion(c, fonts, text, opts),
        Block::Table { head, rows } => table(c, fonts, head, rows, opts, base),
        Block::Rule => {
            c.spawn((
                Node {
                    width: percent(100),
                    height: px(1),
                    ..default()
                },
                BackgroundColor::default(),
                Fill(Swatch::Line),
            ));
        }
    }
}

fn list(
    c: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    ordered: bool,
    items: &[ListItem],
    opts: &RenderOpts,
    base: Type,
) {
    c.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(4),
        width: percent(100),
        ..default()
    })
    .with_children(|col| {
        for (i, item) in items.iter().enumerate() {
            col.spawn(Node {
                column_gap: px(8),
                align_items: AlignItems::FlexStart,
                width: percent(100),
                ..default()
            })
            .with_children(|row| {
                row.spawn(Node {
                    min_width: px(16),
                    flex_shrink: 0.0,
                    justify_content: JustifyContent::FlexEnd,
                    ..default()
                })
                .with_children(|m| match item.task {
                    Some(done) => checkbox(m, fonts, done),
                    None if ordered => {
                        m.spawn(text(
                            fonts,
                            format!("{}.", i + 1),
                            Type::MUTED.size(base.size),
                        ));
                    }
                    None => {
                        m.spawn(text(fonts, "•", Type::MUTED.size(base.size)));
                    }
                });
                row.spawn(Node {
                    flex_direction: FlexDirection::Column,
                    flex_grow: 1.0,
                    flex_basis: px(0),
                    min_width: px(0),
                    row_gap: px(4),
                    ..default()
                })
                .with_children(|body| {
                    for b in &item.blocks {
                        block(body, fonts, b, opts, base);
                    }
                });
            });
        }
    });
}

fn checkbox(p: &mut ChildSpawnerCommands, fonts: &UiFonts, done: bool) {
    p.spawn((
        Node {
            width: px(14),
            height: px(14),
            margin: UiRect::top(px(2)),
            border: px(1).all(),
            border_radius: BorderRadius::all(px(3)),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        },
        BackgroundColor::default(),
        Fill(if done { Swatch::Green } else { Swatch::Clear }),
        BorderColor::default(),
        Stroke(if done { Swatch::Green } else { Swatch::Faint }),
    ))
    .with_children(|b| {
        if done {
            b.spawn(text(
                fonts,
                "✓",
                Type::STRONG.size(10.0).ink(Swatch::OnGreen),
            ));
        }
    });
}

/// A file name whose extension names the language of a fenced block.
fn path_for_lang(lang: &str) -> String {
    let ext = match lang.trim().to_ascii_lowercase().as_str() {
        "rust" => "rs",
        "javascript" => "js",
        "typescript" => "ts",
        "python" => "py",
        "golang" => "go",
        "shell" | "zsh" | "console" => "sh",
        "markdown" => "md",
        other => return format!("code.{other}"),
    };
    format!("code.{ext}")
}

fn block_frame() -> impl Bundle {
    (
        Node {
            flex_direction: FlexDirection::Column,
            width: percent(100),
            padding: UiRect::axes(px(12), px(10)),
            border: px(1).all(),
            border_radius: BorderRadius::all(px(6)),
            overflow: Overflow::clip_x(),
            ..default()
        },
        BackgroundColor::default(),
        Fill(Swatch::Chrome),
        BorderColor::default(),
        Stroke(Swatch::Line),
    )
}

fn code_block(
    c: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    lang: Option<&str>,
    source: &str,
    opts: &RenderOpts,
) {
    let all: Vec<&str> = source.trim_end_matches('\n').split('\n').collect();
    let shown = &all[..all.len().min(MAX_CODE_LINES)];
    let path = lang.map(path_for_lang).unwrap_or_default();
    let spans = highlight(&path, &shown.join("\n"));
    c.spawn(block_frame()).with_children(|b| {
        for (i, line) in shown.iter().enumerate() {
            let line_spans = spans.get(i).map(Vec::as_slice).unwrap_or_default();
            b.spawn(code_line(
                fonts,
                opts.code_size,
                spans_for(line, line_spans, None),
            ));
        }
        if all.len() > shown.len() {
            b.spawn(text(
                fonts,
                format!("… {} more lines", all.len() - shown.len()),
                Type::META,
            ));
        }
    });
}

fn suggestion(c: &mut ChildSpawnerCommands, fonts: &UiFonts, source: &str, opts: &RenderOpts) {
    c.spawn((
        Node {
            flex_direction: FlexDirection::Column,
            width: percent(100),
            border: px(1).all(),
            border_radius: BorderRadius::all(px(6)),
            overflow: Overflow::clip(),
            ..default()
        },
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|s| {
        s.spawn((
            Node {
                padding: UiRect::axes(px(12), px(6)),
                ..default()
            },
            BackgroundColor::default(),
            Fill(Swatch::Chrome),
            children![text(fonts, "Suggested change", Type::META)],
        ));
        let removed = opts.suggestion_base.as_deref().unwrap_or("");
        for (sign, fill, body) in [
            ("−", Swatch::RemovedBg, removed),
            ("+", Swatch::AddedBg, source),
        ] {
            for line in body.lines() {
                s.spawn((
                    Node {
                        column_gap: px(8),
                        padding: UiRect::axes(px(12), px(2)),
                        align_items: AlignItems::FlexStart,
                        ..default()
                    },
                    BackgroundColor::default(),
                    Fill(fill),
                ))
                .with_children(|row| {
                    row.spawn(text(fonts, sign, Type::MONO.size(opts.code_size)));
                    row.spawn(code_line(fonts, opts.code_size, spans_for(line, &[], None)));
                });
            }
        }
    });
}

fn table(
    c: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    head: &[Vec<Inline>],
    rows: &[Vec<Vec<Inline>>],
    opts: &RenderOpts,
    base: Type,
) {
    c.spawn((
        Node {
            flex_direction: FlexDirection::Column,
            width: percent(100),
            border: px(1).all(),
            border_radius: BorderRadius::all(px(6)),
            overflow: Overflow::clip(),
            ..default()
        },
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|t| {
        let bold = Type {
            weight: 600,
            ..base
        };
        table_row(t, fonts, head, opts, bold, Swatch::Chrome);
        for row in rows {
            table_row(t, fonts, row, opts, base, Swatch::Clear);
        }
    });
}

fn table_row(
    t: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    cells: &[Vec<Inline>],
    opts: &RenderOpts,
    base: Type,
    fill: Swatch,
) {
    t.spawn((
        Node {
            width: percent(100),
            border: UiRect::bottom(px(1)),
            ..default()
        },
        BackgroundColor::default(),
        Fill(fill),
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|r| {
        for cell in cells {
            r.spawn(Node {
                flex_grow: 1.0,
                flex_basis: px(0),
                min_width: px(0),
                padding: UiRect::axes(px(10), px(6)),
                ..default()
            })
            .with_children(|c| inlines(c, fonts, cell, base, opts));
        }
    });
}

/// The face and style of a word.
fn face(fonts: &UiFonts, base: Type, style: &Style) -> (Type, TextFont) {
    let mut t = base;
    if style.strong {
        t.weight = t.weight.max(600);
    }
    if style.link.is_some() {
        t.ink = Swatch::Green;
    }
    let mut f = font(fonts, t);
    if style.em {
        // The bundled upright face does not slant on its own.
        f.font = FontSource::Handle(fonts.italic.clone());
    }
    (t, f)
}

fn space_margin(size: f32, space: bool) -> UiRect {
    UiRect::right(px(if space { (size * SPACE).round() } else { 0.0 }))
}

fn inlines(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    items: &[Inline],
    base: Type,
    opts: &RenderOpts,
) {
    p.spawn(Node {
        flex_direction: FlexDirection::Row,
        flex_wrap: FlexWrap::Wrap,
        align_items: AlignItems::FlexEnd,
        row_gap: px(3),
        width: percent(100),
        min_width: px(0),
        ..default()
    })
    .with_children(|row| {
        for (k, item) in items.iter().enumerate() {
            match item {
                Inline::Word {
                    text,
                    style,
                    space_after,
                } => {
                    let same_link = |other: Option<&Inline>| {
                        matches!(other, Some(Inline::Word { style: s, .. })
                            if s.link.is_some() && s.link == style.link)
                    };
                    // The space between two words of one link stays inside the text, so the
                    // underline runs across it.
                    let joined = *space_after && same_link(items.get(k + 1));
                    let first = !(k > 0 && same_link(items.get(k - 1)));
                    let label = if joined {
                        format!("{text} ")
                    } else {
                        text.clone()
                    };
                    let (t, f) = face(fonts, base, style);
                    let mut e = row.spawn((
                        Text::new(label),
                        f,
                        TextColor::default(),
                        Ink(t.ink),
                        TextLayout::new(Justify::Left, LineBreak::WordOrCharacter),
                        Node {
                            margin: space_margin(base.size, *space_after && !joined),
                            max_width: percent(100),
                            ..default()
                        },
                    ));
                    if style.strike {
                        e.insert(Strikethrough);
                    }
                    if let Some(url) = &style.link {
                        e.insert(link_parts(url));
                        if first {
                            e.insert(TabIndex(0));
                        }
                    }
                }
                Inline::Code { text, space_after } => {
                    let t = Type::MONO.size((base.size - 1.0).max(10.0)).ink(Swatch::Fg);
                    row.spawn((
                        Text::new(text.clone()),
                        font(fonts, t),
                        TextColor::default(),
                        Ink(t.ink),
                        TextLayout::new(Justify::Left, LineBreak::WordOrCharacter),
                        Node {
                            margin: space_margin(base.size, *space_after),
                            max_width: percent(100),
                            padding: UiRect::axes(px(4), px(1)),
                            border_radius: BorderRadius::all(px(4)),
                            ..default()
                        },
                        BackgroundColor::default(),
                        Fill(Swatch::Line),
                    ));
                }
                Inline::Emoji { file, space_after } => {
                    let size = base.size * EMOJI_SCALE;
                    row.spawn((
                        Node {
                            width: px(size),
                            height: px(size),
                            margin: UiRect::right(px(if *space_after { 4.0 } else { 1.0 })),
                            flex_shrink: 0.0,
                            ..default()
                        },
                        MdEmoji(file.to_string()),
                    ));
                }
                Inline::Image {
                    url, space_after, ..
                } => {
                    if is_external(url) && !opts.load_external {
                        link_chip(row, fonts, url, "Image from another site");
                    } else {
                        row.spawn((
                            Node {
                                margin: UiRect::right(px(if *space_after { 4.0 } else { 0.0 })),
                                max_width: percent(100),
                                ..default()
                            },
                            MdImage(url.clone()),
                        ));
                    }
                }
                Inline::Break => {
                    row.spawn(Node {
                        width: percent(100),
                        height: px(0),
                        ..default()
                    });
                }
            }
        }
    });
}

pub struct MarkdownPlugin;

impl Plugin for MarkdownPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EmojiImages>()
            .add_observer(on_link)
            .add_observer(on_copy)
            .add_systems(Update, (group_hover, fill_emoji));
    }
}

fn on_link(activate: On<Activate>, links: Query<&MdLink>, urls: Option<ResMut<OpenUrls>>) {
    if let Ok(link) = links.get(activate.entity) {
        visit(urls, &link.0);
    }
}

fn on_copy(
    activate: On<Activate>,
    copies: Query<&CopyMarkdown>,
    recorder: Option<ResMut<CopiedText>>,
    clipboard: Option<ResMut<Clipboard>>,
) {
    let Ok(copy) = copies.get(activate.entity) else {
        return;
    };
    match (recorder, clipboard) {
        (Some(mut recorded), _) => recorded.0.push(copy.0.clone()),
        (None, Some(mut clipboard)) => {
            if let Err(e) = clipboard.set_text(copy.0.clone()) {
                tracing::warn!(error = %e, "cannot copy to the clipboard");
            }
        }
        (None, None) => {}
    }
}

/// While any word of a link is hovered, every word of that link (same URL) shows the hover
/// style: foreground ink and a solid underline.
fn group_hover(
    theme: Res<Theme>,
    changed: Query<(), (With<MdLink>, Changed<Hovered>)>,
    mut links: Query<(&MdLink, &Hovered, &mut Ink, &mut UnderlineColor)>,
) {
    if !theme.is_changed() && changed.is_empty() {
        return;
    }
    let over: HashSet<String> = links
        .iter()
        .filter(|(_, hovered, ..)| hovered.get())
        .map(|(link, ..)| link.0.clone())
        .collect();
    for (link, _, mut ink, mut underline) in &mut links {
        let on = over.contains(&link.0);
        let (want_ink, want_line) = if on {
            (Swatch::Fg, theme.tokens.get(Swatch::Fg))
        } else {
            (Swatch::Green, Color::NONE)
        };
        if ink.0 != want_ink {
            ink.0 = want_ink;
        }
        if underline.0 != want_line {
            underline.0 = want_line;
        }
    }
}

/// Gives each emoji box its picture.
fn fill_emoji(
    mut commands: Commands,
    boxes: Query<(Entity, &MdEmoji), Without<ImageNode>>,
    mut emoji: ResMut<EmojiImages>,
    mut images: ResMut<Assets<Image>>,
) {
    for (entity, e) in &boxes {
        let handle = emoji.get(&e.0, &mut images);
        commands.entity(entity).insert(ImageNode::new(handle));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::asset::uuid_handle;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::text::TextSpan;
    use clusia_highlight::Class;

    use crate::snapshot::Snapshot;
    use crate::testing;
    use crate::theme::{DARK, LIGHT};
    use crate::ui::markdown::parse::parse;

    fn fonts() -> UiFonts {
        UiFonts {
            sans: uuid_handle!("00000000-0000-4000-8000-000000000001"),
            mono: uuid_handle!("00000000-0000-4000-8000-000000000002"),
            italic: uuid_handle!("00000000-0000-4000-8000-000000000003"),
        }
    }

    fn word(text: &str, space: bool) -> Inline {
        Inline::Word {
            text: text.into(),
            style: Style::default(),
            space_after: space,
        }
    }

    fn styled(text: &str, space: bool, style: Style) -> Inline {
        Inline::Word {
            text: text.into(),
            style,
            space_after: space,
        }
    }

    fn link(text: &str, space: bool, url: &str) -> Inline {
        styled(
            text,
            space,
            Style {
                link: Some(url.into()),
                ..Style::default()
            },
        )
    }

    /// Builds `blocks` under a fresh root and runs a frame.
    fn render_with(app: &mut App, blocks: Vec<Block>, opts: RenderOpts) -> Entity {
        let root = app.world_mut().spawn(Node::default()).id();
        let fonts = fonts();
        app.world_mut()
            .run_system_once(move |mut commands: Commands| {
                commands.entity(root).with_children(|p| {
                    markdown(p, &fonts, &blocks, &opts);
                });
            })
            .unwrap();
        app.update();
        root
    }

    fn render(app: &mut App, blocks: Vec<Block>) -> Entity {
        render_with(app, blocks, RenderOpts::default())
    }

    fn fresh() -> App {
        testing::app(Snapshot::default())
    }

    /// The texts of every `Text` under `root`, in tree order.
    fn texts(app: &App, root: Entity) -> Vec<String> {
        fn walk(world: &World, e: Entity, out: &mut Vec<String>) {
            if let Some(t) = world.get::<Text>(e) {
                out.push(t.0.clone());
            }
            if let Some(children) = world.get::<Children>(e) {
                for c in children {
                    walk(world, *c, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(app.world(), root, &mut out);
        out
    }

    fn entity_with_text(app: &App, root: Entity, wanted: &str) -> Entity {
        fn walk(world: &World, e: Entity, wanted: &str) -> Option<Entity> {
            if world.get::<Text>(e).is_some_and(|t| t.0 == wanted) {
                return Some(e);
            }
            world
                .get::<Children>(e)?
                .iter()
                .find_map(|c| walk(world, c, wanted))
        }
        walk(app.world(), root, wanted).unwrap_or_else(|| panic!("no text {wanted:?}"))
    }

    fn weight(app: &App, e: Entity) -> u16 {
        app.world().get::<TextFont>(e).unwrap().weight.0
    }

    fn size(app: &App, e: Entity) -> f32 {
        match app.world().get::<TextFont>(e).unwrap().font_size {
            FontSize::Px(v) => v,
            other => panic!("not px: {other:?}"),
        }
    }

    fn count<C: Component>(app: &mut App, root: Entity) -> usize {
        let mut q = app.world_mut().query::<(Entity, &C)>();
        let all: Vec<Entity> = q.iter(app.world()).map(|(e, _)| e).collect();
        all.into_iter()
            .filter(|e| is_under(app.world(), *e, root))
            .count()
    }

    fn is_under(world: &World, mut e: Entity, root: Entity) -> bool {
        loop {
            if e == root {
                return true;
            }
            match world.get::<ChildOf>(e) {
                Some(parent) => e = parent.parent(),
                None => return false,
            }
        }
    }

    #[test]
    fn words_wrap_in_a_row_and_spaces_are_margins() {
        let mut app = fresh();
        let root = render(
            &mut app,
            vec![Block::Paragraph(vec![
                word("Hello", true),
                word("world", false),
            ])],
        );
        let hello = entity_with_text(&app, root, "Hello");
        let world = entity_with_text(&app, root, "world");
        let row = app.world().get::<ChildOf>(hello).unwrap().parent();
        let row_node = app.world().get::<Node>(row).unwrap();
        assert_eq!(row_node.flex_wrap, FlexWrap::Wrap);
        assert_eq!(row_node.align_items, AlignItems::FlexEnd);
        assert_eq!(row_node.width, percent(100));
        assert_eq!(app.world().get::<Node>(hello).unwrap().margin.right, px(4));
        assert_eq!(app.world().get::<Node>(world).unwrap().margin.right, px(0));
        assert_eq!(
            app.world().get::<Node>(hello).unwrap().max_width,
            percent(100)
        );
        assert_eq!(
            app.world().get::<TextLayout>(hello).unwrap().linebreak,
            LineBreak::WordOrCharacter
        );
        assert_eq!(app.world().get::<TextColor>(hello).unwrap().0, LIGHT.fg);
        assert_eq!(texts(&app, root), ["Hello", "world"]);
    }

    #[test]
    fn styles_pick_faces_and_chips() {
        let mut app = fresh();
        let strong = Style {
            strong: true,
            ..Style::default()
        };
        let em = Style {
            em: true,
            ..Style::default()
        };
        let strike = Style {
            strike: true,
            ..Style::default()
        };
        let root = render(
            &mut app,
            vec![Block::Paragraph(vec![
                styled("bold", true, strong),
                styled("slanted", true, em),
                styled("gone", true, strike),
                Inline::Code {
                    text: "expires_at".into(),
                    space_after: false,
                },
            ])],
        );
        let bold = entity_with_text(&app, root, "bold");
        assert_eq!(weight(&app, bold), 600);
        let slanted = entity_with_text(&app, root, "slanted");
        assert_eq!(
            app.world().get::<TextFont>(slanted).unwrap().font,
            FontSource::Handle(fonts().italic)
        );
        let gone = entity_with_text(&app, root, "gone");
        assert!(app.world().get::<Strikethrough>(gone).is_some());
        let code = entity_with_text(&app, root, "expires_at");
        assert_eq!(app.world().get::<Fill>(code).unwrap().0, Swatch::Line);
        assert_eq!(
            app.world().get::<TextFont>(code).unwrap().font,
            FontSource::Handle(fonts().mono)
        );
        assert_eq!(size(&app, code), 12.0);
        assert_eq!(
            app.world().get::<BackgroundColor>(code).unwrap().0,
            LIGHT.line
        );
    }

    #[test]
    fn headings_follow_the_type_scale() {
        let mut app = fresh();
        let root = render(
            &mut app,
            vec![
                Block::Heading(1, vec![word("One", false)]),
                Block::Heading(2, vec![word("Two", false)]),
                Block::Heading(3, vec![word("Three", false)]),
            ],
        );
        let h = |name| entity_with_text(&app, root, name);
        assert_eq!((size(&app, h("One")), weight(&app, h("One"))), (20.0, 600));
        assert_eq!((size(&app, h("Two")), weight(&app, h("Two"))), (15.0, 600));
        assert_eq!(
            (size(&app, h("Three")), weight(&app, h("Three"))),
            (13.0, 600)
        );
    }

    #[test]
    fn link_words_share_the_url_and_the_underline() {
        let mut app = fresh();
        let url = "https://github.com/rzorzal/clusia";
        let root = render(
            &mut app,
            vec![Block::Paragraph(vec![
                link("the", true, url),
                link("race", true, url),
                link("test", true, url),
                word("now", false),
            ])],
        );
        assert_eq!(
            texts(&app, root),
            ["the ", "race ", "test", "now"],
            "inside a link the space is part of the text"
        );
        let first = entity_with_text(&app, root, "the ");
        let last = entity_with_text(&app, root, "test");
        let w = app.world();
        assert_eq!(w.get::<Node>(first).unwrap().margin.right, px(0));
        assert_eq!(w.get::<Node>(last).unwrap().margin.right, px(4));
        assert!(w.get::<Underline>(first).is_some());
        assert_eq!(w.get::<MdLink>(last).unwrap().0, url);
        assert_eq!(w.get::<Ink>(first).unwrap().0, Swatch::Green);
        assert_eq!(
            count::<TabIndex>(&mut app, root),
            1,
            "one Tab stop per link"
        );
        assert!(app.world().get::<TabIndex>(first).is_some());
        let second = entity_with_text(&app, root, "race ");
        testing::activate(&mut app, second);
        assert_eq!(app.world().resource::<OpenUrls>().0, [url]);
    }

    #[test]
    fn hovering_one_word_lights_the_whole_link() {
        let mut app = fresh();
        let (a, b) = (
            "https://github.com/rzorzal/clusia",
            "https://github.com/octo",
        );
        let root = render(
            &mut app,
            vec![Block::Paragraph(vec![
                link("one", true, a),
                link("two", true, a),
                link("other", false, b),
            ])],
        );
        let one = entity_with_text(&app, root, "one ");
        let two = entity_with_text(&app, root, "two");
        let other = entity_with_text(&app, root, "other");
        app.world_mut().entity_mut(two).insert(Hovered(true));
        app.update();
        let w = app.world();
        assert_eq!(w.get::<TextColor>(one).unwrap().0, LIGHT.fg);
        assert_eq!(w.get::<UnderlineColor>(one).unwrap().0, LIGHT.fg);
        assert_eq!(w.get::<TextColor>(other).unwrap().0, LIGHT.green);
        assert_eq!(w.get::<UnderlineColor>(other).unwrap().0, Color::NONE);
        app.world_mut().entity_mut(two).insert(Hovered(false));
        app.update();
        assert_eq!(app.world().get::<TextColor>(one).unwrap().0, LIGHT.green);
        testing::set_config_locally(&mut app, "appearance.theme", "dark");
        assert_eq!(app.world().get::<TextColor>(one).unwrap().0, DARK.green);
    }

    #[test]
    fn emoji_boxes_get_their_picture() {
        let mut app = fresh();
        let root = render(&mut app, parse("ship :rocket:"));
        let boxes = count::<MdEmoji>(&mut app, root);
        assert_eq!(boxes, 1);
        let mut q = app.world_mut().query::<(&MdEmoji, &Node, &ImageNode)>();
        let (emoji, node, _) = q.single(app.world()).unwrap();
        assert!(
            emoji.0.contains("1f680"),
            "the rocket's Twemoji file: {}",
            emoji.0
        );
        let (Val::Px(w), Val::Px(h)) = (node.width, node.height) else {
            panic!("a fixed size")
        };
        assert!((w - 17.55).abs() < 0.01 && w == h, "{w} x {h}");
    }

    #[test]
    fn images_are_slots_and_other_sites_are_chips() {
        let mut app = fresh();
        let github = "https://github.com/user-attachments/assets/1.png";
        let giphy = "https://media1.giphy.com/media/x/giphy.gif";
        let other = "https://example.com/x.png";
        let image = |url: &str| Inline::Image {
            url: url.into(),
            alt: "alt".into(),
            space_after: false,
        };
        let blocks = vec![Block::Paragraph(vec![
            image(github),
            image(giphy),
            image(other),
        ])];
        let root = render(&mut app, blocks.clone());
        assert_eq!(count::<MdImage>(&mut app, root), 2);
        let chip = entity_with_text(&app, root, "Image from another site — open in browser");
        assert_eq!(app.world().get::<MdLink>(chip).unwrap().0, other);
        let on = render_with(
            &mut app,
            blocks,
            RenderOpts {
                load_external: true,
                ..RenderOpts::default()
            },
        );
        assert_eq!(count::<MdImage>(&mut app, on), 3);
        assert!(is_external("http://github.com/a.png"), "https only");
        assert!(!is_external("https://camo.githubusercontent.com/abc"));
        assert!(is_external("https://github.com.evil.example/a.png"));
    }

    #[test]
    fn lists_quotes_rules_and_tables() {
        let mut app = fresh();
        let item = |task, text: &str| ListItem {
            task,
            blocks: vec![Block::Paragraph(vec![word(text, false)])],
        };
        let root = render(
            &mut app,
            vec![
                Block::List {
                    ordered: true,
                    items: vec![item(None, "first"), item(None, "second")],
                },
                Block::List {
                    ordered: false,
                    items: vec![
                        item(None, "dot"),
                        item(Some(true), "done"),
                        item(Some(false), "todo"),
                    ],
                },
                Block::Quote(vec![Block::Paragraph(vec![word("quoted", false)])]),
                Block::Rule,
                Block::Table {
                    head: vec![vec![word("Name", false)], vec![word("Role", false)]],
                    rows: vec![vec![vec![word("octo", false)], vec![word("author", false)]]],
                },
            ],
        );
        assert_eq!(
            texts(&app, root),
            [
                "1.", "first", "2.", "second", "•", "dot", "✓", "done", "todo", "quoted", "Name",
                "Role", "octo", "author"
            ]
        );
        let quote = entity_with_text(&app, root, "quoted");
        assert_eq!(app.world().get::<Ink>(quote).unwrap().0, Swatch::Muted);
        let name = entity_with_text(&app, root, "Name");
        assert_eq!(weight(&app, name), 600, "table heads are bold");
        assert_eq!(weight(&app, entity_with_text(&app, root, "octo")), 400);
    }

    #[test]
    fn code_blocks_are_colored_lines_with_a_cap() {
        let mut app = fresh();
        let root = render(
            &mut app,
            vec![Block::Code {
                lang: Some("rust".into()),
                text: "let x = 1;\nlet y = 2;\n".into(),
            }],
        );
        let mut q = app.world_mut().query::<(&Ink, &TextSpan)>();
        let keywords = q
            .iter(app.world())
            .filter(|(ink, _)| ink.0 == Swatch::Code(Class::Keyword))
            .count();
        assert_eq!(keywords, 2, "`let` on both lines is a keyword");
        assert!(count::<TextSpan>(&mut app, root) > 2);

        let long = (1..=250).map(|n| format!("line {n}\n")).collect::<String>();
        let capped = render(
            &mut app,
            vec![Block::Code {
                lang: None,
                text: long,
            }],
        );
        let all = texts(&app, capped);
        assert_eq!(all.last().map(String::as_str), Some("… 50 more lines"));
    }

    #[test]
    fn suggestions_are_a_two_row_mini_diff() {
        let mut app = fresh();
        let opts = RenderOpts {
            suggestion_base: Some("let a = 1;".into()),
            ..RenderOpts::default()
        };
        let root = render_with(&mut app, vec![Block::Suggestion("let a = 2;".into())], opts);
        let fills = |app: &mut App, swatch| {
            let mut q = app.world_mut().query::<&Fill>();
            q.iter(app.world()).filter(|f| f.0 == swatch).count()
        };
        assert_eq!(fills(&mut app, Swatch::RemovedBg), 1);
        assert_eq!(fills(&mut app, Swatch::AddedBg), 1);
        assert!(texts(&app, root).contains(&"Suggested change".to_string()));

        let mut alone = fresh();
        render(&mut alone, vec![Block::Suggestion("let a = 2;".into())]);
        assert_eq!(
            fills(&mut alone, Swatch::RemovedBg),
            0,
            "no base: only the added row"
        );
        assert_eq!(fills(&mut alone, Swatch::AddedBg), 1);
    }

    #[test]
    fn a_line_is_inline_only_and_cut_with_an_ellipsis() {
        let blocks = vec![
            Block::Paragraph(vec![
                word("See", true),
                Inline::Image {
                    url: "https://github.com/a.png".into(),
                    alt: "the graph".into(),
                    space_after: false,
                },
            ]),
            Block::Code {
                lang: None,
                text: "let a = 1;\nlet b = 2;".into(),
            },
        ];
        let words = |items: &[Inline]| -> Vec<String> {
            items
                .iter()
                .map(|i| match i {
                    Inline::Word { text, .. } | Inline::Code { text, .. } => text.clone(),
                    other => format!("{other:?}"),
                })
                .collect()
        };
        assert_eq!(
            words(&line_inlines(&blocks, 80)),
            ["See", "the graph", "let a = 1;"],
            "images give their alt text, code its first line"
        );
        let cut = line_inlines(&blocks, 6);
        assert_eq!(words(&cut), ["See", "th…"]);
        assert!(line_inlines(&[Block::Rule], 10).is_empty());
        let spaced = [Block::Paragraph(vec![
            word("aaa", true),
            word("bbb", true),
            word("ccc", false),
        ])];
        assert_eq!(
            words(&line_inlines(&spaced, 8)),
            ["aaa", "bbb…"],
            "stopping between words still says so"
        );
        assert_eq!(words(&line_inlines(&spaced, 12)), ["aaa", "bbb", "ccc"]);
        let linked = [Block::Paragraph(vec![link(
            "site",
            false,
            "https://github.com/octo",
        )])];
        let flat = line_inlines(&linked, 20);
        assert!(matches!(&flat[0], Inline::Word { style, .. } if style.link.is_none()));
    }

    #[test]
    fn markdown_line_draws_one_row() {
        let mut app = fresh();
        let root = app.world_mut().spawn(Node::default()).id();
        let fonts = fonts();
        app.world_mut()
            .run_system_once(move |mut commands: Commands| {
                commands.entity(root).with_children(|p| {
                    markdown_line(
                        p,
                        &fonts,
                        &[Block::Paragraph(vec![
                            word("one", true),
                            word("two", false),
                        ])],
                        Type::BODY,
                        &RenderOpts::default(),
                        80,
                    );
                });
            })
            .unwrap();
        app.update();
        assert_eq!(texts(&app, root), ["one", "two"]);
        assert_eq!(count::<MdBody>(&mut app, root), 0, "no block column");
    }

    #[test]
    fn copy_button_copies_the_source() {
        let mut app = fresh();
        let fonts = fonts();
        let button = app
            .world_mut()
            .run_system_once(move |mut commands: Commands| {
                commands.spawn(copy_button(&fonts, "**hi** there")).id()
            })
            .unwrap();
        app.update();
        testing::activate(&mut app, button);
        assert_eq!(app.world().resource::<CopiedText>().0, ["**hi** there"]);
    }

    #[test]
    fn a_hard_break_forces_a_new_line() {
        let mut app = fresh();
        let root = render(
            &mut app,
            vec![Block::Paragraph(vec![
                word("a", false),
                Inline::Break,
                word("b", false),
            ])],
        );
        let mut q = app.world_mut().query::<(&Node, Option<&Text>)>();
        let breaks = q
            .iter(app.world())
            .filter(|(n, t)| t.is_none() && n.width == percent(100) && n.height == px(0))
            .count();
        assert_eq!(breaks, 1);
        assert_eq!(texts(&app, root), ["a", "b"]);
    }

    #[test]
    fn render_opts_follow_the_config() {
        let mut config = clusia_core::Config::default();
        assert_eq!(
            RenderOpts::from_config(13.0, &config),
            RenderOpts {
                code_size: 13.0,
                suggestion_base: None,
                load_external: false,
            }
        );
        config.media.load_external_images = true;
        assert!(RenderOpts::from_config(14.0, &config).load_external);
    }
}

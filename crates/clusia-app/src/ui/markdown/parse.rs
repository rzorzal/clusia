//! Markdown → blocks of inline runs, with no Bevy in sight.
//!
//! Every word, code run, emoji and image is its own `Inline`, because the window lays a
//! paragraph out as a wrapping row of nodes. `space_after` records the whitespace that
//! followed an inline so the row can draw it as a margin that never starts a line.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::ui::emoji::{self, Piece};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Paragraph(Vec<Inline>),
    Heading(u8, Vec<Inline>),
    List {
        ordered: bool,
        items: Vec<ListItem>,
    },
    Quote(Vec<Block>),
    Code {
        lang: Option<String>,
        text: String,
    },
    /// The body of a ```` ```suggestion ```` fence.
    Suggestion(String),
    Table {
        head: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    Rule,
}

/// `task` is `Some(checked)` for a `- [ ]` / `- [x]` item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Word {
        text: String,
        style: Style,
        space_after: bool,
    },
    Code {
        text: String,
        space_after: bool,
    },
    /// A Twemoji file name such as `1f680.png`.
    Emoji {
        file: &'static str,
        space_after: bool,
    },
    Image {
        url: String,
        alt: String,
        space_after: bool,
    },
    Break,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Style {
    pub strong: bool,
    pub em: bool,
    pub strike: bool,
    pub link: Option<String>,
}

impl Inline {
    fn set_space_after(&mut self, on: bool) {
        match self {
            Inline::Word { space_after, .. }
            | Inline::Code { space_after, .. }
            | Inline::Emoji { space_after, .. }
            | Inline::Image { space_after, .. } => *space_after = on,
            Inline::Break => {}
        }
    }
}

pub fn parse(md: &str) -> Vec<Block> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut p = Builder::default();
    p.stack.push(Frame::Container(Vec::new()));
    for event in Parser::new_ext(md, options) {
        p.event(event);
    }
    p.flush_text();
    p.flush_paragraph();
    match p.stack.pop() {
        Some(Frame::Container(blocks)) => blocks,
        _ => Vec::new(),
    }
}

enum Frame {
    /// The document or a block quote.
    Container(Vec<Block>),
    List {
        ordered: bool,
        items: Vec<ListItem>,
    },
    Item {
        task: Option<bool>,
        blocks: Vec<Block>,
    },
}

#[derive(Default)]
struct TableState {
    head: Vec<Vec<Inline>>,
    rows: Vec<Vec<Vec<Inline>>>,
    cells: Vec<Vec<Inline>>,
}

#[derive(Default)]
struct Builder {
    stack: Vec<Frame>,
    inlines: Vec<Inline>,
    /// Adjacent text events, joined so a word or URL cut by the parser is one word again.
    text: String,
    strong: u32,
    em: u32,
    strike: u32,
    links: Vec<String>,
    code: Option<(Option<String>, String)>,
    image: Option<(String, String)>,
    table: Option<TableState>,
}

impl Builder {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Text(t) => self.text(&t),
            Event::SoftBreak => self.text.push(' '),
            other => {
                self.flush_text();
                self.other(other);
            }
        }
    }

    fn other(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(end) => self.end(end),
            Event::Code(t) => match &mut self.image {
                Some((_, alt)) => alt.push_str(&t),
                None => self.inlines.push(Inline::Code {
                    text: t.to_string(),
                    space_after: false,
                }),
            },
            Event::HardBreak => {
                if !matches!(self.inlines.last(), None | Some(Inline::Break)) {
                    self.mark_space(false);
                    self.inlines.push(Inline::Break);
                }
            }
            Event::Rule => {
                self.flush_paragraph();
                self.push_block(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                if let Some(Frame::Item { task, .. }) = self.stack.last_mut() {
                    *task = Some(done);
                }
            }
            Event::Html(html) => {
                self.flush_paragraph();
                self.html(&html);
                self.flush_paragraph();
            }
            Event::InlineHtml(html) => self.html(&html),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph | Tag::Heading { .. } | Tag::HtmlBlock => self.flush_paragraph(),
            Tag::BlockQuote(_) => {
                self.flush_paragraph();
                self.stack.push(Frame::Container(Vec::new()));
            }
            Tag::CodeBlock(kind) => {
                self.flush_paragraph();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().map(str::to_string)
                    }
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(first) => {
                self.flush_paragraph();
                self.stack.push(Frame::List {
                    ordered: first.is_some(),
                    items: Vec::new(),
                });
            }
            Tag::Item => {
                self.flush_paragraph();
                self.stack.push(Frame::Item {
                    task: None,
                    blocks: Vec::new(),
                });
            }
            Tag::Table(_) => {
                self.flush_paragraph();
                self.table = Some(TableState::default());
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.cells.clear();
                }
            }
            Tag::Emphasis => self.em += 1,
            Tag::Strong => self.strong += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => self.links.push(dest_url.to_string()),
            Tag::Image { dest_url, .. } => self.image = Some((dest_url.to_string(), String::new())),
            _ => {}
        }
    }

    fn end(&mut self, end: TagEnd) {
        match end {
            TagEnd::Paragraph | TagEnd::HtmlBlock => self.flush_paragraph(),
            TagEnd::Heading(level) => {
                let inlines = self.take_inlines();
                self.push_block(Block::Heading(level as u8, inlines));
            }
            TagEnd::BlockQuote(_) => {
                self.flush_paragraph();
                if let Some(Frame::Container(blocks)) = self.stack.pop() {
                    self.push_block(Block::Quote(blocks));
                }
            }
            TagEnd::CodeBlock => {
                if let Some((lang, mut text)) = self.code.take() {
                    if text.ends_with('\n') {
                        text.pop();
                    }
                    self.push_block(match lang.as_deref() {
                        Some("suggestion") => Block::Suggestion(text),
                        _ => Block::Code { lang, text },
                    });
                }
            }
            TagEnd::Item => {
                self.flush_paragraph();
                if let Some(Frame::Item { task, blocks }) = self.stack.pop()
                    && let Some(Frame::List { items, .. }) = self.stack.last_mut()
                {
                    items.push(ListItem { task, blocks });
                }
            }
            TagEnd::List(_) => {
                if let Some(Frame::List { ordered, items }) = self.stack.pop() {
                    self.push_block(Block::List { ordered, items });
                }
            }
            TagEnd::TableCell => {
                let cell = self.take_inlines();
                if let Some(t) = &mut self.table {
                    t.cells.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.head = std::mem::take(&mut t.cells);
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.cells);
                    t.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.push_block(Block::Table {
                        head: t.head,
                        rows: t.rows,
                    });
                }
            }
            TagEnd::Emphasis => self.em = self.em.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                self.links.pop();
            }
            TagEnd::Image => {
                if let Some((url, alt)) = self.image.take() {
                    self.inlines.push(Inline::Image {
                        url,
                        alt,
                        space_after: false,
                    });
                }
            }
            _ => {}
        }
    }

    fn style(&self) -> Style {
        Style {
            strong: self.strong > 0,
            em: self.em > 0,
            strike: self.strike > 0,
            link: self.links.last().cloned(),
        }
    }

    fn push_block(&mut self, block: Block) {
        match self.stack.last_mut() {
            Some(Frame::Container(blocks)) | Some(Frame::Item { blocks, .. }) => blocks.push(block),
            _ => {}
        }
    }

    /// Closes the open run of inlines as a paragraph of the current container.
    fn flush_paragraph(&mut self) {
        self.flush_text();
        let inlines = self.take_inlines();
        if !inlines.is_empty() {
            self.push_block(Block::Paragraph(inlines));
        }
    }

    fn take_inlines(&mut self) -> Vec<Inline> {
        self.flush_text();
        let mut inlines = std::mem::take(&mut self.inlines);
        while matches!(inlines.last(), Some(Inline::Break)) {
            inlines.pop();
        }
        if let Some(last) = inlines.last_mut() {
            last.set_space_after(false);
        }
        join_link_spaces(&mut inlines);
        inlines
    }

    fn mark_space(&mut self, on: bool) {
        if let Some(last) = self.inlines.last_mut() {
            last.set_space_after(on);
        }
    }

    fn text(&mut self, t: &str) {
        if let Some((_, buf)) = &mut self.code {
            buf.push_str(t);
        } else if let Some((_, alt)) = &mut self.image {
            alt.push_str(t);
        } else {
            self.text.push_str(t);
        }
    }

    fn flush_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        if text.starts_with(char::is_whitespace) {
            self.space();
        }
        for (i, word) in text.split_whitespace().enumerate() {
            if i > 0 {
                self.space();
            }
            self.word(word);
        }
        if text.ends_with(char::is_whitespace) {
            self.space();
        }
    }

    fn space(&mut self) {
        if !matches!(self.inlines.last(), None | Some(Inline::Break)) {
            self.mark_space(true);
        }
    }

    /// One whitespace-free chunk: bare URLs become links, then `:shortcode:` and Unicode emoji
    /// become emoji, and what is left stays text.
    fn word(&mut self, word: &str) {
        let in_link = !self.links.is_empty();
        let mut rest = word;
        while !rest.is_empty() {
            let (before, url, after) = if in_link {
                (rest, None, "")
            } else {
                split_url(rest)
            };
            self.words_and_emoji(before);
            if let Some(url) = url {
                self.inlines.push(Inline::Word {
                    text: url.to_string(),
                    style: Style {
                        link: Some(url.to_string()),
                        ..self.style()
                    },
                    space_after: false,
                });
            }
            rest = after;
        }
    }

    fn words_and_emoji(&mut self, text: &str) {
        for segment in split_shortcodes(text) {
            match segment {
                Segment::Emoji(file) => self.inlines.push(Inline::Emoji {
                    file,
                    space_after: false,
                }),
                Segment::Text(t) => {
                    for piece in emoji::split_emoji(t) {
                        match piece {
                            Piece::Emoji(file) => self.inlines.push(Inline::Emoji {
                                file,
                                space_after: false,
                            }),
                            Piece::Text(t) => {
                                let style = self.style();
                                self.inlines.push(Inline::Word {
                                    text: t.to_string(),
                                    style,
                                    space_after: false,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    /// Pasted images arrive as `<img src="…">`, not as `![](…)`; `<br>` breaks the line; the
    /// rest of the HTML is dropped.
    fn html(&mut self, html: &str) {
        for tag in html_tags(html) {
            match tag {
                HtmlTag::Img { url, alt } => self.inlines.push(Inline::Image {
                    url,
                    alt,
                    space_after: false,
                }),
                HtmlTag::Br => {
                    if !matches!(self.inlines.last(), None | Some(Inline::Break)) {
                        self.mark_space(false);
                        self.inlines.push(Inline::Break);
                    }
                }
            }
        }
    }
}

/// The words of one link keep the space between them inside their text, so the underline
/// runs across it instead of stopping at a margin.
fn join_link_spaces(inlines: &mut [Inline]) {
    for i in 0..inlines.len().saturating_sub(1) {
        let next_link = match &inlines[i + 1] {
            Inline::Word { style, .. } => style.link.clone(),
            _ => None,
        };
        if let Inline::Word {
            text,
            style,
            space_after,
        } = &mut inlines[i]
            && *space_after
            && style.link.is_some()
            && style.link == next_link
        {
            text.push(' ');
            *space_after = false;
        }
    }
}

/// Splits `word` into text before the first `http(s)://` URL, the URL, and what follows.
/// Trailing punctuation (and a closing parenthesis with no opener in the URL) stays outside.
fn split_url(word: &str) -> (&str, Option<&str>, &str) {
    let mut from = 0;
    while let Some(found) = ["https://", "http://"]
        .iter()
        .filter_map(|s| word[from..].find(s).map(|p| (from + p, s.len())))
        .min()
    {
        let (start, scheme) = found;
        let boundary = word[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let mut end = word.len();
        while end > start + scheme {
            let url = &word[start..end];
            let last = url.chars().next_back().unwrap_or(' ');
            let closing = last == ')' && url.matches(')').count() > url.matches('(').count();
            if ".,:;!?'\"*_~".contains(last) || closing {
                end -= last.len_utf8();
            } else {
                break;
            }
        }
        if boundary && end > start + scheme {
            return (&word[..start], Some(&word[start..end]), &word[end..]);
        }
        from = start + scheme;
    }
    (word, None, "")
}

enum Segment<'a> {
    Text(&'a str),
    Emoji(&'static str),
}

/// Replaces each `:name:` GitHub knows with its emoji.
fn split_shortcodes(text: &str) -> Vec<Segment<'_>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut plain_from = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b':'
            && let Some(len) = text[i + 1..].find(':')
        {
            let name = &text[i + 1..i + 1 + len];
            let valid = !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_+-".contains(&b));
            if valid && let Some(entry) = emoji::by_shortcode(name) {
                if plain_from < i {
                    out.push(Segment::Text(&text[plain_from..i]));
                }
                out.push(Segment::Emoji(entry.file));
                i += len + 2;
                plain_from = i;
                continue;
            }
        }
        i += 1;
    }
    if plain_from < text.len() {
        out.push(Segment::Text(&text[plain_from..]));
    }
    out
}

enum HtmlTag {
    Img { url: String, alt: String },
    Br,
}

fn html_tags(html: &str) -> Vec<HtmlTag> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('>') else { break };
        let tag = &rest[..close];
        let name = tag
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match name.as_str() {
            "img" => {
                if let Some(url) = attribute(tag, "src") {
                    out.push(HtmlTag::Img {
                        url,
                        alt: attribute(tag, "alt").unwrap_or_default(),
                    });
                }
            }
            "br" => out.push(HtmlTag::Br),
            _ => {}
        }
        rest = &rest[close + 1..];
    }
    out
}

/// The quoted value of `name="…"` (or `'…'`) inside one tag.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find(name) {
        let at = from + p;
        from = at + name.len();
        let before_ok = at > 0 && lower.as_bytes()[at - 1].is_ascii_whitespace();
        let after = tag[from..].trim_start();
        if before_ok && let Some(value) = after.strip_prefix('=') {
            let value = value.trim_start();
            let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'')?;
            let inner = &value[1..];
            let end = inner.find(quote)?;
            return Some(inner[..end].to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, space_after: bool) -> Inline {
        Inline::Word {
            text: text.into(),
            style: Style::default(),
            space_after,
        }
    }

    fn styled(text: &str, style: Style, space_after: bool) -> Inline {
        Inline::Word {
            text: text.into(),
            style,
            space_after,
        }
    }

    fn link(url: &str) -> Style {
        Style {
            link: Some(url.into()),
            ..Style::default()
        }
    }

    fn para(md: &str) -> Vec<Inline> {
        match parse(md).as_slice() {
            [Block::Paragraph(inlines)] => inlines.clone(),
            other => panic!("not one paragraph: {other:?}"),
        }
    }

    #[test]
    fn paragraphs_and_spaces() {
        assert_eq!(
            para("hello  big\nworld"),
            vec![word("hello", true), word("big", true), word("world", false)]
        );
        assert_eq!(
            parse("hello"),
            vec![Block::Paragraph(vec![word("hello", false)])]
        );
        assert_eq!(parse("one\n\ntwo").len(), 2);
        assert!(parse("").is_empty() && parse("  \n\n ").is_empty());
        assert_eq!(
            para("a  \nb"),
            vec![word("a", false), Inline::Break, word("b", false)]
        );
    }

    #[test]
    fn styles_nest() {
        let strong = Style {
            strong: true,
            ..Style::default()
        };
        let both = Style {
            strong: true,
            em: true,
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
        assert_eq!(
            para("a **b *c* d** ~~e~~ _f_"),
            vec![
                word("a", true),
                styled("b", strong.clone(), true),
                styled("c", both, true),
                styled("d", strong, true),
                styled("e", strike, true),
                styled("f", em, false),
            ]
        );
        assert_eq!(
            para("foo**bar**"),
            vec![
                word("foo", false),
                styled(
                    "bar",
                    Style {
                        strong: true,
                        ..Style::default()
                    },
                    false
                )
            ]
        );
    }

    #[test]
    fn inline_code_keeps_its_text_and_spacing() {
        assert_eq!(
            para("call `a  b` now"),
            vec![
                word("call", true),
                Inline::Code {
                    text: "a  b".into(),
                    space_after: true
                },
                word("now", false)
            ]
        );
    }

    #[test]
    fn links_keep_their_inner_spaces_and_bare_urls_link_themselves() {
        let u = "https://github.com/rzorzal/clusia";
        assert_eq!(
            para("see [the race test](https://acme.dev/t) ok"),
            vec![
                word("see", true),
                styled("the ", link("https://acme.dev/t"), false),
                styled("race ", link("https://acme.dev/t"), false),
                styled("test", link("https://acme.dev/t"), true),
                word("ok", false),
            ]
        );
        assert_eq!(
            para(&format!("at {u}.")),
            vec![
                word("at", true),
                styled(u, link(u), false),
                word(".", false)
            ]
        );
        assert_eq!(
            para(&format!("({u}) and <{u}>")),
            vec![
                word("(", false),
                styled(u, link(u), false),
                word(")", true),
                word("and", true),
                styled(u, link(u), false),
            ]
        );
        assert_eq!(
            para("https://en.wikipedia.org/wiki/Rust_(language)"),
            vec![styled(
                "https://en.wikipedia.org/wiki/Rust_(language)",
                link("https://en.wikipedia.org/wiki/Rust_(language)"),
                false
            )]
        );
        assert_eq!(
            para("see xhttps://a.b and https://"),
            vec![
                word("see", true),
                word("xhttps://a.b", true),
                word("and", true),
                word("https://", false),
            ]
        );
        assert_eq!(
            para("snake_case_url https://a.b/c_d_e?x=1&y=2"),
            vec![
                word("snake_case_url", true),
                styled(
                    "https://a.b/c_d_e?x=1&y=2",
                    link("https://a.b/c_d_e?x=1&y=2"),
                    false
                ),
            ],
            "text the parser cuts at `_` and `&` is one word again"
        );
    }

    #[test]
    fn shortcodes_become_emoji_but_not_in_code() {
        assert_eq!(
            para("ship it :rocket:!"),
            vec![
                word("ship", true),
                word("it", true),
                Inline::Emoji {
                    file: "1f680.png",
                    space_after: false
                },
                word("!", false)
            ]
        );
        assert_eq!(
            para("`:tada:` :nope: 12:30:45"),
            vec![
                Inline::Code {
                    text: ":tada:".into(),
                    space_after: true
                },
                word(":nope:", true),
                word("12:30:45", false)
            ]
        );
        assert!(matches!(
            parse("```\n:tada:\n```").as_slice(),
            [Block::Code { text, .. }] if text == ":tada:"
        ));
        assert_eq!(
            para(":+1::tada:"),
            vec![
                Inline::Emoji {
                    file: "1f44d.png",
                    space_after: false
                },
                Inline::Emoji {
                    file: "1f389.png",
                    space_after: false
                }
            ]
        );
    }

    #[test]
    fn unicode_emoji_clusters_are_single_inlines() {
        let family = emoji::twemoji_file("👨\u{200D}👩\u{200D}👧\u{200D}👦").expect("family");
        assert_eq!(
            para("ok 👍🏽 👨\u{200D}👩\u{200D}👧\u{200D}👦 🇧🇷done"),
            vec![
                word("ok", true),
                Inline::Emoji {
                    file: "1f44d-1f3fd.png",
                    space_after: true
                },
                Inline::Emoji {
                    file: family,
                    space_after: true
                },
                Inline::Emoji {
                    file: "1f1e7-1f1f7.png",
                    space_after: false
                },
                word("done", false)
            ]
        );
        assert_eq!(para("© 2026"), vec![word("©", true), word("2026", false)]);
    }

    #[test]
    fn images_from_markdown_and_pasted_html() {
        assert_eq!(
            para("![a cat](https://media.giphy.com/c.gif) fun"),
            vec![
                Inline::Image {
                    url: "https://media.giphy.com/c.gif".into(),
                    alt: "a cat".into(),
                    space_after: true
                },
                word("fun", false)
            ]
        );
        let html = "<img width=\"500\" alt=\"shot\" src=\"https://github.com/user-attachments/assets/0a1b\" />";
        let image = Inline::Image {
            url: "https://github.com/user-attachments/assets/0a1b".into(),
            alt: "shot".into(),
            space_after: false,
        };
        assert_eq!(parse(html), vec![Block::Paragraph(vec![image.clone()])]);
        assert_eq!(
            para(&format!("a {html} b<br>c")),
            vec![
                word("a", true),
                Inline::Image {
                    url: "https://github.com/user-attachments/assets/0a1b".into(),
                    alt: "shot".into(),
                    space_after: true
                },
                word("b", false),
                Inline::Break,
                word("c", false)
            ]
        );
        assert_eq!(
            parse("<img alt='x'>"),
            Vec::<Block>::new(),
            "an image tag without a source is dropped"
        );
    }

    #[test]
    fn lists_and_task_items() {
        let blocks = parse("- one\n- [x] two\n- [ ] three\n\n1. a\n2. b");
        let [
            Block::List {
                ordered: false,
                items,
            },
            Block::List {
                ordered: true,
                items: numbered,
            },
        ] = blocks.as_slice()
        else {
            panic!("{blocks:?}")
        };
        assert_eq!(
            items.iter().map(|i| i.task).collect::<Vec<_>>(),
            vec![None, Some(true), Some(false)]
        );
        assert_eq!(
            items[0].blocks,
            vec![Block::Paragraph(vec![word("one", false)])]
        );
        assert_eq!(numbered.len(), 2);
    }

    #[test]
    fn nested_lists_and_loose_items() {
        let blocks = parse("- a\n  - b\n\n- c\n\n  more");
        let [Block::List { items, .. }] = blocks.as_slice() else {
            panic!("{blocks:?}")
        };
        assert_eq!(items.len(), 2);
        assert!(matches!(
            items[0].blocks.as_slice(),
            [Block::Paragraph(_), Block::List { .. }]
        ));
        assert!(matches!(
            items[1].blocks.as_slice(),
            [Block::Paragraph(_), Block::Paragraph(_)]
        ));
    }

    #[test]
    fn quotes_nest_blocks() {
        let blocks = parse("> quoted\n>\n> - item\n\nafter");
        let [Block::Quote(inner), Block::Paragraph(_)] = blocks.as_slice() else {
            panic!("{blocks:?}")
        };
        assert!(matches!(
            inner.as_slice(),
            [Block::Paragraph(_), Block::List { .. }]
        ));
    }

    #[test]
    fn headings_rules_and_tables() {
        let blocks = parse("## Title **x**\n\n---\n\n| a | b |\n|---|---|\n| 1 | `2` |\n| 3 | 4 |");
        let [
            Block::Heading(2, title),
            Block::Rule,
            Block::Table { head, rows },
        ] = blocks.as_slice()
        else {
            panic!("{blocks:?}")
        };
        assert_eq!(title.len(), 2);
        assert_eq!(head, &vec![vec![word("a", false)], vec![word("b", false)]]);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0][1],
            vec![Inline::Code {
                text: "2".into(),
                space_after: false
            }]
        );
        assert_eq!(rows[1][0], vec![word("3", false)]);
    }

    #[test]
    fn code_blocks_and_suggestions() {
        assert_eq!(
            parse("```rust\nlet x = 1;\n\nlet y = 2;\n```"),
            vec![Block::Code {
                lang: Some("rust".into()),
                text: "let x = 1;\n\nlet y = 2;".into()
            }]
        );
        assert_eq!(
            parse("```\nplain\n```"),
            vec![Block::Code {
                lang: None,
                text: "plain".into()
            }]
        );
        assert_eq!(
            parse("    indented"),
            vec![Block::Code {
                lang: None,
                text: "indented".into()
            }]
        );
        assert_eq!(
            parse("```suggestion\n    let n = 2;\n```"),
            vec![Block::Suggestion("    let n = 2;".into())]
        );
        assert_eq!(
            parse("```suggestion\n```"),
            vec![Block::Suggestion(String::new())]
        );
    }

    #[test]
    fn long_words_stay_whole() {
        let long = "x".repeat(300);
        let path = "crates/clusia-app/src/ui/markdown/parse.rs:120:44";
        assert_eq!(
            para(&format!("{long} {path}")),
            vec![word(&long, true), word(path, false)]
        );
    }
}

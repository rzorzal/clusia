//! Markdown → blocks of inline runs, with no Bevy in sight.
//!
//! Every word, code run, emoji and image is its own `Inline`, because the window lays a
//! paragraph out as a wrapping row of nodes. `space_after` records the whitespace that
//! followed an inline so the row can draw it as a margin that never starts a line.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use clusia_core::PrRef;

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
        link: Option<String>,
        space_after: bool,
    },
    /// A Twemoji file name such as `1f680.png`.
    Emoji {
        file: &'static str,
        link: Option<String>,
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
    parse_with(md, None)
}

/// `parse` for a comment on `pr`, where a bare `#123` names an issue or pull request of the
/// same repository and so can be a link.
pub fn parse_in(md: &str, pr: &PrRef) -> Vec<Block> {
    parse_with(md, Some(pr))
}

fn parse_with(md: &str, pr: Option<&PrRef>) -> Vec<Block> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut p = Builder {
        repo: pr.map(|pr| (pr.owner.clone(), pr.repo.clone())),
        ..Builder::default()
    };
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

/// Containers nested deeper than this are flattened into their parent, so no `Block` tree is
/// deep enough to overflow the stack through its recursive `Drop`, `Clone` or `PartialEq`.
const MAX_DEPTH: usize = 32;

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
    /// One entry per open quote, list or item: `true` when it was flattened and has no frame.
    flattened: Vec<bool>,
    inlines: Vec<Inline>,
    /// Adjacent text events, joined so a word or URL cut by the parser is one word again.
    text: String,
    strong: u32,
    em: u32,
    strike: u32,
    /// One entry per open link: its address, or `None` when the window cannot open it.
    links: Vec<Option<String>>,
    code: Option<(Option<String>, String)>,
    image: Option<(String, String)>,
    table: Option<TableState>,
    /// The closing text of the HTML comment or `<script>` / `<style>` body still open.
    html_skip: Option<&'static str>,
    /// The owner and name of the repository a bare `#123` refers to.
    repo: Option<(String, String)>,
}

impl Builder {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Text(t) => self.text(&t),
            Event::SoftBreak => match &mut self.image {
                Some((_, alt)) => alt.push(' '),
                None => self.text.push(' '),
            },
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
                    link: self.style().link,
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
                let flat = self.stack.len() >= MAX_DEPTH;
                self.flattened.push(flat);
                if !flat {
                    self.stack.push(Frame::Container(Vec::new()));
                }
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
                let flat = self.stack.len() >= MAX_DEPTH;
                self.flattened.push(flat);
                if !flat {
                    self.stack.push(Frame::List {
                        ordered: first.is_some(),
                        items: Vec::new(),
                    });
                }
            }
            Tag::Item => {
                self.flush_paragraph();
                // An item follows its list: it is flat exactly when the list was.
                let flat = !matches!(self.stack.last(), Some(Frame::List { .. }));
                self.flattened.push(flat);
                if !flat {
                    self.stack.push(Frame::Item {
                        task: None,
                        blocks: Vec::new(),
                    });
                }
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
            Tag::Link { dest_url, .. } => self.links.push(openable(&dest_url)),
            Tag::Image { dest_url, .. } => self.image = Some((dest_url.to_string(), String::new())),
            _ => {}
        }
    }

    fn end(&mut self, end: TagEnd) {
        match end {
            TagEnd::Paragraph => self.flush_paragraph(),
            TagEnd::HtmlBlock => {
                self.html_skip = None;
                self.flush_paragraph();
            }
            TagEnd::Heading(level) => {
                let inlines = self.take_inlines();
                self.push_block(Block::Heading(level as u8, inlines));
            }
            TagEnd::BlockQuote(_) => {
                self.flush_paragraph();
                if !self.flattened.pop().unwrap_or(false)
                    && let Some(Frame::Container(blocks)) = self.stack.pop()
                {
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
                if !self.flattened.pop().unwrap_or(false)
                    && let Some(Frame::Item { task, blocks }) = self.stack.pop()
                    && let Some(Frame::List { items, .. }) = self.stack.last_mut()
                {
                    items.push(ListItem { task, blocks });
                }
            }
            TagEnd::List(_) => {
                if !self.flattened.pop().unwrap_or(false)
                    && let Some(Frame::List { ordered, items }) = self.stack.pop()
                {
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
            link: self.links.last().cloned().flatten(),
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
                        link: openable(url),
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
                    link: self.style().link,
                    space_after: false,
                }),
                Segment::Text(t) => {
                    for piece in emoji::split_emoji(t) {
                        match piece {
                            Piece::Emoji(file) => self.inlines.push(Inline::Emoji {
                                file,
                                link: self.style().link,
                                space_after: false,
                            }),
                            Piece::Text(t) => self.references(t),
                        }
                    }
                }
            }
        }
    }

    /// Text that may hold `@user`, `#123` and `owner/repo#123`, each of which becomes a link
    /// unless the text is already inside one.
    fn references(&mut self, text: &str) {
        let style = self.style();
        let repo = self.repo.as_ref().map(|(o, r)| (o.as_str(), r.as_str()));
        let spans = if self.links.is_empty() {
            split_references(text, repo)
        } else {
            vec![(text, None)]
        };
        for (piece, url) in spans {
            let style = match url {
                Some(url) => Style {
                    link: Some(url),
                    ..style.clone()
                },
                None => style.clone(),
            };
            self.inlines.push(Inline::Word {
                text: piece.to_string(),
                style,
                space_after: false,
            });
        }
    }

    /// Pasted images arrive as `<img src="…">`, not as `![](…)`; `<br>` breaks the line. The
    /// text between the tags of an HTML block (`<div>hello</div>`) stays; the tags, comments
    /// and the contents of `<script>` and `<style>` do not.
    fn html(&mut self, html: &str) {
        for part in html_parts(html, &mut self.html_skip) {
            match part {
                HtmlPart::Text(t) => {
                    self.text(&decode_entities(t));
                    self.flush_text();
                }
                HtmlPart::Img { url, alt } => {
                    self.flush_text();
                    self.inlines.push(Inline::Image {
                        url,
                        alt,
                        space_after: false,
                    });
                }
                HtmlPart::Br => {
                    self.flush_text();
                    if !matches!(self.inlines.last(), None | Some(Inline::Break)) {
                        self.mark_space(false);
                        self.inlines.push(Inline::Break);
                    }
                }
            }
        }
    }
}

/// `url` when the window can open it (only https; see `platform_open::open_url`), so any
/// other link is shown as plain text rather than as a link that does nothing.
fn openable(url: &str) -> Option<String> {
    url.starts_with("https://").then(|| url.to_string())
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

/// Splits `text` into runs, each with the GitHub address it links to, if any: `@user`,
/// `@org/team`, `owner/repo#123`, and `#123` when `repo` says which repository it is in.
fn split_references<'a>(
    text: &'a str,
    repo: Option<(&str, &str)>,
) -> Vec<(&'a str, Option<String>)> {
    let mut out = Vec::new();
    let mut plain_from = 0;
    let mut prev: Option<char> = None;
    let mut chars = text.char_indices();
    while let Some((i, c)) = chars.next() {
        let boundary = !prev.is_some_and(|p| p.is_alphanumeric() || "_-/.@#".contains(p));
        prev = Some(c);
        if !boundary {
            continue;
        }
        let rest = &text[i..];
        let found = match c {
            '@' => mention(rest),
            '#' => repo.and_then(|(owner, name)| issue(rest, owner, name)),
            c if c.is_ascii_alphanumeric() => full_reference(rest),
            _ => None,
        };
        let Some((len, url)) = found else { continue };
        if plain_from < i {
            out.push((&text[plain_from..i], None));
        }
        out.push((&rest[..len], Some(url)));
        plain_from = i + len;
        // The reference's own characters are consumed; the next one starts after it.
        let skip = text[i..plain_from].chars().count() - 1;
        chars.by_ref().take(skip).for_each(drop);
        prev = text[..plain_from].chars().next_back();
    }
    if plain_from < text.len() {
        out.push((&text[plain_from..], None));
    }
    out
}

/// The length of the leading run of `rest` whose characters satisfy `ok`.
fn run(rest: &str, ok: impl Fn(char) -> bool) -> usize {
    rest.find(|c: char| !ok(c)).unwrap_or(rest.len())
}

fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

/// `@user`, or `@org/team`.
fn mention(rest: &str) -> Option<(usize, String)> {
    let body = &rest[1..];
    let user = run(body, name_char);
    if user == 0 || user > 39 || body.starts_with('-') || body[..user].ends_with('-') {
        return None;
    }
    if let Some(after) = body[user..].strip_prefix('/') {
        let team = run(after, |c| name_char(c) || c == '_');
        if team > 0 && !after[..team].ends_with('-') {
            let url = format!(
                "https://github.com/orgs/{}/teams/{}",
                &body[..user],
                &after[..team]
            );
            return Some((1 + user + 1 + team, url));
        }
    }
    Some((1 + user, format!("https://github.com/{}", &body[..user])))
}

/// The digits after the `#` of `rest`, when nothing alphanumeric follows them.
fn number(rest: &str) -> Option<usize> {
    let digits = run(rest, |c| c.is_ascii_digit());
    let follows = rest[digits..].chars().next();
    (digits > 0 && digits <= 9 && !follows.is_some_and(|c| c.is_alphanumeric() || c == '_'))
        .then_some(digits)
}

fn issue(rest: &str, owner: &str, repo: &str) -> Option<(usize, String)> {
    let digits = number(&rest[1..])?;
    let n = &rest[1..1 + digits];
    Some((
        1 + digits,
        format!("https://github.com/{owner}/{repo}/issues/{n}"),
    ))
}

/// `owner/repo#123`.
fn full_reference(rest: &str) -> Option<(usize, String)> {
    let owner = run(rest, |c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let after = rest[owner..].strip_prefix('/')?;
    let repo = run(after, |c| c.is_ascii_alphanumeric() || "._-".contains(c));
    let tail = after[repo..].strip_prefix('#')?;
    let digits = number(tail)?;
    if owner == 0 || repo == 0 || owner > 39 {
        return None;
    }
    let url = format!(
        "https://github.com/{}/{}/issues/{}",
        &rest[..owner],
        &after[..repo],
        &tail[..digits]
    );
    Some((owner + 1 + repo + 1 + digits, url))
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

enum HtmlPart<'a> {
    Text(&'a str),
    Img { url: String, alt: String },
    Br,
}

/// What an HTML-block line says that the window shows. `skip` is the construct (a comment or
/// the body of `<script>` / `<style>`) a previous line left open, and the closing text it is
/// waiting for.
fn html_parts<'a>(html: &'a str, skip: &mut Option<&'static str>) -> Vec<HtmlPart<'a>> {
    let mut out = Vec::new();
    let mut rest = html;
    loop {
        if let Some(close) = *skip {
            let lower = rest.to_ascii_lowercase();
            match lower.find(close) {
                Some(at) => {
                    rest = &rest[at + close.len()..];
                    *skip = None;
                }
                None => return out,
            }
        }
        let Some(open) = rest.find('<') else {
            if !rest.is_empty() {
                out.push(HtmlPart::Text(rest));
            }
            return out;
        };
        let after = &rest[open + 1..];
        let starts_tag = after.starts_with(['!', '/']) || after.starts_with(char::is_alphabetic);
        if !starts_tag {
            // A lone `<` is text; the next one starts the search again.
            let upto = open + 1;
            out.push(HtmlPart::Text(&rest[..upto]));
            rest = &rest[upto..];
            continue;
        }
        if open > 0 {
            out.push(HtmlPart::Text(&rest[..open]));
        }
        if let Some(comment) = after.strip_prefix("!--") {
            *skip = Some("-->");
            rest = comment;
            continue;
        }
        let Some(close) = after.find('>') else {
            return out;
        };
        let tag = &after[..close];
        let name = tag
            .split(|c: char| c.is_whitespace() || c == '/')
            .find(|n| !n.is_empty())
            .unwrap_or("")
            .to_ascii_lowercase();
        let closing = tag.starts_with('/');
        match name.as_str() {
            "img" => {
                if let Some(url) = attribute(tag, "src") {
                    out.push(HtmlPart::Img {
                        url,
                        alt: attribute(tag, "alt").unwrap_or_default(),
                    });
                }
            }
            "br" => out.push(HtmlPart::Br),
            "script" if !closing && !tag.ends_with('/') => *skip = Some("</script>"),
            "style" if !closing && !tag.ends_with('/') => *skip = Some("</style>"),
            _ => {}
        }
        rest = &after[close + 1..];
    }
}

/// The entities HTML written by hand uses; others stay as written.
fn decode_entities(text: &str) -> String {
    [
        ("&nbsp;", " "),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&amp;", "&"),
    ]
    .iter()
    .fold(text.to_string(), |t, (from, to)| t.replace(from, to))
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
    fn code_and_emoji_inside_a_link_keep_it() {
        let url = "https://github.com/acme/widgets";
        assert_eq!(
            para("[`parse()` :rocket: 🚀](https://github.com/acme/widgets)"),
            vec![
                Inline::Code {
                    text: "parse()".into(),
                    link: Some(url.into()),
                    space_after: true,
                },
                Inline::Emoji {
                    file: "1f680.png",
                    link: Some(url.into()),
                    space_after: true,
                },
                Inline::Emoji {
                    file: "1f680.png",
                    link: Some(url.into()),
                    space_after: false,
                },
            ]
        );
        assert_eq!(
            para("`a` :rocket:"),
            vec![
                Inline::Code {
                    text: "a".into(),
                    link: None,
                    space_after: true,
                },
                Inline::Emoji {
                    file: "1f680.png",
                    link: None,
                    space_after: false,
                },
            ]
        );
        assert_eq!(
            para("[`a`](http://insecure.example)"),
            vec![Inline::Code {
                text: "a".into(),
                link: None,
                space_after: false,
            }],
            "a link the window cannot open is not a link"
        );
    }

    #[test]
    fn html_blocks_keep_their_text() {
        assert_eq!(para("<div>hello</div>"), vec![word("hello", false)]);
        assert_eq!(
            parse("<details>\n<summary>Details</summary>\n\nmore here\n\n</details>"),
            vec![
                Block::Paragraph(vec![word("Details", false)]),
                Block::Paragraph(vec![word("more", true), word("here", false)]),
            ]
        );
        assert_eq!(
            para("<p>fish &amp; chips &lt;3</p>"),
            vec![
                word("fish", true),
                word("&", true),
                word("chips", true),
                word("<3", false)
            ]
        );
        assert_eq!(
            para("<div>a < b</div>"),
            vec![word("a", true), word("<", true), word("b", false)]
        );
    }

    #[test]
    fn html_blocks_drop_comments_scripts_and_styles() {
        assert_eq!(
            para("<!-- hidden > note -->\n<div>shown</div>"),
            vec![word("shown", false)]
        );
        assert_eq!(
            para("<!--\nhidden\n-->\n<div>shown</div>"),
            vec![word("shown", false)]
        );
        assert_eq!(
            para("<style>\np { color: red }\n</style>\n<div>shown</div>"),
            vec![word("shown", false)]
        );
    }

    #[test]
    fn html_images_keep_their_place_among_text() {
        assert_eq!(
            para("<p>see <img src=\"https://example.com/a.png\" alt=\"a\"> below</p>"),
            vec![
                word("see", true),
                Inline::Image {
                    url: "https://example.com/a.png".into(),
                    alt: "a".into(),
                    space_after: true,
                },
                word("below", false),
            ]
        );
        assert_eq!(
            para("a <b>bold</b> c"),
            vec![word("a", true), word("bold", true), word("c", false)],
            "inline tags are dropped, their text is already text"
        );
    }

    fn widgets() -> PrRef {
        PrRef::new("acme", "widgets", 7).unwrap()
    }

    fn para_in(md: &str) -> Vec<Inline> {
        match parse_in(md, &widgets()).as_slice() {
            [Block::Paragraph(inlines)] => inlines.clone(),
            other => panic!("not one paragraph: {other:?}"),
        }
    }

    #[test]
    fn mentions_and_references_become_links() {
        assert_eq!(
            para_in("thanks @alice, see #12 and acme/gadgets#3."),
            vec![
                word("thanks", true),
                styled("@alice", link("https://github.com/alice"), false),
                word(",", true),
                word("see", true),
                styled(
                    "#12",
                    link("https://github.com/acme/widgets/issues/12"),
                    true
                ),
                word("and", true),
                styled(
                    "acme/gadgets#3",
                    link("https://github.com/acme/gadgets/issues/3"),
                    false
                ),
                word(".", false),
            ]
        );
        assert_eq!(
            para_in("ping @acme/reviewers"),
            vec![
                word("ping", true),
                styled(
                    "@acme/reviewers",
                    link("https://github.com/orgs/acme/teams/reviewers"),
                    false
                ),
            ]
        );
    }

    #[test]
    fn text_that_only_looks_like_a_reference_stays_text() {
        assert_eq!(
            para_in("mail bob@example.com, C#, #12abc, @-x, #, @"),
            vec![
                word("mail", true),
                word("bob@example.com,", true),
                word("C#,", true),
                word("#12abc,", true),
                word("@-x,", true),
                word("#,", true),
                word("@", false),
            ]
        );
        assert_eq!(
            para_in("`@alice` [@bob](https://github.com/bob2) <https://example.com/#12>"),
            vec![
                Inline::Code {
                    text: "@alice".into(),
                    link: None,
                    space_after: true,
                },
                styled("@bob", link("https://github.com/bob2"), true),
                styled(
                    "https://example.com/#12",
                    link("https://example.com/#12"),
                    false
                ),
            ]
        );
    }

    #[test]
    fn a_bare_issue_number_needs_the_repository() {
        assert_eq!(para("see #12"), vec![word("see", true), word("#12", false)]);
        assert_eq!(
            para("see @alice acme/widgets#12"),
            vec![
                word("see", true),
                styled("@alice", link("https://github.com/alice"), true),
                styled(
                    "acme/widgets#12",
                    link("https://github.com/acme/widgets/issues/12"),
                    false
                ),
            ]
        );
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
                    link: None,
                    space_after: true
                },
                word("now", false)
            ]
        );
    }

    #[test]
    fn links_that_cannot_open_are_plain_text() {
        assert_eq!(
            para("see [the docs](http://acme.dev/t) or [this](../x) ok"),
            vec![
                word("see", true),
                word("the", true),
                word("docs", true),
                word("or", true),
                word("this", true),
                word("ok", false),
            ]
        );
        assert_eq!(
            para("at <http://acme.dev/t> and http://acme.dev/u"),
            vec![
                word("at", true),
                word("http://acme.dev/t", true),
                word("and", true),
                word("http://acme.dev/u", false),
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
                    link: None,
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
                    link: None,
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
                    link: None,
                    space_after: false
                },
                Inline::Emoji {
                    file: "1f389.png",
                    link: None,
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
                    link: None,
                    space_after: true
                },
                Inline::Emoji {
                    file: family,
                    link: None,
                    space_after: true
                },
                Inline::Emoji {
                    file: "1f1e7-1f1f7.png",
                    link: None,
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
                link: None,
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

    fn depth(blocks: &[Block]) -> usize {
        let mut deepest = 0;
        let mut pending: Vec<(&[Block], usize)> = vec![(blocks, 1)];
        while let Some((level, d)) = pending.pop() {
            deepest = deepest.max(d);
            for block in level {
                match block {
                    Block::Quote(inner) => pending.push((inner, d + 1)),
                    Block::List { items, .. } => {
                        for item in items {
                            pending.push((&item.blocks, d + 2));
                        }
                    }
                    _ => {}
                }
            }
        }
        deepest
    }

    #[test]
    fn hostile_input_never_panics() {
        let cases = vec![
            format!("{} x", ">".repeat(100_000)),
            "- ".repeat(100_000),
            format!(
                "|{}\n|{}\n|{}",
                " a |".repeat(10_000),
                "---|".repeat(10_000),
                " b |".repeat(10_000)
            ),
            format!("| a |\n|---|\n{}", "| b |\n".repeat(10_000)),
            "```rust\nnever closed".to_string(),
            "<".to_string(),
            "<img src=\"".to_string(),
            "![unclosed".to_string(),
            "[unclosed".to_string(),
            ":".repeat(10_000),
        ];
        std::thread::spawn(move || {
            for md in &cases {
                let blocks = parse(md);
                assert!(depth(&blocks) <= 40, "nesting is capped");
                let copy = blocks.clone();
                assert!(copy == blocks);
                drop(copy);
                drop(blocks);
            }
        })
        .join()
        .expect("no panic or overflow on the default test stack");
    }

    #[test]
    fn capped_nesting_keeps_the_text() {
        let blocks = parse(&format!("{} deep", ">".repeat(200)));
        assert!(depth(&blocks) <= 40);
        let mut level = blocks.as_slice();
        loop {
            match level {
                [Block::Quote(inner)] => level = inner,
                [Block::Paragraph(p)] => {
                    assert_eq!(p, &vec![word("deep", false)]);
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn soft_break_in_alt_text_stays_in_the_alt() {
        assert_eq!(
            para("a ![x\ny](u) b"),
            vec![
                word("a", true),
                Inline::Image {
                    url: "u".into(),
                    alt: "x y".into(),
                    space_after: true
                },
                word("b", false)
            ]
        );
    }

    #[test]
    fn small_edge_cases() {
        let blocks = parse("| a |\n|---|\n| x<br>y |");
        let [Block::Table { rows, .. }] = blocks.as_slice() else {
            panic!("{blocks:?}")
        };
        assert_eq!(
            rows[0][0],
            vec![word("x", false), Inline::Break, word("y", false)]
        );
        assert_eq!(
            parse("# Title  \n"),
            vec![Block::Heading(1, vec![word("Title", false)])]
        );
        assert_eq!(
            para("[![alt](https://a.b/i.png)](https://a.b)"),
            vec![Inline::Image {
                url: "https://a.b/i.png".into(),
                alt: "alt".into(),
                space_after: false
            }]
        );
        assert_eq!(
            para("[https://a.b/x](https://c.d)"),
            vec![styled("https://a.b/x", link("https://c.d"), false)]
        );
        assert_eq!(para(":SHOUT:"), vec![word(":SHOUT:", false)]);
        assert_eq!(para(":rocket"), vec![word(":rocket", false)]);
    }
}

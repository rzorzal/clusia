//! Parsed bodies and highlighted fenced blocks, kept across respawns. Comments, thread cards
//! and chat lines are built again whenever their view changes, almost always with the same
//! text, so the parse and the highlight run once per text and the builds share the result.
//!
//! The caches are process-wide because the bodies are built deep inside spawn helpers that see
//! no resources. Each is bounded by the bytes its values hold; evicting an entry a view still
//! holds is harmless, since the view keeps its own `Arc`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use clusia_core::PrRef;
use clusia_highlight::{Span, highlight};

use crate::ui::lru::ByteLru;
use crate::ui::markdown::parse::{self, Block, Inline, ListItem};

/// The parsed bodies kept, at most. A long comment parses into tens of kilobytes.
pub const BLOCKS_CACHE_BYTES: usize = 16 << 20;
/// The highlighted fenced blocks kept, at most.
pub const CODE_CACHE_BYTES: usize = 8 << 20;

/// Everything besides the text that changes a parse: the host `@mentions` link to and the
/// repository a bare `#123` belongs to.
type Refs = (String, Option<(String, String)>);

/// Values keyed by `(hash of their source, what else they depend on)`. The source is kept with
/// the value and compared on a hit, so two texts with the same hash never share a result.
pub(crate) struct Memo<K, V> {
    lru: ByteLru<(u64, K), (Box<str>, Arc<V>)>,
}

impl<K: Eq + Hash + Clone, V> Memo<K, V> {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            lru: ByteLru::new(cap),
        }
    }

    fn key(source: &str, with: K) -> (u64, K) {
        let mut h = DefaultHasher::new();
        source.hash(&mut h);
        (h.finish(), with)
    }

    fn hit(&mut self, key: &(u64, K), source: &str) -> Option<Arc<V>> {
        self.lru
            .get(key)
            .filter(|(kept, _)| **kept == *source)
            .map(|(_, value)| value.clone())
    }

    /// Stores `value` for `source` and lets the least recently used entries go past the cap.
    /// `bytes` is what the value and the key hold on the heap; the entry itself, the `Arc`'s
    /// allocation and the kept source are added here, so a cache of tiny bodies is bounded too.
    fn store(&mut self, key: (u64, K), source: &str, value: Arc<V>, bytes: usize) {
        let overhead = size_of::<((u64, K), (Box<str>, Arc<V>))>() + size_of::<(usize, usize, V)>();
        self.lru.insert(
            key.clone(),
            (source.into(), value),
            bytes + source.len() + overhead,
        );
        self.lru.evict(|k, _| *k == key);
    }

    /// The texts kept.
    #[cfg(test)]
    fn sources(&self) -> impl Iterator<Item = &str> {
        self.lru
            .keys()
            .filter_map(|k| self.lru.peek(k))
            .map(|(source, _)| &**source)
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.lru.bytes()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lru.len()
    }
}

/// The value for `source` from `memo`, made by `make` when it is not there. The lock is not
/// held while `make` runs, so a slow parse never stalls a build on another thread.
fn memoized<K: Eq + Hash + Clone, V>(
    memo: &Mutex<Memo<K, V>>,
    source: &str,
    with: K,
    make: impl FnOnce() -> V,
    bytes: impl FnOnce(&V) -> usize,
) -> Arc<V> {
    let key = Memo::<K, V>::key(source, with);
    if let Some(value) = lock(memo).hit(&key, source) {
        return value;
    }
    let value = Arc::new(make());
    let size = bytes(&value);
    lock(memo).store(key, source, value.clone(), size);
    value
}

/// A poisoned lock only means a build panicked mid-insert; the map itself is still whole.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

static BLOCKS: LazyLock<Mutex<Memo<Refs, Vec<Block>>>> =
    LazyLock::new(|| Mutex::new(Memo::new(BLOCKS_CACHE_BYTES)));
static CODE: LazyLock<Mutex<Memo<String, Vec<Vec<Span>>>>> =
    LazyLock::new(|| Mutex::new(Memo::new(CODE_CACHE_BYTES)));

fn blocks(md: &str, refs: Refs, make: impl FnOnce() -> Vec<Block>) -> Arc<Vec<Block>> {
    let key_bytes = refs.0.len() + refs.1.as_ref().map_or(0, |(o, r)| o.len() + r.len());
    memoized(&BLOCKS, md, refs, make, |b| blocks_bytes(b) + key_bytes)
}

/// `parse::parse_on`, parsed once per text and host.
pub fn parse_on(md: &str, host: &str) -> Arc<Vec<Block>> {
    blocks(md, (host.to_string(), None), || parse::parse_on(md, host))
}

/// `parse::parse_in`, parsed once per text, host and repository.
pub fn parse_in(md: &str, host: &str, pr: &PrRef) -> Arc<Vec<Block>> {
    let refs = (host.to_string(), Some((pr.owner.clone(), pr.repo.clone())));
    blocks(md, refs, || parse::parse_in(md, host, pr))
}

/// `clusia_highlight::highlight`, run once per code and path. The spans hold classes, not
/// colors, so a theme change needs no new highlight.
pub fn highlighted(path: &str, code: &str) -> Arc<Vec<Vec<Span>>> {
    memoized(
        &CODE,
        code,
        path.to_string(),
        || highlight_uncached(path, code),
        |lines| spans_bytes(lines) + path.len(),
    )
}

/// `clusia_highlight::highlight` with no cache, for text still growing: each of its prefixes
/// would be an entry never asked for again.
pub fn highlight_uncached(path: &str, code: &str) -> Vec<Vec<Span>> {
    #[cfg(test)]
    counts::highlighted(code);
    highlight(path, code)
}

fn spans_bytes(lines: &Vec<Vec<Span>>) -> usize {
    lines.capacity() * size_of::<Vec<Span>>()
        + lines
            .iter()
            .map(|l| l.capacity() * size_of::<Span>())
            .sum::<usize>()
}

/// The heap the blocks hold, roughly: their vectors and strings.
fn blocks_bytes(blocks: &Vec<Block>) -> usize {
    blocks.capacity() * size_of::<Block>() + blocks.iter().map(block_bytes).sum::<usize>()
}

fn block_bytes(block: &Block) -> usize {
    match block {
        Block::Paragraph(items) | Block::Heading(_, items) => inlines_bytes(items),
        Block::List { items, .. } => {
            items.capacity() * size_of::<ListItem>()
                + items.iter().map(|i| blocks_bytes(&i.blocks)).sum::<usize>()
        }
        Block::Quote(inner) => blocks_bytes(inner),
        Block::Code { lang, text } => lang.as_ref().map_or(0, String::capacity) + text.capacity(),
        Block::Suggestion(text) => text.capacity(),
        Block::Table { head, rows } => {
            let row = |cells: &Vec<Vec<Inline>>| {
                cells.capacity() * size_of::<Vec<Inline>>()
                    + cells.iter().map(inlines_bytes).sum::<usize>()
            };
            row(head)
                + rows.capacity() * size_of::<Vec<Vec<Inline>>>()
                + rows.iter().map(row).sum::<usize>()
        }
        Block::Rule => 0,
    }
}

fn inlines_bytes(items: &Vec<Inline>) -> usize {
    let opt = |s: &Option<String>| s.as_ref().map_or(0, String::capacity);
    items.capacity() * size_of::<Inline>()
        + items
            .iter()
            .map(|i| match i {
                Inline::Word { text, style, .. } => text.capacity() + opt(&style.link),
                Inline::Code { text, link, .. } => text.capacity() + opt(link),
                Inline::Emoji { link, .. } => opt(link),
                Inline::Image { url, alt, .. } => url.capacity() + alt.capacity(),
                Inline::Break => 0,
            })
            .sum::<usize>()
}

/// How many times each text was parsed (by `parse`, cached or not) or highlighted (by a fenced
/// block, cached or not). Tests run in parallel and share the caches, so they count per text,
/// with texts no other test uses.
#[cfg(test)]
pub(crate) mod counts {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    static PARSES: LazyLock<Mutex<HashMap<String, usize>>> = LazyLock::new(Default::default);
    static HIGHLIGHTS: LazyLock<Mutex<HashMap<String, usize>>> = LazyLock::new(Default::default);

    pub(crate) fn parsed(md: &str) {
        *super::lock(&PARSES).entry(md.to_string()).or_default() += 1;
    }

    pub(super) fn highlighted(code: &str) {
        *super::lock(&HIGHLIGHTS)
            .entry(code.to_string())
            .or_default() += 1;
    }

    pub(crate) fn parses(md: &str) -> usize {
        super::lock(&PARSES).get(md).copied().unwrap_or(0)
    }

    pub(crate) fn highlights(code: &str) -> usize {
        super::lock(&HIGHLIGHTS).get(code).copied().unwrap_or(0)
    }

    /// The parsed bodies and the highlighted blocks kept whose text contains `needle`.
    pub(crate) fn cached_containing(needle: &str) -> (usize, usize) {
        let blocks = super::lock(&super::BLOCKS)
            .sources()
            .filter(|s| s.contains(needle))
            .count();
        let code = super::lock(&super::CODE)
            .sources()
            .filter(|s| s.contains(needle))
            .count();
        (blocks, code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widgets() -> PrRef {
        PrRef {
            owner: "acme".into(),
            repo: "widgets".into(),
            number: 7,
        }
    }

    /// Headings, lists, links, mentions, code, tables and the rest, as comments write them.
    const SAMPLES: &[&str] = &[
        "# Title\n\nSome **bold** and _em_ and ~~gone~~ text.",
        "- one\n- two\n  1. nested\n- [x] done\n- [ ] todo",
        "See [the docs](https://example.com/docs) and https://example.com/raw.",
        "thanks @alice and @acme/core for #12 and rzorzal/clusia#3 :rocket:",
        "```rust\nfn main() {\n    let x = 1;\n}\n```\n\nand `inline` code",
        "| Name | Role |\n| --- | --- |\n| octo | author |\n| mona | reviewer |",
        "> quoted\n> > twice\n\n---\n\n![alt](https://github.com/a.png)",
        "```suggestion\nlet better = 2;\n```",
    ];

    #[test]
    fn the_cached_parse_is_the_parse() {
        for md in SAMPLES {
            assert_eq!(
                *parse_on(md, "github.com"),
                parse::parse_on(md, "github.com")
            );
            assert_eq!(
                *parse_in(md, "github.com", &widgets()),
                parse::parse_in(md, "github.com", &widgets())
            );
            // Again, now from the cache.
            assert_eq!(
                *parse_in(md, "github.com", &widgets()),
                parse::parse_in(md, "github.com", &widgets())
            );
        }
    }

    #[test]
    fn the_same_text_parses_once() {
        let md = "cache-test: the same text parses once, @alice";
        let first = parse_in(md, "github.com", &widgets());
        let again = parse_in(md, "github.com", &widgets());
        assert_eq!(counts::parses(md), 1);
        assert!(Arc::ptr_eq(&first, &again), "both builds share the blocks");
    }

    #[test]
    fn a_host_or_repository_change_parses_again() {
        let md = "cache-test: host change @alice and #5";
        let other = PrRef {
            repo: "gadgets".into(),
            ..widgets()
        };
        let on_com = parse_in(md, "github.com", &widgets());
        let on_ghe = parse_in(md, "ghe.example.com", &widgets());
        let in_other = parse_in(md, "github.com", &other);
        let bare = parse_on(md, "github.com");
        assert_eq!(counts::parses(md), 4, "one parse per host and repository");
        assert_ne!(on_com, on_ghe, "mentions link to the other host");
        assert_eq!(*on_ghe, parse::parse_in(md, "ghe.example.com", &widgets()));
        assert_eq!(*in_other, parse::parse_in(md, "github.com", &other));
        assert_eq!(
            *bare,
            parse::parse_on(md, "github.com"),
            "no repo, no #5 link"
        );
    }

    #[test]
    fn the_same_code_highlights_once_per_language() {
        let code = "let cache_test_once = 1;";
        let first = highlighted("code.rs", code);
        assert_eq!(*first, highlight("code.rs", code));
        highlighted("code.rs", code);
        assert_eq!(counts::highlights(code), 1);
        assert_eq!(*highlighted("code.py", code), highlight("code.py", code));
        assert_eq!(
            counts::highlights(code),
            2,
            "another language highlights again"
        );
    }

    #[test]
    fn a_memo_stays_under_its_cap() {
        let memo = Mutex::new(Memo::new(4096));
        for n in 0..200 {
            let md = format!("body number {n} with a few words in it");
            let value = memoized(&memo, &md, (), || parse::parse(&md), blocks_bytes);
            assert_eq!(*value, parse::parse(&md));
        }
        let memo = lock(&memo);
        assert!(memo.bytes() <= 4096, "{} bytes", memo.bytes());
        assert!(memo.len() > 1, "it still keeps the recent ones");
    }

    #[test]
    fn a_hash_clash_never_serves_another_text() {
        let memo = Mutex::new(Memo::<(), Vec<Block>>::new(1 << 20));
        let key = Memo::<(), Vec<Block>>::key("first", ());
        let value = Arc::new(parse::parse("first"));
        lock(&memo).store(key, "first", value, 0);
        // A second text landing on the same key.
        assert!(lock(&memo).hit(&key, "second").is_none());
        assert!(lock(&memo).hit(&key, "first").is_some());
    }
}

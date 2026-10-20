//! The diff's syntax colors per file and head. The app highlights a file the first time on
//! `AsyncComputeTaskPool` (the grammars load on first use, and a large file takes long), shows
//! it plain until then, and keeps the results in a cache bounded by bytes that lets go of a
//! closed tab's heads.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task, block_on, poll_once};
use clusia_core::PrRef;
use clusia_core::draft::Side;
use clusia_highlight::{Span, highlight};
use clusia_view::diff::{Row, SideText, side_text};

use crate::review_state::{Phase, ReviewTabs};
use crate::ui::lru::ByteLru;

/// The highlighted rows the cache keeps, at most. A 5,000-line file is about 2 MB of spans.
pub const HIGHLIGHT_CACHE_BYTES: usize = 64 << 20;

/// `(head_sha, path)`: a new head re-highlights, a file switch does not.
type Key = (String, String);

/// One file's rows highlighted per side, indexed by row.
#[derive(Debug, Clone, Default)]
pub(crate) struct Highlighted {
    pub(crate) left: Vec<Option<Vec<Span>>>,
    pub(crate) right: Vec<Option<Vec<Span>>>,
}

impl Highlighted {
    /// Every row with no spans: shown plain.
    fn plain(rows: usize) -> Self {
        Self {
            left: vec![None; rows],
            right: vec![None; rows],
        }
    }

    fn bytes(&self) -> usize {
        let side = |rows: &Vec<Option<Vec<Span>>>| {
            rows.len() * size_of::<Option<Vec<Span>>>()
                + rows
                    .iter()
                    .flatten()
                    .map(|spans| spans.capacity() * size_of::<Span>())
                    .sum::<usize>()
        };
        side(&self.left) + side(&self.right)
    }
}

/// Highlights each side as one text, so multi-line constructs color right.
fn highlight_rows(path: &str, rows: usize, sides: [(Side, SideText); 2]) -> Highlighted {
    let mut lit = Highlighted::plain(rows);
    for (side, text) in sides {
        let lines = highlight(path, &text.text);
        let out = match side {
            Side::Left => &mut lit.left,
            Side::Right => &mut lit.right,
        };
        for (i, &row) in text.rows.iter().enumerate() {
            out[row] = lines.get(i).cloned();
        }
    }
    lit
}

#[derive(Resource)]
pub struct HighlightCache {
    lit: ByteLru<Key, Highlighted>,
    /// Whether a file not highlighted yet is highlighted in the background (the app) or right
    /// away (the default, for code that builds a view outside the app).
    background: bool,
    pending: HashMap<Key, Task<Highlighted>>,
    /// Keys asked for since the last landing: what the views on screen show. Every landing
    /// rebuilds those views, so they ask again and stay kept.
    asked: HashSet<Key>,
    /// The head each open tab showed, to know which heads a closed tab leaves behind.
    heads: HashMap<PrRef, String>,
}

impl Default for HighlightCache {
    fn default() -> Self {
        Self::with_cap(HIGHLIGHT_CACHE_BYTES)
    }
}

impl HighlightCache {
    pub fn with_cap(cap: usize) -> Self {
        Self {
            lit: ByteLru::new(cap),
            background: false,
            pending: HashMap::new(),
            asked: HashSet::new(),
            heads: HashMap::new(),
        }
    }

    /// A cache that highlights new files on `AsyncComputeTaskPool`.
    pub fn in_background(cap: usize) -> Self {
        Self {
            background: true,
            ..Self::with_cap(cap)
        }
    }

    pub fn len(&self) -> usize {
        self.lit.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lit.is_empty()
    }

    /// The bytes the cached spans hold.
    pub fn bytes(&self) -> usize {
        self.lit.bytes()
    }

    /// Files being highlighted in the background.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// The rows of `path` at `head`, highlighted. In the background a file seen for the first
    /// time comes back plain while its highlight runs; `HighlightsLanded` changes once it is in.
    pub(crate) fn get(&mut self, head: &str, path: &str, rows: &[Row]) -> Cow<'_, Highlighted> {
        let key = (head.to_string(), path.to_string());
        if !self.lit.contains_key(&key) {
            let sides = [Side::Left, Side::Right].map(|side| (side, side_text(rows, side)));
            if !self.background {
                let lit = highlight_rows(path, rows.len(), sides);
                let bytes = lit.bytes();
                self.lit.insert(key.clone(), lit, bytes);
                self.lit.evict(|k, _| *k == key);
            } else {
                self.asked.insert(key.clone());
                self.pending.entry(key).or_insert_with(|| {
                    let (path, n) = (path.to_string(), rows.len());
                    AsyncComputeTaskPool::get()
                        .spawn(async move { highlight_rows(&path, n, sides) })
                });
                return Cow::Owned(Highlighted::plain(rows.len()));
            }
        } else if self.background {
            self.asked.insert(key.clone());
        }
        Cow::Borrowed(self.lit.get(&key).expect("cached above"))
    }

    /// Caches the highlights that finished; `true` when any did.
    fn land(&mut self, finished: Vec<(Key, Highlighted)>) -> bool {
        if finished.is_empty() {
            return false;
        }
        for (key, lit) in finished {
            let bytes = lit.bytes();
            self.asked.insert(key.clone());
            self.lit.insert(key, lit, bytes);
        }
        let asked = std::mem::take(&mut self.asked);
        self.lit.evict(|k, _| asked.contains(k));
        true
    }

    /// Forgets the heads no open tab shows any more: a closed tab's, or a tab's older head.
    fn follow_tabs(&mut self, tabs: &ReviewTabs) {
        let open: HashMap<PrRef, String> = tabs
            .0
            .iter()
            .filter_map(|(pr, tab)| match &tab.phase {
                Phase::Ready(ready) => Some((pr.clone(), ready.view.pr.head_sha.clone())),
                // A tab loading again keeps the head it showed.
                _ => self.heads.get(pr).map(|head| (pr.clone(), head.clone())),
            })
            .collect();
        let live: HashSet<&String> = open.values().collect();
        let dead: HashSet<String> = self
            .heads
            .values()
            .filter(|head| !live.contains(head))
            .cloned()
            .collect();
        if !dead.is_empty() {
            self.lit.retain(|(head, _), _| !dead.contains(head));
            self.pending.retain(|(head, _), _| !dead.contains(head));
            self.asked.retain(|(head, _)| !dead.contains(head));
        }
        self.heads = open;
    }
}

/// Changes when highlights finished in the background, so the views showing them plain are
/// built again.
#[derive(Resource, Debug, Default)]
pub struct HighlightsLanded(pub u32);

fn land_highlights(mut cache: ResMut<HighlightCache>, mut landed: ResMut<HighlightsLanded>) {
    if cache.pending.is_empty() {
        return;
    }
    let mut finished = Vec::new();
    cache
        .pending
        .retain(|key, task| match block_on(poll_once(task)) {
            Some(lit) => {
                finished.push((key.clone(), lit));
                false
            }
            None => true,
        });
    if cache.land(finished) {
        landed.0 += 1;
    }
}

fn follow_tabs(tabs: Res<ReviewTabs>, mut cache: ResMut<HighlightCache>) {
    cache.follow_tabs(&tabs);
}

/// Blocks on the highlights in flight and caches them, as if they had all just finished.
#[cfg(test)]
pub(crate) fn finish_highlights(world: &mut World) {
    let mut cache = world.resource_mut::<HighlightCache>();
    let finished = std::mem::take(&mut cache.pending)
        .into_iter()
        .map(|(key, task)| (key, block_on(task)))
        .collect();
    if cache.land(finished) {
        world.resource_mut::<HighlightsLanded>().0 += 1;
    }
}

pub struct HighlightPlugin;

impl Plugin for HighlightPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(HighlightCache::in_background(HIGHLIGHT_CACHE_BYTES))
            .init_resource::<HighlightsLanded>()
            .add_systems(
                PreUpdate,
                (
                    follow_tabs.run_if(resource_changed::<ReviewTabs>),
                    land_highlights,
                )
                    .chain(),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_view::diff::parse_patch;

    use crate::testing;

    const PATCH: &str = "@@ -1,2 +1,2 @@\n fn main() {\n-    let a = 1;\n+    let b = \"two\";\n }";

    fn rows() -> Vec<Row> {
        parse_patch(PATCH)
    }

    /// A cache highlighting in the background, outside an app (which sets the pool up).
    fn background(cap: usize) -> HighlightCache {
        AsyncComputeTaskPool::get_or_init(bevy::tasks::TaskPool::new);
        HighlightCache::in_background(cap)
    }

    fn colored(lit: &Highlighted) -> usize {
        lit.left.iter().chain(&lit.right).flatten().count()
    }

    #[test]
    fn the_default_highlights_right_away() {
        let mut cache = HighlightCache::default();
        let rows = rows();
        assert!(colored(&cache.get("h1", "src/main.rs", &rows)) > 0);
        assert_eq!(cache.pending(), 0);
    }

    #[test]
    fn in_the_background_a_new_file_is_plain_until_its_highlight_lands() {
        let mut cache = background(HIGHLIGHT_CACHE_BYTES);
        let rows = rows();
        let first = cache.get("h1", "src/main.rs", &rows).into_owned();
        assert_eq!(colored(&first), 0, "nothing highlighted inside the frame");
        assert_eq!(first.left.len(), rows.len());
        cache.get("h1", "src/main.rs", &rows);
        assert_eq!(cache.pending(), 1, "asked twice, highlighted once");
        let finished: Vec<_> = std::mem::take(&mut cache.pending)
            .into_iter()
            .map(|(k, t)| (k, block_on(t)))
            .collect();
        assert!(cache.land(finished));
        assert!(colored(&cache.get("h1", "src/main.rs", &rows)) > 0);
        assert_eq!(cache.pending(), 0);
    }

    fn fill(cache: &mut HighlightCache, head: &str, paths: &[&str]) {
        let rows = rows();
        for path in paths {
            cache.get(head, path, &rows);
        }
        let finished: Vec<_> = std::mem::take(&mut cache.pending)
            .into_iter()
            .map(|(k, t)| (k, block_on(t)))
            .collect();
        cache.land(finished);
    }

    #[test]
    fn the_cache_stays_under_its_cap_and_keeps_what_is_shown() {
        let one = {
            let mut cache = HighlightCache::default();
            cache.get("h", "a.rs", &rows());
            cache.bytes()
        };
        let mut cache = background(one * 2);
        fill(&mut cache, "h", &["a.rs", "b.rs"]);
        assert_eq!(cache.len(), 2);
        // The view now shows `c.rs` and `a.rs`: `b.rs` is the one to go.
        fill(&mut cache, "h", &["c.rs", "a.rs"]);
        let mut keys: Vec<_> = cache.lit.keys().map(|(_, p)| p.as_str()).collect();
        keys.sort();
        assert_eq!(keys, ["a.rs", "c.rs"]);
        assert!(cache.bytes() <= one * 2);
        // A view showing more than fits keeps it all rather than highlighting it again forever.
        fill(&mut cache, "h", &["a.rs", "c.rs", "d.rs"]);
        assert_eq!(cache.len(), 3);
    }

    /// Nodes inked with a syntax color other than plain.
    fn colored_nodes(app: &mut App) -> usize {
        use crate::theme::Swatch;
        use crate::ui::kit::Ink;
        use clusia_highlight::Class;
        let mut q = app.world_mut().query::<&Ink>();
        q.iter(app.world())
            .filter(|ink| matches!(ink.0, Swatch::Code(c) if c != Class::Plain))
            .count()
    }

    #[test]
    fn the_diff_is_built_plain_inside_the_frame_then_again_in_color() {
        let mut app = testing::demo_review_app();
        assert!(app.world().resource::<HighlightCache>().background);
        finish_highlights(app.world_mut());
        testing::settle(&mut app);
        let in_color = colored_nodes(&mut app);
        assert!(in_color > 0, "the demo's diff shows in color");
        // As if the review were seen for the first time.
        app.insert_resource(HighlightCache::in_background(HIGHLIGHT_CACHE_BYTES));
        app.world_mut().resource_mut::<ReviewTabs>().set_changed();
        app.update();
        assert_eq!(
            colored_nodes(&mut app),
            0,
            "nothing was highlighted inside the frame"
        );
        assert!(app.world().resource::<HighlightCache>().pending() > 0);
        let landed = app.world().resource::<HighlightsLanded>().0;
        finish_highlights(app.world_mut());
        assert!(app.world().resource::<HighlightsLanded>().0 > landed);
        app.update();
        assert_eq!(colored_nodes(&mut app), in_color, "built again in color");
    }

    #[test]
    fn closing_a_tab_drops_its_highlights() {
        let mut app = testing::demo_review_app();
        finish_highlights(app.world_mut());
        testing::settle(&mut app);
        assert!(!app.world().resource::<HighlightCache>().is_empty());
        let pr = crate::fixture::demo_pr();
        app.world_mut().resource_mut::<ReviewTabs>().0.remove(&pr);
        app.update();
        let cache = app.world().resource::<HighlightCache>();
        assert!(cache.is_empty(), "nothing of the closed tab is kept");
        assert_eq!(cache.bytes(), 0);
    }
}

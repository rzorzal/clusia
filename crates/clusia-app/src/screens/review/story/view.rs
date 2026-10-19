//! The Story view as pure data: the steps with their read state, the summary, the current
//! card, the banners and the version footer. `story_view` reads the tab, the model and the
//! checks; `ui` draws it. Nothing here touches the world, so every state has a plain test.

use clusia_calls::StaticStep;
use clusia_core::checks::{CheckKind, Finding};
use clusia_core::draft::DraftKind;
use clusia_core::hunks::HunkRef;
use clusia_core::story::{Chip, StepHistory, StorySpec, StoryVersion, StoryVersionState};
use clusia_protocol::StoryState;

use super::model::StoryModel;
use crate::review_state::{EditTarget, Tab, TabUi};
use crate::screens::review::checks::ChecksModel;
use crate::screens::review::diff::{Below, HighlightCache, UnifiedLine, excerpt};
use crate::theme::Swatch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    Read,
    Current,
    Open,
    Removed,
}

/// What a step did since the version it is compared with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Added,
    Changed,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListRow {
    pub id: String,
    /// The step's place among the steps; `None` for a removed one.
    pub number: Option<usize>,
    pub title: String,
    pub meta: String,
    pub state: RowState,
    pub mechanical: bool,
    /// A static function step: its title is code.
    pub mono: bool,
    pub mark: Option<Mark>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseCaseChip {
    pub name: String,
    /// The dot: `Swatch::Family(index % 12)` of the use case's place in the story.
    pub swatch: Swatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryView {
    pub sentence: String,
    pub before: Vec<Chip>,
    pub after: Vec<Chip>,
    pub use_cases: Vec<UseCaseChip>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    Draft,
    Audit,
    Security,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteView {
    pub kind: NoteKind,
    pub text: String,
    pub who: String,
}

/// A step's lines of one file.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub path: String,
    pub lines: Vec<(UnifiedLine, Vec<Below>)>,
    pub hidden: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CardView {
    pub number: usize,
    pub step: String,
    pub title: String,
    pub mono: bool,
    /// `file.rs:43`, one per file of the step's hunks.
    pub places: Vec<String>,
    pub explanation: String,
    /// Compare: what changed since the version before.
    pub since: Option<String>,
    pub mark: Option<Mark>,
    pub use_cases: Vec<UseCaseChip>,
    pub blocks: Vec<Block>,
    pub notes: Vec<NoteView>,
    pub read: bool,
    /// The title of the step after this one, and its id.
    pub next: Option<String>,
    pub next_id: Option<String>,
    pub first_hunk: Option<HunkRef>,
    /// The shown version takes read marks (the latest, or the static order).
    pub can_mark: bool,
    /// The version number a comment or a mark names: `0` with no version.
    pub version: u32,
    /// The composer is open for a comment on this step.
    pub editing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherView {
    pub step: String,
    pub number: Option<usize>,
    pub title: String,
    pub text: String,
    pub mono: bool,
    pub use_cases: Vec<UseCaseChip>,
    pub read: bool,
    pub mark: Option<Mark>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Banner {
    Writing {
        harness: String,
    },
    Waiting,
    /// `rewrite`: the daemon lists a version, so **Retry** rewrites; else it writes.
    Failed {
        message: String,
        rewrite: bool,
    },
    NotWritten,
    Outdated {
        written: String,
        now: String,
        behind: u32,
    },
    Unavailable {
        harness: String,
    },
    Manual {
        harness: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Legend {
    pub since: u32,
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FooterView {
    pub number: u32,
    pub of: u32,
    pub prev: bool,
    pub next: bool,
    /// The version the changes are shown against, and whether the box is checked.
    pub compare: Option<(u32, bool)>,
    /// An older version: read-only.
    pub old: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListView {
    pub heading: String,
    pub counts: String,
    pub rows: Vec<ListRow>,
    pub note: String,
    pub footer: Option<FooterView>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BodyView {
    pub banner: Option<Banner>,
    pub legend: Option<Legend>,
    pub summary: Option<SummaryView>,
    pub card: Option<CardView>,
    pub others: Vec<OtherView>,
    pub code_size: f32,
    /// The composer's refusal, re-rendered under it.
    pub editor_error: Option<String>,
    /// The composer waits for the daemon (**Saving…**).
    pub saving: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoryView {
    pub list: ListView,
    pub body: BodyView,
}

/// What the view needs besides the tab, the model and the checks.
pub struct StoryCtx<'a> {
    pub harness: &'a str,
    /// `harness.story`: the agent writes when the view opens.
    pub story_setting: bool,
    pub live: bool,
    pub code_size: f32,
}

/// The version on show: the one the tab names when the daemon lists it, else the latest.
struct Shown<'a> {
    number: Option<u32>,
    is_latest: bool,
    version: Option<&'a StoryVersion>,
    /// Its spec, when it is a written version.
    spec: Option<&'a StorySpec>,
}

fn shown<'a>(ui: &TabUi, m: &'a StoryModel) -> Shown<'a> {
    let latest = m.latest_number();
    let number = ui
        .story_version
        .filter(|n| m.versions.iter().any(|v| v.number == *n))
        .or(latest);
    let version = number.and_then(|n| m.loaded.get(&n));
    let spec = version
        .filter(|v| v.state == StoryVersionState::Written)
        .and_then(|v| v.spec.as_ref());
    Shown {
        number,
        is_latest: number == latest,
        version,
        spec,
    }
}

/// A step, written or static, as the view lists it.
struct Item {
    id: String,
    title: String,
    meta: String,
    explanation: String,
    hunks: Vec<HunkRef>,
    use_cases: Vec<UseCaseChip>,
    mechanical: bool,
    mono: bool,
    history: Option<StepHistory>,
    since: Option<String>,
}

fn use_case_chips(spec: &StorySpec, ids: &[String]) -> Vec<UseCaseChip> {
    ids.iter()
        .filter_map(|id| {
            let at = spec.use_cases.iter().position(|u| &u.id == id)?;
            Some(UseCaseChip {
                name: spec.use_cases[at].name.clone(),
                swatch: Swatch::Family((at % 12) as u8),
            })
        })
        .collect()
}

fn items(m: &StoryModel, s: &Shown) -> Vec<Item> {
    match s.spec {
        Some(spec) => spec
            .steps
            .iter()
            .map(|st| Item {
                id: st.id.clone(),
                title: st.title.clone(),
                meta: st.subtitle.clone(),
                explanation: st.explanation.clone(),
                hunks: st.hunks.clone(),
                use_cases: use_case_chips(spec, &st.use_cases),
                mechanical: st.mechanical,
                mono: false,
                history: st.history,
                since: st.since_previous.clone(),
            })
            .collect(),
        None => m.static_steps.iter().map(static_item).collect(),
    }
}

fn static_item(st: &StaticStep) -> Item {
    Item {
        id: st.id.clone(),
        title: st.title.clone(),
        meta: st.subtitle.clone(),
        explanation: st.explanation.clone(),
        hunks: st.hunks.clone(),
        use_cases: Vec::new(),
        mechanical: false,
        mono: !st.other,
        history: None,
        since: None,
    }
}

/// How many steps are read, of how many: the marks belong to the latest version and the static
/// order.
pub(crate) fn step_counts(ui: &TabUi, m: &StoryModel) -> (usize, usize) {
    let s = shown(ui, m);
    let steps = items(m, &s);
    let read = if s.is_latest {
        steps.iter().filter(|i| m.read.contains(&i.id)).count()
    } else {
        0
    };
    (read, steps.len())
}

/// The versions compared, `(since, shown)`: the box is checked, the version is a written one
/// after the first, and the agent said something about history.
pub(crate) fn compare_active(ui: &TabUi, m: &StoryModel) -> Option<(u32, u32)> {
    let s = shown(ui, m);
    let (spec, number) = (s.spec?, s.number?);
    let about_history =
        spec.steps.iter().any(|st| st.history.is_some()) || !spec.removed.is_empty();
    (ui.story_compare && number > 1 && about_history).then_some((number - 1, number))
}

fn sha7(sha: &str) -> String {
    sha.get(..7).unwrap_or(sha).to_string()
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "S" }
}

fn area_label(area: &str) -> String {
    let spaced = area.replace('-', " ");
    let mut chars = spaced.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

fn overlaps(hunks: &[HunkRef], path: &str, from: u32, to: u32) -> bool {
    hunks
        .iter()
        .any(|h| h.path == path && h.start_line <= to.max(from) && from <= h.end_line)
}

fn notes_for(
    tab: &Tab,
    checks: Option<&ChecksModel>,
    hunks: &[HunkRef],
    harness: &str,
) -> Vec<NoteView> {
    let mut out = Vec::new();
    if let Some(ready) = tab.ready() {
        for item in &ready.view.review.draft.items {
            let Some(anchor) = &item.anchor else { continue };
            if item.kind != DraftKind::LineComment || !item.accepted {
                continue;
            }
            let from = anchor.start_line.unwrap_or(anchor.line);
            if overlaps(hunks, &anchor.path, from, anchor.line) {
                out.push(NoteView {
                    kind: NoteKind::Draft,
                    text: item
                        .body
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                    who: "You".into(),
                });
            }
        }
    }
    let Some(checks) = checks else { return out };
    for (kind, note) in [
        (CheckKind::Audit, NoteKind::Audit),
        (CheckKind::Security, NoteKind::Security),
    ] {
        let Some(result) = &checks.kind(kind).result else {
            continue;
        };
        for f in &result.findings {
            if checks.accepted.contains(&f.id) || checks.dismissed.contains(&f.id) {
                continue;
            }
            if !finding_in(f, hunks) {
                continue;
            }
            let text = match note {
                NoteKind::Audit => format!("{} ({})", f.title, area_label(&f.area)),
                _ => f.title.clone(),
            };
            out.push(NoteView {
                kind: note,
                text,
                who: harness.to_string(),
            });
        }
    }
    out
}

fn finding_in(f: &Finding, hunks: &[HunkRef]) -> bool {
    let Some(first) = f.start_line.or(f.line) else {
        return false;
    };
    let last = f.end_line.or(f.line).unwrap_or(first);
    overlaps(hunks, &f.file, first, last)
}

/// The paths of `hunks` in the order they first appear, each with its ranges.
fn by_file(hunks: &[HunkRef]) -> Vec<(String, Vec<(u32, u32)>)> {
    let mut out: Vec<(String, Vec<(u32, u32)>)> = Vec::new();
    for h in hunks {
        match out.iter_mut().find(|(p, _)| *p == h.path) {
            Some((_, ranges)) => ranges.push((h.start_line, h.end_line)),
            None => out.push((h.path.clone(), vec![(h.start_line, h.end_line)])),
        }
    }
    out
}

fn banner(m: &StoryModel, s: &Shown, ctx: &StoryCtx) -> Option<Banner> {
    if !s.is_latest {
        return None;
    }
    let harness = ctx.harness.to_string();
    match &m.state {
        StoryState::Writing => return Some(Banner::Writing { harness }),
        StoryState::Waiting => return Some(Banner::Waiting),
        _ => {}
    }
    let rewrite = !m.versions.is_empty();
    if let Some(StoryVersionState::Failed { message }) = s.version.map(|v| &v.state) {
        return Some(Banner::Failed {
            message: message.clone(),
            rewrite,
        });
    }
    if let StoryState::Failed { message } = &m.state {
        return Some(Banner::Failed {
            message: message.clone(),
            rewrite,
        });
    }
    if matches!(
        s.version.map(|v| &v.state),
        Some(StoryVersionState::NotWritten)
    ) {
        return Some(Banner::NotWritten);
    }
    if let Some(version) = s.version.filter(|_| s.spec.is_some())
        && m.outdated()
    {
        return Some(Banner::Outdated {
            written: sha7(&version.commit),
            now: sha7(&m.head),
            behind: m.new_commits,
        });
    }
    if m.versions.is_empty() && m.state == StoryState::Idle {
        if !ctx.live {
            return Some(Banner::Unavailable { harness });
        }
        if !ctx.story_setting {
            return Some(Banner::Manual { harness });
        }
    }
    None
}

/// The whole Story view of a ready tab; `None` before the tab is ready or the daemon ever
/// answered (an answer gone stale while a run ended still shows).
pub fn story_view(
    tab: &Tab,
    model: Option<&StoryModel>,
    checks: Option<&ChecksModel>,
    ctx: &StoryCtx,
    cache: &mut HighlightCache,
) -> Option<StoryView> {
    let ready = tab.ready()?;
    let m = model.filter(|m| m.known)?;
    let ui = &tab.ui;
    let s = shown(ui, m);
    let steps = items(m, &s);
    let compare = compare_active(ui, m);
    let is_read = |id: &str| s.is_latest && m.read.contains(id);
    let current = ui
        .story_step
        .as_deref()
        .and_then(|id| steps.iter().position(|i| i.id == id))
        .or_else(|| steps.iter().position(|i| !is_read(&i.id)))
        .or(if steps.is_empty() { None } else { Some(0) });
    let mark_of = |i: &Item| -> Option<Mark> {
        compare?;
        match i.history? {
            StepHistory::Added => Some(Mark::Added),
            StepHistory::Changed => Some(Mark::Changed),
            StepHistory::Same => None,
        }
    };
    // Marks and comments name the version whose steps are listed: the static order is 0, even
    // beside a version that is still writing or failed.
    let version_no = if s.spec.is_some() {
        s.number.unwrap_or(0)
    } else {
        0
    };

    let mut rows: Vec<ListRow> = steps
        .iter()
        .enumerate()
        .map(|(at, i)| ListRow {
            id: i.id.clone(),
            number: Some(at + 1),
            title: i.title.clone(),
            meta: match (mark_of(i), s.number) {
                (Some(Mark::Added), Some(n)) => format!("{} · new in version {n}", i.meta),
                _ => i.meta.clone(),
            },
            state: if Some(at) == current {
                RowState::Current
            } else if is_read(&i.id) {
                RowState::Read
            } else {
                RowState::Open
            },
            mechanical: i.mechanical,
            mono: i.mono,
            mark: mark_of(i),
        })
        .collect();
    let removed = s
        .spec
        .filter(|_| compare.is_some())
        .map(|spec| &spec.removed);
    if let (Some(removed), Some(n)) = (removed, s.number) {
        let trailing = steps.iter().rev().take_while(|i| i.mechanical).count();
        let at = rows.len() - trailing;
        let gone = removed.iter().map(|r| ListRow {
            id: r.id.clone(),
            number: None,
            title: r.title.clone(),
            meta: format!("{} · gone in version {n}", r.subtitle),
            state: RowState::Removed,
            mechanical: false,
            mono: false,
            mark: Some(Mark::Removed),
        });
        rows.splice(at..at, gone);
    }

    let (read, total) = step_counts(ui, m);
    let harness = ctx.harness;
    let note = match (s.spec, s.version) {
        (Some(_), Some(v)) => format!(
            "Steps are ordered so each one only needs the ones before it. Written by {harness} from commit {}; every line still links to the real diff.",
            sha7(&v.commit)
        ),
        _ if m.busy() => format!(
            "Ordered from the code: each function comes after the ones it calls. {harness} is writing names, a summary and the use cases."
        ),
        _ => "Ordered from the code: each function comes after the ones it calls.".to_string(),
    };
    let footer = s.number.map(|number| FooterView {
        number,
        of: m.latest_number().unwrap_or(number),
        prev: m.versions.iter().any(|v| v.number < number),
        next: m.versions.iter().any(|v| v.number > number),
        compare: (number > 1).then(|| (number - 1, ui.story_compare)),
        old: !s.is_latest,
    });
    let list = ListView {
        heading: format!("THE CHANGE IN {} STEP{}", steps.len(), plural(steps.len())),
        counts: format!("{read} of {total} read"),
        rows,
        note,
        footer,
    };

    let summary = s
        .spec
        .filter(|spec| !spec.sentence.is_empty())
        .map(|spec| SummaryView {
            sentence: spec.sentence.clone(),
            before: spec.before.clone(),
            after: spec.after.clone(),
            use_cases: spec
                .use_cases
                .iter()
                .enumerate()
                .map(|(at, u)| UseCaseChip {
                    name: u.name.clone(),
                    swatch: Swatch::Family((at % 12) as u8),
                })
                .collect(),
        });
    let legend = compare.zip(s.spec).map(|((since, _), spec)| Legend {
        since,
        added: steps
            .iter()
            .filter(|i| i.history == Some(StepHistory::Added))
            .count(),
        changed: steps
            .iter()
            .filter(|i| i.history == Some(StepHistory::Changed))
            .count(),
        removed: spec.removed.len(),
    });

    let editor = ui.editor.as_ref();
    let card = current.map(|at| {
        let i = &steps[at];
        let places = by_file(&i.hunks)
            .iter()
            .map(|(path, ranges)| format!("{}:{}", file_name(path), ranges[0].0))
            .collect();
        let blocks = by_file(&i.hunks)
            .into_iter()
            .filter_map(|(path, ranges)| {
                let ex = excerpt(ready, ui, cache, &path, &ranges)?;
                Some(Block {
                    path: ex.path,
                    lines: ex.lines,
                    hidden: ex.hidden,
                })
            })
            .collect();
        CardView {
            number: at + 1,
            step: i.id.clone(),
            title: i.title.clone(),
            mono: i.mono,
            places,
            explanation: i.explanation.clone(),
            since: i.since.clone().filter(|_| compare.is_some()),
            mark: mark_of(i),
            use_cases: i.use_cases.clone(),
            blocks,
            notes: notes_for(tab, checks, &i.hunks, harness),
            read: is_read(&i.id),
            next: steps.get(at + 1).map(|n| n.title.clone()),
            next_id: steps.get(at + 1).map(|n| n.id.clone()),
            first_hunk: i.hunks.first().cloned(),
            can_mark: s.is_latest,
            version: version_no,
            editing: matches!(
                editor.map(|e| &e.target),
                Some(EditTarget::StoryStep { number, step }) if *number == version_no && *step == i.id
            ),
        }
    });
    let others = steps
        .iter()
        .enumerate()
        .filter(|(at, _)| Some(*at) != current)
        .map(|(at, i)| OtherView {
            step: i.id.clone(),
            number: Some(at + 1),
            title: i.title.clone(),
            text: if i.explanation.is_empty() {
                i.meta.clone()
            } else {
                i.explanation.clone()
            },
            mono: i.mono,
            use_cases: i.use_cases.clone(),
            read: is_read(&i.id),
            mark: mark_of(i),
        })
        .collect();
    Some(StoryView {
        list,
        body: BodyView {
            banner: banner(m, &s, ctx),
            legend,
            summary,
            card,
            others,
            code_size: ctx.code_size,
            editor_error: editor.and_then(|e| e.error.clone()),
            saving: editor.is_some_and(|e| e.ticket.is_some()),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{StoryTell, Tell};
    use crate::fixture;
    use crate::review_state::{DiffMode, Phase, Ready};
    use crate::screens::review::checks::ChecksModel;
    use crate::testing::NOW;
    use clusia_core::story::{StoryVersion, StoryVersionState, Tone};
    use clusia_protocol::StoryState;

    fn tab() -> Tab {
        let (view, news) = fixture::demo_review(NOW);
        let mut tab = Tab::loading(DiffMode::Story);
        tab.phase = Phase::Ready(Box::new(Ready {
            view,
            news,
            cached_at: None,
        }));
        tab
    }

    fn tell(versions: Vec<StoryVersion>, state: StoryState, read: &[&str]) -> StoryTell {
        let demo = fixture::demo_story(NOW);
        StoryTell::Loaded {
            pr: fixture::demo_pr(),
            versions: versions
                .iter()
                .map(|v| clusia_protocol::StoryVersionInfo {
                    number: v.number,
                    commit: v.commit.clone(),
                    state: v.state.clone(),
                })
                .collect(),
            latest: versions.last().cloned().map(Box::new),
            state,
            static_steps: demo.static_steps,
            read: read.iter().map(|s| s.to_string()).collect(),
            head: demo.head,
            new_commits: 0,
        }
    }

    fn model(versions: Vec<StoryVersion>, state: StoryState, read: &[&str]) -> StoryModel {
        let mut m = StoryModel::default();
        m.apply(&tell(versions, state, read));
        m
    }

    fn written() -> StoryModel {
        model(
            vec![fixture::demo_story(NOW).first],
            StoryState::Idle,
            &["refresh-early"],
        )
    }

    fn compared() -> StoryModel {
        let demo = fixture::demo_story(NOW);
        model(
            vec![demo.first, demo.second],
            StoryState::Idle,
            &["refresh-early"],
        )
    }

    fn statically(state: StoryState) -> StoryModel {
        model(Vec::new(), state, &[])
    }

    fn ctx() -> StoryCtx<'static> {
        StoryCtx {
            harness: "Claude Code",
            story_setting: true,
            live: true,
            code_size: 13.0,
        }
    }

    fn view(tab: &Tab, m: &StoryModel) -> StoryView {
        story_view(tab, Some(m), None, &ctx(), &mut HighlightCache::default()).expect("a view")
    }

    fn titles(v: &StoryView) -> Vec<&str> {
        v.list.rows.iter().map(|r| r.title.as_str()).collect()
    }

    #[test]
    fn a_written_story_lists_its_steps_with_one_read_and_the_first_unread_current() {
        let v = view(&tab(), &written());
        assert_eq!(v.list.heading, "THE CHANGE IN 5 STEPS");
        assert_eq!(v.list.counts, "1 of 5 read");
        assert_eq!(
            titles(&v),
            [
                "Refresh one minute early",
                "One refresh at a time",
                "Safer saves",
                "Tests for the race",
                "Small and mechanical"
            ]
        );
        let states: Vec<RowState> = v.list.rows.iter().map(|r| r.state).collect();
        assert_eq!(
            states,
            [
                RowState::Read,
                RowState::Current,
                RowState::Open,
                RowState::Open,
                RowState::Open
            ]
        );
        assert_eq!(v.list.rows[0].meta, "refresh.rs · 3 lines · 1 thread");
        assert_eq!(v.list.rows[1].number, Some(2));
        assert!(v.list.rows[4].mechanical && !v.list.rows[1].mechanical);
        assert!(v.list.rows.iter().all(|r| !r.mono && r.mark.is_none()));
        assert_eq!(
            v.list.note,
            "Steps are ordered so each one only needs the ones before it. Written by Claude Code from commit 9be1d07; every line still links to the real diff."
        );
        assert!(v.body.banner.is_none() && v.body.legend.is_none());
    }

    #[test]
    fn the_summary_card_has_the_sentence_the_strips_and_coloured_use_cases() {
        let v = view(&tab(), &written());
        let s = v.body.summary.expect("a summary");
        assert!(
            s.sentence
                .starts_with("tokens are now refreshed a minute before they expire")
        );
        assert_eq!(s.before.len(), 4);
        assert_eq!(s.before[3].text, "exchange again · rejected");
        assert_eq!(s.before[3].tone, Tone::Bad);
        assert_eq!((s.after.len(), s.after[1].tone), (6, Tone::Good));
        let names: Vec<&str> = s.use_cases.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Request with an expired token",
                "Two requests refresh at once",
                "Restart keeps the login"
            ]
        );
        let dots: Vec<Swatch> = s.use_cases.iter().map(|u| u.swatch).collect();
        assert_eq!(
            dots,
            [Swatch::Family(0), Swatch::Family(1), Swatch::Family(2)]
        );
    }

    #[test]
    fn the_current_card_names_its_files_its_text_its_use_cases_and_the_next_step() {
        let v = view(&tab(), &written());
        let c = v.body.card.expect("a card");
        assert_eq!(
            (c.number, c.step.as_str(), c.title.as_str()),
            (2, "one-at-a-time", "One refresh at a time")
        );
        assert_eq!(c.places, ["refresh.rs:43", "mod.rs:3"]);
        assert!(
            c.explanation
                .starts_with("A new lock makes concurrent callers queue")
        );
        let uc: Vec<&str> = c.use_cases.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(
            uc,
            [
                "Two requests refresh at once",
                "Request with an expired token"
            ]
        );
        assert_eq!(c.next.as_deref(), Some("Safer saves"));
        assert!(!c.read && c.can_mark && c.mark.is_none());
        assert_eq!(c.blocks.len(), 2);
        assert_eq!(c.blocks[0].path, "src/auth/refresh.rs");
        assert_eq!(
            c.first_hunk
                .as_ref()
                .map(|h| (h.path.as_str(), h.start_line)),
            Some(("src/auth/refresh.rs", 43))
        );
        let others: Vec<&str> = v.body.others.iter().map(|o| o.title.as_str()).collect();
        assert_eq!(
            others,
            [
                "Refresh one minute early",
                "Safer saves",
                "Tests for the race",
                "Small and mechanical"
            ],
            "every other step, in order"
        );
        assert!(v.body.others[0].read);
    }

    #[test]
    fn the_current_step_follows_the_tab_and_falls_back_to_the_first_unread() {
        let mut t = tab();
        t.ui.story_step = Some("safer-saves".into());
        let v = view(&t, &written());
        assert_eq!(v.body.card.as_ref().map(|c| c.number), Some(3));
        assert_eq!(v.list.rows[2].state, RowState::Current);
        t.ui.story_step = Some("no-such-step".into());
        assert_eq!(view(&t, &written()).body.card.map(|c| c.number), Some(2));
        let all_read = model(
            vec![fixture::demo_story(NOW).first],
            StoryState::Idle,
            &[
                "refresh-early",
                "one-at-a-time",
                "safer-saves",
                "race-tests",
                "mechanical",
            ],
        );
        let v = view(&tab(), &all_read);
        assert_eq!(v.list.counts, "5 of 5 read");
        assert_eq!(
            v.body.card.map(|c| c.number),
            Some(1),
            "all read: the first"
        );
    }

    #[test]
    fn the_last_step_has_no_next_and_a_read_step_can_be_unmarked() {
        let mut t = tab();
        t.ui.story_step = Some("mechanical".into());
        let c = view(&t, &written()).body.card.expect("a card");
        assert_eq!(c.next, None);
        t.ui.story_step = Some("refresh-early".into());
        let c = view(&t, &written()).body.card.expect("a card");
        assert!(c.read);
    }

    #[test]
    fn the_static_order_has_function_steps_in_mono_and_no_summary() {
        let v = view(&tab(), &statically(StoryState::Writing));
        assert_eq!(v.list.heading, "THE CHANGE IN 6 STEPS");
        assert_eq!(v.list.counts, "0 of 6 read");
        assert_eq!(
            titles(&v),
            [
                "HttpClient::exchange",
                "TokenStore::save",
                "TokenStore::new",
                "TokenStore::refresh",
                "tests::refresh_race",
                "Other files"
            ]
        );
        let mono: Vec<bool> = v.list.rows.iter().map(|r| r.mono).collect();
        assert_eq!(mono, [true, true, true, true, true, false]);
        assert_eq!(v.list.rows[0].meta, "client/http.rs · +4 −1");
        assert!(v.body.summary.is_none());
        let c = v.body.card.expect("a card");
        assert_eq!(
            (c.number, c.mono, c.explanation.as_str()),
            (
                1,
                true,
                "Changed body · called by TokenStore::refresh · comes first because nothing it calls changed"
            )
        );
        assert_eq!(
            v.body.others[0].text,
            "Moved from src/auth/store.rs:20 · called by TokenStore::refresh · comes first because nothing it calls changed",
            "the one-liners carry the explanation too"
        );
        assert_eq!(c.places, ["http.rs:18"]);
        assert_eq!(
            v.list.note,
            "Ordered from the code: each function comes after the ones it calls. Claude Code is writing names, a summary and the use cases."
        );
        assert_eq!(
            v.body.banner,
            Some(Banner::Writing {
                harness: "Claude Code".into()
            })
        );
        assert!(v.list.footer.is_none(), "no version, no stepper");
    }

    #[test]
    fn a_static_order_beside_a_listed_version_is_version_zero() {
        let demo = fixture::demo_story(NOW);
        let mut failed = demo.first.clone();
        failed.state = StoryVersionState::Failed {
            message: "bad json".into(),
        };
        failed.spec = None;
        let mut writing = demo.second.clone();
        writing.number = 3;
        writing.state = StoryVersionState::Writing;
        writing.spec = None;
        for m in [
            model(vec![failed], StoryState::Idle, &[]),
            model(
                vec![demo.first, demo.second, writing],
                StoryState::Writing,
                &[],
            ),
        ] {
            let c = view(&tab(), &m).body.card.expect("a card");
            assert!(c.mono, "the static order");
            assert_eq!(c.version, 0);
        }
        assert_eq!(
            view(&tab(), &written()).body.card.map(|c| c.version),
            Some(1)
        );
    }

    #[test]
    fn every_banner_has_its_text_its_button_and_its_turn() {
        let t = tab();
        let banner = |m: &StoryModel| view(&t, m).body.banner;
        assert_eq!(
            banner(&statically(StoryState::Waiting)),
            Some(Banner::Waiting)
        );
        assert_eq!(
            banner(&statically(StoryState::Failed {
                message: "timed out".into()
            })),
            Some(Banner::Failed {
                message: "timed out".into(),
                rewrite: false
            })
        );

        let mut failed = fixture::demo_story(NOW).first;
        failed.state = StoryVersionState::Failed {
            message: "bad json".into(),
        };
        failed.spec = None;
        assert_eq!(
            banner(&model(vec![failed], StoryState::Idle, &[])),
            Some(Banner::Failed {
                message: "bad json".into(),
                rewrite: true
            })
        );

        let mut stopped = fixture::demo_story(NOW).first;
        stopped.state = StoryVersionState::NotWritten;
        stopped.spec = None;
        assert_eq!(
            banner(&model(vec![stopped], StoryState::Idle, &[])),
            Some(Banner::NotWritten)
        );

        let mut both = fixture::demo_story(NOW).first;
        both.state = StoryVersionState::NotWritten;
        both.spec = None;
        assert_eq!(
            banner(&model(
                vec![both],
                StoryState::Failed {
                    message: "timed out".into()
                },
                &[]
            )),
            Some(Banner::Failed {
                message: "timed out".into(),
                rewrite: true
            }),
            "a failed state wins over a version that was not written"
        );

        let mut old = fixture::demo_story(NOW).first;
        old.commit = fixture::DEMO_OLD_SHA.into();
        let mut behind = model(vec![old], StoryState::Idle, &["refresh-early"]);
        behind.new_commits = 2;
        assert_eq!(
            banner(&behind),
            Some(Banner::Outdated {
                written: "3f2a91c".into(),
                now: "9be1d07".into(),
                behind: 2
            })
        );

        assert_eq!(banner(&written()), None);

        let offline = StoryCtx {
            live: false,
            ..ctx()
        };
        let v = story_view(
            &t,
            Some(&statically(StoryState::Idle)),
            None,
            &offline,
            &mut HighlightCache::default(),
        )
        .expect("a view");
        assert_eq!(
            v.body.banner,
            Some(Banner::Unavailable {
                harness: "Claude Code".into()
            })
        );
        let off = StoryCtx {
            story_setting: false,
            ..ctx()
        };
        let v = story_view(
            &t,
            Some(&statically(StoryState::Idle)),
            None,
            &off,
            &mut HighlightCache::default(),
        )
        .expect("a view");
        assert_eq!(
            v.body.banner,
            Some(Banner::Manual {
                harness: "Claude Code".into()
            })
        );
        assert_eq!(
            banner(&statically(StoryState::Idle)),
            None,
            "idle, live and on: the trigger is about to write"
        );
    }

    #[test]
    fn a_story_that_is_being_rewritten_shows_the_static_order_with_the_writing_banner() {
        let demo = fixture::demo_story(NOW);
        let mut next = demo.second.clone();
        next.number = 3;
        next.state = StoryVersionState::Writing;
        next.spec = None;
        let m = model(
            vec![demo.first, demo.second, next],
            StoryState::Writing,
            &[],
        );
        let v = view(&tab(), &m);
        assert_eq!(v.list.rows.len(), 6);
        assert!(
            v.list.rows[0].mono,
            "the static order, until version 3 is written"
        );
        assert!(matches!(v.body.banner, Some(Banner::Writing { .. })));
        assert_eq!(
            v.list.footer.as_ref().map(|f| (f.number, f.of)),
            Some((3, 3))
        );
    }

    #[test]
    fn the_footer_steps_between_versions_and_offers_the_comparison_after_the_first() {
        let f = view(&tab(), &compared()).list.footer.expect("a footer");
        assert_eq!(
            (f.number, f.of, f.prev, f.next, f.old),
            (2, 2, true, false, false)
        );
        assert_eq!(f.compare, Some((1, false)));
        let f = view(&tab(), &written()).list.footer.expect("a footer");
        assert_eq!((f.number, f.of, f.prev, f.next), (1, 1, false, false));
        assert_eq!(f.compare, None, "nothing to compare with on version 1");
    }

    #[test]
    fn an_older_version_is_read_only_without_marks_or_a_banner() {
        let mut t = tab();
        t.ui.story_version = Some(1);
        let mut m = compared();
        m.apply(&StoryTell::Version {
            pr: fixture::demo_pr(),
            version: Box::new(fixture::demo_story_version(NOW, 1).expect("v1")),
        });
        let v = view(&t, &m);
        assert_eq!(v.list.heading, "THE CHANGE IN 5 STEPS");
        assert_eq!(
            v.list.counts, "0 of 5 read",
            "marks are of the latest version"
        );
        let f = v.list.footer.expect("a footer");
        assert_eq!(
            (f.number, f.of, f.prev, f.next, f.old),
            (1, 2, false, true, true)
        );
        assert!(!v.body.card.expect("a card").can_mark);
        assert_eq!(v.body.banner, None);
        t.ui.story_version = Some(1);
        let missing = view(&t, &compared());
        assert!(
            missing.list.rows[0].mono,
            "version 1 has not arrived: the static order meanwhile"
        );
    }

    #[test]
    fn comparing_marks_the_added_the_changed_and_the_removed() {
        let mut t = tab();
        t.ui.story_compare = true;
        let v = view(&t, &compared());
        assert_eq!(v.list.heading, "THE CHANGE IN 6 STEPS");
        assert_eq!(
            v.body.legend,
            Some(Legend {
                since: 1,
                added: 1,
                changed: 1,
                removed: 1
            })
        );
        assert_eq!(
            titles(&v),
            [
                "Refresh one minute early",
                "One refresh at a time",
                "Safer saves",
                "Retry with a limit",
                "Tests for the race",
                "Retry forever on 5xx",
                "Small and mechanical"
            ],
            "the removed step sits before the trailing mechanical one"
        );
        let marks: Vec<Option<Mark>> = v.list.rows.iter().map(|r| r.mark).collect();
        assert_eq!(
            marks,
            [
                None,
                Some(Mark::Changed),
                None,
                Some(Mark::Added),
                None,
                Some(Mark::Removed),
                None
            ]
        );
        assert_eq!(v.list.rows[3].meta, "client/http.rs · new in version 2");
        let gone = &v.list.rows[5];
        assert_eq!((gone.number, gone.state), (None, RowState::Removed));
        assert_eq!(gone.meta, "client/http.rs · gone in version 2");
        assert_eq!(
            v.list.rows[6].number,
            Some(6),
            "numbers skip the removed step"
        );
        let c = v.body.card.expect("a card");
        assert_eq!(c.mark, Some(Mark::Changed));
        assert_eq!(
            c.since.as_deref(),
            Some(
                "the lock is released before the token is saved, so a slow disk no longer blocks requests."
            )
        );
        assert_eq!(v.list.counts, "1 of 6 read");

        t.ui.story_compare = false;
        let v = view(&t, &compared());
        assert!(v.body.legend.is_none() && v.list.rows.iter().all(|r| r.mark.is_none()));
        assert_eq!(v.list.rows.len(), 6);
        assert_eq!(v.body.card.expect("a card").since, None);
    }

    #[test]
    fn the_comparison_needs_a_version_after_the_first_with_something_to_show() {
        let mut t = tab();
        t.ui.story_compare = true;
        assert_eq!(compare_active(&t.ui, &compared()), Some((1, 2)));
        assert_eq!(compare_active(&t.ui, &written()), None);
        assert_eq!(compare_active(&t.ui, &statically(StoryState::Idle)), None);
        t.ui.story_compare = false;
        assert_eq!(compare_active(&t.ui, &compared()), None);
    }

    #[test]
    fn the_notes_are_the_drafts_and_findings_in_the_steps_lines_and_not_the_accepted_ones() {
        let t = tab();
        let draft = t.ready().expect("ready").view.review.draft.clone();
        let Tell::Checks(checks_tell) = fixture::demo_checks_tell(&fixture::demo_pr(), NOW, &draft)
        else {
            panic!("a checks tell");
        };
        let mut checks = ChecksModel::default();
        checks.apply(&checks_tell);
        let notes = |step: &str, checks: &ChecksModel| {
            let mut t = tab();
            t.ui.story_step = Some(step.into());
            story_view(
                &t,
                Some(&written()),
                Some(checks),
                &ctx(),
                &mut HighlightCache::default(),
            )
            .and_then(|v| v.body.card)
            .map(|c| c.notes)
            .expect("a card")
        };
        let n = notes("one-at-a-time", &checks);
        let kinds: Vec<NoteKind> = n.iter().map(|x| x.kind).collect();
        assert_eq!(kinds, [NoteKind::Draft, NoteKind::Audit]);
        assert!(
            n[0].text
                .starts_with("Holding the lock across the network call")
        );
        assert_eq!(n[0].who, "You");
        assert_eq!(
            n[1].text,
            "The refresh lock is held across a network call (Concurrency)"
        );
        assert_eq!(n[1].who, "Claude Code");
        let safer: Vec<NoteKind> = notes("safer-saves", &checks)
            .iter()
            .map(|x| x.kind)
            .collect();
        assert_eq!(
            safer,
            [NoteKind::Draft, NoteKind::Security],
            "store.rs:88's draft, and the security finding on that line"
        );

        let mut accepted = checks.clone();
        let id = accepted
            .audit
            .result
            .as_ref()
            .and_then(|r| {
                r.findings
                    .iter()
                    .find(|f| f.title.starts_with("The refresh lock"))
            })
            .map(|f| f.id.clone())
            .expect("the finding");
        accepted.accepted.insert(id.clone());
        assert_eq!(
            notes("one-at-a-time", &accepted)
                .iter()
                .map(|x| x.kind)
                .collect::<Vec<_>>(),
            [NoteKind::Draft],
            "an accepted finding is already a draft item"
        );
        let mut dismissed = checks.clone();
        dismissed.dismissed.insert(id);
        assert_eq!(notes("one-at-a-time", &dismissed).len(), 1);
    }

    #[test]
    fn a_step_with_no_lines_in_the_patch_still_has_a_card() {
        let mut demo = fixture::demo_story(NOW);
        let spec = demo.first.spec.as_mut().expect("a spec");
        spec.steps[1].hunks = vec![clusia_core::hunks::HunkRef {
            path: "gone.rs".into(),
            start_line: 1,
            end_line: 3,
        }];
        let m = model(vec![demo.first], StoryState::Idle, &["refresh-early"]);
        let c = view(&tab(), &m).body.card.expect("a card");
        assert!(c.blocks.is_empty());
        assert_eq!(c.places, ["gone.rs:1"]);
    }

    #[test]
    fn no_model_or_no_ready_review_gives_no_view() {
        assert!(story_view(&tab(), None, None, &ctx(), &mut HighlightCache::default()).is_none());
        let loading = Tab::loading(DiffMode::Story);
        assert!(
            story_view(
                &loading,
                Some(&written()),
                None,
                &ctx(),
                &mut HighlightCache::default()
            )
            .is_none()
        );
    }

    #[test]
    fn the_status_counts_steps_read_and_the_comparison() {
        let mut t = tab();
        assert_eq!(step_counts(&t.ui, &written()), (1, 5));
        assert_eq!(step_counts(&t.ui, &statically(StoryState::Idle)), (0, 6));
        assert_eq!(step_counts(&t.ui, &StoryModel::default()), (0, 0));
        t.ui.story_version = Some(1);
        assert_eq!(
            step_counts(&t.ui, &compared()).1,
            6,
            "version 1 is not loaded: the static order"
        );
    }
}

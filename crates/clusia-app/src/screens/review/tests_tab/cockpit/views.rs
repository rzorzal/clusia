//! What each region of the cockpit draws, as small pure views of the model. The list, header,
//! center, and copilot views hold only printable, bounded text. `CompareView` carries the
//! comparison, last side and main phase as the daemon sent them: their drawer makes each text
//! printable. A region compares its view with the one it built to know whether it must rebuild.

use std::collections::BTreeMap;

use clusia_core::cockpit::{
    BenchView, COPILOT_BUSY, COPILOT_TEXT_MAX, CockpitActivity, CompareOutcome, ComparisonView,
    CopilotMessage, DeltaLabel, E2E_DIR, Freshness, ItemKind, ItemRef, ItemSupport, MainPhase,
    Metric, RunSide, Sample, ScenarioView, SideView, Suggestion, Verdict, bench_status,
    scenario_status,
};
use clusia_core::functions::{FunctionChange, ParamKind};
use clusia_core::printable::{printable, printable_lines};
use clusia_core::tests::RunnerKind;

use super::CockpitModel;
use crate::screens::review::tests_tab::runner_label;

/// Titles, `qualified` names and input values: at most this many characters.
pub const TITLE_SHOWN: usize = 120;
/// Purposes, the steps line, reasons and `failed`.
pub const PURPOSE_SHOWN: usize = 600;
pub const VALUE_SHOWN: usize = 200;

/// `text` made printable and cut to `max` characters, with "…" where it was cut.
pub fn shown(text: &str, max: usize) -> String {
    let plain = printable(text);
    match plain.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &plain[..at]),
        None => plain,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Running,
    Worse,
    Warn,
    Faint,
}

/// The color a status line gets.
pub fn tone_of(status: &str) -> Tone {
    if status.starts_with("running") {
        Tone::Running
    } else if status.contains("worse") || status.split_whitespace().next() == Some("failed") {
        Tone::Worse
    } else if status.starts_with("not approved") || status.starts_with("approve again") {
        Tone::Warn
    } else {
        Tone::Faint
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MainStatus {
    Ready { at: String },
    NeedsSetup { at: String },
    Preparing { at: String },
    Failed { at: String },
    NotSet,
    Unavailable { reason: String },
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HeaderView {
    pub main: MainStatus,
    pub agent: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub item: ItemRef,
    pub title: String,
    pub mono: bool,
    pub status: String,
    pub tone: Tone,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunAll {
    pub label: String,
    pub off: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmptyCenter {
    pub title: String,
    pub text: String,
    pub button: String,
    pub foot: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListView {
    pub rows: Vec<Row>,
    pub scenarios_writing: Option<String>,
    pub bench_writing: BTreeMap<String, String>,
    pub on_main: bool,
    pub on_main_note: Option<String>,
    pub run_all: RunAll,
    pub empty_center: Option<EmptyCenter>,
    pub failed: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CenterView {
    pub loading: bool,
    pub item: Option<ItemRef>,
    pub title: String,
    pub path: String,
    pub purpose: String,
    pub steps: String,
    pub edited: bool,
    pub support_note: Option<String>,
    pub failed: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChartView {
    pub live: bool,
    pub rounds: u32,
    pub pr: Vec<Sample>,
    pub main: Vec<Sample>,
    pub memory_worse: bool,
    pub cpu_worse: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoundsSoFar {
    pub pr: u32,
    pub main: u32,
    pub of: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompareView {
    pub item: Option<ItemRef>,
    pub subtitle: String,
    pub comparison: Option<ComparisonView>,
    pub last: Option<SideView>,
    pub running: Option<RoundsSoFar>,
    pub main: MainPhase,
    pub first_build: bool,
    pub runner: Option<RunnerKind>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CopilotThreadView {
    pub messages: Vec<CopilotMessage>,
    pub busy_line: Option<String>,
    pub failed: Option<String>,
    pub placeholder: String,
    pub disabled: bool,
    pub explain: bool,
    pub next: bool,
    pub intro: Option<String>,
}

pub fn header_view(model: &CockpitModel, agent: &str, _now: i64) -> HeaderView {
    let agent = printable(agent);
    let Some(view) = &model.view else {
        return HeaderView {
            main: MainStatus::Unknown,
            agent,
        };
    };
    let at = view
        .main
        .at
        .as_deref()
        .map(|a| printable(&a.chars().take(7).collect::<String>()))
        .unwrap_or_default();
    let main = match &view.main.phase {
        MainPhase::NotCreated => MainStatus::NotSet,
        MainPhase::Creating | MainPhase::Preparing { .. } => MainStatus::Preparing { at },
        MainPhase::NeedsSetup => MainStatus::NeedsSetup { at },
        MainPhase::Ready => MainStatus::Ready { at },
        MainPhase::Failed { .. } => MainStatus::Failed { at },
        MainPhase::Unavailable { reason } => MainStatus::Unavailable {
            reason: shown(reason, PURPOSE_SHOWN),
        },
    };
    HeaderView { main, agent }
}

fn item_of(kind: ItemKind, id: &str) -> ItemRef {
    ItemRef {
        kind,
        id: id.to_string(),
    }
}

fn running_is(model: &CockpitModel, item: &ItemRef) -> bool {
    model.running_item() == Some(item)
}

fn failed_text(model: &CockpitModel) -> Option<String> {
    let failed = model.view.as_ref()?.failed.as_ref()?;
    (model.dismissed_failed.as_ref() != Some(failed)).then(|| shown(failed, PURPOSE_SHOWN))
}

pub fn list_view(model: &CockpitModel, agent: &str, _now: i64) -> ListView {
    let agent = printable(agent);
    let on_main = model.on_main;
    let mut view = ListView {
        rows: Vec::new(),
        scenarios_writing: None,
        bench_writing: BTreeMap::new(),
        on_main,
        on_main_note: None,
        run_all: RunAll {
            label: if on_main {
                "Run all · this PR and main".into()
            } else {
                "Run all · this PR".into()
            },
            off: model
                .run_disabled_reason(None, on_main, model.tests_busy)
                .map(|r| shown(&r, PURPOSE_SHOWN)),
        },
        empty_center: None,
        failed: failed_text(model),
    };
    let Some(cockpit) = &model.view else {
        return view;
    };
    if let Some(CockpitActivity::Writing { what }) = &cockpit.activity {
        if what == "scenarios" {
            view.scenarios_writing = Some(format!("{agent} is writing scenarios…"));
        } else if let Some(slug) = what.strip_prefix("bench:") {
            view.bench_writing
                .insert(slug.to_string(), format!("{agent} is writing a benchmark…"));
        }
    }
    if let MainPhase::Unavailable { reason } = &cockpit.main.phase {
        view.on_main_note = Some(shown(reason, PURPOSE_SHOWN));
    }
    for s in &cockpit.scenarios {
        let item = item_of(ItemKind::Scenario, &s.slug);
        let status = scenario_status(s, running_is(model, &item));
        view.rows.push(Row {
            title: shown(&s.title, TITLE_SHOWN),
            mono: false,
            tone: tone_of(&status),
            status: shown(&status, PURPOSE_SHOWN),
            selected: model.selected.as_ref() == Some(&item),
            item,
        });
    }
    for b in &cockpit.benches {
        let item = item_of(ItemKind::Bench, &b.slug);
        let status = match view.bench_writing.get(&b.slug) {
            Some(text) => text.clone(),
            None => bench_status(b, running_is(model, &item)),
        };
        view.rows.push(Row {
            title: shown(&b.qualified, TITLE_SHOWN),
            mono: true,
            tone: tone_of(&status),
            status: shown(&status, PURPOSE_SHOWN),
            selected: model.selected.as_ref() == Some(&item),
            item,
        });
    }
    if model.loaded && cockpit.scenarios.is_empty() && cockpit.benches.is_empty() {
        let foot = match cockpit.runner {
            Some(runner) => format!(
                "Nothing runs until you click Run · {} · no network",
                runner_label(runner)
            ),
            None => "Nothing runs until you click Run · no network".into(),
        };
        view.empty_center = Some(EmptyCenter {
            title: "Check this change end to end".into(),
            text: format!(
                "{agent} reads the diff and writes scenarios: a script, its steps and inputs \
                 with fake values. You read and approve each one before it runs, on this PR \
                 and, if you want, on main."
            ),
            button: format!("Write scenarios with {agent}"),
            foot,
        });
    }
    view
}

fn kind_word(kind: ParamKind) -> &'static str {
    match kind {
        ParamKind::Text => "text",
        ParamKind::Int => "int",
        ParamKind::Float => "float",
        ParamKind::Bool => "bool",
        ParamKind::Json => "json",
    }
}

fn change_word(change: FunctionChange) -> &'static str {
    match change {
        FunctionChange::Changed => "changed",
        FunctionChange::New => "new",
        FunctionChange::Signature => "signature changed",
        FunctionChange::Moved => "moved",
    }
}

fn support_note(support: &ItemSupport) -> Option<String> {
    match support {
        ItemSupport::NoCommand { reason }
        | ItemSupport::Stale { reason }
        | ItemSupport::Unsupported { reason } => Some(shown(reason, PURPOSE_SHOWN)),
        ItemSupport::Ready | ItemSupport::NeedsApproval => None,
    }
}

fn scenario_center(s: &ScenarioView, agent: &str) -> CenterView {
    let steps: Vec<String> = s
        .steps
        .iter()
        .enumerate()
        .map(|(i, step)| format!("{} {}", i + 1, printable(step)))
        .collect();
    CenterView {
        loading: false,
        item: Some(item_of(ItemKind::Scenario, &s.slug)),
        title: shown(&s.title, TITLE_SHOWN),
        path: shown(&format!("{E2E_DIR}/{}", s.slug), TITLE_SHOWN),
        purpose: format!(
            "{} Written by {agent} from the diff; edit anything.",
            shown(&s.purpose, PURPOSE_SHOWN)
        )
        .trim_start()
        .to_string(),
        steps: shown(&steps.join(" · "), PURPOSE_SHOWN),
        edited: s.edited,
        support_note: support_note(&s.support),
        failed: None,
    }
}

fn bench_center(b: &BenchView, agent: &str) -> CenterView {
    let params: Vec<String> = b
        .params
        .iter()
        .map(|p| format!("{}: {}", printable(&p.name), kind_word(p.kind)))
        .collect();
    CenterView {
        loading: false,
        item: Some(item_of(ItemKind::Bench, &b.slug)),
        title: shown(
            &format!("fn {}({})", printable(&b.qualified), params.join(", ")),
            TITLE_SHOWN,
        ),
        path: shown(&b.file, TITLE_SHOWN),
        purpose: format!(
            "{} · {}. Benchmark written by {agent} from the diff; edit anything.",
            shown(&b.file, TITLE_SHOWN),
            change_word(b.change)
        ),
        steps: String::new(),
        edited: b.edited,
        support_note: support_note(&b.support),
        failed: None,
    }
}

pub fn center_view(model: &CockpitModel, agent: &str, _now: i64) -> CenterView {
    let agent = printable(agent);
    let mut view = if let Some(s) = model.selected_scenario() {
        scenario_center(s, &agent)
    } else if let Some(b) = model.selected_bench() {
        bench_center(b, &agent)
    } else {
        CenterView {
            loading: false,
            item: None,
            title: String::new(),
            path: String::new(),
            purpose: String::new(),
            steps: String::new(),
            edited: false,
            support_note: None,
            failed: None,
        }
    };
    view.loading = !model.loaded;
    view.failed = failed_text(model);
    view
}

/// The comparison and the last run of the selected item.
fn results_of(model: &CockpitModel) -> (Option<&ComparisonView>, Option<&SideView>) {
    if let Some(s) = model.selected_scenario() {
        (s.comparison.as_ref(), s.last.as_ref())
    } else if let Some(b) = model.selected_bench() {
        (b.comparison.as_ref(), b.last.as_ref())
    } else {
        (None, None)
    }
}

/// Whether the row of `metric` may be drawn as worse: only a verdict that is one, between two
/// sides that are about what is under review now.
fn worse(c: &ComparisonView, metric: Metric) -> bool {
    let verdict_is_one = !matches!(
        c.verdict,
        Verdict::Running | Verdict::Stopped | Verdict::NotEnoughRounds
    );
    verdict_is_one
        && c.pr.freshness == Freshness::Current
        && c.main.freshness == Freshness::Current
        && c.rows
            .iter()
            .any(|r| r.metric == metric && r.delta.label == DeltaLabel::Worse)
}

pub fn chart_view(model: &CockpitModel, _agent: &str, _now: i64) -> ChartView {
    let (comparison, last) = results_of(model);
    let mut view = ChartView {
        live: false,
        rounds: 0,
        pr: Vec::new(),
        main: Vec::new(),
        memory_worse: false,
        cpu_worse: false,
    };
    if let Some(c) = comparison {
        view.pr = c.pr.series.points.clone();
        view.main = c.main.series.points.clone();
        view.rounds = c.pr.rounds;
        view.memory_worse = worse(c, Metric::PeakMemory);
        view.cpu_worse = worse(c, Metric::AvgCpu);
    } else if let Some(last) = last {
        view.pr = last.series.points.clone();
        view.rounds = last.rounds;
    }
    let running = model
        .selected
        .as_ref()
        .filter(|item| model.running_item() == Some(*item));
    if running.is_some()
        && let Some(CockpitActivity::Running {
            run, side, round, ..
        }) = model.view.as_ref().and_then(|v| v.activity.as_ref())
        && let Some(points) = model
            .live
            .as_ref()
            .filter(|l| &l.run == run)
            .and_then(|l| l.points.get(&(*side, *round)))
    {
        match side {
            RunSide::Pr => view.pr = points.clone(),
            RunSide::Main => view.main = points.clone(),
        }
        view.live = true;
    }
    view
}

pub fn compare_view(model: &CockpitModel, _agent: &str, _now: i64) -> CompareView {
    let (comparison, last) = results_of(model);
    let running_here = model
        .selected
        .as_ref()
        .is_some_and(|item| model.running_item() == Some(item));
    let rounds = comparison.map_or(0, |c| c.pr.rounds);
    let (main, first_build, runner) = match &model.view {
        Some(v) => (v.main.phase.clone(), v.main.first_build, v.runner),
        None => (MainPhase::NotCreated, false, None),
    };
    let running = running_here.then(|| {
        let of = match model.view.as_ref().and_then(|v| v.activity.as_ref()) {
            Some(CockpitActivity::Running { rounds, .. }) => *rounds,
            _ => 0,
        };
        RoundsSoFar {
            pr: rounds,
            main: comparison.map_or(0, |c| c.main.rounds),
            of,
        }
    });
    CompareView {
        item: model.selected.clone(),
        subtitle: if running_here {
            "median ± spread · rounds so far".into()
        } else {
            format!("median ± spread · {rounds} rounds")
        },
        comparison: comparison.cloned(),
        last: last.cloned(),
        running,
        main,
        first_build,
        runner,
    }
}

fn copilot_message(message: &CopilotMessage) -> CopilotMessage {
    CopilotMessage {
        id: message.id,
        at: message.at,
        from: message.from,
        item: message.item.clone(),
        text: printable_lines(&message.text)
            .chars()
            .take(COPILOT_TEXT_MAX)
            .collect(),
        suggestions: message
            .suggestions
            .iter()
            .map(|s| Suggestion {
                kind: s.kind,
                title: printable(&s.title),
                why: printable(&s.why),
                function: s.function.clone(),
            })
            .collect(),
    }
}

pub fn copilot_view(model: &CockpitModel, agent: &str, _now: i64) -> CopilotThreadView {
    let agent = printable(agent);
    let Some(view) = &model.view else {
        return CopilotThreadView {
            messages: Vec::new(),
            busy_line: None,
            failed: None,
            placeholder: "Ask for a scenario, or what to try next…".into(),
            disabled: false,
            explain: false,
            next: false,
            intro: Some(INTRO.into()),
        };
    };
    let copilot = &view.copilot;
    let empty = view.scenarios.is_empty() && view.benches.is_empty();
    let finished = |c: &Option<ComparisonView>| {
        c.as_ref()
            .is_some_and(|c| c.outcome == CompareOutcome::Done)
    };
    let has_comparison = model
        .selected_scenario()
        .is_some_and(|s| finished(&s.comparison))
        || model
            .selected_bench()
            .is_some_and(|b| finished(&b.comparison));
    let about_it = model.selected.as_ref().is_some_and(|item| {
        copilot
            .messages
            .iter()
            .any(|m| m.item.as_ref() == Some(item))
    });
    CopilotThreadView {
        messages: copilot.messages.iter().map(copilot_message).collect(),
        busy_line: copilot.busy.then(|| format!("{agent} is answering…")),
        failed: copilot.failed.as_deref().map(|f| shown(f, PURPOSE_SHOWN)),
        placeholder: if copilot.busy {
            COPILOT_BUSY.into()
        } else if empty {
            "Ask for a scenario, or what to try next…".into()
        } else {
            "Ask why, or what to try next…".into()
        },
        disabled: copilot.busy,
        explain: has_comparison && !about_it,
        next: model.loaded && !copilot.busy,
        intro: copilot.messages.is_empty().then(|| INTRO.into()),
    }
}

const INTRO: &str = "Ask what to test, or why a result came out the way it did. The copilot \
    reads the diff and the results; it never runs anything itself.";

#[cfg(test)]
mod tests {
    use clusia_core::cockpit::{CopilotFrom, Verdict};

    use super::*;
    use crate::bridge::CockpitTell;
    use crate::fixture::{self, DemoCockpit, demo_cockpit};
    use crate::screens::review::tests_tab::cockpit::CockpitModel;
    use crate::testing::NOW;
    use clusia_core::cockpit::{CockpitView, CompareOutcome, ItemKind, ItemRef, NO_SCENARIOS};

    const AGENT: &str = "Claude Code";

    fn model_of(view: CockpitView) -> CockpitModel {
        let mut m = CockpitModel::default();
        m.apply(&CockpitTell::Loaded {
            pr: fixture::demo_pr(),
            view: Box::new(view),
        });
        m
    }

    fn scenario_model() -> CockpitModel {
        model_of(demo_cockpit(NOW, DemoCockpit::Scenario))
    }

    fn select(m: &mut CockpitModel, kind: ItemKind, id: &str) {
        m.select(ItemRef {
            kind,
            id: id.into(),
        });
    }

    fn backoff(m: &CockpitModel) -> String {
        m.view.as_ref().expect("view").benches[0].slug.clone()
    }

    #[test]
    fn a_status_has_its_tone() {
        assert_eq!(tone_of("running…"), Tone::Running);
        assert_eq!(tone_of("memory +116% · worse than main"), Tone::Worse);
        assert_eq!(tone_of("failed · main: passed"), Tone::Worse);
        assert_eq!(tone_of("passed · main: failed"), Tone::Faint);
        assert_eq!(tone_of("not approved"), Tone::Warn);
        assert_eq!(tone_of("approve again: the script changed"), Tone::Warn);
        assert_eq!(tone_of("not run"), Tone::Faint);
    }

    #[test]
    fn shown_is_printable_and_cut() {
        assert_eq!(shown("abc", 3), "abc");
        assert_eq!(shown("abcd", 3), "abc…");
        assert_eq!(shown("é\u{202E}", 10), printable("é\u{202E}"));
        assert!(!shown("a\u{202E}b", 10).contains('\u{202E}'));
    }

    #[test]
    fn the_list_is_the_mockups() {
        let mut m = scenario_model();
        let list = list_view(&m, AGENT, NOW);
        let rows: Vec<(&str, &str)> = list
            .rows
            .iter()
            .map(|r| (r.title.as_str(), r.status.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("Two requests refresh at once", "passed · main: failed"),
                (
                    "Refresh under steady traffic",
                    "memory +116% · worse than main"
                ),
                ("Token endpoint is down", "not approved"),
                ("backoff_delay", "no clear difference (±3%)"),
                ("TokenStore::refresh", "not run"),
            ]
        );
        assert_eq!(
            list.rows.iter().map(|r| r.mono).collect::<Vec<_>>(),
            [false, false, false, true, true]
        );
        assert!(list.rows[0].selected && list.rows.iter().filter(|r| r.selected).count() == 1);
        assert_eq!(list.run_all.label, "Run all · this PR");
        assert!(list.empty_center.is_none());
        m.on_main = true;
        let list = list_view(&m, AGENT, NOW);
        assert_eq!(list.run_all.label, "Run all · this PR and main");
        assert_eq!(list.run_all.off, None);

        let mut empty = model_of(demo_cockpit(NOW, DemoCockpit::Empty));
        let list = list_view(&empty, AGENT, NOW);
        assert!(list.rows.is_empty());
        let center = list.empty_center.expect("the empty card");
        assert_eq!(center.title, "Check this change end to end");
        assert_eq!(center.button, "Write scenarios with Claude Code");
        assert_eq!(
            center.foot,
            "Nothing runs until you click Run · container · no network"
        );
        assert_eq!(list.run_all.off.as_deref(), Some(NO_SCENARIOS));

        let mut writing = demo_cockpit(NOW, DemoCockpit::Empty);
        writing.activity = Some(CockpitActivity::Writing {
            what: "scenarios".into(),
        });
        empty.apply(&CockpitTell::Changed {
            pr: fixture::demo_pr(),
            view: Box::new(writing),
        });
        let list = list_view(&empty, AGENT, NOW);
        assert_eq!(
            list.scenarios_writing.as_deref(),
            Some("Claude Code is writing scenarios…")
        );
        assert!(list.empty_center.is_some());

        let mut one_bench = demo_cockpit(NOW, DemoCockpit::Scenario);
        one_bench.scenarios.clear();
        one_bench.benches.truncate(1);
        let m1 = model_of(one_bench);
        assert!(list_view(&m1, AGENT, NOW).empty_center.is_none());
    }

    #[test]
    fn a_running_bench_and_a_writing_bench_say_so_on_their_row() {
        let view = demo_cockpit(NOW, DemoCockpit::BenchRunning);
        let m = model_of(view);
        let list = list_view(&m, AGENT, NOW);
        let running: Vec<&Row> = list
            .rows
            .iter()
            .filter(|r| r.tone == Tone::Running)
            .collect();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].title, "backoff_delay");
        assert_eq!(running[0].status, "running…");

        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        let slug = view.benches[0].slug.clone();
        view.activity = Some(CockpitActivity::Writing {
            what: format!("bench:{slug}"),
        });
        let m = model_of(view);
        let list = list_view(&m, AGENT, NOW);
        assert_eq!(list.rows[3].status, "Claude Code is writing a benchmark…");
        assert_eq!(list.bench_writing[&slug], list.rows[3].status);
    }

    #[test]
    fn the_header_names_mains_state() {
        assert_eq!(
            header_view(&CockpitModel::default(), AGENT, NOW).main,
            MainStatus::Unknown
        );
        let main = |state| header_view(&model_of(demo_cockpit(NOW, state)), AGENT, NOW).main;
        assert_eq!(main(DemoCockpit::Empty), MainStatus::NotSet);
        assert_eq!(
            main(DemoCockpit::Scenario),
            MainStatus::Ready {
                at: "4e1f2a0".into()
            }
        );
        assert!(matches!(
            main(DemoCockpit::Main),
            MainStatus::NeedsSetup { .. }
        ));
        let phase = |phase| {
            let mut v = demo_cockpit(NOW, DemoCockpit::Scenario);
            v.main.phase = phase;
            header_view(&model_of(v), AGENT, NOW).main
        };
        assert!(matches!(
            phase(MainPhase::Preparing { step: "x".into() }),
            MainStatus::Preparing { .. }
        ));
        assert!(matches!(
            phase(MainPhase::Creating),
            MainStatus::Preparing { .. }
        ));
        assert!(matches!(
            phase(MainPhase::Failed {
                message: "m".into()
            }),
            MainStatus::Failed { .. }
        ));
        assert_eq!(
            phase(MainPhase::Unavailable {
                reason: "a\u{202E}b".into()
            }),
            MainStatus::Unavailable {
                reason: printable("a\u{202E}b")
            }
        );
        assert_eq!(
            header_view(&scenario_model(), "a\u{202E}b", NOW).agent,
            printable("a\u{202E}b")
        );
    }

    #[test]
    fn the_center_of_a_scenario_and_of_a_benchmark() {
        let mut m = scenario_model();
        select(&mut m, ItemKind::Scenario, "refresh-under-steady-traffic");
        let c = center_view(&m, AGENT, NOW);
        assert_eq!(c.title, "Refresh under steady traffic");
        assert_eq!(c.path, ".clusia/e2e/refresh-under-steady-traffic");
        assert_eq!(
            c.purpose,
            "Many callers ask for a token while it expires every few seconds. Written by Claude Code from the diff; edit anything."
        );
        assert_eq!(
            c.steps,
            "1 Store a token that expires soon · 2 Call token() from every caller · 3 Count exchanges and errors"
        );
        assert!(!c.loading);
        let slug = backoff(&m);
        select(&mut m, ItemKind::Bench, &slug);
        let c = center_view(&m, AGENT, NOW);
        assert_eq!(c.title, "fn backoff_delay(attempt: int, base: text)");
        assert_eq!(
            c.purpose,
            "http/retry.rs · signature changed. Benchmark written by Claude Code from the diff; edit anything."
        );
        assert!(center_view(&CockpitModel::default(), AGENT, NOW).loading);
    }

    #[test]
    fn a_failed_turn_hides_once_dismissed_until_its_text_changes() {
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.failed = Some("the agent stopped".into());
        let mut m = model_of(view);
        assert_eq!(
            list_view(&m, AGENT, NOW).failed.as_deref(),
            Some("the agent stopped")
        );
        assert_eq!(
            center_view(&m, AGENT, NOW).failed.as_deref(),
            Some("the agent stopped")
        );
        m.dismissed_failed = Some("the agent stopped".into());
        assert_eq!(list_view(&m, AGENT, NOW).failed, None);
        let mut other = demo_cockpit(NOW, DemoCockpit::Scenario);
        other.failed = Some("another".into());
        m.apply(&CockpitTell::Changed {
            pr: fixture::demo_pr(),
            view: Box::new(other),
        });
        assert_eq!(list_view(&m, AGENT, NOW).failed.as_deref(), Some("another"));
        m.apply(&CockpitTell::Changed {
            pr: fixture::demo_pr(),
            view: Box::new(demo_cockpit(NOW, DemoCockpit::Scenario)),
        });
        assert_eq!(m.dismissed_failed, None);
    }

    #[test]
    fn untrusted_text_is_plain_and_bounded_in_every_view() {
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.scenarios[0].title = "[x](http://e) **b** \u{202E}evil".into();
        view.scenarios[1].title = "t".repeat(10_000);
        view.scenarios[1].purpose = "p".repeat(10_000);
        view.copilot.messages[0].text = format!("{}\u{202E}", "a".repeat(10_000));
        view.copilot.messages[1].suggestions[0].title = "x\u{202E}y".into();
        view.failed = Some("f".repeat(10_000));
        view.scenarios[2].support = ItemSupport::Stale {
            reason: format!("\u{202E}{}", "r".repeat(10_000)),
        };
        let mut m = model_of(view);
        let list = list_view(&m, AGENT, NOW);
        assert!(!list.rows[0].title.contains('\u{202E}'));
        assert!(
            list.rows[0].title.contains("[x](http://e)"),
            "literal text, not stripped"
        );
        assert!(list.rows[1].title.chars().count() <= TITLE_SHOWN + 1);
        assert!(list.failed.expect("failed").chars().count() <= PURPOSE_SHOWN + 1);
        let mut stale_only = demo_cockpit(NOW, DemoCockpit::Scenario);
        for s in &mut stale_only.scenarios {
            s.support = ItemSupport::Stale {
                reason: format!("\u{202E}{}", "r".repeat(10_000)),
            };
        }
        let stale = list_view(&model_of(stale_only), AGENT, NOW);
        let off = stale.run_all.off.expect("a reason");
        assert!(!off.contains('\u{202E}') && off.chars().count() <= PURPOSE_SHOWN + 1);
        let c = center_view(&m, AGENT, NOW);
        assert!(!c.title.contains('\u{202E}'));
        select(&mut m, ItemKind::Scenario, "refresh-under-steady-traffic");
        let c = center_view(&m, AGENT, NOW);
        assert!(c.title.chars().count() <= TITLE_SHOWN + 1);
        assert!(c.purpose.chars().count() <= PURPOSE_SHOWN + 1 + 60);
        let thread = copilot_view(&m, AGENT, NOW);
        assert!(!thread.messages[0].text.contains('\u{202E}'));
        assert!(thread.messages[0].text.chars().count() <= COPILOT_TEXT_MAX);
        assert!(!thread.messages[1].suggestions[0].title.contains('\u{202E}'));
    }

    #[test]
    fn a_verdict_that_isnt_one_marks_no_chart_line_worse() {
        let mut m = scenario_model();
        select(&mut m, ItemKind::Scenario, "refresh-under-steady-traffic");
        let chart = chart_view(&m, AGENT, NOW);
        assert!(chart.memory_worse);
        assert!(!chart.cpu_worse);
        assert_eq!(chart.rounds, 3);
        let with = |edit: &dyn Fn(&mut clusia_core::cockpit::ComparisonView)| {
            let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
            edit(view.scenarios[1].comparison.as_mut().expect("comparison"));
            let mut m = model_of(view);
            select(&mut m, ItemKind::Scenario, "refresh-under-steady-traffic");
            chart_view(&m, AGENT, NOW).memory_worse
        };
        for verdict in [Verdict::Running, Verdict::Stopped, Verdict::NotEnoughRounds] {
            assert!(!with(&|c| c.verdict = verdict.clone()), "{verdict:?}");
        }
        assert!(!with(&|c| c.pr.freshness = Freshness::OlderCommit));
        assert!(!with(&|c| c.main.freshness = Freshness::OlderBase));
    }

    #[test]
    fn the_chart_draws_the_live_round_while_it_runs() {
        let view = demo_cockpit(NOW, DemoCockpit::BenchRunning);
        let mut m = model_of(view);
        let slug = backoff(&m);
        select(&mut m, ItemKind::Bench, &slug);
        let points = vec![Sample {
            t_ms: 0,
            rss_kb: Some(3000),
            cpu_milli: Some(900),
        }];
        assert!(!chart_view(&m, AGENT, NOW).live);
        m.apply(&CockpitTell::Samples {
            pr: fixture::demo_pr(),
            run: "demo-bench-run".into(),
            item: ItemRef {
                kind: ItemKind::Bench,
                id: slug,
            },
            side: RunSide::Main,
            round: 3,
            points: points.clone(),
        });
        let chart = chart_view(&m, AGENT, NOW);
        assert!(chart.live);
        assert_eq!(chart.main, points);
        assert_eq!(chart.rounds, 3);

        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        let last = view.scenarios[1].comparison.take().expect("comparison").pr;
        view.scenarios[1].last = Some(last.clone());
        let mut m = model_of(view);
        select(&mut m, ItemKind::Scenario, "refresh-under-steady-traffic");
        let chart = chart_view(&m, AGENT, NOW);
        assert_eq!(chart.pr, last.series.points);
        assert!(chart.main.is_empty());
        assert!(!chart.memory_worse);
    }

    #[test]
    fn compare_says_rounds_so_far_while_running() {
        let mut m = scenario_model();
        select(&mut m, ItemKind::Scenario, "refresh-under-steady-traffic");
        let c = compare_view(&m, AGENT, NOW);
        assert_eq!(c.subtitle, "median ± spread · 3 rounds");
        assert_eq!(c.running, None);
        assert!(c.comparison.is_some());
        assert_eq!(c.runner, Some(RunnerKind::Container));

        let mut m = model_of(demo_cockpit(NOW, DemoCockpit::BenchRunning));
        let slug = backoff(&m);
        select(&mut m, ItemKind::Bench, &slug);
        let c = compare_view(&m, AGENT, NOW);
        assert_eq!(c.subtitle, "median ± spread · rounds so far");
        assert_eq!(
            c.running,
            Some(RoundsSoFar {
                pr: 3,
                main: 2,
                of: 5
            })
        );

        let m = model_of(demo_cockpit(NOW, DemoCockpit::Main));
        let c = compare_view(&m, AGENT, NOW);
        assert!(c.first_build);
        assert_eq!(c.main, MainPhase::NeedsSetup);
    }

    #[test]
    fn the_copilot_input_and_its_links() {
        let empty = model_of(demo_cockpit(NOW, DemoCockpit::Empty));
        let t = copilot_view(&empty, AGENT, NOW);
        assert_eq!(t.placeholder, "Ask for a scenario, or what to try next…");
        assert!(t.intro.is_some());
        assert!(!t.explain);

        let mut m = scenario_model();
        select(&mut m, ItemKind::Scenario, "refresh-under-steady-traffic");
        let t = copilot_view(&m, AGENT, NOW);
        assert!(!t.explain, "a message is about it");
        assert!(t.next && t.intro.is_none());
        assert_eq!(t.placeholder, "Ask why, or what to try next…");
        assert_eq!(t.messages.len(), 2);
        assert_eq!(t.messages[0].from, CopilotFrom::Reviewer);
        let slug = backoff(&m);
        select(&mut m, ItemKind::Bench, &slug);
        assert!(copilot_view(&m, AGENT, NOW).explain);
        let mut stopped = demo_cockpit(NOW, DemoCockpit::Scenario);
        stopped.benches[0]
            .comparison
            .as_mut()
            .expect("comparison")
            .outcome = CompareOutcome::Stopped;
        let mut m2 = model_of(stopped);
        select(&mut m2, ItemKind::Bench, &slug);
        assert!(
            !copilot_view(&m2, AGENT, NOW).explain,
            "only a finished comparison"
        );

        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.copilot.busy = true;
        m.apply(&CockpitTell::Changed {
            pr: fixture::demo_pr(),
            view: Box::new(view),
        });
        let t = copilot_view(&m, AGENT, NOW);
        assert!(t.disabled && !t.next);
        assert_eq!(t.placeholder, COPILOT_BUSY);
        assert_eq!(t.busy_line.as_deref(), Some("Claude Code is answering…"));
    }
}

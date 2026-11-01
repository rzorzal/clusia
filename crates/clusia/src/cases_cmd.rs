//! `clusia tests --functions` and `--run-cases`: the changed functions of a review, their cases,
//! and a run of them. Every text here comes from the pull request or its agent and is printed
//! through `line_of` or `printable_lines`.

use std::time::Duration;

use clusia_core::PrRef;
use clusia_core::functions::{
    CASES_CHANGED, CaseResult, CaseScope, CaseStatus, CaseView, FunctionChange, FunctionView,
    FunctionsActivity, FunctionsView, NO_CASES, Support, valid_slug,
};
use clusia_core::printable::printable_lines;
use clusia_core::tests::EnvState;
use clusia_protocol::{Client, ClientError, Command as Request, Event, Reply};

use crate::check::line_of;
use crate::run::{CliError, unexpected};
use crate::tests_cmd::{CasesWanted, Interrupts, duration_text, follow, runner_text};

/// How long a view that is still being read is waited for.
pub(crate) const READ_WAIT: Duration = Duration::from_secs(60);

/// The start of the note the daemon gives when it could not read the changed functions.
const UNREADABLE: &str = "The changed functions could not be read";

pub(crate) fn case_label(status: Option<CaseStatus>) -> &'static str {
    match status {
        Some(CaseStatus::Passed) => "PASS",
        Some(CaseStatus::Failed) => "FAIL",
        Some(CaseStatus::Error) => "ERR ",
        Some(CaseStatus::TimedOut) => "TIME",
        Some(CaseStatus::Stopped) => "STOP",
        None => "----",
    }
}

/// The stored result that belongs to the case as the view shows it: the same values (hash) and
/// the same commit. Any other result is of something the reviewer is no longer looking at.
pub(crate) fn current_result<'a>(
    view: &FunctionsView,
    case: &'a CaseView,
) -> Option<&'a CaseResult> {
    case.result
        .as_ref()
        .filter(|r| r.hash == case.hash && r.head == view.head)
}

/// Why a function cannot run; `None` when it can.
pub(crate) fn support_reason(function: &FunctionView) -> Option<String> {
    match &function.support {
        Support::Ready => None,
        Support::Unsupported { reason } | Support::Stale { reason } => Some(line_of(reason)),
        Support::NoCases => Some("No cases written yet; run with --generate".into()),
        Support::NoCommand => Some(format!(
            "The recipe has no command to run {} functions",
            function.lang.map_or("these", |lang| lang.label())
        )),
    }
}

fn change_text(change: FunctionChange) -> &'static str {
    match change {
        FunctionChange::Changed => "changed",
        FunctionChange::New => "new",
        FunctionChange::Signature => "signature",
        FunctionChange::Moved => "moved",
    }
}

fn count_text(n: usize) -> String {
    match n {
        0 => "no cases yet".into(),
        1 => "1 case".into(),
        n => format!("{n} cases"),
    }
}

pub(crate) fn function_lines(view: &FunctionsView) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(note) = &view.note {
        lines.push(line_of(note));
    }
    for function in &view.functions {
        lines.push(format!(
            "{}  {} · {} · {}",
            line_of(&function.qualified),
            line_of(&function.file),
            change_text(function.change),
            count_text(function.cases.len())
        ));
        if let Some(reason) = support_reason(function) {
            lines.push(format!("  Not run: {reason}"));
        }
        for case in &function.cases {
            let current = current_result(view, case);
            let mut line = format!(
                "  {}  {}  {}",
                case_label(current.map(|r| r.status)),
                line_of(&case.id),
                line_of(&case.name)
            );
            match (current, &case.result) {
                (Some(result), _) => line.push_str(&format!("  {}", duration_text(result.took_ms))),
                (None, Some(stored)) if stored.hash != case.hash => {
                    line.push_str("  (values changed since it ran)");
                }
                (None, Some(_)) => line.push_str("  (from an older commit)"),
                (None, None) => {}
            }
            lines.push(line);
        }
    }
    for path in &view.unsupported {
        lines.push(format!("{} · not supported yet", line_of(path)));
    }
    lines
}

/// The function `wanted` names: a slug, else an exact qualified name. Two functions of one name
/// are refused with their ids, as a usage error.
pub(crate) fn pick_function<'a>(
    view: &'a FunctionsView,
    wanted: &str,
) -> Result<&'a FunctionView, CliError> {
    if valid_slug(wanted)
        && let Some(function) = view.functions.iter().find(|f| f.slug == wanted)
    {
        return Ok(function);
    }
    let named: Vec<&FunctionView> = view
        .functions
        .iter()
        .filter(|f| f.qualified == wanted)
        .collect();
    match named.as_slice() {
        [one] => Ok(one),
        [] => Err(CliError::Other(format!(
            "No changed function {}",
            line_of(wanted)
        ))),
        many => {
            let mut text = format!(
                "More than one changed function is named {}; use one of these ids:",
                line_of(wanted)
            );
            for function in many {
                text.push_str(&format!("\n{}", line_of(&function.slug)));
            }
            Err(CliError::Usage(text))
        }
    }
}

pub(crate) fn scope_of(function: Option<&FunctionView>, case: Option<&str>) -> CaseScope {
    match (function, case) {
        (None, _) => CaseScope::All,
        (Some(f), None) => CaseScope::Function {
            function: f.slug.clone(),
        },
        (Some(f), Some(case)) => CaseScope::Case {
            function: f.slug.clone(),
            case: case.into(),
        },
    }
}

fn function_of<'a>(view: &'a FunctionsView, slug: &str) -> Result<&'a FunctionView, CliError> {
    view.functions
        .iter()
        .find(|f| f.slug == slug)
        .ok_or_else(|| CliError::Other(format!("No changed function {}", line_of(slug))))
}

/// The hash the daemon compares a run with: the function's own for a function or a case, the
/// view's for all of them.
pub(crate) fn seen_of(view: &FunctionsView, scope: &CaseScope) -> Result<String, CliError> {
    match scope {
        CaseScope::All => Ok(view.hash.clone()),
        CaseScope::Function { function } => Ok(function_of(view, function)?.hash.clone()),
        CaseScope::Case { function, case } => {
            let f = function_of(view, function)?;
            if f.cases.iter().any(|c| c.id == *case) {
                Ok(f.hash.clone())
            } else {
                Err(CliError::Other(format!(
                    "No case {} in {}",
                    line_of(case),
                    line_of(&f.qualified)
                )))
            }
        }
    }
}

/// How many cases the scope runs: only the cases of functions that can run.
pub(crate) fn runnable(view: &FunctionsView, scope: &CaseScope) -> u32 {
    let ready = |f: &&FunctionView| matches!(f.support, Support::Ready);
    let n: usize = match scope {
        CaseScope::All => view
            .functions
            .iter()
            .filter(ready)
            .map(|f| f.cases.len())
            .sum(),
        CaseScope::Function { function } => view
            .functions
            .iter()
            .filter(|f| f.slug == *function)
            .filter(ready)
            .map(|f| f.cases.len())
            .sum(),
        CaseScope::Case { function, case } => view
            .functions
            .iter()
            .filter(|f| f.slug == *function)
            .filter(ready)
            .map(|f| f.cases.iter().filter(|c| c.id == *case).count())
            .sum(),
    };
    u32::try_from(n).unwrap_or(u32::MAX)
}

pub(crate) fn case_line(qualified: &str, case_name: &str, result: &CaseResult) -> String {
    format!(
        "{}  {}  {}  {}  {}",
        case_label(Some(result.status)),
        line_of(qualified),
        line_of(&result.case),
        line_of(case_name),
        duration_text(result.took_ms)
    )
}

/// What a case that did not pass shows: where it went wrong, then the filtered output tail.
pub(crate) fn case_failure_lines(
    qualified: &str,
    case_name: &str,
    result: &CaseResult,
) -> Vec<String> {
    let mut lines = vec![format!(
        "{}  {}  {}  {}",
        case_label(Some(result.status)),
        line_of(qualified),
        line_of(&result.case),
        line_of(case_name)
    )];
    if let Some(returned) = &result.returned {
        let mut line = format!("  returned: {}", line_of(returned));
        if let Some(expected) = &result.returned_expected
            && expected.trim() != returned.trim()
        {
            line.push_str(&format!(" (expected {})", line_of(expected)));
        }
        lines.push(line);
    }
    for check in &result.state {
        let Some(expected) = &check.expected else {
            continue;
        };
        if check.got.as_ref() == Some(expected) {
            continue;
        }
        let got = check.got.as_deref().map_or("missing".into(), line_of);
        lines.push(format!(
            "  {} = {got} (expected {})",
            line_of(&check.name),
            line_of(expected)
        ));
    }
    if let Some(error) = &result.error {
        lines.push(format!("  error: {}", line_of(error)));
    }
    if let Some(message) = &result.message {
        lines.push(format!("  {}", line_of(message)));
    }
    if !result.output.is_empty() {
        lines.push("  Output:".into());
        lines.extend(
            printable_lines(&result.output)
                .lines()
                .map(|l| format!("    {l}")),
        );
    }
    lines
}

pub(crate) fn cases_totals_line(results: &[CaseResult], env: Option<&EnvState>) -> String {
    let passed = results
        .iter()
        .filter(|r| r.status == CaseStatus::Passed)
        .count();
    let took: u64 = results.iter().map(|r| r.took_ms).sum();
    let mut line = format!(
        "{passed} passed, {} failed in {}",
        results.len() - passed,
        duration_text(took)
    );
    if let Some(last) = results.last() {
        line.push_str(&format!(" ({})", runner_text(last.runner, env)));
    }
    line
}

/// 0 when every case passed; 130 when any was stopped, else 124 when any timed out, else 1.
pub(crate) fn cases_exit_code(results: &[CaseResult]) -> u8 {
    let any = |status| results.iter().any(|r| r.status == status);
    if any(CaseStatus::Stopped) {
        130
    } else if any(CaseStatus::TimedOut) {
        124
    } else if results.iter().any(|r| r.status != CaseStatus::Passed) {
        1
    } else {
        0
    }
}

/// The function and case names a result shows, by the function's slug and the case's id; the
/// ids themselves when the view does not have them.
pub(crate) fn names_of(view: &FunctionsView, slug: &str, result: &CaseResult) -> (String, String) {
    let function = view.functions.iter().find(|f| f.slug == slug);
    let qualified = function.map_or_else(|| slug.to_string(), |f| f.qualified.clone());
    let name = function
        .and_then(|f| f.cases.iter().find(|c| c.id == result.case))
        .map_or_else(|| result.case.clone(), |c| c.name.clone());
    (qualified, name)
}

fn checked_note(view: &FunctionsView) -> Result<(), CliError> {
    match &view.note {
        Some(note) if note.starts_with(UNREADABLE) => Err(CliError::Other(line_of(note))),
        _ => Ok(()),
    }
}

/// The changed functions. The daemon never waits for the call index, so a view that is still
/// being read is waited for here, as long as `READ_WAIT`.
pub(crate) async fn read_functions(
    client: &mut Client,
    pr: &PrRef,
    stop: &mut Interrupts<'_>,
) -> Result<FunctionsView, CliError> {
    let view = match stop
        .ask(
            client,
            Request::GetFunctions { pr: pr.clone() },
            false,
            "reading the changed functions",
        )
        .await?
    {
        Reply::ChangedFunctions(view) => *view,
        other => return Err(unexpected(other)),
    };
    checked_note(&view)?;
    if !view.reading {
        return Ok(view);
    }
    let ready = |event: Event| match event {
        Event::FunctionsChanged { pr: of, view } if of == *pr && !view.reading => Some(*view),
        _ => None,
    };
    if let Some(view) = client.take_events().into_iter().find_map(|(_, e)| ready(e)) {
        checked_note(&view)?;
        return Ok(view);
    }
    let waited = tokio::time::timeout(READ_WAIT, async {
        loop {
            let event = stop.event(client, "reading the changed functions").await?;
            if let Some(view) = ready(event) {
                return Ok::<_, CliError>(view);
            }
        }
    })
    .await;
    match waited {
        Ok(view) => {
            let view = view?;
            checked_note(&view)?;
            Ok(view)
        }
        Err(_) => Err(CliError::Other(
            "The changed functions were not read within 60 s".into(),
        )),
    }
}

/// The scope a wish makes in `view`, and how many cases it runs; refuses what cannot run.
fn plan_run(
    view: &FunctionsView,
    function: Option<&str>,
    case: Option<&str>,
    pr: &PrRef,
) -> Result<(CaseScope, u32, Option<String>), CliError> {
    let picked = function
        .map(|w| {
            pick_function(view, w).map_err(|e| match e {
                CliError::Other(text) => CliError::Other(format!("{text} in {pr}")),
                other => other,
            })
        })
        .transpose()?;
    if let Some(reason) = picked.and_then(support_reason) {
        return Err(CliError::Other(reason));
    }
    let scope = scope_of(picked, case);
    seen_of(view, &scope)?;
    let total = runnable(view, &scope);
    if total == 0 {
        return Err(CliError::Other(NO_CASES.into()));
    }
    Ok((scope, total, picked.map(|f| f.slug.clone())))
}

/// Runs the cases `wanted` names and streams each result. Returns the results, the view as of
/// the run's end (no further read of the daemon), and the slug of the function each result belongs to (in result order).
pub(crate) async fn run_cases(
    client: &mut Client,
    pr: &PrRef,
    view: FunctionsView,
    wanted: &CasesWanted,
    json: bool,
    stop: &mut Interrupts<'_>,
) -> Result<(Vec<CaseResult>, FunctionsView, Vec<String>), CliError> {
    let mut view = view;
    let mut function = wanted.function.clone();
    let mut retried = false;
    let (id, total) = loop {
        let (scope, total, slug) =
            plan_run(&view, function.as_deref(), wanted.case.as_deref(), pr)?;
        let request = Request::RunCases {
            pr: pr.clone(),
            seen: seen_of(&view, &scope)?,
            scope,
        };
        match stop.ask(client, request, true, "the run").await {
            Ok(Reply::CasesStarted { run }) => break (run, total),
            Ok(other) => return Err(unexpected(other)),
            Err(CliError::Client(ClientError::Server(e)))
                if e.message == CASES_CHANGED && !retried =>
            {
                retried = true;
                view = read_functions(client, pr, stop).await?;
                // The slug is the function's identity; its name may be shared by another.
                function = slug;
            }
            Err(e) => return Err(e),
        }
    };
    let mut results: Vec<CaseResult> = Vec::new();
    let mut slugs: Vec<String> = Vec::new();
    let mut started = false;
    let mut latest: Option<FunctionsView> = None;
    follow(client, pr, stop, |event| match event {
        Event::CaseDone {
            pr: of,
            run,
            function,
            result,
        } if of == *pr && run == id => {
            started = true;
            if json {
                if let Ok(line) = serde_json::to_string(&*result) {
                    println!("{line}");
                }
            } else {
                let (qualified, name) = names_of(&view, &function, &result);
                println!("{}", case_line(&qualified, &name, &result));
            }
            results.push(*result);
            slugs.push(function);
            (u32::try_from(results.len()).unwrap_or(u32::MAX) >= total).then_some(())
        }
        Event::FunctionsChanged { pr: of, view } if of == *pr => {
            let end = match &view.activity {
                Some(FunctionsActivity::Running { run, .. }) if *run == id => {
                    started = true;
                    false
                }
                None => started,
                _ => false,
            };
            if !view.reading {
                latest = Some(*view);
            }
            end.then_some(())
        }
        _ => None,
    })
    .await?;
    // The last view told, with each result laid on its case: the run's end is told with the
    // view, and a run that ended by its count may end before that is told.
    let mut after = latest.unwrap_or(view);
    for (result, slug) in results.iter().zip(&slugs) {
        let case = after
            .functions
            .iter_mut()
            .filter(|f| f.slug == *slug)
            .flat_map(|f| f.cases.iter_mut())
            .find(|c| c.id == result.case);
        if let Some(case) = case {
            case.result = Some(result.clone());
        }
    }
    Ok((results, after, slugs))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use clusia_core::functions::{
        CASES_CHANGED, CaseOrigin, CaseResult, CaseScope, CaseStatus, CaseView, Expect,
        FunctionChange, FunctionLang, FunctionView, FunctionsView, StateCheck, Support,
    };
    use clusia_core::tests::RunnerKind;

    use super::*;

    const HEAD: &str = "9be1d07";

    fn result(case: &str, hash: &str, head: &str, status: CaseStatus, ms: u64) -> CaseResult {
        CaseResult {
            case: case.into(),
            head: head.into(),
            hash: hash.into(),
            status,
            returned: None,
            returned_expected: None,
            error: None,
            state: Vec::new(),
            output: String::new(),
            took_ms: ms,
            runner: RunnerKind::Container,
            exit_code: Some(0),
            message: None,
            at: 1_761_600_000,
        }
    }

    fn case(id: &str, name: &str, hash: &str, result: Option<CaseResult>) -> CaseView {
        CaseView {
            id: id.into(),
            name: name.into(),
            origin: CaseOrigin::Agent,
            edited: false,
            values: BTreeMap::new(),
            expect: Expect::default(),
            hash: hash.into(),
            result,
        }
    }

    fn function(
        slug: &str,
        qualified: &str,
        support: Support,
        cases: Vec<CaseView>,
    ) -> FunctionView {
        FunctionView {
            slug: slug.into(),
            qualified: qualified.into(),
            name: qualified.rsplit("::").next().unwrap().into(),
            file: "store/refresh.rs".into(),
            lang: Some(FunctionLang::Rust),
            change: FunctionChange::Changed,
            signature: "pub async fn refresh(&self)".into(),
            start_line: 40,
            diff_lines: None,
            params: Vec::new(),
            state_label: None,
            support,
            cases,
            driver: None,
            hash: format!("hash-of-{slug}"),
        }
    }

    fn view(functions: Vec<FunctionView>) -> FunctionsView {
        FunctionsView {
            functions,
            ..FunctionsView::new(HEAD)
        }
    }

    const REFRESH: &str = "tokenstore_refresh-0123456789";

    #[test]
    fn the_list_shows_current_results_only() {
        let mut v = view(vec![
            function(
                REFRESH,
                "TokenStore::refresh",
                Support::Ready,
                vec![
                    case(
                        "a1",
                        "Endpoint fails, old token kept",
                        "h1",
                        Some(result("a1", "h1", HEAD, CaseStatus::Failed, 210)),
                    ),
                    case(
                        "a2",
                        "Valid token, no exchange",
                        "h2",
                        Some(result("a2", "h2", HEAD, CaseStatus::Passed, 10)),
                    ),
                    case("a3", "Expired token, one exchange", "h3", None),
                    case(
                        "a4",
                        "Two callers share one exchange",
                        "h4",
                        Some(result("a4", "old", HEAD, CaseStatus::Passed, 5)),
                    ),
                    case(
                        "a5",
                        "Slow endpoint",
                        "h5",
                        Some(result("a5", "h5", "3a9c1e7", CaseStatus::Passed, 5)),
                    ),
                    case(
                        "a6",
                        "Crash",
                        "h6",
                        Some(result("a6", "h6", HEAD, CaseStatus::Error, 0)),
                    ),
                ],
            ),
            function(
                "config_token_path-abcdef0123",
                "Config::token_path",
                Support::Stale {
                    reason: "The signature changed since these cases were written".into(),
                },
                Vec::new(),
            ),
            function(
                "tokenstore_exchange-0011223344",
                "TokenStore::exchange",
                Support::NoCases,
                Vec::new(),
            ),
            function(
                "backoff_delay-5566778899",
                "backoff_delay",
                Support::NoCommand,
                Vec::new(),
            ),
        ]);
        v.functions[1].change = FunctionChange::Signature;
        v.functions[2].change = FunctionChange::New;
        v.functions[3].change = FunctionChange::Moved;
        v.unsupported = vec!["ui/badge.swift".into()];
        assert_eq!(
            function_lines(&v),
            [
                "TokenStore::refresh  store/refresh.rs · changed · 6 cases",
                "  FAIL  a1  Endpoint fails, old token kept  210 ms",
                "  PASS  a2  Valid token, no exchange  10 ms",
                "  ----  a3  Expired token, one exchange",
                "  ----  a4  Two callers share one exchange  (values changed since it ran)",
                "  ----  a5  Slow endpoint  (from an older commit)",
                "  ERR   a6  Crash  0 ms",
                "Config::token_path  store/refresh.rs · signature · no cases yet",
                "  Not run: The signature changed since these cases were written",
                "TokenStore::exchange  store/refresh.rs · new · no cases yet",
                "  Not run: No cases written yet; run with --generate",
                "backoff_delay  store/refresh.rs · moved · no cases yet",
                "  Not run: The recipe has no command to run Rust functions",
                "ui/badge.swift · not supported yet",
            ]
        );
    }

    #[test]
    fn hostile_text_in_the_list_is_filtered() {
        let mut f = function(
            REFRESH,
            "evil\u{1b}[2J\u{202e}fn",
            Support::Unsupported {
                reason: "Not public\u{1b}]8;;http://x\u{7}".into(),
            },
            vec![case("a1", "na\u{202e}me\nsecond", "h1", None)],
        );
        f.file = "a/b\u{1b}.rs".into();
        let lines = function_lines(&view(vec![f])).join("\n");
        for bad in ['\u{1b}', '\u{202e}', '\u{7}'] {
            assert!(!lines.contains(bad), "{lines:?}");
        }
        assert!(lines.contains("evil\\u{1b}[2J\\u{202e}fn"), "{lines}");
        assert!(lines.contains("na\\u{202e}me second"), "{lines}");
    }

    #[test]
    fn a_function_is_picked_by_slug_then_by_exact_name() {
        let v = view(vec![
            function(REFRESH, "TokenStore::refresh", Support::Ready, Vec::new()),
            function("alpha-0000000001", "dup", Support::Ready, Vec::new()),
            function("beta-0000000002", "dup", Support::Ready, Vec::new()),
        ]);
        assert_eq!(
            pick_function(&v, REFRESH).unwrap().qualified,
            "TokenStore::refresh"
        );
        assert_eq!(
            pick_function(&v, "TokenStore::refresh").unwrap().slug,
            REFRESH
        );
        let ambiguous = pick_function(&v, "dup").unwrap_err();
        assert_eq!(ambiguous.exit_code(), 2);
        let text = ambiguous.to_string();
        assert!(
            text.contains("alpha-0000000001") && text.contains("beta-0000000002"),
            "{text}"
        );
        assert!(
            pick_function(&v, "nope")
                .unwrap_err()
                .to_string()
                .contains("No changed function nope")
        );
    }

    #[test]
    fn the_hash_sent_is_the_one_of_the_scope() {
        let v = view(vec![function(
            REFRESH,
            "TokenStore::refresh",
            Support::Ready,
            vec![case("a1", "n", "h1", None), case("a2", "n", "h2", None)],
        )]);
        let f = &v.functions[0];
        let one = scope_of(Some(f), Some("a2"));
        assert_eq!(
            one,
            CaseScope::Case {
                function: REFRESH.into(),
                case: "a2".into()
            }
        );
        assert_eq!(
            seen_of(&v, &one).unwrap(),
            "hash-of-tokenstore_refresh-0123456789"
        );
        assert_eq!(seen_of(&v, &CaseScope::All).unwrap(), v.hash);
        assert_eq!(runnable(&v, &one), 1);
        assert_eq!(runnable(&v, &scope_of(Some(f), None)), 2);
        assert_eq!(runnable(&v, &CaseScope::All), 2);
        assert!(
            seen_of(&v, &scope_of(Some(f), Some("zz")))
                .unwrap_err()
                .to_string()
                .contains("No case zz in TokenStore::refresh")
        );
    }

    #[test]
    fn a_failure_shows_what_was_returned_the_state_the_error_and_the_output() {
        let mut r = result("a1", "h1", HEAD, CaseStatus::Failed, 210);
        r.returned = Some("Err(RefreshError::Http(500))".into());
        r.error = Some("boom\u{202e}".into());
        r.state = vec![
            StateCheck {
                name: "token".into(),
                got: Some("".into()),
                expected: Some("old".into()),
            },
            StateCheck {
                name: "calls".into(),
                got: Some("1".into()),
                expected: Some("1".into()),
            },
            StateCheck {
                name: "cache".into(),
                got: None,
                expected: Some("warm".into()),
            },
            StateCheck {
                name: "seen".into(),
                got: Some("x".into()),
                expected: None,
            },
        ];
        r.output = "WARN refresh failed: 500\n\u{1b}[31mstore cleared\u{1b}[0m\n".into();
        r.message = Some("The driver exited with 1".into());
        assert_eq!(
            case_failure_lines("TokenStore::refresh", "Endpoint fails", &r),
            [
                "FAIL  TokenStore::refresh  a1  Endpoint fails",
                "  returned: Err(RefreshError::Http(500))",
                "  token =  (expected old)",
                "  cache = missing (expected warm)",
                "  error: boom\\u{202e}",
                "  The driver exited with 1",
                "  Output:",
                "    WARN refresh failed: 500",
                "    \\u{1b}[31mstore cleared\\u{1b}[0m",
            ]
        );
        r.status = CaseStatus::TimedOut;
        assert!(case_failure_lines("f", "n", &r)[0].starts_with("TIME  f  a1  n"));
    }

    #[test]
    fn the_totals_count_every_non_pass_as_failed_and_the_exit_code_ranks_stop_first() {
        let r = |status, ms| result("a1", "h", HEAD, status, ms);
        let mixed = [
            r(CaseStatus::Passed, 1_000),
            r(CaseStatus::Failed, 2_200),
            r(CaseStatus::Error, 1_000),
        ];
        assert_eq!(
            cases_totals_line(&mixed, None),
            "1 passed, 2 failed in 4.2 s (container)"
        );
        assert_eq!(cases_exit_code(&[r(CaseStatus::Passed, 1)]), 0);
        assert_eq!(cases_exit_code(&mixed), 1);
        assert_eq!(
            cases_exit_code(&[r(CaseStatus::Failed, 1), r(CaseStatus::TimedOut, 1)]),
            124
        );
        assert_eq!(
            cases_exit_code(&[r(CaseStatus::TimedOut, 1), r(CaseStatus::Stopped, 1)]),
            130
        );
        assert_eq!(
            CASES_CHANGED,
            "The cases changed since you saw them; look at them again"
        );
    }
}

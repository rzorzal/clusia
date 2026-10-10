//! `clusia check`: runs the security and audit checks of a review and prints what they found.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

use clusia_core::checks::{AreaStatus, CheckKind, CheckResult, Finding, Severity, area_status};
use clusia_core::{Paths, PrRef};
use clusia_protocol::{CheckState, Client, Command as Request, Event, Reply, TurnOrigin, topics};
use serde_json::json;
use tokio::signal::unix::{SignalKind, signal};

use crate::agent::printable;
use crate::review::parse_pr;
use crate::run::{CliError, Output, connect};

/// The kinds a command asks for: one flag picks its kind, none (or both) asks for both.
pub(crate) fn kinds(security: bool, audit: bool) -> Vec<CheckKind> {
    match (security, audit) {
        (true, false) => vec![CheckKind::Security],
        (false, true) => vec![CheckKind::Audit],
        _ => vec![CheckKind::Security, CheckKind::Audit],
    }
}

/// The text on one line: line breaks and runs of spaces become one space, and every other
/// control or invisible character is shown as an escape, so nothing repaints the terminal.
fn line_of(text: &str) -> String {
    printable(&text.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// What a state says as a line of progress; `None` for the ones that say nothing new.
pub(crate) fn progress_line(kind: CheckKind, state: &CheckState) -> Option<String> {
    let said = match state {
        CheckState::Waiting => "waiting for another check".to_string(),
        CheckState::Running { activity: None } => "running".to_string(),
        CheckState::Running {
            activity: Some(activity),
        } => line_of(activity),
        CheckState::Done => "done".to_string(),
        CheckState::Failed { message } => format!("failed: {}", line_of(message)),
        CheckState::NotRun | CheckState::Stale => return None,
    };
    Some(format!("{}: {said}", kind.label()))
}

fn severity_word(severity: Severity) -> &'static str {
    match severity {
        Severity::High => "HIGH",
        Severity::Medium => "MEDIUM",
        Severity::Low => "LOW",
    }
}

/// `HIGH  file:line  title` for a security finding, `file:line  title` for an audit one;
/// `file:3-5` for a range and `file` for none. The text comes from the agent, so it is filtered.
pub(crate) fn finding_line(finding: &Finding) -> String {
    let file = printable(&finding.file);
    let place = match (finding.line, finding.start_line, finding.end_line) {
        (Some(line), _, _) => format!("{file}:{line}"),
        (None, Some(from), Some(to)) => format!("{file}:{from}-{to}"),
        _ => file,
    };
    let title = line_of(&finding.title);
    match finding.severity {
        Some(severity) => format!("{}  {place}  {title}", severity_word(severity)),
        None => format!("{place}  {title}"),
    }
}

fn pass_line(text: &str, place: Option<&str>) -> String {
    match place {
        Some(place) => format!("ok  {}  ({})", line_of(text), line_of(place)),
        None => format!("ok  {}", line_of(text)),
    }
}

fn unreadable_line(count: u32) -> Option<String> {
    match count {
        0 => None,
        1 => Some("1 result could not be read".to_string()),
        n => Some(format!("{n} results could not be read")),
    }
}

/// The findings of `area` that are still open, the most severe first.
fn open_findings<'a>(
    result: &'a CheckResult,
    area: &str,
    hidden: &BTreeSet<String>,
) -> Vec<&'a Finding> {
    let mut found: Vec<&Finding> = result
        .findings
        .iter()
        .filter(|f| f.area == area && !hidden.contains(&f.id))
        .collect();
    found.sort_by_key(|f| f.severity);
    found
}

fn passes_of<'a>(result: &'a CheckResult, area: &str) -> impl Iterator<Item = String> + 'a {
    let area = area.to_string();
    result
        .passes
        .iter()
        .filter(move |p| p.area == area)
        .map(|p| pass_line(&p.text, p.place.as_deref()))
}

/// What one kind's result says. The findings in `hidden` (accepted or dismissed) are left out:
/// the findings grouped by area, the passes, and what could not be read. A clean security
/// result says what the agent did, never that the change is safe.
pub(crate) fn report_lines(
    result: &CheckResult,
    hidden: &BTreeSet<String>,
    area_names: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut lines = Vec::new();
    match result.kind {
        CheckKind::Security => {
            lines.push("Security".to_string());
            let open = open_findings(result, "security", hidden);
            if open.is_empty() {
                let unit = if result.files == 1 { "file" } else { "files" };
                lines.push(format!(
                    "Claude Code found no security issues in {} {unit}.",
                    result.files
                ));
            }
            lines.extend(open.into_iter().map(finding_line));
            lines.extend(passes_of(result, "security"));
        }
        CheckKind::Audit => {
            for area in &result.areas {
                let shown = area_names.get(area).unwrap_or(area);
                lines.push(format!("Audit · {}", line_of(shown)));
                match area_status(result, area, hidden) {
                    AreaStatus::Findings(_) => {
                        lines.extend(
                            open_findings(result, area, hidden)
                                .into_iter()
                                .map(finding_line),
                        );
                    }
                    AreaStatus::Ok => lines.push("No findings in this area.".to_string()),
                    AreaStatus::NotChecked => lines.push("Not checked".to_string()),
                }
                lines.extend(passes_of(result, area));
            }
        }
    }
    lines.extend(unreadable_line(result.unreadable));
    lines
}

fn io_error(e: io::Error) -> CliError {
    CliError::Other(e.to_string())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Asks the daemon to stop what this command started.
async fn stop_started(client: &mut Client, pr: &PrRef, started: &[CheckKind]) {
    for &kind in started {
        let _ = client
            .request(Request::StopCheck {
                pr: pr.clone(),
                kind,
            })
            .await;
    }
}

pub(crate) async fn check(
    paths: &Paths,
    home: Option<&Path>,
    pr: &str,
    security: bool,
    audit: bool,
    json: bool,
) -> Result<Output, CliError> {
    let pr = parse_pr(pr)?;
    let wanted = kinds(security, audit);
    // Caught from here on: a Ctrl-C before the first poll of a handler would end the process
    // without a word.
    let mut interrupt = signal(SignalKind::interrupt()).map_err(io_error)?;
    let mut client = connect(paths, home).await?;
    let opened = async {
        client
            .request(Request::Subscribe {
                topics: vec![topics::AGENT.into()],
            })
            .await?;
        // The checks read the review's worktree, which opening prepares.
        client.request(Request::OpenReview { pr: pr.clone() }).await
    };
    tokio::select! {
        opened = opened => { opened?; }
        _ = interrupt.recv() => {
            eprintln!("Interrupted: nothing was checked.");
            std::process::exit(130);
        }
    }

    let began = now();
    let Reply::Checks { states, .. } = client
        .request(Request::GetChecks { pr: pr.clone() })
        .await?
    else {
        return Err(CliError::Other(
            "the daemon did not answer GetChecks".into(),
        ));
    };
    let mut going: Vec<CheckKind> = Vec::new();
    let mut started: Vec<CheckKind> = Vec::new();
    for &kind in &wanted {
        let state = states.iter().find(|s| s.kind == kind).map(|s| &s.state);
        match state {
            // Opening the review may have started it already: follow that run.
            Some(state @ (CheckState::Waiting | CheckState::Running { .. })) => {
                if !json && let Some(line) = progress_line(kind, state) {
                    eprintln!("{line}");
                }
            }
            _ => match client
                .request(Request::RunCheck {
                    pr: pr.clone(),
                    kind,
                })
                .await
            {
                Ok(_) => started.push(kind),
                Err(e) => {
                    stop_started(&mut client, &pr, &started).await;
                    return Err(e.into());
                }
            },
        }
        going.push(kind);
    }

    let mut failures: Vec<String> = Vec::new();
    let mut asked: BTreeSet<String> = BTreeSet::new();
    let mut finished: Vec<CheckKind> = Vec::new();
    while !going.is_empty() {
        let (_, event) = tokio::select! {
            event = client.next_event() => event?,
            _ = interrupt.recv() => {
                stop_started(&mut client, &pr, &going).await;
                eprintln!("Stopped the checks.");
                std::process::exit(130);
            }
        };
        match event {
            Event::CheckState {
                pr: of,
                kind,
                state,
            } if of == pr && going.contains(&kind) => {
                if !json && let Some(line) = progress_line(kind, &state) {
                    eprintln!("{line}");
                }
                match state {
                    CheckState::Done | CheckState::Stale => {
                        going.retain(|k| *k != kind);
                        finished.push(kind);
                    }
                    CheckState::Failed { message } => {
                        going.retain(|k| *k != kind);
                        failures.push(format!(
                            "{} check failed: {}",
                            kind.label(),
                            line_of(&message)
                        ));
                    }
                    CheckState::NotRun => {
                        going.retain(|k| *k != kind);
                        failures.push(format!("{} check was stopped", kind.label()));
                    }
                    CheckState::Waiting | CheckState::Running { .. } => {}
                }
            }
            Event::PermissionRequested {
                pr: of,
                id,
                tool,
                summary,
                origin,
                ..
            } if of == pr && !origin.is_chat() => {
                if asked.insert(id) && !json {
                    let who = match origin {
                        TurnOrigin::Audit => CheckKind::Audit.label(),
                        _ => CheckKind::Security.label(),
                    };
                    eprintln!(
                        "{who}: waiting for an answer in the window: Claude Code wants to use {}: {}",
                        line_of(&tool),
                        line_of(&summary)
                    );
                }
            }
            _ => {}
        }
    }

    let Reply::Checks {
        results,
        dismissed,
        accepted,
        ..
    } = client
        .request(Request::GetChecks { pr: pr.clone() })
        .await?
    else {
        return Err(CliError::Other(
            "the daemon did not answer GetChecks".into(),
        ));
    };
    let area_names: BTreeMap<String, String> = match client.request(Request::GetConfig).await {
        Ok(Reply::Config(config)) => config
            .harness
            .audit_areas
            .iter()
            .map(|area| (area.id.clone(), area.name.clone()))
            .collect(),
        _ => BTreeMap::new(),
    };
    // A finding that is in the draft or was dismissed is not open: the report leaves it out.
    let hidden: BTreeSet<String> = dismissed.iter().chain(&accepted).cloned().collect();
    let mut sections: Vec<Vec<String>> = Vec::new();
    let mut shown: Vec<&CheckResult> = Vec::new();
    for &kind in &wanted {
        if !finished.contains(&kind) {
            continue;
        }
        // A result older than this command is what an earlier run left: this run was stopped.
        match results.iter().find(|r| r.kind == kind && r.at >= began) {
            Some(result) => {
                sections.push(report_lines(result, &hidden, &area_names));
                shown.push(result);
            }
            None => failures.push(format!("{} check was stopped", kind.label())),
        }
    }
    let human = sections
        .iter()
        .map(|lines| lines.join("\n"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let output = Output {
        human,
        json: json!({ "results": shown, "dismissed": dismissed, "accepted": accepted }),
    };
    if failures.is_empty() {
        return Ok(output);
    }
    // What the finished kinds found is printed before the failure is reported.
    if !shown.is_empty() {
        if json {
            println!("{}", output.json);
        } else {
            println!("{}", crate::agent::streamed(&output.human));
        }
    }
    Err(CliError::Other(failures.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::checks::Pass;

    fn finding(
        area: &str,
        severity: Option<Severity>,
        title: &str,
        line: Option<u32>,
        range: Option<(u32, u32)>,
    ) -> Finding {
        Finding {
            id: format!("id-{area}-{title}"),
            kind: if area == "security" {
                CheckKind::Security
            } else {
                CheckKind::Audit
            },
            area: area.into(),
            severity,
            title: title.into(),
            file: "client/http.rs".into(),
            line,
            start_line: range.map(|r| r.0),
            end_line: range.map(|r| r.1),
            body: "b".into(),
            comment: "c".into(),
            code: None,
            anchored: true,
        }
    }

    fn result(kind: CheckKind, findings: Vec<Finding>, passes: Vec<Pass>) -> CheckResult {
        CheckResult {
            kind,
            head: "h".into(),
            files: 7,
            findings,
            passes,
            unreadable: 0,
            areas: Vec::new(),
            at: 1,
        }
    }

    fn pass(area: &str, text: &str, place: Option<&str>) -> Pass {
        Pass {
            area: area.into(),
            text: text.into(),
            place: place.map(str::to_string),
        }
    }

    fn none() -> BTreeSet<String> {
        BTreeSet::new()
    }

    #[test]
    fn a_flag_picks_its_kind_and_none_asks_for_both() {
        assert_eq!(kinds(true, false), [CheckKind::Security]);
        assert_eq!(kinds(false, true), [CheckKind::Audit]);
        assert_eq!(kinds(false, false), [CheckKind::Security, CheckKind::Audit]);
    }

    #[test]
    fn progress_says_what_the_check_does() {
        let line = |kind, state: CheckState| progress_line(kind, &state);
        assert_eq!(
            line(CheckKind::Security, CheckState::Waiting).as_deref(),
            Some("Security: waiting for another check")
        );
        assert_eq!(
            line(CheckKind::Audit, CheckState::Running { activity: None }).as_deref(),
            Some("Audit: running")
        );
        assert_eq!(
            line(
                CheckKind::Security,
                CheckState::Running {
                    activity: Some("Reading src/auth/store.rs…".into())
                }
            )
            .as_deref(),
            Some("Security: Reading src/auth/store.rs…")
        );
        assert_eq!(
            line(CheckKind::Audit, CheckState::Done).as_deref(),
            Some("Audit: done")
        );
        assert_eq!(line(CheckKind::Audit, CheckState::Stale), None);
        assert_eq!(line(CheckKind::Audit, CheckState::NotRun), None);
    }

    #[test]
    fn what_the_agent_wrote_cannot_repaint_the_terminal() {
        let state = CheckState::Running {
            activity: Some("Reading\u{1b}[2K\u{202e} a\nfile".into()),
        };
        let said = progress_line(CheckKind::Security, &state).unwrap();
        assert!(
            !said.contains('\u{1b}') && !said.contains('\u{202e}'),
            "{said}"
        );
        assert!(!said.contains('\n'), "{said}");
        let line = finding_line(&finding(
            "security",
            Some(Severity::High),
            "Bad\u{1b}[2K\ntitle",
            Some(5),
            None,
        ));
        assert!(!line.contains('\u{1b}') && !line.contains('\n'), "{line}");
    }

    #[test]
    fn a_finding_line_names_severity_place_and_title() {
        let high = finding(
            "security",
            Some(Severity::High),
            "Refresh token written to the debug log",
            Some(52),
            None,
        );
        assert_eq!(
            finding_line(&high),
            "HIGH  client/http.rs:52  Refresh token written to the debug log"
        );
        let range = finding(
            "security",
            Some(Severity::Medium),
            "Open range",
            None,
            Some((3, 5)),
        );
        assert_eq!(
            finding_line(&range),
            "MEDIUM  client/http.rs:3-5  Open range"
        );
        let audit = finding("correctness", None, "Off by one", None, None);
        assert_eq!(finding_line(&audit), "client/http.rs  Off by one");
    }

    #[test]
    fn a_clean_security_result_says_what_the_agent_found_never_that_it_is_safe() {
        let clean = result(CheckKind::Security, Vec::new(), Vec::new());
        assert_eq!(
            report_lines(&clean, &none(), &BTreeMap::new()),
            [
                "Security",
                "Claude Code found no security issues in 7 files."
            ]
        );
        let mut one = result(CheckKind::Security, Vec::new(), Vec::new());
        one.files = 1;
        assert_eq!(
            report_lines(&one, &none(), &BTreeMap::new())[1],
            "Claude Code found no security issues in 1 file."
        );
    }

    #[test]
    fn security_findings_come_most_severe_first_without_the_dismissed_ones() {
        let mut r = result(
            CheckKind::Security,
            vec![
                finding(
                    "security",
                    Some(Severity::Low),
                    "Loose parse",
                    Some(9),
                    None,
                ),
                finding(
                    "security",
                    Some(Severity::High),
                    "Token in the log",
                    Some(2),
                    None,
                ),
                finding(
                    "security",
                    Some(Severity::Medium),
                    "Dismissed",
                    Some(4),
                    None,
                ),
            ],
            vec![pass(
                "security",
                "No secrets in the diff",
                Some("checked 1 file"),
            )],
        );
        r.unreadable = 2;
        let dismissed: BTreeSet<String> = ["id-security-Dismissed".to_string()].into();
        assert_eq!(
            report_lines(&r, &dismissed, &BTreeMap::new()),
            [
                "Security",
                "HIGH  client/http.rs:2  Token in the log",
                "LOW  client/http.rs:9  Loose parse",
                "ok  No secrets in the diff  (checked 1 file)",
                "2 results could not be read",
            ]
        );
        r.unreadable = 1;
        assert_eq!(
            report_lines(&r, &none(), &BTreeMap::new()).last().unwrap(),
            "1 result could not be read"
        );
    }

    #[test]
    fn a_finding_already_in_the_draft_is_left_out() {
        let r = result(
            CheckKind::Security,
            vec![
                finding("security", Some(Severity::High), "Accepted", Some(2), None),
                finding("security", Some(Severity::Low), "Open", Some(9), None),
            ],
            Vec::new(),
        );
        let hidden: BTreeSet<String> = ["id-security-Accepted".to_string()].into();
        assert_eq!(
            report_lines(&r, &hidden, &BTreeMap::new()),
            ["Security", "LOW  client/http.rs:9  Open"]
        );
    }

    #[test]
    fn an_audit_groups_by_area_and_never_calls_an_unchecked_area_ok() {
        let mut r = result(
            CheckKind::Audit,
            vec![finding("correctness", None, "Off by one", Some(3), None)],
            vec![pass("concurrency", "No shared state", None)],
        );
        r.areas = vec!["correctness".into(), "concurrency".into(), "docs".into()];
        let names: BTreeMap<String, String> =
            [("correctness".to_string(), "Correctness".to_string())].into();
        assert_eq!(
            report_lines(&r, &none(), &names),
            [
                "Audit · Correctness",
                "client/http.rs:3  Off by one",
                "Audit · concurrency",
                "No findings in this area.",
                "ok  No shared state",
                "Audit · docs",
                "Not checked",
            ]
        );
        let dismissed: BTreeSet<String> = ["id-correctness-Off by one".to_string()].into();
        let after = report_lines(&r, &dismissed, &names);
        assert_eq!(
            after[..2],
            ["Audit · Correctness", "No findings in this area."],
            "the agent did look at the area; only its finding is gone"
        );
    }
}

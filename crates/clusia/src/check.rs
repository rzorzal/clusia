//! `clusia check`: runs the security and audit checks of a review and prints what they found.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::time::Duration;

use clusia_core::checks::{AreaStatus, CheckKind, CheckResult, Finding, Severity, area_status};
use clusia_core::{Paths, PrRef};
use clusia_protocol::{CheckState, Client, Command as Request, Event, Reply, TurnOrigin, topics};
use serde_json::json;
use tokio::signal::unix::{Signal, SignalKind, signal};

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

/// The findings of `area` that were not dismissed, the most severe first; one already in the
/// draft says so.
fn area_findings(
    result: &CheckResult,
    area: &str,
    accepted: &BTreeSet<String>,
    dismissed: &BTreeSet<String>,
) -> Vec<String> {
    let mut found: Vec<&Finding> = result
        .findings
        .iter()
        .filter(|f| f.area == area && !dismissed.contains(&f.id))
        .collect();
    found.sort_by_key(|f| f.severity);
    found
        .into_iter()
        .map(|f| {
            if accepted.contains(&f.id) {
                format!("{}  (in your draft)", finding_line(f))
            } else {
                finding_line(f)
            }
        })
        .collect()
}

fn passes_of<'a>(result: &'a CheckResult, area: &str) -> impl Iterator<Item = String> + 'a {
    let area = area.to_string();
    result
        .passes
        .iter()
        .filter(move |p| p.area == area)
        .map(|p| pass_line(&p.text, p.place.as_deref()))
}

fn files_of(count: u32) -> String {
    let unit = if count == 1 { "file" } else { "files" };
    format!("{count} {unit}")
}

/// What one kind's result says: the findings grouped by area (an accepted one marked as in the
/// draft, a dismissed one left out and counted at the end), the passes, and what could not be
/// read. The clean sentence is a claim about what the agent found, so it is said only when the
/// result itself has no finding and no unreadable block; it never says the change is safe.
pub(crate) fn report_lines(
    result: &CheckResult,
    accepted: &BTreeSet<String>,
    dismissed: &BTreeSet<String>,
    area_names: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut unreadable_said = false;
    match result.kind {
        CheckKind::Security => {
            lines.push("Security".to_string());
            if result.findings.is_empty() && result.unreadable == 0 {
                lines.push(format!(
                    "Claude Code found no security issues in {}.",
                    files_of(result.files)
                ));
            } else if result.findings.is_empty() {
                lines.push(format!(
                    "Claude Code checked {}; {}.",
                    files_of(result.files),
                    unreadable_line(result.unreadable).unwrap_or_default()
                ));
                unreadable_said = true;
            }
            lines.extend(area_findings(result, "security", accepted, dismissed));
            lines.extend(passes_of(result, "security"));
        }
        CheckKind::Audit => {
            for area in &result.areas {
                let shown = area_names.get(area).unwrap_or(area);
                lines.push(format!("Audit · {}", line_of(shown)));
                match area_status(result, area, dismissed) {
                    AreaStatus::Findings(_) => {
                        lines.extend(area_findings(result, area, accepted, dismissed));
                    }
                    AreaStatus::Ok => lines.push("No findings in this area.".to_string()),
                    AreaStatus::NotChecked => lines.push("Not checked".to_string()),
                }
                lines.extend(passes_of(result, area));
            }
        }
    }
    if !unreadable_said {
        lines.extend(unreadable_line(result.unreadable));
    }
    let gone = result
        .findings
        .iter()
        .filter(|f| dismissed.contains(&f.id))
        .count();
    if gone > 0 {
        lines.push(format!("{gone} dismissed"));
    }
    lines
}

fn io_error(e: io::Error) -> CliError {
    CliError::Other(e.to_string())
}

/// How long the command waits for the daemon once the checks are over or stopped.
const ANSWER_WAIT: Duration = Duration::from_secs(5);

/// Asks the daemon to stop the runs this command follows; whether every stop went through.
async fn stop_started(client: &mut Client, pr: &PrRef, going: &[CheckKind]) -> bool {
    let mut through = true;
    for &kind in going {
        through &= client
            .request(Request::StopCheck {
                pr: pr.clone(),
                kind,
            })
            .await
            .is_ok();
    }
    through
}

/// `stop_started`, given up on at a second Ctrl-C or after `ANSWER_WAIT`, so a daemon that does
/// not answer cannot hold the terminal.
async fn stop_or_give_up(
    client: &mut Client,
    pr: &PrRef,
    going: &[CheckKind],
    interrupt: &mut Signal,
) -> bool {
    tokio::select! {
        through = stop_started(client, pr, going) => through,
        _ = interrupt.recv() => false,
        _ = tokio::time::sleep(ANSWER_WAIT) => false,
    }
}

/// What `asked` gives, or `None` when the daemon takes longer than `ANSWER_WAIT`. A Ctrl-C ends
/// the command.
async fn answered<T>(asked: impl Future<Output = T>, interrupt: &mut Signal) -> Option<T> {
    tokio::select! {
        answer = asked => Some(answer),
        _ = interrupt.recv() => {
            eprintln!("Interrupted.");
            std::process::exit(130);
        }
        _ = tokio::time::sleep(ANSWER_WAIT) => None,
    }
}

/// The check a permission request comes from; `None` for the chat's.
fn kind_of(origin: TurnOrigin) -> Option<CheckKind> {
    match origin {
        TurnOrigin::Chat => None,
        TurnOrigin::Security => Some(CheckKind::Security),
        TurnOrigin::Audit => Some(CheckKind::Audit),
    }
}

/// The note for a permission request of a check this command follows; `None` for any other.
fn permission_note(
    origin: TurnOrigin,
    going: &[CheckKind],
    tool: &str,
    summary: &str,
) -> Option<String> {
    let kind = kind_of(origin).filter(|kind| going.contains(kind))?;
    Some(format!(
        "{}: waiting for an answer in the window: Claude Code wants to use {}: {}",
        kind.label(),
        line_of(tool),
        line_of(summary)
    ))
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

    let Reply::Checks {
        states,
        results: before,
        ..
    } = client
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
    // A kind this command started ends only after its run has been seen waiting or running:
    // an ending told before that (a Stale from the open, a queued Done) is an older run's.
    let mut begun: Vec<CheckKind> = Vec::new();
    while !going.is_empty() {
        let (_, event) = tokio::select! {
            event = client.next_event() => event?,
            _ = interrupt.recv() => {
                if stop_or_give_up(&mut client, &pr, &going, &mut interrupt).await {
                    eprintln!("Stopped the checks.");
                } else {
                    eprintln!(
                        "The daemon did not answer; the checks may still run (Stop in the window)."
                    );
                }
                std::process::exit(130);
            }
        };
        match event {
            Event::CheckState {
                pr: of,
                kind,
                state,
            } if of == pr && going.contains(&kind) => {
                let ending = !matches!(state, CheckState::Waiting | CheckState::Running { .. });
                if !ending && !begun.contains(&kind) {
                    begun.push(kind);
                }
                if ending && started.contains(&kind) && !begun.contains(&kind) {
                    continue;
                }
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
            } if of == pr => {
                if let Some(note) = permission_note(origin, &going, &tool, &summary)
                    && asked.insert(id)
                    && !json
                {
                    eprintln!("{note}");
                }
            }
            _ => {}
        }
    }

    let no_answer = || CliError::Other("the daemon did not answer GetChecks".into());
    let asked = client.request(Request::GetChecks { pr: pr.clone() });
    let Reply::Checks {
        results,
        dismissed,
        accepted,
        ..
    } = answered(asked, &mut interrupt)
        .await
        .ok_or_else(no_answer)??
    else {
        return Err(no_answer());
    };
    let config = answered(client.request(Request::GetConfig), &mut interrupt).await;
    let area_names: BTreeMap<String, String> = match config {
        Some(Ok(Reply::Config(config))) => config
            .harness
            .audit_areas
            .iter()
            .map(|area| (area.id.clone(), area.name.clone()))
            .collect(),
        _ => BTreeMap::new(),
    };
    let accepted_ids: BTreeSet<String> = accepted.iter().cloned().collect();
    let dismissed_ids: BTreeSet<String> = dismissed.iter().cloned().collect();
    let mut sections: Vec<Vec<String>> = Vec::new();
    let mut shown: Vec<&CheckResult> = Vec::new();
    for &kind in &wanted {
        if !finished.contains(&kind) {
            continue;
        }
        // The result that was there before this command is what an earlier run left: this run
        // was stopped.
        match results
            .iter()
            .find(|r| r.kind == kind && !before.contains(r))
        {
            Some(result) => {
                sections.push(report_lines(
                    result,
                    &accepted_ids,
                    &dismissed_ids,
                    &area_names,
                ));
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
    fn a_permission_note_names_only_a_check_this_command_follows() {
        let audit_only = [CheckKind::Audit];
        assert_eq!(
            permission_note(TurnOrigin::Security, &audit_only, "Bash", "make"),
            None,
            "a Security request is not news under --audit"
        );
        assert_eq!(
            permission_note(TurnOrigin::Chat, &audit_only, "Bash", "make"),
            None
        );
        assert_eq!(
            permission_note(TurnOrigin::Audit, &audit_only, "Bash", "make\u{1b}[2K").as_deref(),
            Some(
                "Audit: waiting for an answer in the window: Claude Code wants to use Bash: make\\u{1b}[2K"
            )
        );
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
            report_lines(&clean, &none(), &none(), &BTreeMap::new()),
            [
                "Security",
                "Claude Code found no security issues in 7 files."
            ]
        );
        let mut one = result(CheckKind::Security, Vec::new(), Vec::new());
        one.files = 1;
        assert_eq!(
            report_lines(&one, &none(), &none(), &BTreeMap::new())[1],
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
            report_lines(&r, &none(), &dismissed, &BTreeMap::new()),
            [
                "Security",
                "HIGH  client/http.rs:2  Token in the log",
                "LOW  client/http.rs:9  Loose parse",
                "ok  No secrets in the diff  (checked 1 file)",
                "2 results could not be read",
                "1 dismissed",
            ]
        );
        r.unreadable = 1;
        assert_eq!(
            report_lines(&r, &none(), &none(), &BTreeMap::new())
                .last()
                .unwrap(),
            "1 result could not be read"
        );
    }

    const CLEAN: &str = "Claude Code found no security issues in 7 files.";

    fn two_security_findings() -> CheckResult {
        result(
            CheckKind::Security,
            vec![
                finding("security", Some(Severity::High), "Accepted", Some(2), None),
                finding("security", Some(Severity::Low), "Open", Some(9), None),
            ],
            Vec::new(),
        )
    }

    fn ids(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn a_finding_already_in_the_draft_says_so() {
        let accepted = ids(&["id-security-Accepted"]);
        assert_eq!(
            report_lines(
                &two_security_findings(),
                &accepted,
                &none(),
                &BTreeMap::new()
            ),
            [
                "Security",
                "HIGH  client/http.rs:2  Accepted  (in your draft)",
                "LOW  client/http.rs:9  Open",
            ]
        );
    }

    #[test]
    fn every_finding_accepted_is_not_a_clean_result() {
        let accepted = ids(&["id-security-Accepted", "id-security-Open"]);
        let lines = report_lines(
            &two_security_findings(),
            &accepted,
            &none(),
            &BTreeMap::new(),
        );
        assert!(!lines.iter().any(|l| l == CLEAN), "{lines:?}");
        assert_eq!(
            lines,
            [
                "Security",
                "HIGH  client/http.rs:2  Accepted  (in your draft)",
                "LOW  client/http.rs:9  Open  (in your draft)",
            ]
        );
    }

    #[test]
    fn every_finding_dismissed_is_not_a_clean_result() {
        let dismissed = ids(&["id-security-Accepted", "id-security-Open"]);
        let lines = report_lines(
            &two_security_findings(),
            &none(),
            &dismissed,
            &BTreeMap::new(),
        );
        assert!(!lines.iter().any(|l| l == CLEAN), "{lines:?}");
        assert_eq!(lines, ["Security", "2 dismissed"]);
    }

    #[test]
    fn only_unreadable_blocks_is_not_a_clean_result() {
        let mut r = result(CheckKind::Security, Vec::new(), Vec::new());
        r.unreadable = 3;
        let lines = report_lines(&r, &none(), &none(), &BTreeMap::new());
        assert!(!lines.iter().any(|l| l == CLEAN), "{lines:?}");
        assert_eq!(
            lines,
            [
                "Security",
                "Claude Code checked 7 files; 3 results could not be read."
            ]
        );
    }

    #[test]
    fn an_audit_finding_in_the_draft_keeps_its_area_from_reading_clean() {
        let mut r = result(
            CheckKind::Audit,
            vec![finding("correctness", None, "Off by one", Some(3), None)],
            Vec::new(),
        );
        r.areas = vec!["correctness".into()];
        let accepted = ids(&["id-correctness-Off by one"]);
        assert_eq!(
            report_lines(&r, &accepted, &none(), &BTreeMap::new()),
            [
                "Audit · correctness",
                "client/http.rs:3  Off by one  (in your draft)"
            ]
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
            report_lines(&r, &none(), &none(), &names),
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
        let after = report_lines(&r, &none(), &dismissed, &names);
        assert_eq!(
            after[..2],
            ["Audit · Correctness", "No findings in this area."],
            "the agent did look at the area; only its finding is gone"
        );
        assert_eq!(after.last().unwrap(), "1 dismissed");
    }
}

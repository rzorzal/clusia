//! Case runs: each case of the chosen functions runs its driver once, in the review's runner
//! with no network, its values only in the environment, and its report read back as the case's
//! result.

use std::sync::Arc;
use std::time::{Duration, Instant};

use clusia_core::PrRef;
use clusia_core::functions::{
    CASE_TIMEOUT_SECS, CASES_CHANGED, CaseRef, CaseResult, CaseScope, CaseStatus, CaseView,
    FunctionLang, FunctionsActivity, NO_CASES, StateCheck, Support, valid_slug,
};
use clusia_core::tests::{RunnerKind, TAIL_MAX, TestScope, TestsPhase};
use clusia_protocol::{Event, Outcome, Reply, topics};
use clusia_testrun::exec::{Ended, StopFlag, Stream};
use clusia_testrun::functions::{
    DriverReport, case_env, case_status, driver_target, function_command, place_driver,
    read_report, remove_driver,
};
use clusia_testrun::log::Tail;
use clusia_testrun::names::sha256_hex;

use super::work::{self, RunJob};
use super::{Doing, cases, env};
use crate::reviews;
use crate::state::Shared;
use crate::sync::now_unix;

/// The most of the report lines a case keeps.
const REPORT_MAX: usize = 256 * 1024;
/// A line longer than this goes to the output as it is, unread.
const LINE_MAX: usize = 64 * 1024;

/// A run's stdout and stderr as they arrive: the report lines apart, the rest as the output's
/// tail. Memory stays bounded however much the driver prints.
pub(super) struct CaseSink {
    prefix: Vec<u8>,
    /// The unfinished line of stdout and of stderr.
    lines: [Vec<u8>; 2],
    /// Whether the unfinished line of each stream already went to the tail for its length; the
    /// rest of it follows it there.
    overflowed: [bool; 2],
    report: Vec<u8>,
    tail: Tail,
}

impl CaseSink {
    pub(super) fn new(mark: &str) -> Self {
        Self {
            prefix: format!("{mark} ").into_bytes(),
            lines: [Vec::new(), Vec::new()],
            overflowed: [false, false],
            report: Vec::new(),
            tail: Tail::new(TAIL_MAX),
        }
    }

    pub(super) fn chunk(&mut self, stream: Stream, bytes: &[u8]) {
        let at = match stream {
            Stream::Out => 0,
            Stream::Err => 1,
        };
        let mut rest = bytes;
        while !rest.is_empty() {
            let (piece, ended) = match rest.iter().position(|&b| b == b'\n') {
                Some(end) => (&rest[..=end], true),
                None => (rest, false),
            };
            rest = &rest[piece.len()..];
            if self.overflowed[at] {
                self.tail.push(piece);
            } else {
                self.lines[at].extend_from_slice(piece);
                if ended {
                    let line = std::mem::take(&mut self.lines[at]);
                    self.line(stream, &line);
                } else if self.lines[at].len() > LINE_MAX {
                    let line = std::mem::take(&mut self.lines[at]);
                    self.tail.push(&line);
                    self.overflowed[at] = true;
                }
            }
            if ended {
                self.overflowed[at] = false;
            }
        }
    }

    /// A whole line, its `\n` included.
    fn line(&mut self, stream: Stream, line: &[u8]) {
        if stream == Stream::Out && line.starts_with(&self.prefix) {
            if self.report.len() + line.len() <= REPORT_MAX {
                self.report.extend_from_slice(line);
            }
        } else {
            self.tail.push(line);
        }
    }

    /// The report lines and the output; an unfinished line is output, stdout's first.
    pub(super) fn finish(mut self) -> (String, String) {
        for at in 0..2 {
            let line = std::mem::take(&mut self.lines[at]);
            self.tail.push(&line);
        }
        (
            String::from_utf8_lossy(&self.report).into_owned(),
            self.tail.text(),
        )
    }
}

/// One function of an accepted run, with the cases to run in list order.
pub(super) struct Planned {
    pub slug: String,
    pub lang: FunctionLang,
    pub driver: String,
    /// The Cargo package's folder and name, for a Rust function.
    pub package: Option<(String, String)>,
    pub template: String,
    pub cases: Vec<CaseView>,
}

fn not_ready(reason: &Support, lang: Option<FunctionLang>, qualified: &str) -> Option<String> {
    match reason {
        Support::Ready => None,
        Support::Unsupported { reason } | Support::Stale { reason } => Some(reason.clone()),
        Support::NoCases => Some(NO_CASES.to_string()),
        Support::NoCommand => Some(match lang {
            Some(lang) => format!(
                "The recipe has no command to run {} functions. Refresh the recipe, or add functions.{} to it.",
                lang.label(),
                lang.key()
            ),
            None => {
                format!("Clúsia cannot run {qualified}: no driver can be written in its language")
            }
        }),
    }
}

/// `RunCases`: checks that what would run is what the reviewer saw and approved, then runs
/// the chosen cases in the background.
pub(crate) async fn run_cases(
    shared: &Arc<Shared>,
    pr: &PrRef,
    scope: CaseScope,
    seen: String,
) -> Outcome {
    if let CaseScope::Function { function } | CaseScope::Case { function, .. } = &scope
        && !valid_slug(function)
    {
        return reviews::bad_request("invalid function id");
    }
    if let Err(out) = super::active(shared, pr) {
        return out;
    }
    let usable = match work::ready_to_run(shared, pr).await {
        Ok(usable) => usable,
        Err(out) => return out,
    };
    let read = cases::read(shared, pr).await;
    if let Some(problem) = read.shipped {
        return reviews::invalid_state(problem);
    }
    if let Err(out) = cases::check_seen(&read.view, pr, &scope, &seen) {
        return out;
    }
    let Some(worktree) = read.worktree.clone() else {
        return reviews::invalid_state(cases::OPEN_FIRST);
    };
    let mut chosen = Vec::new();
    for (at, f) in read.view.functions.iter().enumerate() {
        let wanted = match &scope {
            CaseScope::All => true,
            CaseScope::Function { function } | CaseScope::Case { function, .. } => {
                f.slug == *function
            }
        };
        if !wanted {
            continue;
        }
        if let Some(reason) = not_ready(&f.support, f.lang, &f.qualified) {
            if scope == CaseScope::All {
                continue;
            }
            return reviews::invalid_state(reason);
        }
        let cases: Vec<CaseView> = match &scope {
            CaseScope::Case { case, .. } => {
                f.cases.iter().filter(|c| c.id == *case).cloned().collect()
            }
            _ => f.cases.clone(),
        };
        chosen.push((at, cases));
    }
    let total: usize = chosen.iter().map(|(_, cases)| cases.len()).sum();
    if total == 0 {
        return reviews::invalid_state(NO_CASES);
    }
    let mut planned = Vec::new();
    for (at, cases) in chosen {
        let (f, item) = (&read.view.functions[at], &read.items[at]);
        let Some(lang) = f.lang else {
            return reviews::invalid_state(CASES_CHANGED);
        };
        let stored_hash = item.stored.as_ref().and_then(|s| s.driver_hash.as_deref());
        let driver = match cases::read_driver(&worktree, &f.slug, lang) {
            Ok(Some(text)) if Some(sha256_hex(text.as_bytes()).as_str()) == stored_hash => text,
            _ => return reviews::invalid_state(CASES_CHANGED),
        };
        // The command is the one the approved recipe holds, never the file read again.
        let template = usable
            .recipe
            .functions
            .as_ref()
            .and_then(|spec| spec.command(lang));
        let Some(template) = template.filter(|t| Some(*t) == item.command.as_deref()) else {
            return reviews::invalid_state(CASES_CHANGED);
        };
        let package = match (lang, &item.package) {
            (FunctionLang::Rust, Some(Ok(package))) => Some(package.clone()),
            (FunctionLang::Rust, _) => {
                return reviews::invalid_state(clusia_testrun::functions::NO_PACKAGE);
            }
            _ => None,
        };
        planned.push(Planned {
            slug: f.slug.clone(),
            lang,
            driver,
            package,
            template: template.to_string(),
            cases,
        });
    }
    let started = match super::begin(shared, pr, Doing::RunCases, TestsPhase::Waiting) {
        Ok(started) => started,
        Err(refused) => return refused.outcome(pr),
    };
    let run_id = format!("{}-{}", now_unix(), shared.tests.seq());
    shared.tests.with(pr, |entry| {
        if let Some(activity) = entry.activity.as_mut().filter(|a| a.id == started.id) {
            activity.functions = Some(FunctionsActivity::Running {
                run: run_id.clone(),
                scope: scope.clone(),
                total: u32::try_from(total).unwrap_or(u32::MAX),
                done: 0,
                current: None,
            });
        }
    });
    cases::tell_functions(shared, pr).await;
    let job = work::job_of(usable, run_id.clone(), TestScope::All, Vec::new());
    let (id, live, stop) = (started.id, started.live, started.stop);
    let (shared, pr) = (shared.clone(), pr.clone());
    tokio::spawn(async move {
        let inner = tokio::spawn(run(shared.clone(), pr.clone(), id, stop, job, planned));
        if inner.await.is_err() {
            tracing::error!(pr = %pr, "a case run failed inside the daemon");
        }
        cases::end(&shared, &pr, id, None);
        drop(live);
        cases::tell_functions(&shared, &pr).await;
    });
    Outcome::Ok(Reply::CasesStarted { run: run_id })
}

/// The placed driver's path, relative to the worktree, as `place_driver` answers it.
fn placed_path(package_dir: &str, target: &str) -> String {
    if package_dir.is_empty() {
        format!("tests/{target}.rs")
    } else {
        format!("{package_dir}/tests/{target}.rs")
    }
}

/// Removes a placed driver and forgets it, or keeps it in the record for the next run. A path
/// that is not of the placed form was never placed by Clúsia and is forgotten untouched.
async fn unplace(shared: &Shared, pr: &PrRef, worktree: &std::path::Path, placed: &str) {
    match remove_driver(worktree, placed) {
        Ok(()) => {
            env::update_record(shared, pr, |record| record.placed.retain(|p| p != placed)).await;
        }
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
            tracing::warn!(pr = %pr, path = placed, "forgetting a placed path that is not a driver's");
            env::update_record(shared, pr, |record| record.placed.retain(|p| p != placed)).await;
        }
        Err(e) => {
            tracing::warn!(pr = %pr, path = placed, error = %e, "cannot remove a placed driver");
        }
    }
}

/// How one case went, before it becomes a result.
struct CaseEnd {
    status: CaseStatus,
    report: DriverReport,
    state: Vec<StateCheck>,
    output: String,
    took_ms: u64,
    exit_code: Option<i32>,
    message: Option<String>,
}

impl CaseEnd {
    /// A case that could not run, with `message` as why.
    fn error(case: &CaseView, message: String) -> Self {
        let (_, state, _) = case_status(
            &DriverReport::default(),
            &Ended::Exited(None),
            None,
            &case.expect,
        );
        Self {
            status: CaseStatus::Error,
            report: DriverReport::default(),
            state,
            output: String::new(),
            took_ms: 0,
            exit_code: None,
            message: Some(message),
        }
    }
}

/// Stores a case's result as the newest of its case and tells the windows.
async fn record_result(
    shared: &Shared,
    pr: &PrRef,
    job: &RunJob,
    slug: &str,
    case: &CaseView,
    out: CaseEnd,
) {
    let result = CaseResult {
        case: case.id.clone(),
        head: job.head.clone(),
        hash: case.hash.clone(),
        status: out.status,
        returned: out.report.returned,
        returned_expected: case.expect.returned.clone(),
        error: out.report.error,
        state: out.state,
        output: out.output,
        took_ms: out.report.took_ms.unwrap_or(out.took_ms),
        runner: job.env.runner,
        exit_code: out.exit_code,
        message: out.message,
        at: now_unix(),
    };
    env::update_record(shared, pr, |record| {
        record
            .case_results
            .entry(slug.to_string())
            .or_default()
            .insert(case.id.clone(), result.clone());
    })
    .await;
    shared.publish(
        topics::TESTS,
        Event::CaseDone {
            pr: pr.clone(),
            run: job.id.clone(),
            function: slug.to_string(),
            result: Box::new(result),
        },
    );
}

/// Moves the run's progress: `current` before a case, `done` after one.
fn progress(shared: &Shared, pr: &PrRef, act: u64, current: Option<CaseRef>, finished: bool) {
    shared.tests.with(pr, |entry| {
        let running = entry
            .activity
            .as_mut()
            .filter(|a| a.id == act)
            .and_then(|a| a.functions.as_mut());
        if let Some(FunctionsActivity::Running {
            done, current: now, ..
        }) = running
        {
            if finished {
                *done += 1;
            }
            *now = current;
        }
    });
}

/// Runs the planned functions' cases, each in a process of its own, within the run's budget.
async fn run(
    shared: Arc<Shared>,
    pr: PrRef,
    act: u64,
    stop: Arc<StopFlag>,
    job: RunJob,
    planned: Vec<Planned>,
) {
    let Some(_place) = super::place(&shared, &pr, act, &stop).await else {
        return;
    };
    let worktree = job.ctx.worktree.clone();
    for leftover in env::load(&shared, &pr).placed {
        unplace(&shared, &pr, &worktree, &leftover).await;
    }
    let root = match job.env.runner {
        RunnerKind::Container => clusia_testrun::docker::WORK_DIR.to_string(),
        RunnerKind::Local => std::fs::canonicalize(&worktree)
            .unwrap_or_else(|_| worktree.clone())
            .display()
            .to_string(),
    };
    run_planned(&shared, &pr, act, &stop, &job, planned, &root).await;
}

/// The planned functions in order, once the run has its place: each function's cases, each in
/// a process of its own, within the run's budget.
pub(super) async fn run_planned(
    shared: &Arc<Shared>,
    pr: &PrRef,
    act: u64,
    stop: &StopFlag,
    job: &RunJob,
    planned: Vec<Planned>,
    root: &str,
) {
    let budget = Duration::from_secs(u64::from(shared.config.read().await.tests.run_timeout_secs));
    let worktree = job.ctx.worktree.clone();
    let began = Instant::now();
    let mut ended_run = false;
    for function in planned {
        // A stop between two functions places no further driver.
        if ended_run || stop.fired() {
            break;
        }
        let mut placed = None;
        let command = match function.lang {
            FunctionLang::Rust => {
                let (dir, name) = function.package.clone().unwrap_or_default();
                let target = driver_target(&function.slug);
                let path = placed_path(&dir, &target);
                // Recorded before it exists, so a run that dies after placing it still lets
                // the next run remove it.
                env::update_record(shared, pr, |record| {
                    if !record.placed.contains(&path) {
                        record.placed.push(path.clone());
                    }
                })
                .await;
                match place_driver(&worktree, &dir, &target, function.driver.as_bytes()) {
                    Ok(at) => {
                        placed = Some(at);
                        function_command(&function.template, &target, Some(&name))
                    }
                    Err(message) => {
                        env::update_record(shared, pr, |record| {
                            record.placed.retain(|p| *p != path)
                        })
                        .await;
                        Err(message)
                    }
                }
            }
            lang => function_command(
                &function.template,
                &cases::driver_path(&function.slug, lang),
                None,
            ),
        };
        for case in &function.cases {
            if stop.fired() {
                ended_run = true;
                break;
            }
            let current = CaseRef {
                function: function.slug.clone(),
                case: case.id.clone(),
            };
            let out = match &command {
                Err(message) => CaseEnd::error(case, message.clone()),
                Ok(command) => {
                    let left = budget.saturating_sub(began.elapsed());
                    if left.is_zero() {
                        ended_run = true;
                        break;
                    }
                    progress(shared, pr, act, Some(current.clone()), false);
                    cases::tell_functions(shared, pr).await;
                    let limit = left.min(Duration::from_secs(CASE_TIMEOUT_SECS));
                    let out =
                        run_case(shared, pr, act, job, stop, command, case, root, limit).await;
                    let past_budget = out.status == CaseStatus::TimedOut && limit == left;
                    if out.status == CaseStatus::Stopped || past_budget {
                        ended_run = true;
                    }
                    out
                }
            };
            record_result(shared, pr, job, &function.slug, case, out).await;
            progress(shared, pr, act, None, true);
            cases::tell_functions(shared, pr).await;
            if ended_run {
                break;
            }
        }
        if let Some(placed) = placed {
            unplace(shared, pr, &worktree, &placed).await;
        }
    }
}

/// One case: its driver in the review's runner with the case's environment, read back.
#[allow(clippy::too_many_arguments)]
async fn run_case(
    shared: &Shared,
    pr: &PrRef,
    act: u64,
    job: &RunJob,
    stop: &StopFlag,
    command: &str,
    case: &CaseView,
    root: &str,
    limit: Duration,
) -> CaseEnd {
    let mark: String = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
    let extra = case_env(&mark, root, &case.id, &case.values);
    let mut sink = CaseSink::new(&mark);
    let began = Instant::now();
    let ended = work::run_command(
        shared,
        pr,
        act,
        job,
        command,
        limit,
        stop,
        &mut |stream, bytes: &[u8]| sink.chunk(stream, bytes),
        &extra,
    )
    .await;
    let took_ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (report, output) = sink.finish();
    let ended = match ended {
        Ok(ended) => ended,
        Err(e) => {
            let mut out = CaseEnd::error(case, format!("The driver could not start: {e}"));
            out.output = output;
            out.took_ms = took_ms;
            return out;
        }
    };
    let exit = match ended {
        Ended::Exited(code) => code,
        _ => None,
    };
    let report = read_report(&mark, &report);
    let (status, state, message) = case_status(&report, &ended, exit, &case.expect);
    CaseEnd {
        status,
        report,
        state,
        output,
        took_ms,
        exit_code: exit,
        message,
    }
}

#[cfg(test)]
mod tests {
    use clusia_core::tests::TAIL_MAX;
    use clusia_testrun::exec::Stream;
    use clusia_testrun::functions::read_report;

    use super::*;

    const MARK: &str = "0123456789abcdef";

    #[test]
    fn huge_output_is_capped_and_only_marked_stdout_reports() {
        let mut sink = CaseSink::new(MARK);
        sink.chunk(Stream::Out, b"compiling widgets\n");
        sink.chunk(
            Stream::Err,
            format!("{MARK} returned from-stderr\n").as_bytes(),
        );
        sink.chunk(Stream::Out, b"ffffffffffffffff returned other-mark\n");
        let block = vec![b'y'; 1024 * 1024];
        for _ in 0..50 {
            sink.chunk(Stream::Out, &block);
            sink.chunk(Stream::Out, b"\n");
        }
        sink.chunk(
            Stream::Out,
            format!("{MARK} returned Ok(1)\n{MARK} ").as_bytes(),
        );
        sink.chunk(
            Stream::Out,
            format!("took 12\n{MARK} done\ntail line\n").as_bytes(),
        );
        let (report, output) = sink.finish();
        let report = read_report(MARK, &report);
        assert_eq!(report.returned.as_deref(), Some("Ok(1)"));
        assert_eq!((report.took_ms, report.done), (Some(12), true));
        assert!(output.len() <= TAIL_MAX, "{}", output.len());
        assert!(output.ends_with("tail line\n"));
        assert!(!output.contains(&format!("{MARK} done")));

        let mut quiet = CaseSink::new(MARK);
        quiet.chunk(Stream::Out, b"first\n");
        quiet.chunk(Stream::Err, b"warn: x\n");
        quiet.chunk(
            Stream::Out,
            format!("{MARK} done\nlast without newline").as_bytes(),
        );
        let (report, output) = quiet.finish();
        assert_eq!(output, "first\nwarn: x\nlast without newline");
        assert!(read_report(MARK, &report).done);
    }
}

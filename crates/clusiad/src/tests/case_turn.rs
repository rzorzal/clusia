//! The case turns: one forked, read-only agent turn per ask, scoped to one function, that
//! answers with one `clusia-function` block.

use std::sync::Arc;
use std::time::Duration;

use clusia_core::PrRef;
use clusia_core::functions::{
    CASES_MAX, CasePromptContext, CasesAsk, FunctionsActivity, case_prompt,
};
use clusia_core::tests::TestsPhase;
use clusia_harness::extract_recipe_blocks;
use clusia_protocol::{Outcome, Reply, TurnOrigin};

use super::Doing;
use super::cases::{self, Target};
use super::env;
use super::recipe_turn::{RecipeSink, note_log};
use crate::checks;
use crate::reviews;
use crate::sessions;
use crate::state::Shared;
use crate::sync::now_unix;
use crate::turns::{self, SessionSource, Stop, TurnEnd, TurnRun};

/// Runs one case turn: the first `clusia-function` block of the answer, `None` when it holds
/// none. `Err` is why no answer came (the program failed, the limit passed, a stop).
pub(crate) async fn run_case_turn(
    shared: &Arc<Shared>,
    pr: &PrRef,
    mut ctx: CasePromptContext,
    stop: Arc<Stop>,
) -> Result<Option<serde_json::Value>, String> {
    let name = sessions::name_for(shared, pr).await;
    let limit_secs = shared.config.read().await.harness.check_timeout_secs;
    let limit = Duration::from_secs(u64::from(limit_secs));
    let turn = shared.sessions.next_turn(shared, pr).await;
    ctx.forked = true;
    let prompt = case_prompt(&ctx);
    ctx.forked = false;
    let fresh_prompt = case_prompt(&ctx);
    note_log(shared, pr, turn, "waiting");
    let mut sink = RecipeSink {
        shared: shared.clone(),
        pr: pr.clone(),
        turn,
        streamed: String::new(),
        answer: None,
        saw_final: false,
        failed: false,
        message: None,
        activity: &mut |_| {},
    };
    let run = TurnRun {
        pr: pr.clone(),
        turn,
        prompt,
        fresh_prompt: Some(fresh_prompt),
        session: SessionSource::Fork,
        origin: TurnOrigin::Cases,
        places: checks::places(shared),
        limit: Some(limit),
        stop,
    };
    match turns::run(shared, run, &mut sink).await {
        TurnEnd::Done => {
            note_log(shared, pr, turn, "done");
            let text = sink.answer.unwrap_or(sink.streamed);
            Ok(extract_recipe_blocks(&text).functions.into_iter().next())
        }
        TurnEnd::Failed => {
            note_log(shared, pr, turn, "failed");
            Err(sink
                .message
                .unwrap_or_else(|| format!("{name} could not write the cases")))
        }
        TurnEnd::TimedOut => {
            note_log(shared, pr, turn, "failed");
            Err(format!(
                "The case turn ran past its limit of {}s and was stopped",
                limit.as_secs()
            ))
        }
        TurnEnd::Stopped(why) => {
            note_log(shared, pr, turn, "stopped");
            Err(why.message().to_string())
        }
    }
}

/// `WriteCases`: has the agent write cases for one function, in the background.
pub(crate) async fn write_cases(
    shared: &Arc<Shared>,
    pr: &PrRef,
    function: &str,
    ask: CasesAsk,
) -> Outcome {
    let (read, at) = match cases::writable(shared, pr, function).await {
        Ok(found) => found,
        Err(out) => return out,
    };
    let review = match super::active(shared, pr) {
        Ok(review) => review,
        Err(out) => return out,
    };
    let name = sessions::name_for(shared, pr).await;
    let (view, item) = (&read.view.functions[at], &read.items[at]);
    let Some(worktree) = read.worktree.clone() else {
        return reviews::invalid_state(cases::OPEN_FIRST);
    };
    let Some(lang) = view.lang else {
        return reviews::invalid_state(format!(
            "Clúsia cannot run {}: no driver can be written in its language",
            view.qualified
        ));
    };
    let stored_cases = item.stored.as_ref().map_or(0, |s| s.cases.len());
    match &ask {
        CasesAsk::Fresh => {}
        CasesAsk::More if stored_cases == 0 => {
            return reviews::invalid_state(format!(
                "{name} has not written cases for this function yet."
            ));
        }
        CasesAsk::More if view.cases.len() >= CASES_MAX => {
            return reviews::invalid_state(format!(
                "This function has {CASES_MAX} cases, the most it keeps."
            ));
        }
        CasesAsk::More => {}
        CasesAsk::Values { case } => {
            if !view.cases.iter().any(|c| c.id == *case) {
                return reviews::invalid_state(format!("No case {case} in {}", view.qualified));
            }
        }
    }
    let ctx = CasePromptContext {
        pr_title: review.title.clone(),
        forked: false,
        function: item.brief.clone(),
        ask: ask.clone(),
        params: view.params.clone(),
        cases: cases::effective(item.stored.as_ref(), &item.edits)
            .into_iter()
            .map(|e| e.case)
            .collect(),
        driver: cases::read_driver(&worktree, &view.slug, lang)
            .ok()
            .flatten(),
        command: item.command.clone(),
    };
    let target = Target {
        slug: view.slug.clone(),
        qualified: view.qualified.clone(),
        file: view.file.clone(),
        lang,
        signature: item.brief.signature.clone(),
        head: review.head_sha.clone(),
    };
    let started = match super::begin(shared, pr, Doing::Cases, TestsPhase::Waiting) {
        Ok(started) => started,
        Err(refused) => return refused.outcome(pr),
    };
    shared.tests.with(pr, |entry| {
        if let Some(activity) = entry.activity.as_mut().filter(|a| a.id == started.id) {
            activity.functions = Some(FunctionsActivity::Writing {
                function: Some(view.slug.clone()),
                ask: Some(ask.clone()),
            });
        }
    });
    cases::tell_functions(shared, pr).await;
    let (id, live, turn) = (started.id, started.live, started.turn);
    let job = {
        let (shared, pr) = (shared.clone(), pr.clone());
        async move {
            let answer = run_case_turn(&shared, &pr, ctx, turn.clone()).await;
            match answer {
                Ok(Some(block)) => {
                    let _guard = reviews::lock(&shared, &pr).await;
                    if turn.fired() || !env::alive(&shared, &pr) {
                        return None;
                    }
                    cases::apply_block(&worktree, &target, &block, &ask, now_unix())
                        .err()
                        .map(|reasons| {
                            format!("{name}'s cases are not valid: {}", reasons.join("; "))
                        })
                }
                Ok(None) => Some(format!("{name}'s answer holds no clusia-function block")),
                Err(_) if turn.fired() => None,
                Err(message) => Some(message),
            }
        }
    };
    let (shared, pr) = (shared.clone(), pr.clone());
    tokio::spawn(async move {
        let failed = match tokio::spawn(job).await {
            Ok(failed) => failed,
            Err(_) => {
                tracing::error!(pr = %pr, "a case turn failed inside the daemon");
                Some("The case turn stopped unexpectedly".to_string())
            }
        };
        cases::end(&shared, &pr, id, failed);
        drop(live);
        cases::tell_functions(&shared, &pr).await;
    });
    Outcome::Ok(Reply::Ack)
}

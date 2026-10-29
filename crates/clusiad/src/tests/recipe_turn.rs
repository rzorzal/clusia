//! The recipe turn: one forked, read-only agent turn that answers with a recipe.

use std::sync::Arc;
use std::time::Duration;

use clusia_core::PrRef;
use clusia_core::tests::{RecipePromptContext, recipe_prompt};
use clusia_harness::{AgentEvent, extract_recipe_blocks};
use clusia_protocol::{AgentErrorKind, AgentLogEntry, TurnOrigin};
use clusia_testrun::recipe::Recipe;

use crate::agent_log;
use crate::checks;
use crate::sessions;
use crate::state::Shared;
use crate::sync::now_unix;
use crate::turns::{self, Boxed, SessionSource, Stop, TurnEnd, TurnRun, TurnSink};

/// What the agent answered.
pub(crate) struct RecipeAnswer {
    /// The recipe, or why the one it wrote does not validate.
    pub recipe: Result<Recipe, Vec<String>>,
    /// The tests it named as near the change.
    pub near: Vec<String>,
    /// How many blocks of its answer could not be used.
    pub unreadable: u32,
}

/// Runs the recipe turn. `Err` is why no answer came (the program failed, the limit passed, a
/// stop); a recipe that does not validate is an answer.
pub(crate) async fn run_recipe_turn(
    shared: &Arc<Shared>,
    pr: &PrRef,
    ctx: RecipePromptContext,
    stop: Arc<Stop>,
) -> Result<RecipeAnswer, String> {
    run_recipe_turn_with(shared, pr, ctx, stop, &mut |_| {}).await
}

/// `run_recipe_turn`, telling `activity` what the agent is doing as it reads the repository.
pub(crate) async fn run_recipe_turn_with(
    shared: &Arc<Shared>,
    pr: &PrRef,
    mut ctx: RecipePromptContext,
    stop: Arc<Stop>,
    activity: &mut (dyn FnMut(String) + Send),
) -> Result<RecipeAnswer, String> {
    let name = sessions::name_for(shared, pr).await;
    let limit_secs = shared.config.read().await.harness.check_timeout_secs;
    let limit = Duration::from_secs(u64::from(limit_secs));
    let turn = shared.sessions.next_turn(shared, pr).await;
    ctx.forked = true;
    let prompt = recipe_prompt(&ctx);
    ctx.forked = false;
    let fresh_prompt = recipe_prompt(&ctx);
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
        activity,
    };
    let run = TurnRun {
        pr: pr.clone(),
        turn,
        prompt,
        fresh_prompt: Some(fresh_prompt),
        session: SessionSource::Fork,
        origin: TurnOrigin::Recipe,
        places: checks::places(shared),
        limit: Some(limit),
        stop,
    };
    match turns::run(shared, run, &mut sink).await {
        TurnEnd::Done => {
            note_log(shared, pr, turn, "done");
            Ok(answer_of(sink.answer.unwrap_or(sink.streamed)))
        }
        TurnEnd::Failed => {
            note_log(shared, pr, turn, "failed");
            Err(sink
                .message
                .unwrap_or_else(|| format!("{name} could not write the recipe")))
        }
        TurnEnd::TimedOut => {
            note_log(shared, pr, turn, "failed");
            Err(format!(
                "The recipe turn ran past its limit of {}s and was stopped",
                limit.as_secs()
            ))
        }
        TurnEnd::Stopped(why) => {
            note_log(shared, pr, turn, "stopped");
            Err(why.message().to_string())
        }
    }
}

/// The recipe and the near tests of an answer.
fn answer_of(text: String) -> RecipeAnswer {
    let blocks = extract_recipe_blocks(&text);
    let recipe = match blocks.recipe {
        Some(value) => Recipe::from_json(&value),
        None => Err(vec!["The answer holds no clusia-recipe block".to_string()]),
    };
    RecipeAnswer {
        recipe,
        near: blocks.near_tests,
        unreadable: blocks.unreadable,
    }
}

fn note_log(shared: &Shared, pr: &PrRef, turn: u64, state: &str) {
    let entry = AgentLogEntry::Tests {
        at: now_unix(),
        turn,
        state: state.to_string(),
    };
    if let Err(e) = agent_log::append(&shared.paths.agent_log(pr), &entry) {
        tracing::warn!(error = %e, pr = %pr, "cannot append to the agent log");
    }
}

/// Reads what the turn says. The session id is ignored on purpose: a fork never becomes the
/// chat's.
struct RecipeSink<'a> {
    shared: Arc<Shared>,
    pr: PrRef,
    turn: u64,
    /// The text streamed so far, used when no result line carries the answer.
    streamed: String,
    answer: Option<String>,
    saw_final: bool,
    failed: bool,
    message: Option<String>,
    activity: &'a mut (dyn FnMut(String) + Send),
}

impl RecipeSink<'_> {
    fn fail_with(&mut self, message: String) {
        self.failed = true;
        self.message.get_or_insert(message);
    }

    fn handle(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Text(text) => self.streamed.push_str(&text),
            AgentEvent::ToolUse(summary) => (self.activity)(summary),
            AgentEvent::Denied { .. } | AgentEvent::SessionId(_) => {}
            AgentEvent::Error { message, .. } => self.fail_with(message),
            AgentEvent::Final { text, is_error, .. } => {
                self.saw_final = true;
                if !is_error {
                    self.answer = Some(text);
                } else if text.trim().is_empty() {
                    // No text to show: the turn's own "<agent> could not write the recipe" speaks.
                    self.failed = true;
                } else {
                    self.fail_with(text.trim().to_string());
                }
            }
        }
    }
}

impl TurnSink for RecipeSink<'_> {
    fn waiting(&mut self) {}

    fn started(&mut self) {
        note_log(&self.shared, &self.pr, self.turn, "running");
    }

    fn event<'b>(&'b mut self, event: AgentEvent) -> Boxed<'b> {
        Box::pin(async move { self.handle(event) })
    }

    fn error(&mut self, _kind: AgentErrorKind, message: String) {
        self.fail_with(message);
    }

    fn release(&mut self) {}

    fn flush(&mut self) {}

    fn saw_final(&self) -> bool {
        self.saw_final
    }

    fn failed(&self) -> bool {
        self.failed
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use clusia_core::tests::{
        Ecosystem, Engine, EngineKind, EnvState, Existing, RepoSignals, ResultsFormat, RunnerKind,
    };
    use clusia_harness::testkit::{FakeClaude, Script, Turn, near_tests_block, recipe_block};
    use clusia_protocol::AgentLogEntry;
    use clusia_store::save_review;
    use serde_json::json;

    use super::*;
    use crate::checks::tests::lab;
    use crate::turns::Why;

    fn pr(n: u64) -> PrRef {
        format!("acme/widgets#{n}").parse().unwrap()
    }

    fn context() -> RecipePromptContext {
        RecipePromptContext {
            pr_title: "Add retries".into(),
            files: vec!["src/client.rs".into()],
            forked: true,
            env: EnvState {
                head: "h".into(),
                probed_at: 1,
                engine: Some(Engine {
                    kind: EngineKind::Colima,
                    cli: "/opt/homebrew/bin/docker".into(),
                    endpoint: "unix:///Users/mona/.colima/default/docker.sock".into(),
                    context: None,
                    running: true,
                    start: Some("colima start".into()),
                    compose: None,
                }),
                runner: RunnerKind::Container,
                note: None,
                sandbox_ok: true,
                signals: RepoSignals {
                    ecosystems: vec![Ecosystem::Cargo],
                    ..RepoSignals::default()
                },
                existing: Existing::default(),
                inputs_hash: "f00".into(),
            },
            current: None,
            cases: false,
        }
    }

    fn recipe_json() -> serde_json::Value {
        json!({
            "runner": "auto",
            "image": {"base": "rust:1.86"},
            "setup": [{"name": "Fetch dependencies", "run": "cargo fetch", "into": "work", "network": true}],
            "tests": {
                "all": "cargo test --workspace",
                "one": "cargo test --workspace -- --exact {name}",
                "list": "cargo test --workspace -- --list --format terse",
                "results": "libtest",
                "env": {"RUST_BACKTRACE": "1"}
            }
        })
    }

    async fn calls(fake: &tempfile::TempDir, count: usize) -> Vec<clusia_harness::testkit::Call> {
        let dir = fake.path().to_path_buf();
        tokio::task::spawn_blocking(move || {
            FakeClaude::wait_for_calls(&dir, count, Duration::from_secs(5))
        })
        .await
        .unwrap()
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let at = args.iter().position(|a| a == flag).expect(flag);
        &args[at + 1]
    }

    fn states(shared: &Shared, pr: &PrRef) -> Vec<String> {
        crate::agent_log::read(&shared.paths.agent_log(pr))
            .into_iter()
            .filter_map(|entry| match entry {
                AgentLogEntry::Tests { state, .. } => Some(state),
                _ => None,
            })
            .collect()
    }

    fn store_session(shared: &Shared, pr: &PrRef, id: &str) {
        let mut review = crate::reviews::load_stored(shared, pr).unwrap().unwrap();
        review.harness_session = Some(id.into());
        save_review(&shared.paths, &review).unwrap();
    }

    #[tokio::test]
    async fn the_recipe_and_the_near_tests_are_read_from_the_answer() {
        let (_home, _fake, shared) = lab(
            Script::one(Turn::answer_blocks(
                "Here is the recipe.",
                &[
                    recipe_block(recipe_json()),
                    near_tests_block(
                        json!({"tests": ["client::tests::retries", "client::tests::backoff"]}),
                    ),
                ],
            )),
            1,
            |_| {},
        );
        let answer = run_recipe_turn(&shared, &pr(1), context(), Stop::new())
            .await
            .unwrap();
        let recipe = answer.recipe.unwrap();
        assert_eq!(recipe.tests.all, "cargo test --workspace");
        assert_eq!(recipe.tests.results, ResultsFormat::Libtest);
        assert_eq!(recipe.setup[0].run, "cargo fetch");
        assert_eq!(
            answer.near,
            ["client::tests::retries", "client::tests::backoff"]
        );
        assert_eq!(answer.unreadable, 0);
        assert_eq!(states(&shared, &pr(1)), ["waiting", "running", "done"]);
    }

    #[tokio::test]
    async fn a_recipe_that_does_not_validate_is_an_answer_with_reasons() {
        let mut bad = recipe_json();
        bad["tests"]["env"] = json!({"PATH": "/tmp/evil"});
        let (_home, _fake, shared) = lab(
            Script::one(Turn::answer_blocks("x", &[recipe_block(bad)])),
            1,
            |_| {},
        );
        let answer = run_recipe_turn(&shared, &pr(1), context(), Stop::new())
            .await
            .unwrap();
        let reasons = answer.recipe.expect_err("the recipe is invalid");
        assert!(reasons.iter().any(|r| r.contains("PATH")), "{reasons:?}");
    }

    #[tokio::test]
    async fn an_answer_with_no_recipe_block_says_so() {
        let (_home, _fake, shared) = lab(
            Script::one(Turn::answer("I could not tell how to run the tests.")),
            1,
            |_| {},
        );
        let answer = run_recipe_turn(&shared, &pr(1), context(), Stop::new())
            .await
            .unwrap();
        assert_eq!(
            answer.recipe.err(),
            Some(vec!["The answer holds no clusia-recipe block".to_string()])
        );
    }

    #[tokio::test]
    async fn the_turn_forks_the_review_session_and_never_replaces_it() {
        let (_home, fake, shared) = lab(
            Script::one(Turn::answer_blocks("x", &[recipe_block(recipe_json())])),
            1,
            |_| {},
        );
        store_session(&shared, &pr(1), "parent-session");
        run_recipe_turn(&shared, &pr(1), context(), Stop::new())
            .await
            .unwrap();
        let args = calls(&fake, 1).await.remove(0).argv;
        let at = args.iter().position(|a| a == "--resume").unwrap();
        assert_eq!(
            args[at..at + 3],
            ["--resume", "parent-session", "--fork-session"]
        );
        let prompt = value_after(&args, "-p");
        assert!(prompt.starts_with("Write the test recipe for this pull request"));
        assert!(!prompt.contains("Read .clusia/review.md first"));
        let review = crate::reviews::load_stored(&shared, &pr(1))
            .unwrap()
            .unwrap();
        assert_eq!(review.harness_session.as_deref(), Some("parent-session"));
    }

    #[tokio::test]
    async fn without_a_session_the_turn_starts_its_own_and_reads_the_review_first() {
        let (_home, fake, shared) = lab(
            Script::one(Turn::answer_blocks("x", &[recipe_block(recipe_json())])),
            1,
            |_| {},
        );
        run_recipe_turn(&shared, &pr(1), context(), Stop::new())
            .await
            .unwrap();
        let args = calls(&fake, 1).await.remove(0).argv;
        assert!(!args.contains(&"--resume".to_string()));
        assert_eq!(value_after(&args, "--session-id").len(), 36);
        assert!(value_after(&args, "-p").contains("Read .clusia/review.md first"));
    }

    #[tokio::test]
    async fn the_turn_is_a_recipe_turn_with_an_id_from_the_review_counter() {
        let (_home, fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        let stop = Stop::new();
        let task = tokio::spawn({
            let (shared, stop) = (shared.clone(), stop.clone());
            async move { run_recipe_turn(&shared, &pr(1), context(), stop).await }
        });
        let call = calls(&fake, 1).await.remove(0);
        assert_eq!(
            shared.sessions.origin_of(&pr(1), 1),
            Some(TurnOrigin::Recipe),
            "the first turn of a review is 1"
        );
        let bridge: serde_json::Value =
            serde_json::from_str(value_after(&call.argv, "--mcp-config")).unwrap();
        assert_eq!(bridge["mcpServers"]["clusia"]["args"][6], "1");
        stop.fire(Why::User);
        assert_eq!(task.await.unwrap().err().as_deref(), Some("Stopped by you"));
        assert_eq!(shared.sessions.origin_of(&pr(1), 1), None);
        assert_eq!(states(&shared, &pr(1)), ["waiting", "running", "stopped"]);
    }

    #[tokio::test]
    async fn the_turn_waits_for_a_place_of_the_checks() {
        let (_home, fake, shared) = lab(
            Script::one(Turn::answer_blocks("x", &[recipe_block(recipe_json())])),
            1,
            |_| {},
        );
        let places = crate::checks::places(&shared);
        let all = u32::try_from(places.available_permits()).unwrap();
        let held = places.clone().acquire_many_owned(all).await.unwrap();
        let task = tokio::spawn({
            let shared = shared.clone();
            async move { run_recipe_turn(&shared, &pr(1), context(), Stop::new()).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            FakeClaude::calls(fake.path()).is_empty(),
            "no process before a place is free"
        );
        assert_eq!(states(&shared, &pr(1)), ["waiting"]);
        drop(held);
        assert!(task.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn a_turn_past_the_check_limit_is_stopped() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |config| {
            config.harness.check_timeout_secs = 1;
        });
        let error = run_recipe_turn(&shared, &pr(1), context(), Stop::new())
            .await
            .err()
            .unwrap();
        assert_eq!(
            error,
            "The recipe turn ran past its limit of 1s and was stopped"
        );
        assert_eq!(
            states(&shared, &pr(1)).last().map(String::as_str),
            Some("failed")
        );
    }

    #[tokio::test]
    async fn a_program_that_fails_says_what_it_said() {
        let (_home, _fake, shared) = lab(
            Script::one(Turn::lines(&[]).exit(1).stderr("Error: Not logged in\n")),
            1,
            |_| {},
        );
        let error = run_recipe_turn(&shared, &pr(1), context(), Stop::new())
            .await
            .err()
            .unwrap();
        assert!(error.ends_with(": Error: Not logged in"), "{error}");
        assert_eq!(
            states(&shared, &pr(1)).last().map(String::as_str),
            Some("failed")
        );
    }

    #[tokio::test]
    async fn a_failed_result_with_no_text_says_what_the_agent_says() {
        let init = json!({"type": "system", "subtype": "init", "session_id": "s1"}).to_string();
        let result = json!({
            "type": "result", "subtype": "success", "is_error": true, "duration_ms": 5,
            "num_turns": 1, "result": "", "session_id": "s1", "permission_denials": []
        })
        .to_string();
        let (_home, _fake, shared) = lab(Script::one(Turn::lines(&[&init, &result])), 1, |_| {});
        let name = crate::sessions::name_for(&shared, &pr(1)).await;
        let error = run_recipe_turn(&shared, &pr(1), context(), Stop::new())
            .await
            .err()
            .unwrap();
        assert_eq!(error, format!("{name} stopped unexpectedly"));
    }

    #[tokio::test]
    async fn the_prompt_names_every_forbidden_env_entry() {
        // The prompt is written in clusia-core, which cannot see the list: this keeps the two
        // in step, with `X_*` read as "names starting with X_".
        let prompt = recipe_prompt(&context());
        for entry in clusia_testrun::recipe::FORBIDDEN_ENV {
            let name = entry.strip_suffix('*').unwrap_or(entry);
            assert!(prompt.contains(name), "{entry}");
        }
        assert!(prompt.contains("You may set LANG, TERM and CI"));
    }

    #[tokio::test]
    async fn a_stop_before_the_turn_starts_runs_nothing() {
        let (_home, fake, shared) = lab(Script::one(Turn::answer("x")), 1, |_| {});
        let stop = Stop::new();
        stop.fire(Why::Ended);
        let error = run_recipe_turn(&shared, &pr(1), context(), stop)
            .await
            .err()
            .unwrap();
        assert_eq!(error, "Stopped because the review ended");
        assert!(FakeClaude::calls(fake.path()).is_empty());
    }

    #[tokio::test]
    async fn what_the_agent_does_is_told_while_it_works() {
        let init = json!({"type": "system", "subtype": "init", "session_id": "s1"}).to_string();
        let tool = json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "Cargo.toml"}}
        ]}})
        .to_string();
        let tool_result = json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
        ]}})
        .to_string();
        let text = format!("Done.\n\n{}", recipe_block(recipe_json()));
        let result = json!({
            "type": "result", "subtype": "success", "is_error": false, "duration_ms": 5,
            "num_turns": 1, "result": text, "session_id": "s1", "permission_denials": []
        })
        .to_string();
        let (_home, _fake, shared) = lab(
            Script::one(Turn::lines(&[&init, &tool, &tool_result, &result])),
            1,
            |_| {},
        );
        let mut seen: Vec<String> = Vec::new();
        let answer = run_recipe_turn_with(&shared, &pr(1), context(), Stop::new(), &mut |a| {
            seen.push(a)
        })
        .await
        .unwrap();
        assert!(answer.recipe.is_ok());
        assert_eq!(seen.len(), 1);
        assert!(seen[0].starts_with("Read "), "{seen:?}");
    }

    #[tokio::test]
    async fn the_turn_ids_of_one_review_never_repeat() {
        let (_home, fake, shared) = lab(
            Script::one(Turn::answer_blocks("x", &[recipe_block(recipe_json())])),
            1,
            |_| {},
        );
        for _ in 0..2 {
            run_recipe_turn(&shared, &pr(1), context(), Stop::new())
                .await
                .unwrap();
        }
        let calls = calls(&fake, 2).await;
        let turn = |n: usize| -> String {
            let bridge: serde_json::Value =
                serde_json::from_str(value_after(&calls[n].argv, "--mcp-config")).unwrap();
            bridge["mcpServers"]["clusia"]["args"][6]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!((turn(0).as_str(), turn(1).as_str()), ("1", "2"));
    }
}

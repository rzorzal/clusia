//! What a test run needs to say: the machine, the recipe, the setup plan, the runs and what
//! each reported. Pure data; the daemon and `clusia-testrun` do the work.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::prdata::CheckRun;

pub const RECIPE_FILE: &str = ".clusia/recipe.toml";
pub const STATE_DIR: &str = ".clusia/state";
pub const RECIPE_MAX_BYTES: usize = 65_536;
pub const RUNS_PER_HEAD: usize = 5;
pub const RUNS_KEPT: usize = 20;
pub const NEAR_MAX: usize = 30;
pub const RESULTS_MAX: usize = 5000;
pub const MESSAGE_MAX: usize = 4096;
pub const TAIL_MAX: usize = 4096;
pub const LOG_HEAD: usize = 2 * 1024 * 1024;
pub const LOG_TAIL: usize = 8 * 1024 * 1024;
pub const OUTPUT_EVERY_MS: u64 = 250;

pub const STEP_ENGINE: &str = "engine";
pub const STEP_IMAGE: &str = "image";
pub const STEP_SERVICES: &str = "services";
pub const STEP_STACK: &str = "stack";
/// Approves the recipe's `all`, `one`, `some` and `list` commands.
pub const STEP_TESTS: &str = "tests";

/// Where the tests run, as the reviewer or the recipe chooses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunnerChoice {
    #[default]
    Auto,
    Container,
    Local,
}

/// Where the tests run, once resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerKind {
    Container,
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineKind {
    Colima,
    OrbStack,
    DockerDesktop,
    Podman,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Engine {
    pub kind: EngineKind,
    pub cli: String,
    pub endpoint: String,
    pub context: Option<String>,
    pub running: bool,
    /// The exact command that starts it, when Clúsia knows one.
    pub start: Option<String>,
    pub compose: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ecosystem {
    Cargo,
    Node,
    Python,
    Go,
    Jvm,
    Ruby,
}

/// What the repository itself offers for building and running its tests.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RepoSignals {
    pub dockerfiles: Vec<String>,
    pub compose_files: Vec<String>,
    pub devcontainer: Option<String>,
    pub lockfiles: Vec<String>,
    pub toolchains: Vec<String>,
    pub ecosystems: Vec<Ecosystem>,
}

/// What this machine already has for the repository.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Existing {
    pub images: Vec<String>,
    pub reviewer_services: Vec<String>,
    pub local_dirs: Vec<String>,
    pub host_tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvState {
    pub head: String,
    pub probed_at: i64,
    pub engine: Option<Engine>,
    pub runner: RunnerKind,
    pub note: Option<String>,
    pub sandbox_ok: bool,
    pub signals: RepoSignals,
    pub existing: Existing,
    pub inputs_hash: String,
}

/// How a run's output is read: a JUnit report at `path`, or what a test tool prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResultsFormat {
    Junit { path: String },
    Libtest,
    GoJson,
    Pytest,
    Jest,
    Plain,
}

impl ResultsFormat {
    /// `junit:<path>`, `libtest`, `go-json`, `pytest`, `jest` or `plain`; `None` for anything else,
    /// and for a `junit:` with no path.
    pub fn parse(text: &str) -> Option<Self> {
        if let Some(path) = text.strip_prefix("junit:") {
            return (!path.is_empty()).then(|| Self::Junit {
                path: path.to_string(),
            });
        }
        Some(match text {
            "libtest" => Self::Libtest,
            "go-json" => Self::GoJson,
            "pytest" => Self::Pytest,
            "jest" => Self::Jest,
            "plain" => Self::Plain,
            _ => return None,
        })
    }
}

impl fmt::Display for ResultsFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Junit { path } => write!(f, "junit:{path}"),
            Self::Libtest => f.write_str("libtest"),
            Self::GoJson => f.write_str("go-json"),
            Self::Pytest => f.write_str("pytest"),
            Self::Jest => f.write_str("jest"),
            Self::Plain => f.write_str("plain"),
        }
    }
}

/// The recipe as the reviewer sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeSummary {
    pub hash: String,
    /// The file was changed by hand, so Clúsia does not overwrite it.
    pub by_hand: bool,
    pub runner: RunnerChoice,
    pub image: Option<String>,
    pub setup: Vec<String>,
    pub all: String,
    pub one: Option<String>,
    pub some: Option<String>,
    pub list: Option<String>,
    pub results: ResultsFormat,
    pub env: BTreeMap<String, String>,
    pub services: Vec<String>,
    pub problems: Vec<String>,
}

/// A recipe the agent wrote while the file on disk is the reviewer's: kept aside until accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeProposal {
    pub text: String,
    pub hash: String,
    pub at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepPlace {
    Host,
    Container,
    LocalSandbox,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Missing,
    Running,
    Done,
    Failed { message: String },
    Skipped { why: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: String,
    pub title: String,
    pub command: String,
    pub place: StepPlace,
    pub network: bool,
    pub hint: Option<String>,
    pub state: StepState,
    pub approved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCommand {
    /// `all`, `one`, `some` or `list`.
    pub key: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupPlan {
    pub recipe_hash: String,
    pub steps: Vec<PlanStep>,
    pub commands: Vec<PlanCommand>,
    pub commands_approved: bool,
    pub ready: bool,
    pub blocked: Option<String>,
}

/// What a reviewer approves: every step, or the named ones (`STEP_*` ids and `work:<index>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approve {
    All,
    Steps(Vec<String>),
}

/// An approval holds for one recipe hash only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub recipe_hash: String,
    pub steps: BTreeSet<String>,
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestScope {
    All,
    NearChange,
    One { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestCaseResult {
    pub name: String,
    pub file: Option<String>,
    pub status: TestStatus,
    pub duration_ms: Option<u64>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Totals {
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Passed,
    Failed,
    TimedOut,
    Stopped,
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRun {
    pub id: String,
    pub head: String,
    pub scope: TestScope,
    pub runner: RunnerKind,
    pub command: String,
    pub started_at: i64,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub outcome: RunOutcome,
    pub totals: Totals,
    pub results: Vec<TestCaseResult>,
    /// The log's file name inside the review's log folder.
    pub log: Option<String>,
    pub tail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownTest {
    pub name: String,
    pub file: Option<String>,
    pub module: Option<String>,
}

/// Why a test counts as near the change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NearWhy {
    File,
    Module,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NearTest {
    pub name: String,
    pub why: NearWhy,
}

/// The tests the repository's `list` command named at `head`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub head: String,
    pub tests: Vec<KnownTest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestsPhase {
    #[default]
    NotGenerated,
    Generating {
        activity: Option<String>,
    },
    NeedsSetup,
    Waiting,
    Preparing {
        step: String,
    },
    Ready,
    Running {
        run: String,
    },
    Failed {
        message: String,
    },
}

/// Everything the Tests tab and `clusia tests` show of one review. A run is stale when its
/// `head` is not the view's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestsView {
    pub head: String,
    pub phase: TestsPhase,
    pub env: Option<EnvState>,
    pub recipe: Option<RecipeSummary>,
    pub proposal: Option<RecipeProposal>,
    pub plan: Option<SetupPlan>,
    pub runs: Vec<TestRun>,
    pub near: Vec<NearTest>,
    pub github_checks: Vec<CheckRun>,
    pub refresh_available: bool,
}

impl TestsView {
    pub fn new(head: impl Into<String>) -> Self {
        Self {
            head: head.into(),
            phase: TestsPhase::NotGenerated,
            env: None,
            recipe: None,
            proposal: None,
            plan: None,
            runs: Vec::new(),
            near: Vec::new(),
            github_checks: Vec::new(),
            refresh_available: false,
        }
    }
}

/// What is saved of a review's tests between sessions.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TestsRecord {
    pub env: Option<EnvState>,
    /// Hash of the recipe file as Clúsia last wrote it; a file that differs is the reviewer's.
    pub written_hash: Option<String>,
    /// `EnvState::inputs_hash` when the recipe was generated.
    pub generated_inputs: Option<String>,
    pub proposal: Option<RecipeProposal>,
    pub approval: Option<Approval>,
    pub agent_near: Vec<String>,
    pub listing: Option<Listing>,
    /// Newest first.
    pub runs: Vec<TestRun>,
    /// The compose project of the review's running stack.
    pub stack: Option<String>,
    pub generated_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskKind {
    Image,
    Volume,
    Cache,
    WorktreeDir,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskItem {
    pub kind: DiskKind,
    pub name: String,
    pub repo: Option<String>,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskReport {
    pub items: Vec<DiskItem>,
    pub total_bytes: u64,
    pub note: Option<String>,
}

/// What the recipe prompt is written from.
#[derive(Debug, Clone, PartialEq)]
pub struct RecipePromptContext {
    pub pr_title: String,
    pub files: Vec<String>,
    /// Whether the turn continues the review's session; set by the recipe turn.
    pub forked: bool,
    pub env: EnvState,
    pub current: Option<String>,
    /// Whether the agent is also asked for test cases.
    pub cases: bool,
}

/// Drops what the storage rules keep out: runs are newest first, at most `RUNS_PER_HEAD` of one
/// head and `RUNS_KEPT` in all.
pub fn keep_runs(runs: &mut Vec<TestRun>) {
    let mut per_head: BTreeMap<String, usize> = BTreeMap::new();
    runs.retain(|run| {
        let seen = per_head.entry(run.head.clone()).or_default();
        *seen += 1;
        *seen <= RUNS_PER_HEAD
    });
    runs.truncate(RUNS_KEPT);
}

/// The prompt of the recipe turn.
pub fn recipe_prompt(_ctx: &RecipePromptContext) -> String {
    String::new()
}

#[cfg(test)]
mod wire_and_limits {
    use std::collections::BTreeSet;

    use super::*;

    fn run(id: &str, head: &str) -> TestRun {
        TestRun {
            id: id.into(),
            head: head.into(),
            scope: TestScope::All,
            runner: RunnerKind::Local,
            command: "cargo test".into(),
            started_at: 1,
            duration_ms: 2,
            exit_code: Some(0),
            outcome: RunOutcome::Passed,
            totals: Totals::default(),
            results: Vec::new(),
            log: None,
            tail: String::new(),
        }
    }

    fn json<T: serde::Serialize>(value: &T) -> String {
        serde_json::to_string(value).unwrap()
    }

    #[test]
    fn keep_runs_keeps_five_per_head_then_twenty() {
        let mut runs: Vec<TestRun> = (0..7).map(|n| run(&format!("a{n}"), "aaa")).collect();
        keep_runs(&mut runs);
        let ids: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            ["a0", "a1", "a2", "a3", "a4"],
            "newest first, five per head"
        );

        let mut many: Vec<TestRun> = (0..30)
            .map(|n| run(&format!("r{n}"), &format!("head{}", n / 3)))
            .collect();
        keep_runs(&mut many);
        assert_eq!(many.len(), RUNS_KEPT);
        assert_eq!(many[0].id, "r0");
        assert_eq!(many[19].id, "r19");
    }

    #[test]
    fn enums_have_their_wire_names() {
        assert_eq!(json(&RunnerChoice::Auto), r#""auto""#);
        assert_eq!(json(&RunnerKind::Container), r#""container""#);
        assert_eq!(json(&EngineKind::DockerDesktop), r#""docker-desktop""#);
        assert_eq!(json(&EngineKind::OrbStack), r#""orb-stack""#);
        assert_eq!(json(&Ecosystem::Jvm), r#""jvm""#);
        assert_eq!(json(&StepPlace::LocalSandbox), r#""local_sandbox""#);
        assert_eq!(json(&StepState::Missing), r#""missing""#);
        assert_eq!(
            json(&StepState::Failed {
                message: "x".into()
            }),
            r#"{"failed":{"message":"x"}}"#
        );
        assert_eq!(
            json(&StepState::Skipped {
                why: "container only".into()
            }),
            r#"{"skipped":{"why":"container only"}}"#
        );
        assert_eq!(json(&TestScope::NearChange), r#""near_change""#);
        assert_eq!(
            json(&TestScope::One {
                name: "a::b".into()
            }),
            r#"{"one":{"name":"a::b"}}"#
        );
        assert_eq!(json(&RunOutcome::TimedOut), r#""timed_out""#);
        assert_eq!(
            json(&RunOutcome::Error {
                message: "no engine".into()
            }),
            r#"{"error":{"message":"no engine"}}"#
        );
        assert_eq!(json(&NearWhy::Module), r#""module""#);
        assert_eq!(json(&Approve::All), r#""all""#);
        assert_eq!(
            json(&Approve::Steps(vec!["image".into(), "work:0".into()])),
            r#"{"steps":["image","work:0"]}"#
        );
        assert_eq!(json(&DiskKind::WorktreeDir), r#""worktree_dir""#);
        assert_eq!(json(&TestsPhase::NotGenerated), r#""not_generated""#);
        assert_eq!(
            json(&TestsPhase::Generating { activity: None }),
            r#"{"generating":{"activity":null}}"#
        );
        assert_eq!(
            json(&TestsPhase::Preparing {
                step: "image".into()
            }),
            r#"{"preparing":{"step":"image"}}"#
        );
        assert_eq!(TestsPhase::default(), TestsPhase::NotGenerated);
    }

    #[test]
    fn results_formats_parse_display_and_serialize() {
        let junit = ResultsFormat::Junit {
            path: "target/r.xml".into(),
        };
        assert_eq!(junit.to_string(), "junit:target/r.xml");
        assert_eq!(
            ResultsFormat::parse("junit:target/r.xml"),
            Some(junit.clone())
        );
        for (text, format) in [
            ("libtest", ResultsFormat::Libtest),
            ("go-json", ResultsFormat::GoJson),
            ("pytest", ResultsFormat::Pytest),
            ("jest", ResultsFormat::Jest),
            ("plain", ResultsFormat::Plain),
        ] {
            assert_eq!(format.to_string(), text);
            assert_eq!(ResultsFormat::parse(text), Some(format.clone()));
            assert_eq!(json(&format), format!("\"{text}\""));
        }
        assert_eq!(json(&junit), r#"{"junit":{"path":"target/r.xml"}}"#);
        assert_eq!(
            ResultsFormat::parse("junit:"),
            None,
            "a junit report needs a path"
        );
        assert_eq!(ResultsFormat::parse("tap"), None);
        let back: ResultsFormat = serde_json::from_str(&json(&junit)).unwrap();
        assert_eq!(back, junit);
    }

    #[test]
    fn a_new_view_is_empty_and_round_trips() {
        let view = TestsView::new("h".repeat(40));
        assert_eq!(view.phase, TestsPhase::NotGenerated);
        assert!(view.runs.is_empty() && view.github_checks.is_empty() && !view.refresh_available);
        assert_eq!(
            json(&TestsView::new("abc")),
            r#"{"head":"abc","phase":"not_generated","env":null,"recipe":null,"proposal":null,"plan":null,"runs":[],"near":[],"github_checks":[],"refresh_available":false}"#
        );
        let back: TestsView = serde_json::from_str(&json(&view)).unwrap();
        assert_eq!(back, view);
    }

    #[test]
    fn a_full_record_round_trips_and_an_empty_one_loads() {
        let record = TestsRecord {
            env: Some(EnvState {
                head: "h".into(),
                probed_at: 5,
                engine: Some(Engine {
                    kind: EngineKind::Colima,
                    cli: "/opt/homebrew/bin/docker".into(),
                    endpoint: "unix:///Users/mona/.colima/default/docker.sock".into(),
                    context: Some("colima".into()),
                    running: true,
                    start: Some("colima start".into()),
                    compose: Some("2.29.1".into()),
                }),
                runner: RunnerKind::Container,
                note: None,
                sandbox_ok: true,
                signals: RepoSignals {
                    dockerfiles: vec!["Dockerfile".into()],
                    compose_files: Vec::new(),
                    devcontainer: None,
                    lockfiles: vec!["Cargo.lock".into()],
                    toolchains: vec!["rust-toolchain.toml".into()],
                    ecosystems: vec![Ecosystem::Cargo],
                },
                existing: Existing::default(),
                inputs_hash: "0".repeat(64),
            }),
            written_hash: Some("a".repeat(64)),
            generated_inputs: Some("b".repeat(64)),
            proposal: Some(RecipeProposal {
                text: "[tests]\n".into(),
                hash: "c".repeat(64),
                at: 9,
            }),
            approval: Some(Approval {
                recipe_hash: "a".repeat(64),
                steps: BTreeSet::from(["image".to_string(), "tests".to_string()]),
                at: 10,
            }),
            agent_near: vec!["auth::refresh".into()],
            listing: Some(Listing {
                head: "h".into(),
                tests: vec![KnownTest {
                    name: "auth::refresh".into(),
                    file: Some("src/auth.rs".into()),
                    module: Some("auth".into()),
                }],
            }),
            runs: vec![run("r1", "h")],
            stack: Some("clusia-rzorzal-clusia-ab12cd-7".into()),
            generated_at: Some(8),
        };
        let back: TestsRecord = serde_json::from_str(&json(&record)).unwrap();
        assert_eq!(back, record);
        let empty: TestsRecord = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, TestsRecord::default());
    }

    #[test]
    fn a_plan_and_a_run_round_trip() {
        let plan = SetupPlan {
            recipe_hash: "a".repeat(64),
            steps: vec![PlanStep {
                id: STEP_IMAGE.into(),
                title: "Build the image".into(),
                command: "docker build".into(),
                place: StepPlace::Container,
                network: true,
                hint: None,
                state: StepState::Running,
                approved: false,
            }],
            commands: vec![PlanCommand {
                key: "all".into(),
                command: "cargo test".into(),
            }],
            commands_approved: false,
            ready: false,
            blocked: Some("No container engine found.".into()),
        };
        let back: SetupPlan = serde_json::from_str(&json(&plan)).unwrap();
        assert_eq!(back, plan);
        let mut finished = run("r2", "h");
        finished.results = vec![TestCaseResult {
            name: "auth::refresh".into(),
            file: None,
            status: TestStatus::Failed,
            duration_ms: Some(12),
            message: Some("assertion failed".into()),
        }];
        finished.totals = Totals {
            passed: 0,
            failed: 1,
            skipped: 0,
        };
        let back: TestRun = serde_json::from_str(&json(&finished)).unwrap();
        assert_eq!(back, finished);
    }

    #[test]
    fn the_recipe_prompt_takes_its_context() {
        let ctx = RecipePromptContext {
            pr_title: "Fix cache".into(),
            files: vec!["src/a.rs".into()],
            forked: true,
            env: serde_json::from_str(
                r#"{"head":"h","probed_at":0,"engine":null,"runner":"local","note":null,"sandbox_ok":true,"signals":{"dockerfiles":[],"compose_files":[],"devcontainer":null,"lockfiles":[],"toolchains":[],"ecosystems":[]},"existing":{"images":[],"reviewer_services":[],"local_dirs":[],"host_tools":[]},"inputs_hash":""}"#,
            )
            .unwrap(),
            current: None,
            cases: false,
        };
        let prompt: String = recipe_prompt(&ctx);
        drop(prompt);
    }

    #[test]
    fn signals_and_existing_default_to_empty() {
        let signals = RepoSignals::default();
        assert!(signals.dockerfiles.is_empty() && signals.compose_files.is_empty());
        assert!(signals.devcontainer.is_none() && signals.ecosystems.is_empty());
        assert!(signals.lockfiles.is_empty() && signals.toolchains.is_empty());
        assert_eq!(Existing::default().images, Vec::<String>::new());
        assert_eq!(RunnerChoice::default(), RunnerChoice::Auto);
    }

    #[test]
    fn the_constants_are_the_contracts() {
        assert_eq!(RECIPE_FILE, ".clusia/recipe.toml");
        assert_eq!(STATE_DIR, ".clusia/state");
        assert_eq!(RECIPE_MAX_BYTES, 65_536);
        assert_eq!((RUNS_PER_HEAD, RUNS_KEPT, NEAR_MAX), (5, 20, 30));
        assert_eq!((RESULTS_MAX, MESSAGE_MAX, TAIL_MAX), (5000, 4096, 4096));
        assert_eq!((LOG_HEAD, LOG_TAIL), (2 * 1024 * 1024, 8 * 1024 * 1024));
        assert_eq!(OUTPUT_EVERY_MS, 250);
        assert_eq!(
            [
                STEP_ENGINE,
                STEP_IMAGE,
                STEP_SERVICES,
                STEP_STACK,
                STEP_TESTS
            ],
            ["engine", "image", "services", "stack", "tests"]
        );
    }
}

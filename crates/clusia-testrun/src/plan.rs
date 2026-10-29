//! The setup plan: what must be true before the tests can run, step by step, from the recipe,
//! the environment, the facts the daemon measured and what the reviewer approved.

use std::collections::BTreeMap;

use clusia_core::tests::{
    Approval, Engine, EngineKind, EnvState, PlanCommand, PlanStep, RunnerChoice, RunnerKind,
    STEP_ENGINE, STEP_IMAGE, STEP_SERVICES, STEP_STACK, STEP_TESTS, SetupPlan, StepPlace,
    StepState,
};

use crate::names::sha256_hex;
use crate::recipe::{ImageSource, Recipe, SetupStep, StepInto};

/// The block of a container plan whose recipe has no `[image]`.
pub const NO_IMAGE: &str =
    "This recipe names no image. Add an [image] section, or set runner to local.";

const NO_ENGINE: &str = "No container engine found. Install one, or set tests.runner to local.";
const START_ENGINE: &str = "Start your container engine.";
const NO_SANDBOX: &str = "The local sandbox (sandbox-exec) is not available on this Mac. Install a container engine and try again.";
const NO_SERVICES_LOCAL: &str =
    "Services need a container engine. Install one, or remove [services].";

/// What the daemon measured about the machine and the worktree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    pub engine_running: bool,
    pub image_present: bool,
    /// For each `work:<i>` step id, whether its marker matches the step as it is now.
    pub markers: BTreeMap<String, bool>,
    /// `None` until the mount check has run.
    pub mount_ok: Option<bool>,
    pub services_ready: bool,
    pub stack_up: bool,
    /// The pull request ships its own `.clusia/` folder.
    pub clusia_problem: Option<String>,
    pub compose_problem: Option<String>,
}

/// The runner: a configured one wins, then the recipe's, then a container when an engine was
/// found (running or not).
pub fn effective_runner(
    config: RunnerChoice,
    recipe: RunnerChoice,
    engine: Option<&Engine>,
) -> RunnerKind {
    match (config, recipe) {
        (RunnerChoice::Container, _) | (RunnerChoice::Auto, RunnerChoice::Container) => {
            RunnerKind::Container
        }
        (RunnerChoice::Local, _) | (RunnerChoice::Auto, RunnerChoice::Local) => RunnerKind::Local,
        (RunnerChoice::Auto, RunnerChoice::Auto) if engine.is_some() => RunnerKind::Container,
        (RunnerChoice::Auto, RunnerChoice::Auto) => RunnerKind::Local,
    }
}

/// The marker a finished `work` step leaves: it changes with the command, the image it ran in
/// and the repository's inputs, but not with the step's name or its `network` flag.
pub fn work_marker(step: &SetupStep, image: Option<&str>, inputs_hash: &str) -> String {
    sha256_hex(format!("{}\0{}\0{}", step.run, image.unwrap_or(""), inputs_hash).as_bytes())
}

pub fn setup_plan(
    recipe: &Recipe,
    hash: &str,
    env: &EnvState,
    facts: &Facts,
    approval: Option<&Approval>,
) -> SetupPlan {
    let container = env.runner == RunnerKind::Container;
    if let Some(problem) = &facts.clusia_problem {
        return SetupPlan {
            recipe_hash: hash.to_string(),
            steps: Vec::new(),
            commands: Vec::new(),
            commands_approved: false,
            ready: false,
            blocked: Some(problem.clone()),
        };
    }

    let mut steps = Vec::new();
    if container
        && let Some(engine) = &env.engine
        && !facts.engine_running
        && let Some(start) = &engine.start
    {
        let mut step = plan_step(
            STEP_ENGINE,
            engine_title(engine.kind),
            start,
            StepPlace::Host,
            false,
        );
        step.hint = Some("Waits up to 120 s for the engine to answer".into());
        steps.push(step);
    }
    let compose = recipe
        .services
        .as_ref()
        .filter(|_| container)
        .map(|s| s.compose.as_str());
    if container && let Some(image) = &recipe.image {
        let mut step = plan_step(
            STEP_IMAGE,
            "Build the test image",
            &image_command(recipe, image),
            StepPlace::Container,
            true,
        );
        step.state = done_if(facts.image_present);
        steps.push(step);
    }
    if let Some(compose) = compose {
        let command = ["config --format json", "pull", "build"]
            .map(|verb| compose_command(compose, verb))
            .join("\n");
        let mut step = plan_step(
            STEP_SERVICES,
            "Pull and build the compose services",
            &command,
            StepPlace::Container,
            true,
        );
        step.state = done_if(facts.services_ready);
        steps.push(step);
    }
    for (i, setup) in recipe.setup.iter().enumerate() {
        match (setup.into, container) {
            (StepInto::Image, true) => {}
            (StepInto::Image, false) => {
                let mut step = plan_step(
                    &format!("image:{i}"),
                    &setup.name,
                    &setup.run,
                    StepPlace::LocalSandbox,
                    setup.network,
                );
                step.state = StepState::Skipped {
                    why: "container only".into(),
                };
                steps.push(step);
            }
            (StepInto::Work, _) => {
                let id = format!("work:{i}");
                let place = if container {
                    StepPlace::Container
                } else {
                    StepPlace::LocalSandbox
                };
                let mut step = plan_step(&id, &setup.name, &setup.run, place, setup.network);
                step.state = done_if(facts.markers.get(&id).copied().unwrap_or(false));
                steps.push(step);
            }
        }
    }
    if let (Some(compose), Some(services)) = (compose, &recipe.services) {
        let command = format!(
            "{} {}",
            compose_command(compose, "up -d --wait"),
            services.names.join(" ")
        );
        let mut step = plan_step(
            STEP_STACK,
            "Start the compose services",
            &command,
            StepPlace::Host,
            false,
        );
        step.state = done_if(facts.stack_up);
        steps.push(step);
    }
    for step in &mut steps {
        step.approved =
            matches!(step.state, StepState::Skipped { .. }) || approved(approval, hash, &step.id);
    }

    let commands_approved = approved(approval, hash, STEP_TESTS);
    let blocked = blocked(recipe, env, facts, container);
    let steps_done = steps
        .iter()
        .all(|s| matches!(s.state, StepState::Done | StepState::Skipped { .. }));
    let mounted = !container || facts.mount_ok == Some(true);
    SetupPlan {
        recipe_hash: hash.to_string(),
        commands: commands(recipe),
        commands_approved,
        ready: blocked.is_none() && steps_done && commands_approved && mounted,
        blocked,
        steps,
    }
}

fn blocked(recipe: &Recipe, env: &EnvState, facts: &Facts, container: bool) -> Option<String> {
    if let Some(problem) = &facts.compose_problem {
        return Some(problem.clone());
    }
    let message = if container {
        match &env.engine {
            None => NO_ENGINE,
            Some(engine) if !facts.engine_running && engine.start.is_none() => START_ENGINE,
            Some(_) if recipe.image.is_none() => NO_IMAGE,
            Some(_) => return None,
        }
    } else if !env.sandbox_ok {
        NO_SANDBOX
    } else if recipe.services.is_some() {
        NO_SERVICES_LOCAL
    } else {
        return None;
    };
    Some(message.to_string())
}

fn commands(recipe: &Recipe) -> Vec<PlanCommand> {
    let spec = &recipe.tests;
    let mut out = vec![command("all", &spec.all)];
    for (key, text) in [
        ("one", &spec.one),
        ("some", &spec.some),
        ("list", &spec.list),
    ] {
        if let Some(text) = text {
            out.push(command(key, text));
        }
    }
    out
}

fn command(key: &str, command: &str) -> PlanCommand {
    PlanCommand {
        key: key.into(),
        command: command.into(),
    }
}

fn plan_step(id: &str, title: &str, command: &str, place: StepPlace, network: bool) -> PlanStep {
    PlanStep {
        id: id.into(),
        title: title.into(),
        command: command.into(),
        place,
        network,
        hint: None,
        state: StepState::Missing,
        approved: false,
    }
}

fn done_if(done: bool) -> StepState {
    if done {
        StepState::Done
    } else {
        StepState::Missing
    }
}

/// An approval counts for the recipe it was given for, and for the steps it names.
fn approved(approval: Option<&Approval>, hash: &str, id: &str) -> bool {
    approval.is_some_and(|a| a.recipe_hash == hash && a.steps.contains(id))
}

fn engine_title(kind: EngineKind) -> &'static str {
    match kind {
        EngineKind::Colima => "Start Colima",
        EngineKind::OrbStack => "Start OrbStack",
        EngineKind::DockerDesktop => "Start Docker Desktop",
        EngineKind::Podman => "Start Podman",
        EngineKind::Other => "Start the container engine",
    }
}

/// What the image build does, for the reviewer to read; the exact `docker build` arguments are
/// the image planner's.
fn image_command(recipe: &Recipe, image: &ImageSource) -> String {
    let mut text = match image {
        ImageSource::Base { image } => format!("FROM {image}"),
        ImageSource::Dockerfile { path, target: None } => format!("docker build -f {path}"),
        ImageSource::Dockerfile {
            path,
            target: Some(target),
        } => format!("docker build -f {path} --target {target}"),
        ImageSource::Devcontainer => "build from the devcontainer".to_string(),
    };
    for step in recipe.setup.iter().filter(|s| s.into == StepInto::Image) {
        text.push_str("\nRUN ");
        text.push_str(&step.run);
    }
    text
}

fn compose_command(compose: &str, verb: &str) -> String {
    format!("docker compose -f {compose} {verb}")
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use clusia_core::tests::{EngineKind, Existing, RepoSignals};

    use super::*;
    use crate::recipe::{ImageSource, Services, SetupStep, StepInto, TestsSpec};

    const HASH: &str = "feedface";

    fn colima() -> Engine {
        Engine {
            kind: EngineKind::Colima,
            cli: "/opt/homebrew/bin/docker".into(),
            endpoint: "unix:///Users/octo/.colima/default/docker.sock".into(),
            context: Some("colima".into()),
            running: true,
            start: Some("colima start".into()),
            compose: Some("5.4.0".into()),
        }
    }

    fn env(runner: RunnerKind, engine: Option<Engine>) -> EnvState {
        EnvState {
            head: "abc1234".into(),
            probed_at: 0,
            engine,
            runner,
            note: None,
            sandbox_ok: true,
            signals: RepoSignals::default(),
            existing: Existing::default(),
            inputs_hash: "inputs".into(),
        }
    }

    fn recipe() -> Recipe {
        Recipe {
            runner: RunnerChoice::Auto,
            image: Some(ImageSource::Base {
                image: "rust:1.86".into(),
            }),
            setup: vec![
                SetupStep {
                    name: "Install git".into(),
                    run: "apt-get install -y git".into(),
                    into: StepInto::Image,
                    network: true,
                },
                SetupStep {
                    name: "Fetch dependencies".into(),
                    run: "cargo fetch".into(),
                    into: StepInto::Work,
                    network: true,
                },
            ],
            services: Some(Services {
                compose: "docker-compose.yml".into(),
                names: vec!["postgres".into()],
            }),
            tests: TestsSpec {
                all: "cargo test --workspace".into(),
                one: Some("cargo test --workspace -- --exact {name}".into()),
                some: None,
                list: Some("cargo test --workspace -- --list --format terse".into()),
                results: clusia_core::tests::ResultsFormat::Libtest,
                env: BTreeMap::new(),
            },
        }
    }

    fn running() -> Facts {
        Facts {
            engine_running: true,
            ..Default::default()
        }
    }

    fn done() -> Facts {
        Facts {
            engine_running: true,
            image_present: true,
            markers: BTreeMap::from([("work:1".to_string(), true)]),
            mount_ok: Some(true),
            services_ready: true,
            stack_up: true,
            clusia_problem: None,
            compose_problem: None,
        }
    }

    fn approval(steps: &[&str]) -> Approval {
        Approval {
            recipe_hash: HASH.into(),
            steps: steps.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
            at: 0,
        }
    }

    const EVERYTHING: [&str; 6] = ["image", "services", "work:1", "stack", "tests", "engine"];

    fn container_plan(facts: &Facts, approval: Option<&Approval>) -> SetupPlan {
        setup_plan(
            &recipe(),
            HASH,
            &env(RunnerKind::Container, Some(colima())),
            facts,
            approval,
        )
    }

    fn step(id: &str, title: &str, command: &str, place: StepPlace, network: bool) -> PlanStep {
        PlanStep {
            id: id.into(),
            title: title.into(),
            command: command.into(),
            place,
            network,
            hint: None,
            state: StepState::Missing,
            approved: false,
        }
    }

    #[test]
    fn the_container_plan_lists_each_step_in_order() {
        let plan = container_plan(&running(), None);
        assert_eq!(plan.recipe_hash, HASH);
        assert_eq!(
            plan.steps,
            vec![
                step(
                    "image",
                    "Build the test image",
                    "FROM rust:1.86\nRUN apt-get install -y git",
                    StepPlace::Container,
                    true
                ),
                step(
                    "services",
                    "Pull and build the compose services",
                    "docker compose -f docker-compose.yml config --format json\ndocker compose -f docker-compose.yml pull\ndocker compose -f docker-compose.yml build",
                    StepPlace::Container,
                    true,
                ),
                step(
                    "work:1",
                    "Fetch dependencies",
                    "cargo fetch",
                    StepPlace::Container,
                    true
                ),
                step(
                    "stack",
                    "Start the compose services",
                    "docker compose -f docker-compose.yml up -d --wait postgres",
                    StepPlace::Host,
                    false,
                ),
            ]
        );
        assert_eq!(
            plan.commands,
            vec![
                PlanCommand {
                    key: "all".into(),
                    command: "cargo test --workspace".into()
                },
                PlanCommand {
                    key: "one".into(),
                    command: "cargo test --workspace -- --exact {name}".into()
                },
                PlanCommand {
                    key: "list".into(),
                    command: "cargo test --workspace -- --list --format terse".into()
                },
            ]
        );
        assert!(!plan.commands_approved && !plan.ready);
        assert_eq!(plan.blocked, None);
    }

    #[test]
    fn the_image_step_describes_each_image_source() {
        let mut dockerfile = recipe();
        dockerfile.setup.clear();
        dockerfile.services = None;
        dockerfile.image = Some(ImageSource::Dockerfile {
            path: "ci/Dockerfile".into(),
            target: Some("test".into()),
        });
        let plan = setup_plan(
            &dockerfile,
            HASH,
            &env(RunnerKind::Container, Some(colima())),
            &running(),
            None,
        );
        assert_eq!(
            plan.steps[0].command,
            "docker build -f ci/Dockerfile --target test"
        );
        dockerfile.image = Some(ImageSource::Dockerfile {
            path: "Dockerfile".into(),
            target: None,
        });
        let plan = setup_plan(
            &dockerfile,
            HASH,
            &env(RunnerKind::Container, Some(colima())),
            &running(),
            None,
        );
        assert_eq!(plan.steps[0].command, "docker build -f Dockerfile");
        dockerfile.image = Some(ImageSource::Devcontainer);
        let plan = setup_plan(
            &dockerfile,
            HASH,
            &env(RunnerKind::Container, Some(colima())),
            &running(),
            None,
        );
        assert_eq!(plan.steps[0].command, "build from the devcontainer");
        assert_eq!(
            plan.steps.len(),
            1,
            "no steps and no services: just the image"
        );
    }

    #[test]
    fn the_optional_commands_appear_in_a_fixed_order() {
        let mut with_some = recipe();
        with_some.tests.some = Some("cargo test {names}".into());
        with_some.tests.one = None;
        let plan = setup_plan(
            &with_some,
            HASH,
            &env(RunnerKind::Container, Some(colima())),
            &running(),
            None,
        );
        let keys: Vec<_> = plan.commands.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["all", "some", "list"]);
    }

    #[test]
    fn a_stopped_engine_puts_the_start_step_first() {
        let facts = Facts {
            engine_running: false,
            ..Default::default()
        };
        let plan = container_plan(&facts, None);
        let mut expected = step(
            "engine",
            "Start Colima",
            "colima start",
            StepPlace::Host,
            false,
        );
        expected.hint = Some("Waits up to 120 s for the engine to answer".into());
        assert_eq!(plan.steps[0], expected);
        assert_eq!(plan.steps.len(), 5);
        assert_eq!(plan.blocked, None);
        assert!(!plan.ready);
    }

    #[test]
    fn an_engine_that_cannot_be_started_blocks_with_a_note() {
        let mut other = colima();
        other.kind = EngineKind::Other;
        other.start = None;
        let facts = Facts {
            engine_running: false,
            ..Default::default()
        };
        let plan = setup_plan(
            &recipe(),
            HASH,
            &env(RunnerKind::Container, Some(other)),
            &facts,
            None,
        );
        assert_eq!(
            plan.blocked.as_deref(),
            Some("Start your container engine.")
        );
        assert!(plan.steps.iter().all(|s| s.id != "engine"));
    }

    #[test]
    fn the_facts_mark_the_steps_done() {
        let plan = container_plan(&done(), None);
        assert!(
            plan.steps.iter().all(|s| s.state == StepState::Done),
            "{:?}",
            plan.steps
        );
        let partial = Facts {
            image_present: true,
            ..running()
        };
        let states: Vec<_> = container_plan(&partial, None)
            .steps
            .into_iter()
            .map(|s| s.state)
            .collect();
        assert_eq!(
            states,
            [
                StepState::Done,
                StepState::Missing,
                StepState::Missing,
                StepState::Missing
            ]
        );
    }

    #[test]
    fn ready_needs_image_markers_and_mount() {
        let all = approval(&EVERYTHING);
        let ready = container_plan(&done(), Some(&all));
        assert!(ready.ready && ready.commands_approved && ready.blocked.is_none());

        let cases: [(&str, Facts, Option<Approval>); 8] = [
            (
                "the image is missing",
                Facts {
                    image_present: false,
                    ..done()
                },
                Some(all.clone()),
            ),
            (
                "a work marker is missing",
                Facts {
                    markers: BTreeMap::new(),
                    ..done()
                },
                Some(all.clone()),
            ),
            (
                "a work marker is stale",
                Facts {
                    markers: BTreeMap::from([("work:1".to_string(), false)]),
                    ..done()
                },
                Some(all.clone()),
            ),
            (
                "the mount check has not run",
                Facts {
                    mount_ok: None,
                    ..done()
                },
                Some(all.clone()),
            ),
            (
                "the mount check failed",
                Facts {
                    mount_ok: Some(false),
                    ..done()
                },
                Some(all.clone()),
            ),
            (
                "the services are not pulled",
                Facts {
                    services_ready: false,
                    ..done()
                },
                Some(all.clone()),
            ),
            (
                "the stack is down",
                Facts {
                    stack_up: false,
                    ..done()
                },
                Some(all.clone()),
            ),
            (
                "the commands are not approved",
                done(),
                Some(approval(&["image", "services", "work:1", "stack"])),
            ),
        ];
        for (why, facts, approval) in cases {
            let plan = container_plan(&facts, approval.as_ref());
            assert!(!plan.ready, "{why}");
        }
        assert!(!container_plan(&done(), None).ready, "no approval at all");
    }

    #[test]
    fn an_approval_belongs_to_the_recipe_hash_and_to_its_steps() {
        let wrong = Approval {
            recipe_hash: "other".into(),
            ..approval(&EVERYTHING)
        };
        let plan = container_plan(&done(), Some(&wrong));
        assert!(plan.steps.iter().all(|s| !s.approved));
        assert!(!plan.commands_approved && !plan.ready);

        let some = approval(&["image", "work:1"]);
        let plan = container_plan(&running(), Some(&some));
        let approved: Vec<_> = plan
            .steps
            .iter()
            .map(|s| (s.id.as_str(), s.approved))
            .collect();
        assert_eq!(
            approved,
            [
                ("image", true),
                ("services", false),
                ("work:1", true),
                ("stack", false)
            ]
        );
        assert!(!plan.commands_approved);
        let with_tests = approval(&["tests"]);
        assert!(container_plan(&running(), Some(&with_tests)).commands_approved);
    }

    #[test]
    fn local_mode_skips_image_steps_and_runs_work_in_the_sandbox() {
        let mut local = recipe();
        local.services = None;
        let plan = setup_plan(
            &local,
            HASH,
            &env(RunnerKind::Local, None),
            &Facts::default(),
            None,
        );
        let mut skipped = step(
            "image:0",
            "Install git",
            "apt-get install -y git",
            StepPlace::LocalSandbox,
            true,
        );
        skipped.state = StepState::Skipped {
            why: "container only".into(),
        };
        skipped.approved = true;
        assert_eq!(
            plan.steps,
            vec![
                skipped,
                step(
                    "work:1",
                    "Fetch dependencies",
                    "cargo fetch",
                    StepPlace::LocalSandbox,
                    true
                )
            ]
        );
        assert_eq!(plan.blocked, None);

        let facts = Facts {
            markers: BTreeMap::from([("work:1".to_string(), true)]),
            ..Default::default()
        };
        let all = approval(&["work:1", "tests"]);
        let ready = setup_plan(
            &local,
            HASH,
            &env(RunnerKind::Local, None),
            &facts,
            Some(&all),
        );
        assert!(
            ready.ready,
            "no image, no mount check, a skipped step counts as done"
        );
    }

    #[test]
    fn the_blocks_say_what_to_do() {
        let no_engine = setup_plan(
            &recipe(),
            HASH,
            &env(RunnerKind::Container, None),
            &Facts::default(),
            None,
        );
        assert_eq!(
            no_engine.blocked.as_deref(),
            Some("No container engine found. Install one, or set tests.runner to local.")
        );
        let mut imageless = recipe();
        imageless.image = None;
        let plan = setup_plan(
            &imageless,
            HASH,
            &env(RunnerKind::Container, Some(colima())),
            &running(),
            None,
        );
        assert_eq!(
            plan.blocked.as_deref(),
            Some("This recipe names no image. Add an [image] section, or set runner to local.")
        );
        assert_eq!(plan.blocked.as_deref(), Some(NO_IMAGE));
        assert!(plan.steps.iter().all(|s| s.id != "image"));
        let mut broken = env(RunnerKind::Local, None);
        broken.sandbox_ok = false;
        let mut local = recipe();
        local.services = None;
        let plan = setup_plan(&local, HASH, &broken, &Facts::default(), None);
        assert_eq!(
            plan.blocked.as_deref(),
            Some(
                "The local sandbox (sandbox-exec) is not available on this Mac. Install a container engine and try again."
            )
        );
        let plan = setup_plan(
            &recipe(),
            HASH,
            &env(RunnerKind::Local, None),
            &Facts::default(),
            None,
        );
        assert_eq!(
            plan.blocked.as_deref(),
            Some("Services need a container engine. Install one, or remove [services].")
        );
        for plan in [no_engine, plan] {
            assert!(!plan.ready);
            assert!(!plan.steps.is_empty(), "the steps are still listed");
        }
    }

    #[test]
    fn a_compose_problem_blocks_and_a_shipped_clusia_folder_hides_everything() {
        let facts = Facts {
            compose_problem: Some("Compose 2.20.0 is too old; 2.24.4 or newer is needed".into()),
            ..running()
        };
        let plan = container_plan(&facts, None);
        assert_eq!(
            plan.blocked.as_deref(),
            Some("Compose 2.20.0 is too old; 2.24.4 or newer is needed")
        );
        assert!(!plan.steps.is_empty());

        let shipped = Facts {
            clusia_problem: Some(
                "This pull request ships its own .clusia/ folder; Clúsia won't use it.".into(),
            ),
            compose_problem: Some("other".into()),
            ..done()
        };
        let all = approval(&EVERYTHING);
        let plan = container_plan(&shipped, Some(&all));
        assert_eq!(
            plan.blocked.as_deref(),
            Some("This pull request ships its own .clusia/ folder; Clúsia won't use it.")
        );
        assert!(plan.steps.is_empty() && plan.commands.is_empty());
        assert!(!plan.ready && !plan.commands_approved);
    }

    #[test]
    fn the_runner_comes_from_the_config_then_the_recipe_then_the_engine() {
        let engine = colima();
        let cases = [
            (
                RunnerChoice::Local,
                RunnerChoice::Container,
                Some(&engine),
                RunnerKind::Local,
            ),
            (
                RunnerChoice::Container,
                RunnerChoice::Local,
                None,
                RunnerKind::Container,
            ),
            (
                RunnerChoice::Auto,
                RunnerChoice::Local,
                Some(&engine),
                RunnerKind::Local,
            ),
            (
                RunnerChoice::Auto,
                RunnerChoice::Container,
                None,
                RunnerKind::Container,
            ),
            (
                RunnerChoice::Auto,
                RunnerChoice::Auto,
                Some(&engine),
                RunnerKind::Container,
            ),
            (
                RunnerChoice::Auto,
                RunnerChoice::Auto,
                None,
                RunnerKind::Local,
            ),
        ];
        for (config, recipe, engine, expected) in cases {
            assert_eq!(
                effective_runner(config, recipe, engine),
                expected,
                "{config:?} {recipe:?}"
            );
        }
        let mut stopped = colima();
        stopped.running = false;
        assert_eq!(
            effective_runner(RunnerChoice::Auto, RunnerChoice::Auto, Some(&stopped)),
            RunnerKind::Container,
            "an engine that is found counts, running or not"
        );
    }

    #[test]
    fn a_work_marker_depends_on_the_command_the_image_and_the_inputs() {
        let step = SetupStep {
            name: "Fetch dependencies".into(),
            run: "cargo fetch".into(),
            into: StepInto::Work,
            network: true,
        };
        // sha256("cargo fetch\0clusia/rzorzal-clusia-xxxxxx:0123456789abcdef\0abc123")
        assert_eq!(
            work_marker(
                &step,
                Some("clusia/rzorzal-clusia-xxxxxx:0123456789abcdef"),
                "abc123"
            ),
            "f7fde57e70dcc1ffa0d8e490b08961ade733c4e04740a2299e8dfd0ad009cbd1"
        );
        // sha256("cargo fetch\0\0abc123")
        assert_eq!(
            work_marker(&step, None, "abc123"),
            "2c188a625d66452e395f1e93d73d076b7c3692f4f64a2f11b68a05adbf4eee71"
        );
        let renamed = SetupStep {
            name: "Get deps".into(),
            network: false,
            ..step.clone()
        };
        assert_eq!(
            work_marker(&renamed, None, "abc123"),
            work_marker(&step, None, "abc123")
        );
        let changed = SetupStep {
            run: "cargo fetch --locked".into(),
            ..step.clone()
        };
        assert_ne!(
            work_marker(&changed, None, "abc123"),
            work_marker(&step, None, "abc123")
        );
        assert_ne!(
            work_marker(&step, None, "other"),
            work_marker(&step, None, "abc123")
        );
    }
}

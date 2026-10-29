//! The test recipe: what to build, set up and run for a pull request, read from the agent's JSON
//! or from `.clusia/recipe.toml`. Nothing downstream sees a recipe that did not pass
//! [`Recipe::validate`].

use std::collections::BTreeMap;
use std::fmt::Write as _;

use clusia_core::printable::{is_invisible, plain_relative};
use clusia_core::tests::{NEAR_MAX, RecipeSummary, ResultsFormat, RunnerChoice, TestScope};
use serde::Deserialize;

pub const SETUP_MAX: usize = 20;
pub const NAME_MAX: usize = 80;
pub const COMMAND_MAX: usize = 2000;
pub const ENV_MAX: usize = 50;
pub const ENV_VALUE_MAX: usize = 2000;
pub const TEST_NAME_MAX: usize = 1000;
pub const PATH_MAX: usize = 300;

const NO_ONE: &str = "This recipe has no command for running one test";
const REFUSED: &str = "That test name was refused: it is empty, too long, starts with -, or holds a line break or a NUL";
const NO_NEAR: &str = "No tests near this change were found";

/// Names the daemon or the ecosystem caches set; an entry ending in `*` is a prefix. A recipe's
/// `env` may not touch them: matched case-insensitively.
pub const FORBIDDEN_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "USER",
    "CARGO_HOME",
    "CARGO_TARGET_DIR",
    "GIT_*",
    "DOCKER_*",
    "CLUSIA_*",
    "SSH_*",
    "GITHUB_*",
    "GH_*",
    "NPM_CONFIG_CACHE",
    "YARN_CACHE_FOLDER",
    "NPM_CONFIG_STORE_DIR",
    "PIP_CACHE_DIR",
    "PYTHONPYCACHEPREFIX",
    "XDG_CACHE_HOME",
    "GOCACHE",
    "GOMODCACHE",
    "GOPATH",
    "GRADLE_USER_HOME",
    "MAVEN_OPTS",
    "BUNDLE_PATH",
    "GEM_HOME",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub runner: RunnerChoice,
    pub image: Option<ImageSource>,
    pub setup: Vec<SetupStep>,
    pub services: Option<Services>,
    pub tests: TestsSpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    Devcontainer,
    Dockerfile {
        path: String,
        target: Option<String>,
    },
    Base {
        image: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupStep {
    pub name: String,
    pub run: String,
    pub into: StepInto,
    pub network: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepInto {
    Image,
    #[default]
    Work,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Services {
    pub compose: String,
    pub names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestsSpec {
    pub all: String,
    pub one: Option<String>,
    pub some: Option<String>,
    pub list: Option<String>,
    pub results: ResultsFormat,
    pub env: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecipe {
    runner: Option<RunnerChoice>,
    image: Option<RawImage>,
    #[serde(default)]
    setup: Vec<RawStep>,
    services: Option<Services>,
    tests: RawTests,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawImage {
    devcontainer: Option<bool>,
    dockerfile: Option<String>,
    target: Option<String>,
    base: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    name: String,
    run: String,
    #[serde(default)]
    into: StepInto,
    #[serde(default)]
    network: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTests {
    all: String,
    one: Option<String>,
    some: Option<String>,
    list: Option<String>,
    results: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

/// A problem and the rank of its field, so the reading and validation problems merge in field
/// order: image 0, setup 1, services 2, tests.all 3, one 4, some 5, list 6, results 7, env 8.
type Ranked = (u8, String);

const IMAGE: u8 = 0;
const SETUP: u8 = 1;
const SERVICES: u8 = 2;
const ALL: u8 = 3;
const ONE: u8 = 4;
const SOME: u8 = 5;
const LIST: u8 = 6;
const RESULTS: u8 = 7;
const ENV: u8 = 8;

impl RawRecipe {
    fn into_recipe(self) -> (Recipe, Vec<Ranked>) {
        let mut problems = Vec::new();
        let image = self.image.and_then(|raw| raw.into_source(&mut problems));
        let results = match self.tests.results.as_deref() {
            None => ResultsFormat::Plain,
            Some(text) => ResultsFormat::parse(text).unwrap_or_else(|| {
                problems.push((
                    RESULTS,
                    format!(
                        "tests.results \"{text}\" is not one of junit:<path>, libtest, go-json, pytest, jest, plain"
                    ),
                ));
                ResultsFormat::Plain
            }),
        };
        let recipe = Recipe {
            runner: self.runner.unwrap_or_default(),
            image,
            setup: self
                .setup
                .into_iter()
                .map(|s| SetupStep {
                    name: s.name,
                    run: s.run,
                    into: s.into,
                    network: s.network,
                })
                .collect(),
            services: self.services,
            tests: TestsSpec {
                all: self.tests.all,
                one: self.tests.one,
                some: self.tests.some,
                list: self.tests.list,
                results,
                env: self.tests.env,
            },
        };
        (recipe, problems)
    }
}

impl RawImage {
    fn into_source(self, problems: &mut Vec<Ranked>) -> Option<ImageSource> {
        let only_false_devcontainer = self.devcontainer == Some(false)
            && self.dockerfile.is_none()
            && self.base.is_none()
            && self.target.is_none();
        if only_false_devcontainer {
            problems.push((IMAGE, "image.devcontainer must be true".into()));
            return None;
        }
        let sources = usize::from(self.devcontainer == Some(true))
            + usize::from(self.dockerfile.is_some())
            + usize::from(self.base.is_some());
        if sources != 1 {
            problems.push((
                IMAGE,
                "image must set exactly one of devcontainer, dockerfile and base".into(),
            ));
        }
        if self.target.is_some() && self.dockerfile.is_none() {
            problems.push((IMAGE, "image.target needs image.dockerfile".into()));
        }
        if sources != 1 {
            return None;
        }
        if self.devcontainer == Some(true) {
            Some(ImageSource::Devcontainer)
        } else if let Some(path) = self.dockerfile {
            Some(ImageSource::Dockerfile {
                path,
                target: self.target,
            })
        } else {
            self.base.map(|image| ImageSource::Base { image })
        }
    }
}

impl Recipe {
    pub fn from_json(value: &serde_json::Value) -> Result<Recipe, Vec<String>> {
        let raw =
            serde_json::from_value::<RawRecipe>(value.clone()).map_err(|e| vec![e.to_string()])?;
        Self::checked(raw)
    }

    pub fn parse_toml(text: &str) -> Result<Recipe, Vec<String>> {
        let raw = toml::from_str::<RawRecipe>(text).map_err(|e| vec![e.to_string()])?;
        Self::checked(raw)
    }

    fn checked(raw: RawRecipe) -> Result<Recipe, Vec<String>> {
        let (recipe, mut problems) = raw.into_recipe();
        problems.extend(recipe.ranked_problems());
        if problems.is_empty() {
            return Ok(recipe);
        }
        problems.sort_by_key(|(rank, _)| *rank);
        Err(problems.into_iter().map(|(_, text)| text).collect())
    }

    /// Every problem, in field order.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut problems = self.ranked_problems();
        if problems.is_empty() {
            return Ok(());
        }
        problems.sort_by_key(|(rank, _)| *rank);
        Err(problems.into_iter().map(|(_, text)| text).collect())
    }

    fn ranked_problems(&self) -> Vec<Ranked> {
        let mut out = Vec::new();
        self.image_problems(&mut out);
        self.setup_problems(&mut out);
        self.services_problems(&mut out);
        self.tests_problems(&mut out);
        out
    }

    fn image_problems(&self, out: &mut Vec<Ranked>) {
        match &self.image {
            Some(ImageSource::Dockerfile { path, target }) => {
                if !plain_path(path) {
                    out.push((
                        IMAGE,
                        "image.dockerfile must be a plain relative path".into(),
                    ));
                }
                if let Some(target) = target
                    && !valid_stage_name(target)
                {
                    out.push((IMAGE, "image.target is not a valid stage name".into()));
                }
            }
            Some(ImageSource::Base { image }) if !valid_image_name(image) => {
                out.push((IMAGE, "image.base is not a valid image name".into()));
            }
            _ => {}
        }
    }

    fn setup_problems(&self, out: &mut Vec<Ranked>) {
        if self.setup.len() > SETUP_MAX {
            out.push((
                SETUP,
                format!(
                    "setup has {} steps; the most allowed is {SETUP_MAX}",
                    self.setup.len()
                ),
            ));
            return;
        }
        for (i, step) in self.setup.iter().enumerate() {
            let field = |key: &str| format!("setup[{i}].{key}");
            let checks = [
                check_text(&field("name"), &step.name, NAME_MAX),
                check_text(&field("run"), &step.run, COMMAND_MAX),
            ];
            out.extend(checks.into_iter().flatten().map(|p| (SETUP, p)));
        }
    }

    fn services_problems(&self, out: &mut Vec<Ranked>) {
        let Some(services) = &self.services else {
            return;
        };
        if !plain_path(&services.compose) {
            out.push((
                SERVICES,
                "services.compose must be a plain relative path".into(),
            ));
        }
        if services.names.is_empty() {
            out.push((
                SERVICES,
                "services.names must list at least one service".into(),
            ));
        }
        for (i, name) in services.names.iter().enumerate() {
            if !valid_service_name(name) {
                out.push((
                    SERVICES,
                    format!("services.names[{i}] is not a valid service name"),
                ));
            }
        }
    }

    fn tests_problems(&self, out: &mut Vec<Ranked>) {
        let spec = &self.tests;
        out.extend(check_text("tests.all", &spec.all, COMMAND_MAX).map(|p| (ALL, p)));
        for (rank, field, text, placeholder) in [
            (ONE, "tests.one", &spec.one, "{name}"),
            (SOME, "tests.some", &spec.some, "{names}"),
        ] {
            let Some(text) = text else { continue };
            let problem = check_text(field, text, COMMAND_MAX)
                .or_else(|| placeholder_problem(field, text, placeholder));
            out.extend(problem.map(|p| (rank, p)));
        }
        if let Some(list) = &spec.list {
            out.extend(check_text("tests.list", list, COMMAND_MAX).map(|p| (LIST, p)));
        }
        if let ResultsFormat::Junit { path } = &spec.results
            && !plain_path(path)
        {
            out.push((
                RESULTS,
                "tests.results: the junit path must be a plain relative path".into(),
            ));
        }
        self.env_problems(out);
    }

    fn env_problems(&self, out: &mut Vec<Ranked>) {
        let env = &self.tests.env;
        if env.len() > ENV_MAX {
            out.push((
                ENV,
                format!(
                    "tests.env has {} variables; the most allowed is {ENV_MAX}",
                    env.len()
                ),
            ));
            return;
        }
        for (key, value) in env {
            if !valid_env_name(key) {
                out.push((ENV, format!("tests.env key \"{key}\" is not a valid name")));
            } else if env_is_forbidden(key) {
                out.push((ENV, format!("tests.env key \"{key}\" is reserved")));
            }
            let field = format!("tests.env.{key}");
            if value.chars().count() > ENV_VALUE_MAX {
                out.push((
                    ENV,
                    format!("{field} must be at most {ENV_VALUE_MAX} characters"),
                ));
            } else if value.chars().any(char::is_control) {
                out.push((ENV, format!("{field} holds a control character")));
            } else if value.chars().any(is_invisible) {
                out.push((ENV, format!("{field} holds an invisible character")));
            }
        }
    }

    /// The recipe as `.clusia/recipe.toml`, written by hand so the text is exact.
    pub fn to_toml(&self) -> String {
        let mut out = format!("runner = {}\n", quoted(runner_name(self.runner)));
        match &self.image {
            Some(ImageSource::Devcontainer) => out.push_str("\n[image]\ndevcontainer = true\n"),
            Some(ImageSource::Dockerfile { path, target }) => {
                let _ = write!(out, "\n[image]\ndockerfile = {}\n", quoted(path));
                if let Some(target) = target {
                    let _ = writeln!(out, "target = {}", quoted(target));
                }
            }
            Some(ImageSource::Base { image }) => {
                let _ = write!(out, "\n[image]\nbase = {}\n", quoted(image));
            }
            None => {}
        }
        for step in &self.setup {
            let into = match step.into {
                StepInto::Image => "image",
                StepInto::Work => "work",
            };
            let _ = write!(
                out,
                "\n[[setup]]\nname = {}\nrun = {}\ninto = {}\nnetwork = {}\n",
                quoted(&step.name),
                quoted(&step.run),
                quoted(into),
                step.network
            );
        }
        if let Some(services) = &self.services {
            let names: Vec<_> = services.names.iter().map(|n| quoted(n)).collect();
            let _ = write!(
                out,
                "\n[services]\ncompose = {}\nnames = [{}]\n",
                quoted(&services.compose),
                names.join(", ")
            );
        }
        let spec = &self.tests;
        let _ = write!(out, "\n[tests]\nall = {}\n", quoted(&spec.all));
        for (key, value) in [
            ("one", &spec.one),
            ("some", &spec.some),
            ("list", &spec.list),
        ] {
            if let Some(value) = value {
                let _ = writeln!(out, "{key} = {}", quoted(value));
            }
        }
        let _ = writeln!(out, "results = {}", quoted(&spec.results.to_string()));
        if !spec.env.is_empty() {
            let pairs: Vec<_> = spec
                .env
                .iter()
                .map(|(k, v)| format!("{k} = {}", quoted(v)))
                .collect();
            let _ = writeln!(out, "env = {{ {} }}", pairs.join(", "));
        }
        out
    }

    pub fn summary(&self, hash: &str, by_hand: bool) -> RecipeSummary {
        RecipeSummary {
            hash: hash.to_string(),
            by_hand,
            runner: self.runner,
            image: self.image.as_ref().map(|image| match image {
                ImageSource::Devcontainer => "devcontainer".to_string(),
                ImageSource::Dockerfile { path, target: None } => format!("dockerfile: {path}"),
                ImageSource::Dockerfile {
                    path,
                    target: Some(target),
                } => format!("dockerfile: {path} (target {target})"),
                ImageSource::Base { image } => format!("base: {image}"),
            }),
            setup: self.setup.iter().map(|s| s.name.clone()).collect(),
            all: self.tests.all.clone(),
            one: self.tests.one.clone(),
            some: self.tests.some.clone(),
            list: self.tests.list.clone(),
            results: self.tests.results.clone(),
            env: self.tests.env.clone(),
            services: self
                .services
                .as_ref()
                .map(|s| s.names.clone())
                .unwrap_or_default(),
            problems: Vec::new(),
        }
    }
}

fn runner_name(runner: RunnerChoice) -> &'static str {
    match runner {
        RunnerChoice::Auto => "auto",
        RunnerChoice::Container => "container",
        RunnerChoice::Local => "local",
    }
}

/// `text` as a TOML basic string.
fn quoted(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A recipe's identity: the hex sha256 of the file's bytes. An approval names it.
pub fn recipe_hash(bytes: &[u8]) -> String {
    crate::names::sha256_hex(bytes)
}

/// Whether the recipe's `env` may not set `key`.
pub fn env_is_forbidden(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    FORBIDDEN_ENV
        .iter()
        .any(|entry| match entry.strip_suffix('*') {
            Some(prefix) => key.starts_with(prefix),
            None => key == *entry,
        })
}

/// A text field: 1..=`max` characters, no NUL or control character besides line breaks and tabs,
/// and nothing that draws nothing or reorders what is drawn.
fn check_text(field: &str, text: &str, max: usize) -> Option<String> {
    let length = text.chars().count();
    if text.trim().is_empty() || length > max {
        return Some(format!("{field} must be between 1 and {max} characters"));
    }
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Some(format!("{field} holds a control character"));
    }
    if text.chars().any(is_invisible) {
        return Some(format!("{field} holds an invisible character"));
    }
    None
}

/// `None` when every `placeholder` in `text` is a shell word of its own outside quotes, so
/// substituting a quoted name cannot be unquoted, glued to other text or run by a quote's
/// expansion.
fn placeholder_problem(field: &str, text: &str, placeholder: &str) -> Option<String> {
    #[derive(Clone, Copy, PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let chars: Vec<char> = text.chars().collect();
    let needle: Vec<char> = placeholder.chars().collect();
    let mut quote = Quote::None;
    let mut found = false;
    // A here-document body and `$'…'`/`$"…"` quoting each change what a quote or a newline
    // means to the shell, which this scan does not model, so a command holding the
    // placeholder may not use them outside single quotes.
    let mut risky = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if quote != Quote::Single
            && (chars[i..].starts_with(&['<', '<'])
                || chars[i..].starts_with(&['$', '\''])
                || chars[i..].starts_with(&['$', '"']))
        {
            risky = true;
        }
        if quote == Quote::None && chars[i..].starts_with(&needle) {
            found = true;
            let before = i == 0 || chars[i - 1].is_whitespace();
            let after = chars
                .get(i + needle.len())
                .is_none_or(|n| n.is_whitespace() || ";&|)".contains(*n));
            if !(before && after) {
                return Some(format!(
                    "{field} must use {placeholder} as a word of its own, outside quotes"
                ));
            }
            i += needle.len();
            continue;
        }
        if chars[i..].starts_with(&needle) {
            // Inside quotes: still a use of the placeholder, and a refused one.
            return Some(format!(
                "{field} must use {placeholder} as a word of its own, outside quotes"
            ));
        }
        match (quote, c) {
            (Quote::Single, '\'') | (Quote::Double, '"') => quote = Quote::None,
            (Quote::None, '\'') => quote = Quote::Single,
            (Quote::None, '"') => quote = Quote::Double,
            (Quote::None | Quote::Double, '\\') => i += 1,
            _ => {}
        }
        i += 1;
    }
    if found && risky {
        return Some(format!(
            "{field} must use {placeholder} as a word of its own, outside quotes"
        ));
    }
    (!found).then(|| format!("{field} must contain {placeholder}"))
}

fn plain_path(path: &str) -> bool {
    !path.is_empty() && !path.contains('\0') && plain_relative(path, PATH_MAX)
}

fn valid_image_name(text: &str) -> bool {
    (1..=200).contains(&text.len())
        && !text.starts_with('-')
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/@-".contains(&b))
}

fn valid_stage_name(text: &str) -> bool {
    (1..=128).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn valid_service_name(text: &str) -> bool {
    let mut bytes = text.bytes();
    (1..=80).contains(&text.len())
        && bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && bytes.all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn valid_env_name(key: &str) -> bool {
    let mut bytes = key.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `name` single-quoted for `/bin/sh`, or `None` when quoting cannot carry it: empty, too long,
/// a NUL or a line break, or a leading `-` that would make it an option of the runner.
pub fn quote_name(name: &str) -> Option<String> {
    let bad = name.is_empty()
        || name.chars().count() > TEST_NAME_MAX
        || name.starts_with('-')
        || name.contains(['\0', '\n', '\r']);
    (!bad).then(|| format!("'{}'", name.replace('\'', "'\\''")))
}

/// The shell commands that run `scope`. `{name}` is replaced in one pass, so a name that holds
/// the placeholder is not expanded again.
pub fn commands_for(
    spec: &TestsSpec,
    scope: &TestScope,
    near: &[String],
) -> Result<Vec<String>, String> {
    match scope {
        TestScope::All => Ok(vec![spec.all.clone()]),
        TestScope::One { name } => {
            let one = spec.one.as_deref().ok_or(NO_ONE)?;
            let name = quote_name(name).ok_or(REFUSED)?;
            Ok(vec![one.replace("{name}", &name)])
        }
        TestScope::NearChange => {
            if near.is_empty() {
                return Err(NO_NEAR.to_string());
            }
            let names = &near[..near.len().min(NEAR_MAX)];
            if let Some(some) = spec.some.as_deref() {
                let quoted: Vec<String> = names
                    .iter()
                    .map(|n| quote_name(n).ok_or(REFUSED.to_string()))
                    .collect::<Result<_, _>>()?;
                return Ok(vec![some.replace("{names}", &quoted.join(" "))]);
            }
            let one = spec.one.as_deref().ok_or(NO_ONE)?;
            names
                .iter()
                .map(|n| {
                    let name = quote_name(n).ok_or(REFUSED)?;
                    Ok(one.replace("{name}", &name))
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const FULL_TOML: &str = r#"runner = "auto"

[image]
dockerfile = "Dockerfile"
target = "test"

[[setup]]
name = "Fetch dependencies"
run = "cargo fetch"
network = true

[services]
compose = "docker-compose.yml"
names = ["postgres"]

[tests]
all = "cargo test --workspace"
one = "cargo test --workspace -- --exact {name}"
list = "cargo test --workspace -- --list --format terse"
results = "libtest"
env = { RUST_BACKTRACE = "1" }
"#;

    fn full() -> Recipe {
        Recipe {
            runner: RunnerChoice::Auto,
            image: Some(ImageSource::Dockerfile {
                path: "Dockerfile".into(),
                target: Some("test".into()),
            }),
            setup: vec![SetupStep {
                name: "Fetch dependencies".into(),
                run: "cargo fetch".into(),
                into: StepInto::Work,
                network: true,
            }],
            services: Some(Services {
                compose: "docker-compose.yml".into(),
                names: vec!["postgres".into()],
            }),
            tests: TestsSpec {
                all: "cargo test --workspace".into(),
                one: Some("cargo test --workspace -- --exact {name}".into()),
                some: None,
                list: Some("cargo test --workspace -- --list --format terse".into()),
                results: ResultsFormat::Libtest,
                env: BTreeMap::from([("RUST_BACKTRACE".to_string(), "1".to_string())]),
            },
        }
    }

    fn minimal() -> serde_json::Value {
        json!({"tests": {"all": "cargo test"}})
    }

    fn problems(value: serde_json::Value) -> Vec<String> {
        Recipe::from_json(&value).unwrap_err()
    }

    #[test]
    fn the_specs_recipe_parses_from_toml() {
        assert_eq!(Recipe::parse_toml(FULL_TOML), Ok(full()));
    }

    #[test]
    fn the_same_recipe_parses_from_json() {
        let value = json!({
            "runner": "auto",
            "image": {"dockerfile": "Dockerfile", "target": "test"},
            "setup": [{"name": "Fetch dependencies", "run": "cargo fetch", "network": true}],
            "services": {"compose": "docker-compose.yml", "names": ["postgres"]},
            "tests": {
                "all": "cargo test --workspace",
                "one": "cargo test --workspace -- --exact {name}",
                "list": "cargo test --workspace -- --list --format terse",
                "results": "libtest",
                "env": {"RUST_BACKTRACE": "1"}
            }
        });
        assert_eq!(Recipe::from_json(&value), Ok(full()));
    }

    #[test]
    fn a_minimal_recipe_gets_the_defaults() {
        let recipe = Recipe::from_json(&minimal()).unwrap();
        assert_eq!(recipe.runner, RunnerChoice::Auto);
        assert_eq!(recipe.image, None);
        assert!(recipe.setup.is_empty());
        assert_eq!(recipe.services, None);
        assert_eq!(recipe.tests.results, ResultsFormat::Plain);
        assert!(recipe.tests.env.is_empty());
        let step = Recipe::from_json(&json!({
            "setup": [{"name": "Install", "run": "npm ci"}],
            "tests": {"all": "npm test"}
        }))
        .unwrap()
        .setup
        .remove(0);
        assert_eq!((step.into, step.network), (StepInto::Work, false));
    }

    #[test]
    fn the_other_image_sources_and_result_formats_parse() {
        let recipe = Recipe::from_json(&json!({
            "runner": "container",
            "image": {"devcontainer": true},
            "setup": [{"name": "Tools", "run": "apt-get install -y git", "into": "image", "network": true}],
            "tests": {"all": "pytest", "some": "pytest {names}", "results": "junit:out/report.xml"}
        }))
        .unwrap();
        assert_eq!(recipe.runner, RunnerChoice::Container);
        assert_eq!(recipe.image, Some(ImageSource::Devcontainer));
        assert_eq!(recipe.setup[0].into, StepInto::Image);
        assert_eq!(recipe.tests.some.as_deref(), Some("pytest {names}"));
        assert_eq!(
            recipe.tests.results,
            ResultsFormat::Junit {
                path: "out/report.xml".into()
            }
        );
        let base =
            Recipe::from_json(&json!({"image": {"base": "rust:1.86"}, "tests": {"all": "x"}}))
                .unwrap();
        assert_eq!(
            base.image,
            Some(ImageSource::Base {
                image: "rust:1.86".into()
            })
        );
    }

    #[test]
    fn a_recipe_without_tests_or_with_an_unknown_key_is_refused() {
        let missing = problems(json!({"runner": "auto"}));
        assert!(missing.iter().any(|p| p.contains("tests")), "{missing:?}");
        let unknown = problems(json!({"tests": {"all": "x"}, "system": {}}));
        assert!(
            unknown.iter().any(|p| p.contains("unknown field `system`")),
            "{unknown:?}"
        );
        let nested = problems(json!({"tests": {"all": "x", "retries": 3}}));
        assert!(
            nested.iter().any(|p| p.contains("unknown field `retries`")),
            "{nested:?}"
        );
        let toml = Recipe::parse_toml("[tests]\nall = \"x\"\nbogus = 1\n").unwrap_err();
        assert!(
            toml.iter().any(|p| p.contains("unknown field `bogus`")),
            "{toml:?}"
        );
        let broken = Recipe::parse_toml("[tests\n").unwrap_err();
        assert_eq!(broken.len(), 1);
    }

    #[test]
    fn to_toml_writes_the_spec_shape() {
        assert_eq!(
            full().to_toml(),
            FULL_TOML.replace("network = true\n", "into = \"work\"\nnetwork = true\n")
        );
    }

    #[test]
    fn to_toml_round_trips_every_shape() {
        let mut hard = full();
        hard.runner = RunnerChoice::Local;
        hard.image = Some(ImageSource::Base {
            image: "rust:1.86".into(),
        });
        hard.setup.push(SetupStep {
            name: "Say \"hi\"".into(),
            run: "echo \"hi\"\nls\t-l \\ done".into(),
            into: StepInto::Image,
            network: false,
        });
        hard.tests.some = Some("cargo test {names}".into());
        hard.tests.results = ResultsFormat::Junit {
            path: "target/junit.xml".into(),
        };
        hard.tests.env.insert("OCTO_MODE".into(), "a\"b".into());
        for recipe in [hard, Recipe::from_json(&minimal()).unwrap(), full()] {
            assert_eq!(
                Recipe::parse_toml(&recipe.to_toml()),
                Ok(recipe.clone()),
                "{}",
                recipe.to_toml()
            );
        }
        let devcontainer =
            Recipe::from_json(&json!({"image": {"devcontainer": true}, "tests": {"all": "x"}}))
                .unwrap();
        assert!(
            devcontainer
                .to_toml()
                .contains("[image]\ndevcontainer = true\n")
        );
    }

    #[test]
    fn the_hash_is_the_sha256_of_the_bytes() {
        assert_eq!(
            recipe_hash(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            recipe_hash(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_ne!(recipe_hash(FULL_TOML.as_bytes()), recipe_hash(b"x"));
    }

    #[test]
    fn the_summary_is_what_the_window_shows() {
        let summary = full().summary("abc", true);
        assert_eq!(
            summary,
            RecipeSummary {
                hash: "abc".into(),
                by_hand: true,
                runner: RunnerChoice::Auto,
                image: Some("dockerfile: Dockerfile (target test)".into()),
                setup: vec!["Fetch dependencies".into()],
                all: "cargo test --workspace".into(),
                one: Some("cargo test --workspace -- --exact {name}".into()),
                some: None,
                list: Some("cargo test --workspace -- --list --format terse".into()),
                results: ResultsFormat::Libtest,
                env: BTreeMap::from([("RUST_BACKTRACE".to_string(), "1".to_string())]),
                services: vec!["postgres".into()],
                problems: Vec::new(),
            }
        );
        let mut other = full();
        other.image = Some(ImageSource::Devcontainer);
        assert_eq!(
            other.summary("h", false).image.as_deref(),
            Some("devcontainer")
        );
        other.image = Some(ImageSource::Base {
            image: "rust:1.86".into(),
        });
        assert_eq!(
            other.summary("h", false).image.as_deref(),
            Some("base: rust:1.86")
        );
        other.image = Some(ImageSource::Dockerfile {
            path: "ci/Dockerfile".into(),
            target: None,
        });
        assert_eq!(
            other.summary("h", false).image.as_deref(),
            Some("dockerfile: ci/Dockerfile")
        );
        other.image = None;
        assert_eq!(other.summary("h", false).image, None);
    }

    #[test]
    fn every_problem_is_reported_in_field_order() {
        let got = problems(json!({
            "image": {"dockerfile": "a", "base": "b"},
            "setup": [{"name": "", "run": "ok"}],
            "services": {"compose": "/abs.yml", "names": []},
            "tests": {"all": "", "one": "no placeholder", "some": "no placeholder"}
        }));
        assert_eq!(
            got,
            [
                "image must set exactly one of devcontainer, dockerfile and base",
                "setup[0].name must be between 1 and 80 characters",
                "services.compose must be a plain relative path",
                "services.names must list at least one service",
                "tests.all must be between 1 and 2000 characters",
                "tests.one must contain {name}",
                "tests.some must contain {names}",
            ]
        );
    }

    #[test]
    fn the_limits_are_enforced() {
        let many: Vec<_> = (0..21)
            .map(|i| json!({"name": format!("s{i}"), "run": "true"}))
            .collect();
        assert_eq!(
            problems(json!({"setup": many, "tests": {"all": "x"}})),
            ["setup has 21 steps; the most allowed is 20"]
        );
        let long_name = "n".repeat(81);
        assert_eq!(
            problems(json!({"setup": [{"name": long_name, "run": "x"}], "tests": {"all": "x"}})),
            ["setup[0].name must be between 1 and 80 characters"]
        );
        assert!(
            Recipe::from_json(
                &json!({"setup": [{"name": "n".repeat(80), "run": "x"}], "tests": {"all": "x"}})
            )
            .is_ok()
        );
        assert_eq!(
            problems(json!({"tests": {"all": "x".repeat(2001)}})),
            ["tests.all must be between 1 and 2000 characters"]
        );
        assert!(Recipe::from_json(&json!({"tests": {"all": "x".repeat(2000)}})).is_ok());
        let env: serde_json::Map<String, serde_json::Value> =
            (0..51).map(|i| (format!("OCTO_{i}"), json!("1"))).collect();
        assert_eq!(
            problems(json!({"tests": {"all": "x", "env": env}})),
            ["tests.env has 51 variables; the most allowed is 50"]
        );
        assert_eq!(
            problems(json!({"tests": {"all": "x", "env": {"OCTO": "v".repeat(2001)}}})),
            ["tests.env.OCTO must be at most 2000 characters"]
        );
    }

    #[test]
    fn a_nul_or_a_control_character_is_refused_but_a_line_break_in_a_script_is_not() {
        assert_eq!(
            problems(json!({"setup": [{"name": "a", "run": "a\u{0}b"}], "tests": {"all": "x"}})),
            ["setup[0].run holds a control character"]
        );
        assert_eq!(
            problems(json!({"tests": {"all": "a\u{1b}[2Jb"}})),
            ["tests.all holds a control character"]
        );
        assert_eq!(
            problems(json!({"setup": [{"name": "a\u{0}", "run": "x"}], "tests": {"all": "x"}})),
            ["setup[0].name holds a control character"]
        );
        assert!(Recipe::from_json(&json!({"tests": {"all": "set -e\n\tcargo test"}})).is_ok());
        assert_eq!(
            problems(json!({"tests": {"all": "x", "env": {"OCTO": "a\nb"}}})),
            ["tests.env.OCTO holds a control character"],
            "an env value may not hold a line break"
        );
    }

    #[test]
    fn invisible_and_bidi_characters_are_refused_everywhere() {
        for hidden in ['\u{202e}', '\u{200b}', '\u{feff}', '\u{2066}'] {
            let text = format!("echo ok #{hidden}");
            assert_eq!(
                problems(json!({"setup": [{"name": "a", "run": text}], "tests": {"all": "x"}})),
                ["setup[0].run holds an invisible character"],
                "{hidden:?}"
            );
        }
        assert_eq!(
            problems(
                json!({"setup": [{"name": "Fetch\u{200b}", "run": "x"}], "tests": {"all": "x"}})
            ),
            ["setup[0].name holds an invisible character"]
        );
        assert_eq!(
            problems(json!({"tests": {"all": "cargo test\u{202e}", "env": {"OCTO": "v\u{feff}"}}})),
            [
                "tests.all holds an invisible character",
                "tests.env.OCTO holds an invisible character"
            ]
        );
        assert!(
            Recipe::from_json(&json!({"tests": {"all": "echo naïve ✓ 日本"}})).is_ok(),
            "ordinary non-ASCII stays"
        );
    }

    #[test]
    fn a_placeholder_must_be_a_word_of_its_own_outside_quotes() {
        let one = |text: &str| problems(json!({"tests": {"all": "x", "one": text}}));
        let bare = "tests.one must use {name} as a word of its own, outside quotes";
        for bad in [
            "cargo test \"{name}\"",
            "cargo test '{name}'",
            "cargo test x{name}",
            "cargo test {name}x",
            "cargo test --exact={name}",
            "echo \"run {name}\"",
            "echo 'run {name}'",
            "cargo test $({name})x",
            "cat <<EOF\n{name}\nEOF",
            "echo $'a\\' {name}",
            "echo $\"a\" {name}",
        ] {
            assert_eq!(one(bad), [bare], "{bad}");
        }
        for good in [
            "cargo test {name}",
            "cargo test {name};",
            "(cargo test {name})",
            "a | b {name} | c",
            "cargo test -- {name}\n",
            "echo 'x y' {name}",
            "echo it\\'s {name}",
            "a {name}&& b",
            "t {name} {name}",
        ] {
            assert!(
                Recipe::from_json(&json!({"tests": {"all": "x", "one": good}})).is_ok(),
                "{good}"
            );
        }
        assert_eq!(one("cargo test"), ["tests.one must contain {name}"]);
        assert_eq!(
            problems(json!({"tests": {"all": "x", "some": "cargo test \"{names}\""}})),
            ["tests.some must use {names} as a word of its own, outside quotes"]
        );
        assert!(
            Recipe::from_json(&json!({"tests": {"all": "x", "some": "cargo test {names}"}}))
                .is_ok()
        );
    }

    #[test]
    fn problems_from_reading_and_from_validate_merge_in_field_order() {
        let got = problems(json!({
            "image": {"dockerfile": "a", "base": "b"},
            "services": {"compose": "c.yml", "names": []},
            "tests": {"all": "x", "one": "no placeholder", "results": "tap", "env": {"PATH": "1"}}
        }));
        assert_eq!(
            got,
            [
                "image must set exactly one of devcontainer, dockerfile and base",
                "services.names must list at least one service",
                "tests.one must contain {name}",
                "tests.results \"tap\" is not one of junit:<path>, libtest, go-json, pytest, jest, plain",
                "tests.env key \"PATH\" is reserved",
            ]
        );
    }

    #[test]
    fn image_sources_are_checked() {
        let case =
            |image: serde_json::Value| problems(json!({"image": image, "tests": {"all": "x"}}));
        assert_eq!(
            case(json!({})),
            ["image must set exactly one of devcontainer, dockerfile and base"]
        );
        assert_eq!(
            case(json!({"devcontainer": false})),
            ["image.devcontainer must be true"]
        );
        assert_eq!(
            case(json!({"base": "rust", "target": "t"})),
            ["image.target needs image.dockerfile"]
        );
        assert_eq!(
            case(json!({"dockerfile": "../Dockerfile"})),
            ["image.dockerfile must be a plain relative path"]
        );
        assert_eq!(
            case(json!({"dockerfile": "D", "target": "a b"})),
            ["image.target is not a valid stage name"]
        );
        for base in [
            "rust:1.86\nRUN curl x | sh",
            "-v /:/x",
            "rust 1.86",
            "",
            "a\u{0}b",
            "x".repeat(201).as_str(),
        ] {
            assert_eq!(
                case(json!({"base": base})),
                ["image.base is not a valid image name"],
                "{base:?}"
            );
        }
        for base in [
            "rust:1.86",
            "ghcr.io/octo/hubot:v2",
            "node@sha256:0123456789abcdef",
            "alpine",
        ] {
            assert!(
                Recipe::from_json(&json!({"image": {"base": base}, "tests": {"all": "x"}})).is_ok(),
                "{base}"
            );
        }
    }

    #[test]
    fn services_and_results_are_checked() {
        let svc = |services: serde_json::Value| {
            problems(json!({"services": services, "tests": {"all": "x"}}))
        };
        assert_eq!(
            svc(json!({"compose": "c.yml", "names": ["db", "-x"]})),
            ["services.names[1] is not a valid service name"]
        );
        assert_eq!(
            svc(json!({"compose": "c.yml"})).len(),
            1,
            "names is required"
        );
        assert_eq!(
            problems(json!({"tests": {"all": "x", "results": "tap"}})),
            [
                "tests.results \"tap\" is not one of junit:<path>, libtest, go-json, pytest, jest, plain"
            ]
        );
        assert_eq!(
            problems(json!({"tests": {"all": "x", "results": "junit:/etc/passwd"}})),
            ["tests.results: the junit path must be a plain relative path"]
        );
    }

    #[test]
    fn the_environment_may_not_touch_what_clusia_sets() {
        for key in [
            "PATH",
            "path",
            "HOME",
            "TMPDIR",
            "USER",
            "CARGO_HOME",
            "CARGO_TARGET_DIR",
            "GIT_DIR",
            "git_ssh_command",
            "DOCKER_HOST",
            "CLUSIA_SOCKET",
            "SSH_AUTH_SOCK",
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "npm_config_cache",
            "NPM_CONFIG_CACHE",
            "YARN_CACHE_FOLDER",
            "npm_config_store_dir",
            "PIP_CACHE_DIR",
            "PYTHONPYCACHEPREFIX",
            "XDG_CACHE_HOME",
            "GOCACHE",
            "GOMODCACHE",
            "GOPATH",
            "GRADLE_USER_HOME",
            "MAVEN_OPTS",
            "BUNDLE_PATH",
            "GEM_HOME",
        ] {
            assert!(env_is_forbidden(key), "{key}");
            assert_eq!(
                problems(json!({"tests": {"all": "x", "env": {key: "1"}}})),
                [format!("tests.env key \"{key}\" is reserved")]
            );
        }
        for key in [
            "RUST_BACKTRACE",
            "DATABASE_URL",
            "OCTO_MODE",
            "GITLAB_CI",
            "GHOST",
        ] {
            assert!(!env_is_forbidden(key), "{key}");
        }
        assert_eq!(
            problems(json!({"tests": {"all": "x", "env": {"1X": "v", "A-B": "v", "": "v"}}})),
            [
                "tests.env key \"\" is not a valid name",
                "tests.env key \"1X\" is not a valid name",
                "tests.env key \"A-B\" is not a valid name",
            ]
        );
    }

    #[test]
    fn names_are_quoted_and_line_breaks_refused() {
        assert_eq!(
            quote_name("tests::it_works").as_deref(),
            Some("'tests::it_works'")
        );
        assert_eq!(quote_name("it's").as_deref(), Some("'it'\\''s'"));
        assert_eq!(
            quote_name("a b; rm -rf /").as_deref(),
            Some("'a b; rm -rf /'")
        );
        assert_eq!(quote_name("$(id)").as_deref(), Some("'$(id)'"));
        for refused in [
            "line\nbreak",
            "cr\rx",
            "nul\u{0}x",
            "",
            "-pplugin",
            "--exact",
            "-",
        ] {
            assert_eq!(quote_name(refused), None, "{refused:?}");
        }
        assert_eq!(
            quote_name("a-b").as_deref(),
            Some("'a-b'"),
            "only a leading dash is refused"
        );
        assert!(quote_name(&"n".repeat(1000)).is_some());
        assert_eq!(quote_name(&"n".repeat(1001)), None);

        let spec = TestsSpec {
            all: "cargo test".into(),
            one: Some("cargo test -- --exact {name}".into()),
            some: None,
            list: None,
            results: ResultsFormat::Plain,
            env: BTreeMap::new(),
        };
        assert_eq!(
            commands_for(
                &spec,
                &TestScope::One {
                    name: "it's; touch /tmp/x".into()
                },
                &[]
            ),
            Ok(vec![
                "cargo test -- --exact 'it'\\''s; touch /tmp/x'".to_string()
            ])
        );
        let refused = "That test name was refused: it is empty, too long, starts with -, or holds a line break or a NUL";
        for bad in ["a\nb", "a\u{0}b", "", "-pplugin"] {
            assert_eq!(
                commands_for(&spec, &TestScope::One { name: bad.into() }, &[]),
                Err(refused.to_string()),
                "{bad:?}"
            );
        }
        assert_eq!(
            commands_for(
                &spec,
                &TestScope::NearChange,
                &["ok::a".to_string(), "bad\nname".to_string()]
            ),
            Err(refused.to_string())
        );
    }

    #[test]
    fn a_quoted_name_reaches_the_program_exactly() {
        for name in [
            "a b",
            "it's",
            "$(id)",
            "`x`",
            "x;y",
            "*",
            "a-n",
            "\\",
            "a\"b",
            "tests::a<b>",
            "~",
            "{name}",
            "ação ✓",
        ] {
            let quoted = quote_name(name).unwrap();
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf %s {quoted}"))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(out.stdout).unwrap(), name, "{quoted}");
        }
    }

    #[test]
    fn the_placeholder_is_replaced_in_one_pass() {
        let mut spec = full().tests;
        spec.one = Some("t {name} {name}".into());
        assert_eq!(
            commands_for(
                &spec,
                &TestScope::One {
                    name: "{name}".into()
                },
                &[]
            ),
            Ok(vec!["t '{name}' '{name}'".to_string()])
        );
    }

    fn spec_with(one: Option<&str>, some: Option<&str>) -> TestsSpec {
        TestsSpec {
            all: "cargo test".into(),
            one: one.map(String::from),
            some: some.map(String::from),
            list: None,
            results: ResultsFormat::Plain,
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn commands_for_each_scope() {
        let spec = spec_with(Some("cargo test -- --exact {name}"), None);
        assert_eq!(
            commands_for(&spec, &TestScope::All, &[]),
            Ok(vec!["cargo test".to_string()])
        );
        assert_eq!(
            commands_for(
                &spec,
                &TestScope::One {
                    name: "a::b".into()
                },
                &[]
            ),
            Ok(vec!["cargo test -- --exact 'a::b'".to_string()])
        );
        let near = ["a::b".to_string(), "c::d".to_string()];
        assert_eq!(
            commands_for(&spec, &TestScope::NearChange, &near),
            Ok(vec![
                "cargo test -- --exact 'a::b'".to_string(),
                "cargo test -- --exact 'c::d'".to_string(),
            ])
        );
        let some = spec_with(
            Some("cargo test -- --exact {name}"),
            Some("cargo test -- {names}"),
        );
        assert_eq!(
            commands_for(&some, &TestScope::NearChange, &near),
            Ok(vec!["cargo test -- 'a::b' 'c::d'".to_string()])
        );
    }

    #[test]
    fn a_missing_command_or_an_empty_near_set_is_an_error() {
        let none = spec_with(None, None);
        assert_eq!(
            commands_for(&none, &TestScope::One { name: "a".into() }, &[]),
            Err("This recipe has no command for running one test".to_string())
        );
        assert_eq!(
            commands_for(&none, &TestScope::NearChange, &["a".to_string()]),
            Err("This recipe has no command for running one test".to_string())
        );
        let spec = spec_with(Some("t {name}"), Some("t {names}"));
        assert_eq!(
            commands_for(&spec, &TestScope::NearChange, &[]),
            Err("No tests near this change were found".to_string()),
            "never a silent All"
        );
    }

    #[test]
    fn near_change_is_capped() {
        let names: Vec<String> = (0..35).map(|i| format!("t{i}")).collect();
        let one = spec_with(Some("t {name}"), None);
        let commands = commands_for(&one, &TestScope::NearChange, &names).unwrap();
        assert_eq!(commands.len(), NEAR_MAX);
        assert_eq!(commands[29], "t 't29'");
        let some = spec_with(Some("t {name}"), Some("t {names}"));
        let commands = commands_for(&some, &TestScope::NearChange, &names).unwrap();
        assert_eq!(commands.len(), 1);
        assert!(commands[0].ends_with("'t29'") && !commands[0].contains("'t30'"));
    }
}

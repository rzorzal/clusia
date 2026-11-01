//! Run Function tests: the changed functions, their cases and drivers, the results, and the checks and hashes every reader and writer of them shares.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::printable::{is_invisible, plain_relative, printable};
use crate::tests::RunnerKind;

/// The folder under the worktree that holds each function's cases and driver.
pub const FUNCTIONS_DIR: &str = ".clusia/functions";
/// The most changed functions the agent writes cases for in one turn.
pub const FUNCTIONS_MAX: usize = 12;
/// The most changed functions the view lists.
pub const LISTED_MAX: usize = 50;
/// The most cases a fresh turn writes.
pub const CASES_FIRST: usize = 6;
/// The most cases a function keeps.
pub const CASES_MAX: usize = 20;
/// The most parameters a function's cases take.
pub const PARAMS_MAX: usize = 12;
/// The longest value of a parameter, in characters.
pub const VALUE_MAX: usize = 2048;
/// The longest case name, in characters.
pub const CASE_NAME_MAX: usize = 120;
/// The longest returned text, error or state value a result keeps, in characters.
pub const RESULT_TEXT_MAX: usize = 4096;
/// The most state entries a case expects or a result reports.
pub const STATE_MAX: usize = 20;
/// The largest driver, in bytes.
pub const DRIVER_MAX_BYTES: usize = 32_768;
/// The longest one case runs, in seconds.
pub const CASE_TIMEOUT_SECS: u64 = 300;
/// The fence info string of the block that carries one function's cases.
pub const FUNCTION_INFO: &str = "clusia-function";
/// The refusal of a run whose `seen` hash is not the current one.
pub const CASES_CHANGED: &str = "The cases changed since you saw them; look at them again";
/// The refusal of a run with nothing to run.
pub const NO_CASES: &str = "There are no cases to run";
/// The message of a case whose driver printed no `done` line.
pub const NO_RESULT: &str = "The driver ended without a result";

/// The languages a driver can be written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FunctionLang {
    Rust,
    TypeScript,
    JavaScript,
    Python,
}

impl FunctionLang {
    /// Every language, in the order of the recipe's TOML, its summary and the plan's commands.
    pub const ALL: [FunctionLang; 4] = [
        FunctionLang::Rust,
        FunctionLang::Python,
        FunctionLang::TypeScript,
        FunctionLang::JavaScript,
    ];

    /// The recipe key and the call index's name of the language.
    pub fn key(self) -> &'static str {
        match self {
            FunctionLang::Rust => "rust",
            FunctionLang::Python => "python",
            FunctionLang::TypeScript => "typescript",
            FunctionLang::JavaScript => "javascript",
        }
    }

    /// The driver file's extension; JavaScript drivers are always ES modules.
    pub fn ext(self) -> &'static str {
        match self {
            FunctionLang::Rust => "rs",
            FunctionLang::Python => "py",
            FunctionLang::TypeScript => "ts",
            FunctionLang::JavaScript => "mjs",
        }
    }

    /// The name shown to the reviewer.
    pub fn label(self) -> &'static str {
        match self {
            FunctionLang::Rust => "Rust",
            FunctionLang::Python => "Python",
            FunctionLang::TypeScript => "TypeScript",
            FunctionLang::JavaScript => "JavaScript",
        }
    }
}

/// The language of a call index entry, when drivers support it.
pub fn lang_of(calls_lang: &str) -> Option<FunctionLang> {
    FunctionLang::ALL
        .into_iter()
        .find(|lang| lang.key() == calls_lang)
}

/// What kind of text a parameter holds, which decides how its value is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamKind {
    Text,
    Int,
    Float,
    Bool,
    Json,
}

/// One input of a function's driver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Param {
    /// The identifier; the driver reads `CLUSIA_ARG_<NAME>`.
    pub name: String,
    /// What the window calls it.
    pub label: String,
    pub kind: ParamKind,
}

/// What a case expects of the call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Expect {
    /// The text the call should return.
    pub returned: Option<String>,
    /// The state the call should leave, by name.
    pub state: BTreeMap<String, String>,
}

/// One set of values to call a function with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Case {
    pub id: String,
    pub name: String,
    pub values: BTreeMap<String, String>,
    pub expect: Expect,
}

/// The agent's part of a function: what `function.json` holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredFunction {
    pub qualified: String,
    pub file: String,
    pub lang: FunctionLang,
    /// The signature at the head the cases were written for.
    pub signature: String,
    pub params: Vec<Param>,
    pub state_label: Option<String>,
    /// Why no driver can call it, when the agent says so.
    pub unsupported: Option<String>,
    /// Hash of the driver's bytes.
    pub driver_hash: Option<String>,
    pub cases: Vec<Case>,
    /// The number the next agent case takes.
    pub next: u32,
    pub written_head: String,
    pub written_at: i64,
}

/// The reviewer's part of a function: what `edits.json` holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Edits {
    /// Values the reviewer changed, by case id, then parameter.
    pub values: BTreeMap<String, BTreeMap<String, String>>,
    /// Names the reviewer changed, by case id.
    pub names: BTreeMap<String, String>,
    /// Cases the reviewer added.
    pub added: Vec<Case>,
    /// The number the next reviewer case takes.
    pub next: u32,
}

/// Who wrote a case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseOrigin {
    Agent,
    Reviewer,
}

/// How the pull request changed a function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionChange {
    Changed,
    New,
    Signature,
    Moved,
}

/// Whether a function's cases can run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Ready,
    NoCases,
    NoCommand,
    Unsupported { reason: String },
    Stale { reason: String },
}

/// A case as the window shows it, the reviewer's edits laid over the agent's values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseView {
    pub id: String,
    pub name: String,
    pub origin: CaseOrigin,
    /// The reviewer changed something of an agent case; always true for a reviewer's own.
    pub edited: bool,
    pub values: BTreeMap<String, String>,
    pub expect: Expect,
    /// `case_hash` of what would run now.
    pub hash: String,
    /// The newest result, whatever it ran with; the reader compares `hash` and `head`.
    pub result: Option<CaseResult>,
}

/// A changed function as the window shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionView {
    /// The function's id on the wire and its folder name.
    pub slug: String,
    pub qualified: String,
    pub name: String,
    pub file: String,
    pub lang: Option<FunctionLang>,
    pub change: FunctionChange,
    pub signature: String,
    pub start_line: u32,
    /// The first and last changed line of the function, when the diff touches it.
    pub diff_lines: Option<(u32, u32)>,
    pub params: Vec<Param>,
    pub state_label: Option<String>,
    pub support: Support,
    pub cases: Vec<CaseView>,
    /// The driver's path relative to the worktree.
    pub driver: Option<String>,
    /// `function_hash` of what a run would use.
    pub hash: String,
}

/// A case of a function.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseRef {
    pub function: String,
    pub case: String,
}

/// What runs for the functions now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionsActivity {
    Writing {
        function: Option<String>,
        ask: Option<CasesAsk>,
    },
    Running {
        run: String,
        scope: CaseScope,
        total: u32,
        done: u32,
        current: Option<CaseRef>,
    },
}

/// Everything the Run Function tests sub-tab shows for a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionsView {
    pub head: String,
    pub functions: Vec<FunctionView>,
    /// Files of the pull request that no language pack reads.
    pub unsupported: Vec<String>,
    /// The call index is not built yet.
    pub reading: bool,
    pub note: Option<String>,
    pub activity: Option<FunctionsActivity>,
    /// Why the last case turn failed; not part of the hash.
    #[serde(default)]
    pub failed: Option<String>,
    pub hash: String,
}

impl FunctionsView {
    /// An empty view of `head`.
    pub fn new(head: impl Into<String>) -> Self {
        FunctionsView {
            head: head.into(),
            functions: Vec::new(),
            unsupported: Vec::new(),
            reading: false,
            note: None,
            activity: None,
            failed: None,
            hash: view_hash(&[]),
        }
    }
}

/// How a case ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseStatus {
    Passed,
    Failed,
    Error,
    TimedOut,
    Stopped,
}

/// One named piece of state after a call, against what the case expected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateCheck {
    pub name: String,
    /// What the driver reported; none when it reported nothing of that name.
    pub got: Option<String>,
    pub expected: Option<String>,
}

/// What one run of one case produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseResult {
    pub case: String,
    pub head: String,
    /// The `case_hash` it ran with.
    pub hash: String,
    pub status: CaseStatus,
    pub returned: Option<String>,
    /// The case's `expect.returned` when it ran.
    #[serde(default)]
    pub returned_expected: Option<String>,
    pub error: Option<String>,
    /// Every expected name, then the reported ones, in report order.
    pub state: Vec<StateCheck>,
    pub output: String,
    pub took_ms: u64,
    pub runner: RunnerKind,
    pub exit_code: Option<i32>,
    pub message: Option<String>,
    pub at: i64,
}

/// Which cases a run covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseScope {
    All,
    Function { function: String },
    Case { function: String, case: String },
}

/// What the reviewer asks the agent to write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CasesAsk {
    Fresh,
    More,
    Values { case: String },
}

/// A case the reviewer saves: a new one when `id` is none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseInput {
    pub id: Option<String>,
    pub name: String,
    pub values: BTreeMap<String, String>,
}

/// A function's driver, as the window shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverCode {
    pub function: String,
    /// Relative to the worktree.
    pub path: String,
    pub text: String,
    pub hash: String,
}

/// A case in an agent's block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockCase {
    pub name: String,
    pub values: BTreeMap<String, String>,
    pub expect: Expect,
}

/// The agent's answer for one function, checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionBlock {
    pub function: String,
    pub file: String,
    pub unsupported: Option<String>,
    pub state_label: Option<String>,
    pub params: Vec<Param>,
    pub driver: Option<String>,
    pub cases: Vec<BlockCase>,
}

/// A changed function as a prompt names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionBrief {
    pub slug: String,
    pub qualified: String,
    pub file: String,
    pub lang: FunctionLang,
    pub change: FunctionChange,
    pub signature: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// What the prompt of a turn that writes cases for one function is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasePromptContext {
    pub pr_title: String,
    pub forked: bool,
    pub function: FunctionBrief,
    pub ask: CasesAsk,
    pub params: Vec<Param>,
    pub cases: Vec<Case>,
    pub driver: Option<String>,
    pub command: Option<String>,
}

/// The prompt of a turn that writes cases for one function.
pub fn case_prompt(_ctx: &CasePromptContext) -> String {
    String::new()
}

/// Whether `text` is `min..=max` characters with no control or invisible character.
fn one_line(text: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&text.chars().count())
        && !text.chars().any(|c| c.is_control() || is_invisible(c))
}

fn hex_of(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Serializing these types cannot fail; an error would hash the empty input.
fn hash_json(value: &impl Serialize) -> String {
    hex_of(&serde_json::to_vec(value).unwrap_or_default())
}

/// The folder name and wire id of a function: its lowercased name cut to a plain word, then
/// ten hex digits of the file and name together, so no name reaches a path as written.
pub fn function_slug(file: &str, qualified: &str) -> String {
    let mut base = String::new();
    for c in qualified.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            base.push(c);
        } else if !base.ends_with('_') {
            base.push('_');
        }
    }
    let cut: String = base.trim_matches('_').chars().take(40).collect();
    let cut = cut.trim_matches('_');
    let name = if cut.is_empty() { "fn" } else { cut };
    let hash = hex_of(format!("{file}\n{qualified}").as_bytes());
    format!("{name}-{}", &hash[..10])
}

/// Whether `slug` is one `function_slug` can make: `^[a-z0-9_]{1,40}-[0-9a-f]{10}$`.
pub fn valid_slug(slug: &str) -> bool {
    let Some((name, hash)) = slug.rsplit_once('-') else {
        return false;
    };
    (1..=40).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && hash.len() == 10
        && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Why `value` is not a valid `kind`, if it is not. The first failing rule wins.
pub fn check_value(kind: ParamKind, value: &str) -> Result<(), &'static str> {
    if value.chars().count() > VALUE_MAX {
        return Err("At most 2048 characters");
    }
    if value.chars().any(|c| c.is_control() || is_invisible(c)) {
        return Err("Remove the control characters");
    }
    match kind {
        ParamKind::Text => Ok(()),
        ParamKind::Int => value
            .trim()
            .parse::<i64>()
            .map(|_| ())
            .map_err(|_| "Enter a whole number"),
        ParamKind::Float => match value.trim().parse::<f64>() {
            Ok(n) if n.is_finite() => Ok(()),
            _ => Err("Enter a number"),
        },
        ParamKind::Bool => match value.trim() {
            "true" | "false" => Ok(()),
            _ => Err("Enter true or false"),
        },
        ParamKind::Json => serde_json::from_str::<serde_json::Value>(value)
            .map(|_| ())
            .map_err(|_| "Enter valid JSON"),
    }
}

/// Whether `id` is `^[ar][1-9][0-9]{0,5}$`.
fn valid_case_id(id: &str) -> bool {
    let mut chars = id.chars();
    let digits: Vec<char> = match chars.next() {
        Some('a' | 'r') => chars.collect(),
        _ => return false,
    };
    (1..=6).contains(&digits.len()) && digits[0] != '0' && digits.iter().all(|c| c.is_ascii_digit())
}

/// Every problem of a case the reviewer saves, in the order the window shows them.
pub fn check_case_input(params: &[Param], input: &CaseInput) -> Result<(), Vec<String>> {
    let mut problems = Vec::new();
    if input.id.as_deref().is_some_and(|id| !valid_case_id(id)) {
        problems.push("invalid case id".to_string());
    }
    if !one_line(&input.name, 1, CASE_NAME_MAX) {
        problems.push("The case name must be between 1 and 120 characters on one line".into());
    }
    for param in params {
        match input.values.get(&param.name) {
            None => problems.push(format!("{}: missing", param.name)),
            Some(value) => {
                if let Err(message) = check_value(param.kind, value) {
                    problems.push(format!("{}: {message}", param.name));
                }
            }
        }
    }
    let known: BTreeSet<&str> = params.iter().map(|p| p.name.as_str()).collect();
    for key in input.values.keys().filter(|k| !known.contains(k.as_str())) {
        problems.push(format!("{key}: not a parameter of this function"));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Identifies what a case runs with: a changed driver, command, value or expectation changes it,
/// a rename does not.
pub fn case_hash(
    driver_hash: Option<&str>,
    command: Option<&str>,
    values: &BTreeMap<String, String>,
    expect: &Expect,
) -> String {
    hash_json(&(driver_hash, command, values, expect))
}

/// Identifies what a function's run would use; `cases` are `(id, case_hash)` pairs.
pub fn function_hash(
    slug: &str,
    signature: &str,
    params: &[Param],
    support: &Support,
    cases: &[(String, String)],
) -> String {
    hash_json(&(slug, signature, params, support, cases))
}

/// Identifies what the whole view shows, by its functions' hashes in list order.
pub fn view_hash(functions: &[FunctionView]) -> String {
    let hashes: Vec<&str> = functions.iter().map(|f| f.hash.as_str()).collect();
    hash_json(&hashes)
}

/// The text a comment on a function starts with; with a failing case it names the case and why.
pub fn function_comment_prefill(qualified: &str, failing: Option<(&str, &str)>) -> String {
    let part = |text: &str| -> String { printable(text).chars().take(200).collect() };
    let name = part(qualified);
    let q = if name.contains('`') {
        name
    } else {
        format!("`{name}`")
    };
    match failing {
        Some((case, problem)) => {
            format!("{q} fails the case \"{}\": {}.", part(case), part(problem))
        }
        None => format!("{q}: "),
    }
}

/// The first thing a result did not meet, in words; none for a pass.
pub fn result_problem(result: &CaseResult) -> Option<String> {
    if result.status == CaseStatus::Passed {
        return None;
    }
    if let Some(check) = result
        .state
        .iter()
        .find(|c| c.expected.is_some() && c.got != c.expected)
    {
        return Some(format!(
            "{} is {}, expected {}",
            check.name,
            check.got.as_deref().unwrap_or("missing"),
            check.expected.as_deref().unwrap_or_default()
        ));
    }
    if let Some(expected) = &result.returned_expected
        && result.returned.as_deref().map(str::trim) != Some(expected.trim())
    {
        return Some(format!(
            "returned {}, expected {expected}",
            result.returned.as_deref().unwrap_or("nothing")
        ));
    }
    if let Some(error) = &result.error {
        return Some(format!("error: {error}"));
    }
    result.message.clone()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBlock {
    function: String,
    file: String,
    unsupported: Option<String>,
    state_label: Option<String>,
    #[serde(default)]
    params: Vec<Param>,
    driver: Option<String>,
    #[serde(default)]
    cases: Vec<RawCase>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCase {
    name: String,
    #[serde(default)]
    values: BTreeMap<String, String>,
    #[serde(default)]
    expect: Expect,
}

/// Whether `name` is `^[a-z][a-z0-9_]{0,39}$`.
fn valid_param_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 40
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Whether `name` is `^[A-Za-z_][A-Za-z0-9_.]{0,39}$`.
fn valid_state_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.len() <= 40
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

fn driver_problems(driver: &str, problems: &mut Vec<String>) {
    if !(1..=DRIVER_MAX_BYTES).contains(&driver.len()) {
        problems.push(format!(
            "driver must be between 1 and {DRIVER_MAX_BYTES} bytes"
        ));
    }
    if driver
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        problems.push("driver holds a control character".into());
    }
    if driver.chars().any(is_invisible) {
        problems.push("driver holds an invisible character".into());
    }
}

fn case_problems(
    i: usize,
    case: &RawCase,
    params: &[Param],
    names_known: bool,
    problems: &mut Vec<String>,
) {
    if !one_line(&case.name, 1, CASE_NAME_MAX) {
        problems.push(format!(
            "cases[{i}].name must be between 1 and 120 characters on one line"
        ));
    }
    if names_known {
        for param in params {
            if !case.values.contains_key(&param.name) {
                problems.push(format!("cases[{i}].values is missing {}", param.name));
            }
        }
        let known: BTreeSet<&str> = params.iter().map(|p| p.name.as_str()).collect();
        for key in case.values.keys().filter(|k| !known.contains(k.as_str())) {
            problems.push(format!(
                "cases[{i}].values has an unknown parameter {}",
                printable(key)
            ));
        }
        for param in params {
            if let Some(value) = case.values.get(&param.name)
                && let Err(message) = check_value(param.kind, value)
            {
                problems.push(format!("cases[{i}].values.{}: {message}", param.name));
            }
        }
    }
    let expect = &case.expect;
    if let Some(returned) = &expect.returned
        && !one_line(returned, 0, RESULT_TEXT_MAX)
    {
        problems.push(format!(
            "cases[{i}].expect.returned must be at most 4096 characters on one line"
        ));
    }
    if expect.state.len() > STATE_MAX {
        problems.push(format!(
            "cases[{i}].expect.state has {} entries; the most allowed is {STATE_MAX}",
            expect.state.len()
        ));
    }
    for name in expect.state.keys().filter(|n| !valid_state_name(n)) {
        problems.push(format!(
            "cases[{i}].expect.state name \"{}\" is not a valid name",
            printable(name)
        ));
    }
    for (name, value) in &expect.state {
        if !one_line(value, 0, RESULT_TEXT_MAX) {
            problems.push(format!(
                "cases[{i}].expect.state.{} must be at most 4096 characters on one line",
                printable(name)
            ));
        }
    }
}

impl FunctionBlock {
    /// Reads and checks one `clusia-function` block, giving every problem it finds. The match
    /// with the function that was asked for is the caller's.
    pub fn from_json(value: &serde_json::Value) -> Result<FunctionBlock, Vec<String>> {
        let raw: RawBlock =
            serde_json::from_value(value.clone()).map_err(|e| vec![e.to_string()])?;
        let mut problems = Vec::new();
        if !one_line(&raw.function, 1, 300) {
            problems.push("function must be between 1 and 300 characters on one line".into());
        }
        if raw.file.is_empty() || !plain_relative(&raw.file, 512) {
            problems.push("file must be a plain relative path".into());
        }
        if let Some(text) = &raw.unsupported
            && !one_line(text, 1, 200)
        {
            problems.push("unsupported must be between 1 and 200 characters on one line".into());
        }
        if let Some(text) = &raw.state_label
            && !one_line(text, 1, 40)
        {
            problems.push("state_label must be between 1 and 40 characters on one line".into());
        }
        let mut names_known = true;
        if raw.params.len() > PARAMS_MAX {
            names_known = false;
            problems.push(format!(
                "params has {} entries; the most allowed is {PARAMS_MAX}",
                raw.params.len()
            ));
        } else {
            let mut seen = BTreeSet::new();
            for (i, param) in raw.params.iter().enumerate() {
                if !valid_param_name(&param.name) {
                    names_known = false;
                    problems.push(format!(
                        "params[{i}].name \"{}\" must start with a lowercase letter and hold only a-z, 0-9 and _ (at most 40)",
                        printable(&param.name)
                    ));
                } else if !seen.insert(param.name.as_str()) {
                    names_known = false;
                    problems.push(format!("params[{i}].name \"{}\" is used twice", param.name));
                }
                if !one_line(&param.label, 1, 40) {
                    problems.push(format!(
                        "params[{i}].label must be between 1 and 40 characters on one line"
                    ));
                }
            }
        }
        if let Some(driver) = &raw.driver {
            driver_problems(driver, &mut problems);
        }
        if raw.cases.len() > CASES_MAX {
            problems.push(format!(
                "cases has {} entries; the most allowed is {CASES_MAX}",
                raw.cases.len()
            ));
        } else {
            for (i, case) in raw.cases.iter().enumerate() {
                case_problems(i, case, &raw.params, names_known, &mut problems);
            }
        }
        if !problems.is_empty() {
            return Err(problems);
        }
        Ok(FunctionBlock {
            function: raw.function,
            file: raw.file,
            unsupported: raw.unsupported,
            state_label: raw.state_label,
            params: raw.params,
            driver: raw.driver,
            cases: raw
                .cases
                .into_iter()
                .map(|c| BlockCase {
                    name: c.name,
                    values: c.values,
                    expect: c.expect,
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::tests::RunnerKind;

    fn params() -> Vec<Param> {
        vec![
            Param {
                name: "stored_token".into(),
                label: "stored token".into(),
                kind: ParamKind::Text,
            },
            Param {
                name: "endpoint_status".into(),
                label: "endpoint status".into(),
                kind: ParamKind::Int,
            },
        ]
    }

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn block() -> serde_json::Value {
        json!({
            "function": "TokenStore::refresh",
            "file": "store/refresh.rs",
            "state_label": "Store",
            "params": [
                {"name": "stored_token", "label": "stored token", "kind": "text"},
                {"name": "endpoint_status", "label": "endpoint status", "kind": "int"}
            ],
            "driver": "#[test]\nfn clusia_case() {}\n",
            "cases": [{
                "name": "Endpoint fails, old token kept",
                "values": {"stored_token": "\"old\", expires +10 s", "endpoint_status": "500"},
                "expect": {"returned": "Err(RefreshError::Http(500))", "state": {"token": "\"old\""}}
            }]
        })
    }

    fn problems(value: serde_json::Value) -> Vec<String> {
        FunctionBlock::from_json(&value).unwrap_err()
    }

    fn result(status: CaseStatus) -> CaseResult {
        CaseResult {
            case: "a1".into(),
            head: "h".into(),
            hash: "x".into(),
            status,
            returned: Some("Err(RefreshError::Http(500))".into()),
            returned_expected: None,
            error: None,
            state: Vec::new(),
            output: String::new(),
            took_ms: 210,
            runner: RunnerKind::Container,
            exit_code: Some(0),
            message: None,
            at: 1,
        }
    }

    #[test]
    fn a_hostile_name_gives_a_plain_slug() {
        assert_eq!(
            function_slug("store/refresh.rs", "TokenStore::refresh"),
            "tokenstore_refresh-f3910adf0e"
        );
        let hostile = function_slug("store/refresh.rs", "../../etc/x");
        assert_eq!(hostile, "etc_x-27aefffb8e");
        assert!(!hostile.contains('/') && !hostile.contains(".."));
        assert_eq!(function_slug("a.py", "\u{202e}"), "fn-3549ec3a5d");
        let long = function_slug("a.rs", &"a".repeat(50));
        assert_eq!(long.split('-').next().unwrap(), "a".repeat(40));
        for slug in [
            &hostile,
            &long,
            "fn-3549ec3a5d",
            "tokenstore_refresh-f3910adf0e",
        ] {
            assert!(valid_slug(slug), "{slug}");
        }
        for bad in [
            "",
            "../x-0123456789",
            "Token-0123456789",
            "x-012345678",
            "x-0123456789a",
            "x-01234567zz",
            &format!("{}-0123456789", "a".repeat(41)),
            "x_0123456789",
        ] {
            assert!(!valid_slug(bad), "{bad}");
        }
    }

    #[test]
    fn values_are_checked_by_kind() {
        assert_eq!(check_value(ParamKind::Text, ""), Ok(()));
        assert_eq!(
            check_value(ParamKind::Text, "\"old\", expires +10 s"),
            Ok(())
        );
        assert_eq!(check_value(ParamKind::Int, " 500 "), Ok(()));
        assert_eq!(
            check_value(ParamKind::Int, "5.0"),
            Err("Enter a whole number")
        );
        assert_eq!(check_value(ParamKind::Float, "0.25"), Ok(()));
        assert_eq!(check_value(ParamKind::Float, "inf"), Err("Enter a number"));
        assert_eq!(check_value(ParamKind::Bool, "true"), Ok(()));
        assert_eq!(
            check_value(ParamKind::Bool, "yes"),
            Err("Enter true or false")
        );
        assert_eq!(
            check_value(ParamKind::Json, "{\"error\": \"server_error\"}"),
            Ok(())
        );
        assert_eq!(check_value(ParamKind::Json, "{"), Err("Enter valid JSON"));
        assert_eq!(
            check_value(ParamKind::Text, &"x".repeat(2049)),
            Err("At most 2048 characters")
        );
        assert_eq!(
            check_value(ParamKind::Int, &"9".repeat(2049)),
            Err("At most 2048 characters"),
            "length comes first"
        );
        for hostile in ["a\nb", "a\u{1b}[31m", "a\u{202e}b", "a\0"] {
            assert_eq!(
                check_value(ParamKind::Text, hostile),
                Err("Remove the control characters"),
                "{hostile:?}"
            );
        }
    }

    #[test]
    fn a_case_input_names_each_problem() {
        let ok = CaseInput {
            id: Some("a1".into()),
            name: "Endpoint fails".into(),
            values: values(&[("stored_token", "old"), ("endpoint_status", "500")]),
        };
        assert_eq!(check_case_input(&params(), &ok), Ok(()));
        assert_eq!(
            check_case_input(
                &params(),
                &CaseInput {
                    id: None,
                    ..ok.clone()
                }
            ),
            Ok(())
        );
        let bad = CaseInput {
            id: Some("x1".into()),
            name: "two\nlines".into(),
            values: values(&[("endpoint_status", "5xx"), ("zeta", "1"), ("alpha", "2")]),
        };
        assert_eq!(
            check_case_input(&params(), &bad),
            Err(vec![
                "invalid case id".to_string(),
                "The case name must be between 1 and 120 characters on one line".into(),
                "stored_token: missing".into(),
                "endpoint_status: Enter a whole number".into(),
                "alpha: not a parameter of this function".into(),
                "zeta: not a parameter of this function".into(),
            ])
        );
        for id in ["a0", "r", "a1234567", "A1", "r01"] {
            let input = CaseInput {
                id: Some(id.into()),
                ..ok.clone()
            };
            assert!(check_case_input(&params(), &input).is_err(), "{id}");
        }
        assert_eq!(
            check_case_input(
                &params(),
                &CaseInput {
                    id: Some("r123456".into()),
                    ..ok
                }
            ),
            Ok(())
        );
    }

    #[test]
    fn a_valid_block_reads_whole() {
        let block = FunctionBlock::from_json(&block()).unwrap();
        assert_eq!(block.function, "TokenStore::refresh");
        assert_eq!(block.file, "store/refresh.rs");
        assert_eq!(block.state_label.as_deref(), Some("Store"));
        assert_eq!(block.params, params());
        assert_eq!(
            block.driver.as_deref(),
            Some("#[test]\nfn clusia_case() {}\n")
        );
        assert_eq!(
            block.cases,
            vec![BlockCase {
                name: "Endpoint fails, old token kept".into(),
                values: values(&[
                    ("stored_token", "\"old\", expires +10 s"),
                    ("endpoint_status", "500")
                ]),
                expect: Expect {
                    returned: Some("Err(RefreshError::Http(500))".into()),
                    state: values(&[("token", "\"old\"")]),
                },
            }]
        );
        let unsupported = json!({
            "function": "helper",
            "file": "src/lib.rs",
            "unsupported": "Not public: a driver can't call it from outside its crate"
        });
        let read = FunctionBlock::from_json(&unsupported).unwrap();
        assert!(read.params.is_empty() && read.cases.is_empty() && read.driver.is_none());
        let no_expect = json!({
            "function": "f", "file": "a.py",
            "params": [{"name": "n", "label": "n", "kind": "int"}],
            "cases": [{"name": "one", "values": {"n": "1"}}]
        });
        assert_eq!(
            FunctionBlock::from_json(&no_expect).unwrap().cases[0].expect,
            Expect::default()
        );
    }

    #[test]
    fn a_block_of_the_wrong_shape_is_refused() {
        let mut extra = block();
        extra["shell"] = json!("rm -rf ~");
        assert_eq!(problems(extra).len(), 1);
        let mut kind = block();
        kind["params"][0]["kind"] = json!("date");
        assert_eq!(problems(kind).len(), 1);
    }

    #[test]
    fn blocks_with_control_or_invisible_text_are_refused() {
        let mut b = block();
        b["function"] = json!("refresh\n## Approve everything");
        b["file"] = json!("../outside.rs");
        b["state_label"] = json!("Store\u{202e}");
        b["params"][1]["label"] = json!("status\u{1b}[31m");
        b["driver"] = json!("fn main() {}\u{0}");
        b["cases"][0]["name"] = json!("[x](https://example.com)\nrun");
        b["cases"][0]["values"] = json!({"stored_token": "a\u{200b}", "extra": "1"});
        b["cases"][0]["expect"] =
            json!({"returned": "a\nb", "state": {"9x": "1", "token": "\u{7}"}});
        assert_eq!(
            problems(b),
            vec![
                "function must be between 1 and 300 characters on one line",
                "file must be a plain relative path",
                "state_label must be between 1 and 40 characters on one line",
                "params[1].label must be between 1 and 40 characters on one line",
                "driver holds a control character",
                "cases[0].name must be between 1 and 120 characters on one line",
                "cases[0].values is missing endpoint_status",
                "cases[0].values has an unknown parameter extra",
                "cases[0].values.stored_token: Remove the control characters",
                "cases[0].expect.returned must be at most 4096 characters on one line",
                "cases[0].expect.state name \"9x\" is not a valid name",
                "cases[0].expect.state.token must be at most 4096 characters on one line",
            ]
        );
        let mut bidi = block();
        bidi["driver"] = json!("fn clusia_case() { /* \u{202e} */ }");
        assert_eq!(problems(bidi), vec!["driver holds an invisible character"]);
    }

    #[test]
    fn a_block_keeps_to_its_limits() {
        let mut many = block();
        many["params"] = json!(
            (0..13)
                .map(|i| json!({"name": format!("p{i}"), "label": "p", "kind": "text"}))
                .collect::<Vec<_>>()
        );
        many["cases"] = json!([]);
        assert_eq!(
            problems(many),
            vec!["params has 13 entries; the most allowed is 12"]
        );
        let mut twice = block();
        twice["params"][1]["name"] = json!("stored_token");
        twice["cases"] = json!([]);
        assert_eq!(
            problems(twice),
            vec!["params[1].name \"stored_token\" is used twice"]
        );
        let mut upper = block();
        upper["params"][0]["name"] = json!("Stored");
        assert_eq!(
            problems(upper),
            vec![
                "params[0].name \"Stored\" must start with a lowercase letter and hold only a-z, 0-9 and _ (at most 40)"
            ],
            "the values are not compared with a bad name"
        );
        let mut cases = block();
        let case = cases["cases"][0].clone();
        cases["cases"] = json!(vec![case; 21]);
        assert_eq!(
            problems(cases),
            vec!["cases has 21 entries; the most allowed is 20"]
        );
        let mut big = block();
        big["driver"] = json!("x".repeat(DRIVER_MAX_BYTES + 1));
        assert_eq!(
            problems(big),
            vec!["driver must be between 1 and 32768 bytes"]
        );
        let mut state = block();
        state["cases"][0]["expect"]["state"] = json!(
            (0..21)
                .map(|i| (format!("s{i}"), json!("1")))
                .collect::<serde_json::Map<_, _>>()
        );
        assert_eq!(
            problems(state),
            vec!["cases[0].expect.state has 21 entries; the most allowed is 20"]
        );
    }

    #[test]
    fn hashes_are_pinned_and_follow_what_runs() {
        assert_eq!(
            view_hash(&[]),
            "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945"
        );
        assert_eq!(FunctionsView::new("h").hash, view_hash(&[]));
        assert_eq!(
            case_hash(None, None, &BTreeMap::new(), &Expect::default()),
            "f3d7125963d41627e815fdd1ce05a72c7cc988c3aaaa8322ad27152d0d5121b2"
        );
        let expect = Expect {
            returned: Some("1".into()),
            state: values(&[("token", "\"old\"")]),
        };
        let base = case_hash(
            Some("d1"),
            Some("python3 {driver}"),
            &values(&[("now", "+15 s")]),
            &expect,
        );
        assert_eq!(
            base,
            "47ef1a89d22bc5be3e59b0f43aeb2ea32f748d65d7c452008c097b0c2580a0f0"
        );
        for other in [
            case_hash(
                Some("d2"),
                Some("python3 {driver}"),
                &values(&[("now", "+15 s")]),
                &expect,
            ),
            case_hash(
                Some("d1"),
                Some("python {driver}"),
                &values(&[("now", "+15 s")]),
                &expect,
            ),
            case_hash(
                Some("d1"),
                Some("python3 {driver}"),
                &values(&[("now", "+16 s")]),
                &expect,
            ),
            case_hash(
                Some("d1"),
                Some("python3 {driver}"),
                &values(&[("now", "+15 s")]),
                &Expect::default(),
            ),
        ] {
            assert_ne!(other, base);
        }
        let params = vec![Param {
            name: "now".into(),
            label: "now".into(),
            kind: ParamKind::Text,
        }];
        let cases = vec![("a1".to_string(), "h1".to_string())];
        assert_eq!(
            function_hash(
                "tokenstore_refresh-x",
                "fn f()",
                &params,
                &Support::Ready,
                &cases
            ),
            "cb0e8f327a7652b0be372ce529be6261ca81c5ce50d1eb30e0439bf60ede6f83"
        );
        assert_ne!(
            function_hash(
                "tokenstore_refresh-x",
                "fn f()",
                &params,
                &Support::NoCommand,
                &cases
            ),
            function_hash(
                "tokenstore_refresh-x",
                "fn f()",
                &params,
                &Support::Ready,
                &cases
            )
        );
    }

    #[test]
    fn the_view_hash_ignores_a_failed_turn() {
        let mut view = FunctionsView::new("h");
        view.failed =
            Some("Claude Code's cases are not valid: driver holds a control character".into());
        assert_eq!(view.hash, view_hash(&view.functions));
        let back: FunctionsView = serde_json::from_value(json!({
            "head": "h", "functions": [], "unsupported": [], "reading": false,
            "note": null, "activity": null, "hash": view_hash(&[])
        }))
        .unwrap();
        assert_eq!(back, FunctionsView::new("h"), "failed defaults to None");
    }

    #[test]
    fn the_comment_names_the_failing_case() {
        assert_eq!(
            function_comment_prefill(
                "TokenStore::refresh",
                Some((
                    "Endpoint fails, old token kept",
                    "token is \"\", expected \"old\""
                ))
            ),
            "`TokenStore::refresh` fails the case \"Endpoint fails, old token kept\": token is \"\", expected \"old\"."
        );
        assert_eq!(
            function_comment_prefill("TokenStore::refresh", None),
            "`TokenStore::refresh`: "
        );
        assert_eq!(function_comment_prefill("a`b", None), "a`b: ");
        assert_eq!(
            function_comment_prefill("f\n[x](https://e.com)", Some(("c\u{202e}", "e"))),
            "`f\\n[x](https://e.com)` fails the case \"c\\u{202e}\": e."
        );
        let long = "q".repeat(300);
        assert_eq!(
            function_comment_prefill(&long, None),
            format!("`{}`: ", "q".repeat(200))
        );
    }

    #[test]
    fn the_problem_is_the_first_unmet_thing() {
        assert_eq!(result_problem(&result(CaseStatus::Passed)), None);
        let mut state = result(CaseStatus::Failed);
        state.state = vec![
            StateCheck {
                name: "count".into(),
                got: Some("1".into()),
                expected: Some("1".into()),
            },
            StateCheck {
                name: "token".into(),
                got: Some("\"\"".into()),
                expected: Some("\"old\"".into()),
            },
        ];
        state.returned_expected = Some("Ok(())".into());
        assert_eq!(
            result_problem(&state).as_deref(),
            Some("token is \"\", expected \"old\"")
        );
        let mut missing = result(CaseStatus::Failed);
        missing.state = vec![StateCheck {
            name: "token".into(),
            got: None,
            expected: Some("\"old\"".into()),
        }];
        assert_eq!(
            result_problem(&missing).as_deref(),
            Some("token is missing, expected \"old\"")
        );
        let mut returned = result(CaseStatus::Failed);
        returned.returned_expected = Some("Ok(Token)".into());
        assert_eq!(
            result_problem(&returned).as_deref(),
            Some("returned Err(RefreshError::Http(500)), expected Ok(Token)")
        );
        returned.returned = None;
        assert_eq!(
            result_problem(&returned).as_deref(),
            Some("returned nothing, expected Ok(Token)")
        );
        let mut error = result(CaseStatus::Failed);
        error.error = Some("panicked at store.rs:9".into());
        assert_eq!(
            result_problem(&error).as_deref(),
            Some("error: panicked at store.rs:9")
        );
        let mut message = result(CaseStatus::Error);
        message.message = Some("The driver ended without a result (exit 101)".into());
        assert_eq!(
            result_problem(&message).as_deref(),
            Some("The driver ended without a result (exit 101)")
        );
        assert_eq!(result_problem(&result(CaseStatus::Stopped)), None);
    }

    #[test]
    fn languages_map_from_the_call_index() {
        assert_eq!(lang_of("rust"), Some(FunctionLang::Rust));
        assert_eq!(lang_of("typescript"), Some(FunctionLang::TypeScript));
        assert_eq!(lang_of("javascript"), Some(FunctionLang::JavaScript));
        assert_eq!(lang_of("python"), Some(FunctionLang::Python));
        assert_eq!(lang_of("swift"), None);
        let keys: Vec<_> = FunctionLang::ALL.iter().map(|l| l.key()).collect();
        assert_eq!(keys, ["rust", "python", "typescript", "javascript"]);
        let exts: Vec<_> = FunctionLang::ALL.iter().map(|l| l.ext()).collect();
        assert_eq!(exts, ["rs", "py", "ts", "mjs"]);
        let labels: Vec<_> = FunctionLang::ALL.iter().map(|l| l.label()).collect();
        assert_eq!(labels, ["Rust", "Python", "TypeScript", "JavaScript"]);
    }

    #[test]
    fn the_wire_names_are_the_contracts() {
        fn wire(v: impl Serialize) -> String {
            serde_json::to_string(&v).unwrap()
        }
        assert_eq!(wire(FunctionLang::TypeScript), r#""typescript""#);
        assert_eq!(wire(ParamKind::Json), r#""json""#);
        assert_eq!(wire(Support::NoCases), r#""no_cases""#);
        assert_eq!(
            wire(Support::Stale { reason: "r".into() }),
            r#"{"stale":{"reason":"r"}}"#
        );
        assert_eq!(wire(CaseStatus::TimedOut), r#""timed_out""#);
        assert_eq!(wire(CaseScope::All), r#""all""#);
        assert_eq!(
            wire(CaseScope::Case {
                function: "f-0123456789".into(),
                case: "a1".into()
            }),
            r#"{"case":{"function":"f-0123456789","case":"a1"}}"#
        );
        assert_eq!(wire(CasesAsk::More), r#""more""#);
        assert_eq!(
            wire(CasesAsk::Values { case: "a1".into() }),
            r#"{"values":{"case":"a1"}}"#
        );
        assert_eq!(wire(FunctionChange::Signature), r#""signature""#);
        let edits: Edits = serde_json::from_str("{}").unwrap();
        assert_eq!(edits, Edits::default());
        let old: CaseResult = serde_json::from_value(json!({
            "case": "a1", "head": "h", "hash": "x", "status": "passed", "returned": null,
            "error": null, "state": [], "output": "", "took_ms": 1, "runner": "local",
            "exit_code": 0, "message": null, "at": 1
        }))
        .unwrap();
        assert_eq!(old.returned_expected, None);
    }
}

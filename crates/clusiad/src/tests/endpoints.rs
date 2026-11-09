//! The endpoints a reviewer tries against the running system: `.clusia/system/endpoints.toml`,
//! written only by Clúsia from a checked `clusia-endpoints` block, the turn that reads them
//! from the diff, and the turn that fixes a system that did not start.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use clusia_core::PrRef;
use clusia_core::printable::printable;
use clusia_core::system::{
    ENDPOINTS_FILE_MAX, Endpoint, EndpointView, EndpointsPromptContext, EndpointsView,
    LOG_TAIL_LINES, NO_SYSTEM, NOTHING_TO_FIX, SendResult, StoredEndpoints, SystemFailure,
    SystemState, check_endpoint, curl_command, endpoints_from_json, endpoints_prompt,
    failure_prompt_text, unhealthy_text,
};
use clusia_core::tests::{StepPlace, TestsPhase};
use clusia_harness::extract_endpoints_block;
use clusia_protocol::{Outcome, Reply, TurnOrigin};
use clusia_testrun::recipe::Recipe;

use super::cases::{self, Refusal};
use super::cockpit_turn::{Spec, run_text};
use super::env::{self, RecipeState};
use super::system::tell_system;
use super::{Doing, NO_RECIPE};
use crate::reviews;
use crate::sessions;
use crate::state::Shared;
use crate::sync::now_unix;

const FILE: &str = "endpoints.toml";
const NOT_PLAIN: &str = "Clúsia's folder for the endpoints is not a plain folder";
/// How much of a turn's failure is kept, after `printable`.
const FAILED_MAX: usize = 300;

/// The endpoints file as read now.
#[derive(Debug, Default)]
pub(super) struct Loaded {
    pub stored: Option<StoredEndpoints>,
    /// The stored endpoints that pass the checks against the current ports, in order.
    pub list: Vec<Endpoint>,
    pub note: Option<String>,
    /// `.clusia/system` is a link or not a folder: nothing is read or written there.
    pub refused: bool,
}

fn refused() -> Loaded {
    Loaded {
        note: Some(NOT_PLAIN.into()),
        refused: true,
        ..Loaded::default()
    }
}

/// Reads the endpoints file and checks every endpoint again against `ports`, the current
/// recipe's port names: a hand edit is never trusted.
pub(super) fn read_endpoints(worktree: &Path, ports: &[String]) -> Loaded {
    let dir = match cases::dir_at(worktree, &[".clusia", "system"], false) {
        Ok(Some(dir)) => dir,
        Ok(None) => return Loaded::default(),
        Err(Refusal::NotPlain) => return refused(),
        Err(Refusal::Io(message)) => {
            return Loaded {
                note: Some(message),
                ..Loaded::default()
            };
        }
    };
    let bytes = match cases::read_capped(&dir, FILE, ENDPOINTS_FILE_MAX) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Loaded::default(),
        Err(Refusal::NotPlain) => return refused(),
        Err(Refusal::Io(message)) => {
            return Loaded {
                note: Some(message),
                ..Loaded::default()
            };
        }
    };
    let stored = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| toml::from_str::<StoredEndpoints>(text).ok());
    let Some(stored) = stored else {
        return Loaded {
            note: Some(format!(
                "{} cannot be read",
                clusia_core::system::ENDPOINTS_FILE
            )),
            ..Loaded::default()
        };
    };
    let mut list = Vec::new();
    let mut reasons = Vec::new();
    for endpoint in &stored.endpoints {
        match check_endpoint(endpoint, ports) {
            Ok(()) if !list.iter().any(|e: &Endpoint| e.id() == endpoint.id()) => {
                list.push(endpoint.clone());
            }
            Ok(()) => reasons.push(format!("{} is listed twice", endpoint.id())),
            Err(reason) => reasons.push(reason),
        }
    }
    let note = reasons.first().map(|first| {
        format!(
            "{} endpoints were left out: {}",
            reasons.len(),
            printable(first)
        )
    });
    Loaded {
        stored: Some(stored),
        list,
        note,
        refused: false,
    }
}

/// Replaces the endpoints file, never through a link.
pub(super) fn write_endpoints_file(
    worktree: &Path,
    stored: &StoredEndpoints,
) -> Result<(), String> {
    let dir = match cases::dir_at(worktree, &[".clusia", "system"], true) {
        Ok(Some(dir)) => dir,
        Ok(None) => return Err(NOT_PLAIN.into()),
        Err(Refusal::NotPlain) => return Err(NOT_PLAIN.into()),
        Err(Refusal::Io(message)) => return Err(message),
    };
    let text = toml::to_string(stored).map_err(|e| format!("Cannot write the endpoints: {e}"))?;
    cases::write_whole(&dir, FILE, text.as_bytes())
        .map_err(|e| format!("Cannot write {}: {e}", clusia_core::system::ENDPOINTS_FILE))
}

/// The endpoints as the view shows them. `mapped` is each port name's host port while the
/// system lives; only then does an endpoint get its `curl`.
pub(super) fn endpoints_view(
    loaded: Loaded,
    head: &str,
    mapped: &BTreeMap<String, u16>,
    sends: &BTreeMap<String, SendResult>,
    writing: bool,
    failed: Option<String>,
) -> EndpointsView {
    let written_head = loaded.stored.as_ref().map(|s| s.written_head.clone());
    let stale = written_head
        .as_deref()
        .is_some_and(|written| !head.is_empty() && written != head);
    EndpointsView {
        list: loaded
            .list
            .into_iter()
            .map(|endpoint| {
                let id = endpoint.id();
                EndpointView {
                    curl: mapped
                        .get(&endpoint.port)
                        .map(|host| curl_command(&endpoint, *host)),
                    last: sends.get(&id).cloned(),
                    id,
                    endpoint,
                }
            })
            .collect(),
        written_head,
        stale,
        writing,
        failed,
        note: loaded.note,
    }
}

/// Why an answer's endpoints were not used.
fn not_valid(agent: &str, reasons: &[String]) -> String {
    let text = printable(&format!(
        "{agent}'s endpoints are not valid: {}",
        reasons.join("; ")
    ));
    text.chars().take(FAILED_MAX).collect()
}

/// Writes the endpoints of an answer whose recipe has `[system]`, checked against that
/// recipe's ports. Answers why nothing was written, if so. The caller holds the review lock.
pub(super) fn adopt(
    worktree: &Path,
    recipe: &Recipe,
    block: Option<&serde_json::Value>,
    head: &str,
    at: i64,
    agent: &str,
) -> Option<String> {
    let spec = recipe.system.as_ref()?;
    let Some(block) = block else {
        return Some(not_valid(
            agent,
            &["the answer holds no clusia-endpoints block".to_string()],
        ));
    };
    let (endpoints, reasons) = endpoints_from_json(block, &spec.port_names());
    if endpoints.is_empty() {
        let reasons = if reasons.is_empty() {
            vec!["the block lists no endpoint".to_string()]
        } else {
            reasons
        };
        return Some(not_valid(agent, &reasons));
    }
    let stored = StoredEndpoints {
        written_head: head.to_string(),
        written_at: at,
        endpoints,
    };
    write_endpoints_file(worktree, &stored).err()
}

/// `WriteEndpoints`: a forked, read-only turn reads the endpoints from the diff, in the
/// background.
pub(crate) async fn write_endpoints(shared: &Arc<Shared>, pr: &PrRef) -> Outcome {
    let review = match super::active(shared, pr) {
        Ok(review) => review,
        Err(out) => return out,
    };
    let Ok(ctx) = env::Ctx::new(shared, pr) else {
        return reviews::invalid_state(cases::OPEN_FIRST);
    };
    if let Some(problem) = env::refresh_shipped(shared, &ctx).await {
        return reviews::invalid_state(problem);
    }
    let recipe = match env::recipe_state(&ctx.worktree) {
        RecipeState::Valid { recipe, .. } => *recipe,
        RecipeState::Missing => return reviews::invalid_state(NO_RECIPE),
        RecipeState::Problem(message) => return reviews::invalid_state(message),
    };
    let Some(spec) = recipe.system.clone() else {
        return reviews::invalid_state(NO_SYSTEM);
    };
    let loaded = read_endpoints(&ctx.worktree, &spec.port_names());
    if loaded.refused {
        return reviews::invalid_state(NOT_PLAIN);
    }
    let started = match super::begin(shared, pr, Doing::Endpoints, TestsPhase::Waiting) {
        Ok(started) => started,
        Err(refused) => return refused.outcome(pr),
    };
    // `writing: true` goes out before the `Ack`, so a client always sees the turn begin.
    tell_system(shared, pr).await;
    let existing: Vec<String> = loaded.list.iter().map(Endpoint::id).collect();
    let files = shared
        .review_caches
        .get(pr)
        .await
        .map(|cache| cache.files.iter().map(|f| f.path.clone()).collect())
        .unwrap_or_default();
    let view = spec.spec_view(&recipe.tests.env, &[]);
    let mut prompt = EndpointsPromptContext {
        pr_title: review.title.clone(),
        forked: true,
        files,
        ports: view.ports,
        start: spec.start.clone(),
        existing,
    };
    let forked = endpoints_prompt(&prompt);
    prompt.forked = false;
    let job_spec = Spec {
        origin: TurnOrigin::System,
        prompt: forked,
        fresh_prompt: endpoints_prompt(&prompt),
        failure: "read the endpoints",
    };
    let (shared, pr) = (shared.clone(), pr.clone());
    let worktree = ctx.worktree.clone();
    tokio::spawn(async move {
        let id = started.id;
        let job = {
            let (shared, pr) = (shared.clone(), pr.clone());
            let turn = started.turn.clone();
            async move {
                let agent = sessions::name_for(&shared, &pr).await;
                let text = match run_text(&shared, &pr, job_spec, turn.clone()).await {
                    Ok(text) => text,
                    Err(_) if turn.fired() => return None,
                    Err(message) => return Some(printable(&message)),
                };
                let (block, _) = extract_endpoints_block(&text);
                let _guard = reviews::lock(&shared, &pr).await;
                if turn.fired() || !env::alive(&shared, &pr) {
                    return None;
                }
                // The recipe as it stands now: the ports may have changed during the turn.
                let recipe = match env::recipe_state(&worktree) {
                    RecipeState::Valid { recipe, .. } => *recipe,
                    _ => return Some(NO_SYSTEM.to_string()),
                };
                let head = reviews::load_stored(&shared, &pr)
                    .ok()
                    .flatten()
                    .map(|review| review.head_sha)
                    .unwrap_or_default();
                adopt(
                    &worktree,
                    &recipe,
                    block.as_ref(),
                    &head,
                    now_unix(),
                    &agent,
                )
                .or_else(|| recipe.system.is_none().then(|| NO_SYSTEM.to_string()))
            }
        };
        let failed = match tokio::spawn(job).await {
            Ok(failed) => failed,
            Err(_) => {
                tracing::error!(pr = %pr, "an endpoints turn failed inside the daemon");
                Some("The turn stopped unexpectedly".to_string())
            }
        };
        shared.tests.with(&pr, |entry| {
            if entry.activity.as_ref().is_some_and(|a| a.id == id) {
                entry.activity = None;
                entry.endpoints_failed = failed;
            }
        });
        drop(started.live);
        tell_system(&shared, &pr).await;
    });
    Outcome::Ok(Reply::Ack)
}

/// The failure a fix is asked about: the stored one, or, for a system that never became
/// healthy, one built from its health check and the start command's log.
fn failure_to_fix(shared: &Shared, pr: &PrRef, recipe: &Recipe) -> Option<SystemFailure> {
    shared.tests.with(pr, |entry| {
        let run = entry.system.as_ref()?;
        match run.state {
            SystemState::Failed => run.failure.clone(),
            SystemState::Unhealthy => {
                if let Some(failure) = run.failure.clone() {
                    return Some(failure);
                }
                let spec = recipe.system.as_ref()?;
                let check = spec
                    .health
                    .as_ref()
                    .map(|h| h.text())
                    .unwrap_or_else(|| "The health check".into());
                let secs = spec.health.as_ref().map_or(0, |h| h.timeout_secs());
                let tail = run
                    .logs
                    .get("start")
                    .map(|log| log.ring.tail(LOG_TAIL_LINES).1.join("\n"))
                    .unwrap_or_default();
                Some(SystemFailure {
                    command: spec.start.clone(),
                    place: StepPlace::Container,
                    exit_code: None,
                    message: unhealthy_text(&check, run.health.as_ref(), secs),
                    process: Some("start".into()),
                    tail,
                })
            }
            _ => None,
        }
    })
}

/// `FixSystem`: the recipe turn again, with what failed as data.
pub(crate) async fn fix_system(shared: &Arc<Shared>, pr: &PrRef) -> Outcome {
    if let Err(out) = super::active(shared, pr) {
        return out;
    }
    let Ok(ctx) = env::Ctx::new(shared, pr) else {
        return reviews::invalid_state(cases::OPEN_FIRST);
    };
    let recipe = match env::recipe_state(&ctx.worktree) {
        RecipeState::Valid { recipe, .. } => *recipe,
        _ => return reviews::invalid_state(NOTHING_TO_FIX),
    };
    let Some(failure) = failure_to_fix(shared, pr, &recipe) else {
        return reviews::invalid_state(NOTHING_TO_FIX);
    };
    super::generate_with(shared, pr, Some(failure_prompt_text(&failure))).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn post_token() -> Endpoint {
        Endpoint {
            method: clusia_core::system::HttpMethod::Post,
            path: "/oauth/token".into(),
            port: "api".into(),
            why: "Refresh goes through the shared queue.".into(),
            new: false,
            headers: BTreeMap::from([(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )]),
            body: Some("grant_type=refresh_token&refresh_token=fake_rt_7c2e".into()),
            params: BTreeMap::new(),
        }
    }

    fn delete_token() -> Endpoint {
        Endpoint {
            method: clusia_core::system::HttpMethod::Delete,
            path: "/oauth/token/{id}".into(),
            port: "api".into(),
            why: "Revoking drops queued waiters.".into(),
            new: false,
            headers: BTreeMap::new(),
            body: None,
            params: BTreeMap::from([("id".to_string(), "fake_tok_1".to_string())]),
        }
    }

    pub(in crate::tests) fn stored() -> StoredEndpoints {
        StoredEndpoints {
            written_head: "a".repeat(40),
            written_at: 1_762_300_000,
            endpoints: vec![post_token(), delete_token()],
        }
    }

    const HOSTILE_TOML: &str = "written_head = \"h1\"\nwritten_at = 1\n\n[[endpoints]]\nmethod = \"GET\"\npath = \"/x\"\nport = \"api\"\nwhy = \"Looks fine.\"\n\n[endpoints.headers]\nhost = \"evil.example\"\n";

    #[test]
    fn the_endpoints_file_round_trips_and_is_checked_again_on_read() {
        let wt = tempdir();
        let stored = stored();
        write_endpoints_file(wt.path(), &stored).unwrap();
        let loaded = read_endpoints(wt.path(), &["api".into(), "admin".into()]);
        assert_eq!(loaded.list, stored.endpoints);
        assert_eq!(loaded.note, None);
        // The recipe no longer has the port `api`: its endpoints are left out, and the note
        // says so.
        let loaded = read_endpoints(wt.path(), &["admin".into()]);
        assert!(loaded.list.is_empty());
        assert!(
            loaded
                .note
                .unwrap()
                .starts_with("2 endpoints were left out: ")
        );
        // A hand edit with a reserved header is refused on read, not trusted.
        std::fs::write(
            wt.path().join(".clusia/system/endpoints.toml"),
            HOSTILE_TOML,
        )
        .unwrap();
        let loaded = read_endpoints(wt.path(), &["api".into()]);
        assert!(loaded.list.is_empty());
        assert!(loaded.note.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_or_a_non_folder_at_clusia_system_writes_nothing() {
        let wt = tempdir();
        std::fs::create_dir_all(wt.path().join(".clusia")).unwrap();
        let outside = tempdir();
        std::os::unix::fs::symlink(outside.path(), wt.path().join(".clusia/system")).unwrap();
        let loaded = read_endpoints(wt.path(), &["api".into()]);
        assert!(loaded.refused);
        assert_eq!(loaded.note.as_deref(), Some(NOT_PLAIN));
        assert!(write_endpoints_file(wt.path(), &stored()).is_err());
        assert_eq!(
            std::fs::read_dir(outside.path()).unwrap().count(),
            0,
            "nothing written through the link"
        );
        // A file where the folder should be.
        let wt = tempdir();
        std::fs::create_dir_all(wt.path().join(".clusia")).unwrap();
        std::fs::write(wt.path().join(".clusia/system"), "x").unwrap();
        assert!(read_endpoints(wt.path(), &["api".into()]).refused);
        assert!(write_endpoints_file(wt.path(), &stored()).is_err());
    }
}

//! The QA cockpit's benchmarks: the loop file and `bench.json` Clúsia keeps next to a function's
//! cases, the reviewer's edits, and the view the clients get.

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::Arc;

use clusia_core::PrRef;
use clusia_core::cockpit::{
    BENCH_JSON_MAX_BYTES, BENCH_MAX_BYTES, BENCH_METRICS, BENCH_ROUNDS_DEFAULT, BENCH_SECS_DEFAULT,
    BENCH_SECS_MAX, BENCH_SECS_MIN, BENCH_WARMUP_DEFAULT, BENCH_WARMUP_MAX, BenchBlock, BenchEdits,
    BenchInput, BenchView, ItemKind, ItemRef, ItemSupport, ROUNDS_MAX, SCENARIO_ROUNDS_MIN,
    StoredBench, bench_hash, bench_input_hash, bench_status, script_sha,
};
use clusia_core::functions::{
    FUNCTIONS_DIR, FunctionChange, FunctionLang, FunctionView, Param, Support, check_value,
};
use clusia_core::tests::TestsRecord;
use clusia_protocol::{ErrorCode, Outcome, ProtocolError};

use super::cases::{self, Refusal};
use super::compare::{self, Now};
use super::scenarios;
use crate::reviews;
use crate::state::Shared;

pub(super) const BENCH_FOLDER_REFUSED: &str =
    "Clúsia's folder for this benchmark is not a plain folder";
const BENCH_JSON: &str = "bench.json";
const BENCH_EDITS: &str = "bench_edits.json";

/// A listed benchmark with what a run needs of it.
#[allow(dead_code)]
pub(super) struct BenchItem {
    pub slug: String,
    pub qualified: String,
    pub file: String,
    pub lang: FunctionLang,
    pub change: FunctionChange,
    pub stored: Option<StoredBench>,
    pub values: BTreeMap<String, String>,
    pub warmup: u32,
    pub secs: u32,
    pub rounds: u32,
    /// The recipe's `functions.<lang>` command.
    pub template: Option<String>,
    /// The loop file as it is on disk now.
    pub loop_file: Option<Vec<u8>>,
    pub package: Option<Result<(String, String), String>>,
    pub params: Vec<Param>,
    pub input_hash: String,
}

/// What is stored for one benchmark.
#[derive(Default)]
struct Loaded {
    stored: Option<StoredBench>,
    edits: BenchEdits,
    loop_file: Option<Vec<u8>>,
    /// One of its files is a link or not a regular file.
    refused: bool,
}

/// The path of the loop file, relative to the worktree.
fn loop_path(slug: &str, lang: FunctionLang) -> String {
    format!("{FUNCTIONS_DIR}/{slug}/bench.{}", lang.ext())
}

/// Reads what is stored for `slug` without following a link. Edits whose values no longer check
/// against `params` are dropped.
fn load(worktree: &Path, slug: &str, params: &[Param]) -> Loaded {
    let refused = Loaded {
        refused: true,
        ..Loaded::default()
    };
    let dir = match cases::folder(worktree, slug, false) {
        Ok(Some(dir)) => dir,
        Ok(None) => return Loaded::default(),
        Err(Refusal::NotPlain) => return refused,
        Err(Refusal::Io(message)) => {
            tracing::warn!(slug, message, "cannot open the folder of a benchmark");
            return Loaded::default();
        }
    };
    let stored = match cases::read_capped(&dir, BENCH_JSON, BENCH_JSON_MAX_BYTES) {
        Ok(Some(bytes)) => match serde_json::from_slice::<StoredBench>(&bytes) {
            Ok(stored) => Some(stored),
            Err(e) => {
                tracing::warn!(slug, error = %e, "a bench.json could not be read");
                None
            }
        },
        Ok(None) => None,
        Err(Refusal::NotPlain) => return refused,
        Err(Refusal::Io(message)) => {
            tracing::warn!(slug, message, "a bench.json could not be read");
            None
        }
    };
    let Some(stored) = stored else {
        return Loaded::default();
    };
    let mut edits = match cases::read_capped(&dir, BENCH_EDITS, BENCH_JSON_MAX_BYTES) {
        Ok(Some(bytes)) => serde_json::from_slice::<BenchEdits>(&bytes).unwrap_or_else(|e| {
            tracing::warn!(slug, error = %e, "a bench_edits.json could not be read");
            BenchEdits::default()
        }),
        Ok(None) => BenchEdits::default(),
        Err(Refusal::NotPlain) => return refused,
        Err(Refusal::Io(message)) => {
            tracing::warn!(slug, message, "a bench_edits.json could not be read");
            BenchEdits::default()
        }
    };
    edits.values.retain(|key, value| {
        params
            .iter()
            .find(|p| p.name == *key)
            .is_some_and(|p| check_value(p.kind, value).is_ok())
    });
    let name = format!("bench.{}", stored.lang.ext());
    let loop_file = match cases::read_capped(&dir, &name, BENCH_MAX_BYTES) {
        Ok(file) => file,
        Err(Refusal::NotPlain) => return refused,
        Err(Refusal::Io(message)) => {
            tracing::warn!(slug, message, "a benchmark file could not be read");
            None
        }
    };
    Loaded {
        stored: Some(stored),
        edits,
        loop_file,
        refused: false,
    }
}

/// The reason a benchmark cannot run, the first that applies, or `Ready`.
fn support_of(
    refused: bool,
    function: &FunctionView,
    item: &cases::Item,
    stored: Option<&StoredBench>,
    loop_file: Option<&[u8]>,
) -> ItemSupport {
    if refused {
        return ItemSupport::Unsupported {
            reason: BENCH_FOLDER_REFUSED.into(),
        };
    }
    if let Support::Unsupported { reason } = &function.support {
        return ItemSupport::Unsupported {
            reason: reason.clone(),
        };
    }
    if matches!(item.package, Some(Err(_))) {
        return ItemSupport::Unsupported {
            reason: clusia_testrun::functions::NO_PACKAGE.into(),
        };
    }
    if item.command.is_none() || function.lang.is_none() {
        let lang = function.lang.map_or("these", FunctionLang::label);
        return ItemSupport::NoCommand {
            reason: format!("The recipe has no command to run {lang} functions"),
        };
    }
    if let Some(stored) = stored
        && !item.brief.signature.is_empty()
        && !stored.signature.is_empty()
        && stored.signature != item.brief.signature
    {
        return ItemSupport::Stale {
            reason: "The signature changed since this benchmark was written".into(),
        };
    }
    if let (Some(stored), loop_file) = (stored, loop_file) {
        match loop_file {
            None => {
                return ItemSupport::Stale {
                    reason: "The benchmark file is missing".into(),
                };
            }
            Some(bytes) if script_sha(bytes) != stored.bench_sha => {
                return ItemSupport::Stale {
                    reason: "The benchmark file changed on disk".into(),
                };
            }
            Some(_) => {}
        }
    }
    ItemSupport::Ready
}

/// The benchmark of one listed function, when it has one. A function whose folder is refused
/// lists no benchmark.
pub(super) fn read_one(
    worktree: &Path,
    item: &cases::Item,
    function: &FunctionView,
    record: &TestsRecord,
    now: &Now<'_>,
    running: bool,
) -> Option<(BenchView, BenchItem)> {
    if item.refused {
        return None;
    }
    let params = item
        .stored
        .as_ref()
        .map(|s| s.params.clone())
        .unwrap_or_default();
    let loaded = load(worktree, &function.slug, &params);
    if loaded.stored.is_none() && !loaded.refused {
        return None;
    }
    let stored = loaded.stored;
    let lang = stored
        .as_ref()
        .map(|s| s.lang)
        .or(function.lang)
        .unwrap_or(item.brief.lang);
    let base_values = stored
        .as_ref()
        .map(|s| s.values.clone())
        .unwrap_or_default();
    let mut values = base_values.clone();
    values.extend(loaded.edits.values.clone());
    let (stored_warmup, stored_secs, stored_rounds) = stored.as_ref().map_or(
        (
            BENCH_WARMUP_DEFAULT,
            BENCH_SECS_DEFAULT,
            BENCH_ROUNDS_DEFAULT,
        ),
        |s| (s.warmup, s.secs, s.rounds),
    );
    let warmup = loaded.edits.warmup.unwrap_or(stored_warmup);
    let secs = loaded.edits.secs.unwrap_or(stored_secs);
    let rounds = loaded.edits.rounds.unwrap_or(stored_rounds);
    let edited = values != base_values
        || warmup != stored_warmup
        || secs != stored_secs
        || rounds != stored_rounds;
    let sha = loaded
        .loop_file
        .as_deref()
        .map(script_sha)
        .or_else(|| stored.as_ref().map(|s| s.bench_sha.clone()))
        .unwrap_or_default();
    let input_hash = bench_input_hash(
        &sha,
        item.command.as_deref().unwrap_or_default(),
        &values,
        warmup,
        secs,
    );
    let support = support_of(
        loaded.refused,
        function,
        item,
        stored.as_ref(),
        loaded.loop_file.as_deref(),
    );
    let item_ref = ItemRef {
        kind: ItemKind::Bench,
        id: function.slug.clone(),
    };
    let (last, comparison) =
        compare::shown(record, &item_ref, &input_hash, now, &BENCH_METRICS, false);
    let mut view = BenchView {
        slug: function.slug.clone(),
        qualified: item.brief.qualified.clone(),
        file: item.brief.file.clone(),
        lang,
        change: item.brief.change,
        params: params.clone(),
        values: values.clone(),
        warmup,
        secs,
        rounds,
        edited,
        path: loop_path(&function.slug, lang),
        support,
        input_hash: input_hash.clone(),
        last,
        comparison,
        status: String::new(),
        hash: String::new(),
    };
    view.hash = bench_hash(&input_hash, rounds, &view.support);
    view.status = bench_status(&view, running);
    let bench = BenchItem {
        slug: function.slug.clone(),
        qualified: item.brief.qualified.clone(),
        file: item.brief.file.clone(),
        lang,
        change: item.brief.change,
        stored,
        values,
        warmup,
        secs,
        rounds,
        template: item.command.clone(),
        loop_file: loaded.loop_file,
        package: item.package.clone(),
        params,
        input_hash,
    };
    Some((view, bench))
}

fn refusal_text(refusal: Refusal) -> String {
    match refusal {
        Refusal::NotPlain => BENCH_FOLDER_REFUSED.to_string(),
        Refusal::Io(message) => message,
    }
}

fn function_folder(worktree: &Path, slug: &str, create: bool) -> Result<OwnedFd, String> {
    match cases::folder(worktree, slug, create) {
        Ok(Some(dir)) => Ok(dir),
        Ok(None) => Err(format!("cannot open the folder of {slug}")),
        Err(refusal) => Err(refusal_text(refusal)),
    }
}

/// Writes `bench_edits.json`, or removes it when nothing is left of the overlay.
fn store_edits(worktree: &Path, slug: &str, edits: &BenchEdits) -> Result<(), String> {
    let dir = function_folder(worktree, slug, false)?;
    if *edits == BenchEdits::default() {
        return cases::remove_at(&dir, BENCH_EDITS)
            .map_err(|e| format!("cannot remove {BENCH_EDITS}: {e}"));
    }
    let json = serde_json::to_vec_pretty(edits).map_err(|e| e.to_string())?;
    cases::write_whole(&dir, BENCH_EDITS, &json)
        .map_err(|e| format!("cannot write {BENCH_EDITS}: {e}"))
}

/// Writes a new benchmark: the loop file first and `bench.json` after it. The cases and their
/// driver are never touched. The caller holds the review lock.
pub(super) struct NewBench<'a> {
    pub slug: &'a str,
    pub lang: FunctionLang,
    pub qualified: &'a str,
    pub file: &'a str,
    /// The signature at head.
    pub signature: &'a str,
    pub head: &'a str,
    pub now: i64,
}

pub(super) fn adopt_bench(
    worktree: &Path,
    new: &NewBench<'_>,
    block: &BenchBlock,
) -> Result<(), String> {
    let dir = function_folder(worktree, new.slug, true)?;
    let name = format!("bench.{}", new.lang.ext());
    cases::write_whole(&dir, &name, block.bench.as_bytes())
        .map_err(|e| format!("cannot write {name}: {e}"))?;
    let stored = StoredBench {
        qualified: new.qualified.to_string(),
        file: new.file.to_string(),
        lang: new.lang,
        signature: new.signature.to_string(),
        values: block.values.clone(),
        warmup: block.warmup.unwrap_or(BENCH_WARMUP_DEFAULT),
        secs: block.secs.unwrap_or(BENCH_SECS_DEFAULT),
        rounds: BENCH_ROUNDS_DEFAULT,
        bench_sha: script_sha(block.bench.as_bytes()),
        written_head: new.head.to_string(),
        written_at: new.now,
    };
    let json = serde_json::to_vec_pretty(&stored).map_err(|e| e.to_string())?;
    cases::write_whole(&dir, BENCH_JSON, &json)
        .map_err(|e| format!("cannot write {BENCH_JSON}: {e}"))?;
    // The reviewer's settings were for the loop that was replaced.
    cases::remove_at(&dir, BENCH_EDITS).map_err(|e| format!("cannot remove {BENCH_EDITS}: {e}"))
}

/// Replaces only the values of benchmark `slug`; the reviewer's value overlay goes, its other
/// overlays stay. The caller holds the review lock.
pub(super) fn apply_values(
    worktree: &Path,
    slug: &str,
    params: &[Param],
    block: &BenchBlock,
) -> Result<(), String> {
    let loaded = load(worktree, slug, params);
    let Some(mut stored) = loaded.stored else {
        return Err(format!("There is no benchmark for {slug} yet"));
    };
    stored.values.clone_from(&block.values);
    let dir = function_folder(worktree, slug, false)?;
    let json = serde_json::to_vec_pretty(&stored).map_err(|e| e.to_string())?;
    cases::write_whole(&dir, BENCH_JSON, &json)
        .map_err(|e| format!("cannot write {BENCH_JSON}: {e}"))?;
    let mut edits = loaded.edits;
    edits.values.clear();
    store_edits(worktree, slug, &edits)
}

/// The stored benchmark of function `slug`, when it has one.
pub(super) fn stored_of(worktree: &Path, slug: &str, params: &[Param]) -> Option<StoredBench> {
    load(worktree, slug, params).stored
}

fn write_failed(message: String) -> Outcome {
    if message == BENCH_FOLDER_REFUSED || message == cases::FOLDER_REFUSED {
        reviews::invalid_state(message)
    } else {
        Outcome::Err(ProtocolError::new(ErrorCode::Internal, message))
    }
}

/// What a benchmark edit works on, read again under the review lock.
struct Editing {
    worktree: std::path::PathBuf,
    stored: StoredBench,
    params: Vec<Param>,
}

#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
fn editing(worktree: Option<&Path>, item: &cases::Item) -> Result<Editing, Outcome> {
    let Some(worktree) = worktree else {
        return Err(reviews::invalid_state(cases::OPEN_FIRST));
    };
    let params = item
        .stored
        .as_ref()
        .map(|s| s.params.clone())
        .unwrap_or_default();
    let loaded = load(worktree, &item.brief.slug, &params);
    if loaded.refused {
        return Err(reviews::invalid_state(BENCH_FOLDER_REFUSED));
    }
    let Some(stored) = loaded.stored else {
        return Err(reviews::invalid_state(format!(
            "There is no benchmark for {} yet",
            item.brief.qualified
        )));
    };
    Ok(Editing {
        worktree: worktree.to_path_buf(),
        stored,
        params,
    })
}

/// `SaveBench`: the reviewer's whole overlay of one benchmark. Only what differs from the
/// agent's is kept.
pub(crate) async fn save_bench(
    shared: &Arc<Shared>,
    pr: &PrRef,
    function: &str,
    input: BenchInput,
) -> Outcome {
    let (read, at) = match cases::writable(shared, pr, function).await {
        Ok(found) => found,
        Err(out) => return out,
    };
    if let Some(warmup) = input.warmup
        && warmup > BENCH_WARMUP_MAX
    {
        return reviews::bad_request(format!(
            "Warm-up calls must be between 0 and {BENCH_WARMUP_MAX}"
        ));
    }
    if let Some(secs) = input.secs
        && !(BENCH_SECS_MIN..=BENCH_SECS_MAX).contains(&secs)
    {
        return reviews::bad_request(format!(
            "Seconds must be between {BENCH_SECS_MIN} and {BENCH_SECS_MAX}"
        ));
    }
    if let Some(rounds) = input.rounds
        && !(SCENARIO_ROUNDS_MIN..=ROUNDS_MAX).contains(&rounds)
    {
        return reviews::bad_request(format!(
            "Rounds must be between {SCENARIO_ROUNDS_MIN} and {ROUNDS_MAX}"
        ));
    }
    {
        let _guard = reviews::lock(shared, pr).await;
        let Editing {
            worktree,
            stored,
            params,
            ..
        } = match editing(read.worktree.as_deref(), &read.items[at]) {
            Ok(editing) => editing,
            Err(out) => return out,
        };
        let mut edits = BenchEdits::default();
        for (name, value) in &input.values {
            let Some(param) = params.iter().find(|p| p.name == *name) else {
                return reviews::bad_request(format!("No parameter {name} in this function"));
            };
            if let Err(message) = check_value(param.kind, value) {
                return reviews::bad_request(message);
            }
            if stored.values.get(name) != Some(value) {
                edits.values.insert(name.clone(), value.clone());
            }
        }
        edits.warmup = input.warmup.filter(|w| *w != stored.warmup);
        edits.secs = input.secs.filter(|s| *s != stored.secs);
        edits.rounds = input.rounds.filter(|r| *r != stored.rounds);
        if let Err(message) = store_edits(&worktree, function, &edits) {
            return write_failed(message);
        }
    }
    scenarios::changed(shared, pr).await
}

/// `ResetBench`: drops the reviewer's edits of one benchmark.
pub(crate) async fn reset_bench(shared: &Arc<Shared>, pr: &PrRef, function: &str) -> Outcome {
    let (read, at) = match cases::writable(shared, pr, function).await {
        Ok(found) => found,
        Err(out) => return out,
    };
    {
        let _guard = reviews::lock(shared, pr).await;
        let Editing { worktree, .. } = match editing(read.worktree.as_deref(), &read.items[at]) {
            Ok(editing) => editing,
            Err(out) => return out,
        };
        if let Err(message) = store_edits(&worktree, function, &BenchEdits::default()) {
            return write_failed(message);
        }
    }
    scenarios::changed(shared, pr).await
}

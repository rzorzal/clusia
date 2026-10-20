//! The call graph of a pull request: an index of every function of the head commit, built off
//! the main loop from git objects within a 10 s budget, kept in memory for the few reviews in
//! use and on disk for the next start, and the graphs, searches, code and comments read from it.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use clusia_calls::{
    CallGraph, CallIndex, ChangeKind, ChangedFunction, Function, FunctionId, INDEX_BUDGET_SECS,
    Indexer, MAX_FILE_BYTES, MAX_FILES, MAX_GRAPH_NODES, Resolved, Skipped, SourceFile,
    changed_functions, dependencies_from_manifests, find, language_name, pack_for, resolve,
    signature_text, subgraph,
};
use clusia_core::config::CALLS_DEPTH_RANGE;
use clusia_core::hunks::{FileHunks, parse_hunks};
use clusia_core::printable::{printable, printable_lines};
use clusia_core::{Anchor, DraftKind, Origin, PrRef, Side, fits_diff};
use clusia_git::{
    GitError, TreeFile, diff_between, file_at, is_object_id, ls_tree, merge_base, read_blobs,
    repo_of_worktree, vendored_or_generated,
};
use clusia_protocol::{
    AnchorInput, CallsIndexState, CodeLine, DiagramCode, ErrorCode, Event, FunctionHit, Outcome,
    ProtocolError, Reply, topics,
};
use clusia_store::calls::{delete_calls, load_index, save_index};
use tokio::sync::Semaphore;
use tokio::task::AbortHandle;

use crate::reviews;
use crate::state::Shared;

/// The key prefix of a node outside the repository.
pub(crate) const OUTSIDE_PREFIX: &str = "outside:";
/// Indexes kept in memory, across reviews.
const MAX_READY: usize = 8;
/// Builds that run at once, across reviews.
const BUILDS: usize = 2;
/// Files read and parsed together.
const BATCH: usize = 256;
/// Manifests read for the dependencies.
const MAX_MANIFESTS: usize = 200;
/// How long a failed build is not tried again for the same commit.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// How often `wait_ready` looks.
const POLL: Duration = Duration::from_millis(50);
/// The budget of the index of the changed files at the merge base.
const BASE_BUDGET: Duration = Duration::from_secs(10);
const REASON_MAX: usize = 200;
const ID_MAX: usize = 128;
const QUERY_MAX: usize = 200;
const EXPAND_MAX: usize = 64;
/// The most lines and the longest line `read_function_code` gives.
const CODE_LINES_MAX: u32 = 200;
const LINE_MAX: usize = 1_000;
const TOO_BIG: &str = "larger than 512 KiB";
const NOT_TEXT: &str = "not UTF-8 text";
const ATTRIBUTES_UNREAD: &str =
    "the attributes of the commit could not be read; vendored and generated files were indexed";
const NOT_READY: &str = "The call graph is not ready yet";
const NOT_BUILT: &str = "The call graph could not be built";

/// What the graph of one head is read from: its index, the changed files at the merge base,
/// the resolved calls, the changed functions and the hunks of the pull request.
pub(crate) struct Built {
    pub index: CallIndex,
    /// The merge base of the review's base and the head.
    pub base: String,
    pub base_index: CallIndex,
    pub resolved: Resolved,
    pub changed: Vec<ChangedFunction>,
    pub hunks: Vec<FileHunks>,
    /// The blob of each path of the head, for signatures read later.
    oids: HashMap<String, String>,
    /// The signatures read so far, by function id.
    signatures: Mutex<HashMap<FunctionId, String>>,
}

impl Built {
    /// The function `id` at head, else a removed one at the base; `true` when at head.
    pub(crate) fn locate(&self, id: &str) -> Option<(&Function, bool)> {
        fn find_in<'a>(index: &'a CallIndex, id: &str) -> Option<&'a Function> {
            index.functions.iter().find(|f| f.id.0 == id)
        }
        find_in(&self.index, id).map(|f| (f, true)).or_else(|| {
            self.changed
                .iter()
                .any(|c| c.kind == ChangeKind::Removed && c.id.0 == id)
                .then(|| find_in(&self.base_index, id))
                .flatten()
                .map(|f| (f, false))
        })
    }

    /// The pull request's files whose language no pack reads.
    pub(crate) fn unsupported(&self) -> Vec<String> {
        let touched: BTreeSet<&str> = self
            .hunks
            .iter()
            .flat_map(|f| f.new_path.iter().chain(&f.old_path))
            .map(String::as_str)
            .collect();
        self.index
            .unsupported
            .iter()
            .filter(|p| touched.contains(p.as_str()))
            .cloned()
            .collect()
    }

    /// The changed function at head that comes first by path, then line.
    fn first_changed(&self) -> Option<FunctionId> {
        self.changed
            .iter()
            .filter(|c| c.kind != ChangeKind::Removed)
            .filter_map(|c| self.locate(&c.id.0))
            .filter(|(_, at_head)| *at_head)
            .map(|(f, _)| f)
            .min_by(|a, b| (&a.path, a.start_line).cmp(&(&b.path, b.start_line)))
            .map(|f| f.id.clone())
    }
}

struct Building {
    id: u64,
    commit: String,
    abort: Option<AbortHandle>,
}

struct Failure {
    commit: String,
    message: String,
    at: Instant,
}

#[derive(Default)]
pub(crate) struct Table {
    /// The least recently used first.
    pub(crate) ready: VecDeque<((PrRef, String), Arc<Built>)>,
    building: HashMap<PrRef, Building>,
    failed: HashMap<PrRef, Failure>,
    /// Builds started since the daemon started; the id of the next one.
    pub(crate) builds: u64,
}

pub(crate) struct Indexes {
    table: Mutex<Table>,
    places: Arc<Semaphore>,
}

impl Default for Indexes {
    fn default() -> Self {
        Self {
            table: Mutex::new(Table::default()),
            places: Arc::new(Semaphore::new(BUILDS)),
        }
    }
}

impl Indexes {
    pub(crate) fn table(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The index of `commit` for `pr`, which becomes the most recently used.
    pub(crate) fn ready(&self, pr: &PrRef, commit: &str) -> Option<Arc<Built>> {
        let mut table = self.table();
        let at = table
            .ready
            .iter()
            .position(|((p, c), _)| p == pr && c == commit)?;
        let entry = table.ready.remove(at)?;
        let built = entry.1.clone();
        table.ready.push_back(entry);
        Some(built)
    }

    fn insert(&self, pr: &PrRef, built: Arc<Built>) {
        let mut table = self.table();
        let key = (pr.clone(), built.index.commit.clone());
        table.ready.retain(|(k, _)| *k != key);
        table.ready.push_back((key, built));
        while table.ready.len() > MAX_READY {
            table.ready.pop_front();
        }
    }
}

fn tell(shared: &Shared, pr: &PrRef, state: CallsIndexState) {
    shared.publish(
        topics::AGENT,
        Event::CallsIndexChanged {
            pr: pr.clone(),
            state,
        },
    );
}

fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn shown(id: &str) -> String {
    printable(&cut(id, ID_MAX))
}

fn reason(message: String) -> String {
    cut(&printable(&message), REASON_MAX)
}

fn unreadable(e: GitError) -> String {
    reason(format!("The repository could not be read: {e}"))
}

fn seven(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

fn not_found(message: String) -> Outcome {
    Outcome::Err(ProtocolError::new(ErrorCode::NotFound, message))
}

fn no_function(id: &str) -> Outcome {
    not_found(format!(
        "there is no function {} in the call graph",
        shown(id)
    ))
}

pub(crate) async fn ensure_index(shared: &Arc<Shared>, pr: &PrRef) -> CallsIndexState {
    ensure_with(shared, pr, Duration::from_secs(INDEX_BUDGET_SECS)).await
}

/// `ensure_index` with the budget of a new build.
async fn ensure_with(shared: &Arc<Shared>, pr: &PrRef, budget: Duration) -> CallsIndexState {
    let review = match reviews::load_stored(shared, pr) {
        Ok(Some(review)) => review,
        _ => {
            return CallsIndexState::Failed {
                message: format!("no review for {pr}; open it first"),
            };
        }
    };
    let commit = review.head_sha;
    if let Some(built) = shared.indexes.ready(pr, &commit) {
        return CallsIndexState::Ready {
            commit,
            cut: built.index.cut,
        };
    }
    let id = {
        let mut table = shared.indexes.table();
        if table.building.get(pr).is_some_and(|b| b.commit == commit) {
            return CallsIndexState::Building;
        }
        if let Some(failure) = table
            .failed
            .get(pr)
            .filter(|f| f.commit == commit && f.at.elapsed() < RETRY_AFTER)
        {
            return CallsIndexState::Failed {
                message: failure.message.clone(),
            };
        }
        table.failed.remove(pr);
        if let Some(abort) = table.building.remove(pr).and_then(|older| older.abort) {
            abort.abort();
        }
        table.builds += 1;
        let id = table.builds;
        // Registered before the task starts, so a build that ends at once still finds itself.
        table.building.insert(
            pr.clone(),
            Building {
                id,
                commit: commit.clone(),
                abort: None,
            },
        );
        id
    };
    tell(shared, pr, CallsIndexState::Building);
    let task = tokio::spawn(build(
        shared.clone(),
        pr.clone(),
        id,
        commit,
        review.base_sha,
        budget,
    ));
    if let Some(building) = shared
        .indexes
        .table()
        .building
        .get_mut(pr)
        .filter(|b| b.id == id)
    {
        building.abort = Some(task.abort_handle());
    }
    CallsIndexState::Building
}

async fn build(
    shared: Arc<Shared>,
    pr: PrRef,
    id: u64,
    commit: String,
    base_sha: String,
    budget: Duration,
) {
    let outcome = {
        let _place = shared.indexes.places.clone().acquire_owned().await;
        assemble(&shared, &pr, &commit, &base_sha, budget).await
    };
    settle(&shared, &pr, id, &commit, outcome).await;
}

/// Keeps what build `id` made, unless it was dropped meanwhile. Under the review lock, so a
/// publish or a discard that deleted the index is never undone by a late write.
async fn settle(
    shared: &Shared,
    pr: &PrRef,
    id: u64,
    commit: &str,
    outcome: Result<(Built, bool), String>,
) {
    let _guard = reviews::lock(shared, pr).await;
    {
        let mut table = shared.indexes.table();
        if !table.building.get(pr).is_some_and(|b| b.id == id) {
            return;
        }
        table.building.remove(pr);
    }
    let state = match outcome {
        Ok((built, cached)) => {
            // A partial index stays in memory only: stored, a restart would serve it for this
            // head for good.
            if !cached
                && !built.index.cut
                && matches!(reviews::load_stored(shared, pr), Ok(Some(_)))
                && let Err(e) = save_index(&shared.paths, pr, &built.index)
            {
                tracing::warn!(error = %e, pr = %pr, "cannot store the call graph index");
            }
            let cut = built.index.cut;
            shared.indexes.insert(pr, Arc::new(built));
            CallsIndexState::Ready {
                commit: commit.to_string(),
                cut,
            }
        }
        Err(message) => {
            shared.indexes.table().failed.insert(
                pr.clone(),
                Failure {
                    commit: commit.to_string(),
                    message: message.clone(),
                    at: Instant::now(),
                },
            );
            CallsIndexState::Failed { message }
        }
    };
    tell(shared, pr, state);
}

/// Everything the graph of `commit` needs; `true` when the head index came from the cache.
async fn assemble(
    shared: &Shared,
    pr: &PrRef,
    commit: &str,
    base_sha: &str,
    budget: Duration,
) -> Result<(Built, bool), String> {
    let deadline = Instant::now() + budget;
    if !is_object_id(commit) || !is_object_id(base_sha) {
        return Err("The pull request's commits are not known yet".into());
    }
    let repo = repo_of_worktree(&shared.paths.worktree_for(pr))
        .await
        .map_err(unreadable)?;
    let base = merge_base(&repo, base_sha, commit)
        .await
        .map_err(unreadable)?;
    let hunks = parse_hunks(
        &diff_between(&repo, &base, commit)
            .await
            .map_err(unreadable)?,
    );
    let head_tree = ls_tree(&repo, commit).await.map_err(unreadable)?;
    let new_paths: BTreeSet<String> = hunks.iter().filter_map(|f| f.new_path.clone()).collect();
    let old_paths: BTreeSet<String> = hunks.iter().filter_map(|f| f.old_path.clone()).collect();

    let (stored, wait) = (shared.paths.clone(), (pr.clone(), commit.to_string()));
    let cached = tokio::task::spawn_blocking(move || load_index(&stored, &wait.0, &wait.1))
        .await
        .map_err(|_| NOT_BUILT.to_string())?;
    let from_cache = cached.is_some();
    let (index, head_texts) = match cached {
        Some(index) => {
            let texts = texts(&repo, &head_tree, &new_paths)
                .await
                .map_err(unreadable)?;
            (index, texts)
        }
        None => {
            let paths: Vec<String> = head_tree.iter().map(|f| f.path.clone()).collect();
            let (excluded, attributes_read) = match vendored_or_generated(&repo, commit, &paths)
                .await
            {
                Ok(excluded) => (excluded, true),
                Err(e) => {
                    tracing::warn!(error = %e, "the attributes of the commit could not be read; nothing is left out");
                    (HashSet::new(), false)
                }
            };
            let dependencies = manifests(&repo, &head_tree, &excluded)
                .await
                .map_err(unreadable)?;
            let (mut index, texts) = index_head(
                &repo,
                commit,
                &head_tree,
                &excluded,
                &new_paths,
                dependencies,
                deadline,
            )
            .await?;
            if !attributes_read {
                index.skipped.push(Skipped {
                    path: ".gitattributes".into(),
                    reason: ATTRIBUTES_UNREAD.into(),
                });
                index.skipped.sort_by(|a, b| a.path.cmp(&b.path));
            }
            (index, texts)
        }
    };

    let base_tree = if old_paths.is_empty() {
        Vec::new()
    } else {
        ls_tree(&repo, &base).await.map_err(unreadable)?
    };
    let base_texts = texts(&repo, &base_tree, &old_paths)
        .await
        .map_err(unreadable)?;
    let oids: HashMap<String, String> = head_tree.into_iter().map(|f| (f.path, f.oid)).collect();

    let base_commit = base.clone();
    let (index, base_index, resolved, changed, hunks) = tokio::task::spawn_blocking(move || {
        let mut indexer = Indexer::new(
            &base_commit,
            index.dependencies.clone(),
            Instant::now() + BASE_BUDGET,
        );
        let mut ordered: Vec<(&String, &String)> = base_texts.iter().collect();
        ordered.sort();
        for (path, text) in ordered {
            if !indexer.add(SourceFile { path, text }) {
                break;
            }
        }
        let base_index = indexer.finish();
        let resolved = resolve(&index);
        let changed = changed_functions(
            &index,
            &base_index,
            &hunks,
            &|p| head_texts.get(p).cloned(),
            &|p| base_texts.get(p).cloned(),
        );
        (index, base_index, resolved, changed, hunks)
    })
    .await
    .map_err(|_| NOT_BUILT.to_string())?;
    let built = Built {
        index,
        base,
        base_index,
        resolved,
        changed,
        hunks,
        oids,
        signatures: Mutex::new(HashMap::new()),
    };
    Ok((built, from_cache))
}

/// The files of `wanted` that have a pack, read at the tree's commit (at most `MAX_FILE_BYTES`).
async fn texts(
    repo: &Path,
    tree: &[TreeFile],
    wanted: &BTreeSet<String>,
) -> Result<HashMap<String, String>, GitError> {
    let files: Vec<&TreeFile> = tree
        .iter()
        .filter(|f| {
            wanted.contains(&f.path) && pack_for(&f.path).is_some() && f.size <= MAX_FILE_BYTES
        })
        .collect();
    let oids: Vec<String> = files.iter().map(|f| f.oid.clone()).collect();
    let read = read_blobs(repo, &oids, MAX_FILE_BYTES).await?;
    Ok(files
        .iter()
        .zip(read)
        .filter_map(|(f, text)| text.map(|text| (f.path.clone(), text)))
        .collect())
}

fn is_manifest(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(name, "Cargo.toml" | "package.json" | "pyproject.toml")
        || (name.starts_with("requirements") && name.ends_with(".txt"))
}

/// The dependency names of the manifests of the tree.
async fn manifests(
    repo: &Path,
    tree: &[TreeFile],
    excluded: &HashSet<String>,
) -> Result<Vec<String>, GitError> {
    let files: Vec<&TreeFile> = tree
        .iter()
        .filter(|f| is_manifest(&f.path) && !excluded.contains(&f.path) && f.size <= MAX_FILE_BYTES)
        .take(MAX_MANIFESTS)
        .collect();
    let oids: Vec<String> = files.iter().map(|f| f.oid.clone()).collect();
    let read = read_blobs(repo, &oids, MAX_FILE_BYTES).await?;
    let sources: Vec<SourceFile<'_>> = files
        .iter()
        .zip(&read)
        .filter_map(|(f, text)| {
            text.as_deref().map(|text| SourceFile {
                path: &f.path,
                text,
            })
        })
        .collect();
    Ok(dependencies_from_manifests(&sources))
}

/// The index of the head: the pull request's files first, then by path, read and parsed in
/// batches until the deadline; and the text of the pull request's files.
async fn index_head(
    repo: &Path,
    commit: &str,
    tree: &[TreeFile],
    excluded: &HashSet<String>,
    changed: &BTreeSet<String>,
    dependencies: Vec<String>,
    deadline: Instant,
) -> Result<(CallIndex, HashMap<String, String>), String> {
    let mut files: Vec<&TreeFile> = tree
        .iter()
        .filter(|f| pack_for(&f.path).is_some() && !excluded.contains(&f.path))
        .collect();
    files.sort_by(|a, b| {
        (!changed.contains(&a.path), &a.path).cmp(&(!changed.contains(&b.path), &b.path))
    });
    let mut cut = files.len() > MAX_FILES;
    files.truncate(MAX_FILES);
    let mut skipped = Vec::new();
    let mut texts = HashMap::new();
    let mut indexer = Indexer::new(commit, dependencies, deadline);
    for batch in files.chunks(BATCH) {
        let (big, small): (Vec<&TreeFile>, Vec<&TreeFile>) =
            batch.iter().partition(|f| f.size > MAX_FILE_BYTES);
        skipped.extend(big.iter().map(|f| Skipped {
            path: f.path.clone(),
            reason: TOO_BIG.into(),
        }));
        let left = deadline.saturating_duration_since(Instant::now());
        let oids: Vec<String> = small.iter().map(|f| f.oid.clone()).collect();
        let Ok(read) = tokio::time::timeout(left, read_blobs(repo, &oids, MAX_FILE_BYTES)).await
        else {
            cut = true;
            break;
        };
        let mut sources = Vec::new();
        for (file, text) in small.iter().zip(read.map_err(unreadable)?) {
            match text {
                Some(text) => sources.push((file.path.clone(), text)),
                None => skipped.push(Skipped {
                    path: file.path.clone(),
                    reason: NOT_TEXT.into(),
                }),
            }
        }
        let (back, more, sources) = tokio::task::spawn_blocking(move || {
            let mut more = true;
            for (path, text) in &sources {
                if !indexer.add(SourceFile { path, text }) {
                    more = false;
                    break;
                }
            }
            (indexer, more, sources)
        })
        .await
        .map_err(|_| NOT_BUILT.to_string())?;
        indexer = back;
        texts.extend(
            sources
                .into_iter()
                .filter(|(path, _)| changed.contains(path)),
        );
        if !more {
            cut = true;
            break;
        }
    }
    let mut index = indexer.finish();
    index.cut |= cut;
    index.skipped.extend(skipped);
    index.skipped.sort_by(|a, b| a.path.cmp(&b.path));
    index.skipped.dedup();
    index.unsupported = tree
        .iter()
        .filter(|f| pack_for(&f.path).is_none() && language_name(&f.path).is_some())
        .map(|f| f.path.clone())
        .collect();
    index.unsupported.sort();
    Ok((index, texts))
}

pub(crate) async fn wait_ready(
    shared: &Arc<Shared>,
    pr: &PrRef,
    commit: &str,
    limit: Duration,
) -> Option<Arc<Built>> {
    let give_up = Instant::now() + limit;
    loop {
        if let Some(built) = shared.indexes.ready(pr, commit) {
            return Some(built);
        }
        if matches!(
            ensure_index(shared, pr).await,
            CallsIndexState::Failed { .. }
        ) || Instant::now() >= give_up
        {
            return None;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The built index of the review's head, or the refusal to send while it is not.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
async fn ready_for_review(shared: &Arc<Shared>, pr: &PrRef) -> Result<Arc<Built>, Outcome> {
    match ensure_index(shared, pr).await {
        CallsIndexState::Ready { commit, .. } => shared
            .indexes
            .ready(pr, &commit)
            .ok_or_else(|| reviews::invalid_state(NOT_READY)),
        CallsIndexState::Failed { message } if message.contains("open it first") => {
            Err(reviews::invalid_state(message))
        }
        _ => Err(reviews::invalid_state(NOT_READY)),
    }
}

/// Fills the signature of each node with a function, reading only the files of nodes not
/// signed before.
async fn sign(shared: &Shared, pr: &PrRef, built: &Built, graph: &mut CallGraph) {
    let missing: HashSet<FunctionId> = {
        let known = built.signatures.lock().unwrap_or_else(|p| p.into_inner());
        graph
            .nodes
            .iter()
            .filter_map(|n| n.function.clone())
            .filter(|id| !known.contains_key(id))
            .collect()
    };
    if !missing.is_empty() {
        let functions: Vec<&Function> = built
            .index
            .functions
            .iter()
            .filter(|f| missing.contains(&f.id))
            .collect();
        let paths: BTreeSet<&str> = functions.iter().map(|f| f.path.as_str()).collect();
        let files: Vec<(&str, &String)> = paths
            .into_iter()
            .filter_map(|path| built.oids.get(path).map(|oid| (path, oid)))
            .collect();
        let oids: Vec<String> = files.iter().map(|(_, oid)| (*oid).clone()).collect();
        let read = match repo_of_worktree(&shared.paths.worktree_for(pr)).await {
            Ok(repo) => read_blobs(&repo, &oids, MAX_FILE_BYTES).await.ok(),
            Err(_) => None,
        };
        if let Some(read) = read {
            let text_of: HashMap<&str, String> = files
                .iter()
                .zip(read)
                .filter_map(|((path, _), text)| text.map(|text| (*path, text)))
                .collect();
            let mut known = built.signatures.lock().unwrap_or_else(|p| p.into_inner());
            for f in functions {
                if let Some(text) = text_of.get(f.path.as_str()) {
                    known.insert(f.id.clone(), signature_text(text, f));
                }
            }
        }
    }
    let known = built.signatures.lock().unwrap_or_else(|p| p.into_inner());
    for node in &mut graph.nodes {
        if let Some(signature) = node.function.as_ref().and_then(|id| known.get(id)) {
            node.signature = signature.clone();
        }
    }
}

pub(crate) async fn get_calls(
    shared: &Arc<Shared>,
    pr: &PrRef,
    center: Option<&str>,
    depth: Option<u8>,
    expand: &[i8],
) -> Outcome {
    let depth = match depth {
        Some(depth) => depth,
        None => shared.config.read().await.diff.calls_depth,
    };
    if !CALLS_DEPTH_RANGE.contains(&depth) {
        return reviews::bad_request(format!(
            "the depth must be between {} and {}, got {depth}",
            CALLS_DEPTH_RANGE.start(),
            CALLS_DEPTH_RANGE.end()
        ));
    }
    if expand.len() > EXPAND_MAX {
        return reviews::bad_request(format!("at most {EXPAND_MAX} levels can be expanded"));
    }
    let state = ensure_index(shared, pr).await;
    let built = match &state {
        CallsIndexState::Ready { commit, .. } => shared.indexes.ready(pr, commit),
        _ => None,
    };
    let Some(built) = built else {
        return Outcome::Ok(Reply::Calls {
            graph: None,
            index: state,
            unsupported: Vec::new(),
        });
    };
    let centre = match center {
        Some(id) => match built.index.functions.iter().find(|f| f.id.0 == id) {
            Some(f) => Some(f.id.clone()),
            None => return no_function(id),
        },
        None => built.first_changed(),
    };
    let unsupported = built.unsupported();
    let Some(centre) = centre else {
        return Outcome::Ok(Reply::Calls {
            graph: None,
            index: state,
            unsupported,
        });
    };
    let (drawn, expand) = (built.clone(), expand.to_vec());
    let graph = tokio::task::spawn_blocking(move || {
        subgraph(
            &drawn.index,
            &drawn.resolved,
            &drawn.changed,
            &centre,
            depth,
            &expand,
        )
    })
    .await;
    let Ok(mut graph) = graph else {
        return Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            "the call graph could not be drawn",
        ));
    };
    sign(shared, pr, &built, &mut graph).await;
    Outcome::Ok(Reply::Calls {
        graph: Some(graph),
        index: state,
        unsupported,
    })
}

/// `f` as a hit of the search, with its change from `changed_functions`.
fn hit(built: &Built, f: &Function) -> FunctionHit {
    let kind = built.changed.iter().find(|c| c.id == f.id).map(|c| c.kind);
    FunctionHit {
        id: f.id.0.clone(),
        qualified: f.qualified.clone(),
        path: f.path.clone(),
        line: f.start_line,
        changed: kind.is_some(),
        kind,
    }
}

pub(crate) async fn find_function(shared: &Arc<Shared>, pr: &PrRef, query: &str) -> Outcome {
    let query = query.trim();
    if query.chars().count() > QUERY_MAX {
        return reviews::bad_request(format!("the query is longer than {QUERY_MAX} characters"));
    }
    let built = match ensure_index(shared, pr).await {
        CallsIndexState::Ready { commit, .. } => shared.indexes.ready(pr, &commit),
        _ => None,
    };
    let Some(built) = built else {
        return Outcome::Ok(Reply::Functions(Vec::new()));
    };
    let hits = if query.is_empty() {
        let mut changed: Vec<&Function> = built
            .changed
            .iter()
            .filter(|c| c.kind != ChangeKind::Removed)
            .filter_map(|c| built.locate(&c.id.0))
            .filter(|(_, at_head)| *at_head)
            .map(|(f, _)| f)
            .collect();
        changed.sort_by(|a, b| (&a.path, a.start_line).cmp(&(&b.path, b.start_line)));
        changed
            .into_iter()
            .take(MAX_GRAPH_NODES)
            .map(|f| hit(&built, f))
            .collect()
    } else {
        let by_id: HashMap<&FunctionId, &Function> =
            built.index.functions.iter().map(|f| (&f.id, f)).collect();
        let mut hits: Vec<FunctionHit> = find(&built.index, query)
            .iter()
            .filter_map(|id| by_id.get(id))
            .map(|f| hit(&built, f))
            .collect();
        hits.sort_by_key(|h| h.kind.is_none());
        hits
    };
    Outcome::Ok(Reply::Functions(hits))
}

/// The head lines the pull request adds to `path`.
fn added_at_head(hunks: &[FileHunks], path: &str) -> BTreeSet<u32> {
    hunks
        .iter()
        .filter(|f| f.new_path.as_deref() == Some(path))
        .flat_map(|f| &f.hunks)
        .flat_map(|h| h.new_start..h.new_start + h.new_len)
        .collect()
}

/// The lines of `function` in the first hunk of its file that adds lines inside it.
fn first_changed_lines(hunks: &[FileHunks], function: &Function) -> Option<(u32, u32)> {
    hunks
        .iter()
        .filter(|f| f.new_path.as_deref() == Some(function.path.as_str()))
        .flat_map(|f| &f.hunks)
        .filter(|h| h.new_len > 0)
        .map(|h| {
            (
                h.new_start.max(function.start_line),
                (h.new_start + h.new_len - 1).min(function.end_line),
            )
        })
        .filter(|(start, end)| start <= end)
        .min()
}

pub(crate) async fn read_function_code(
    shared: &Arc<Shared>,
    pr: &PrRef,
    function: &str,
) -> Outcome {
    let built = match ready_for_review(shared, pr).await {
        Ok(built) => built,
        Err(out) => return out,
    };
    let Some((f, at_head)) = built.locate(function) else {
        return no_function(function);
    };
    let commit = if at_head {
        built.index.commit.clone()
    } else {
        built.base.clone()
    };
    let unavailable = |why: String| {
        Outcome::Ok(Reply::FunctionCode(DiagramCode {
            path: f.path.clone(),
            commit: commit.clone(),
            start_line: f.start_line,
            lines: Vec::new(),
            unavailable: Some(why),
        }))
    };
    let text = match repo_of_worktree(&shared.paths.worktree_for(pr)).await {
        Ok(repo) => file_at(&repo, &commit, &f.path, MAX_FILE_BYTES).await.ok(),
        Err(_) => None,
    };
    let Some(text) = text else {
        return unavailable(format!(
            "The code at {} is no longer available.",
            seven(&commit)
        ));
    };
    let last = f
        .end_line
        .min(f.start_line.saturating_add(CODE_LINES_MAX - 1));
    let added = if at_head {
        added_at_head(&built.hunks, &f.path)
    } else {
        BTreeSet::new()
    };
    // Lines count from 1: a function a damaged cache puts at line 0 shows no code.
    let lines: Vec<CodeLine> = (f.start_line..=last)
        .filter(|_| f.start_line > 0)
        .zip(text.lines().skip((f.start_line as usize).saturating_sub(1)))
        .map(|(number, line)| {
            let kept = cut(line, LINE_MAX);
            let shown = printable_lines(&kept);
            CodeLine {
                number,
                text: if line.chars().count() > LINE_MAX {
                    format!("{shown}…")
                } else {
                    shown
                },
                changed: added.contains(&number),
            }
        })
        .collect();
    if lines.is_empty() {
        return unavailable(format!(
            "These lines are not in the file at {}.",
            seven(&commit)
        ));
    }
    Outcome::Ok(Reply::FunctionCode(DiagramCode {
        path: f.path.clone(),
        commit,
        start_line: f.start_line,
        lines,
        unavailable: None,
    }))
}

async fn add_general(shared: &Shared, client: &str, pr: &PrRef, text: &str) -> Outcome {
    let item = reviews::NewItem {
        kind: DraftKind::General,
        anchor: None,
        thread: None,
        body: text,
    };
    reviews::add_item_as(shared, client, pr, Origin::Human, item).await
}

pub(crate) async fn add_calls_comment(
    shared: &Arc<Shared>,
    client: &str,
    pr: &PrRef,
    function: &str,
    body: &str,
) -> Outcome {
    let comment = body.trim();
    if comment.is_empty() {
        return reviews::bad_request("the comment is empty");
    }
    if let Some(name) = function.strip_prefix(OUTSIDE_PREFIX) {
        return add_general(shared, client, pr, &format!("{}: {comment}", shown(name))).await;
    }
    let built = match ready_for_review(shared, pr).await {
        Ok(built) => built,
        Err(out) => return out,
    };
    let Some((f, at_head)) = built.locate(function) else {
        return no_function(function);
    };
    let head = built.index.commit.clone();
    // GitHub's files are read without the review's lock; only the write takes it, and a head
    // that moved meanwhile sends the comment to the review body.
    let on_lines = match first_changed_lines(&built.hunks, f).filter(|_| at_head) {
        Some((start, end)) => match reviews::files_for(shared, pr, &head).await {
            Ok(files) => {
                let at = Anchor {
                    path: f.path.clone(),
                    line: end,
                    start_line: (start != end).then_some(start),
                    side: Side::Right,
                    commit: head.clone(),
                };
                fits_diff(&files, &at).then_some(AnchorInput {
                    path: at.path,
                    line: end,
                    start_line: at.start_line,
                    side: Side::Right,
                })
            }
            Err(_) => None,
        },
        None => None,
    };
    let _guard = reviews::lock(shared, pr).await;
    let head_now = match reviews::load_existing(shared, pr) {
        Ok(review) => review.head_sha,
        Err(out) => return out,
    };
    let item = match on_lines {
        Some(anchor) if head_now == head => reviews::NewItem {
            kind: DraftKind::LineComment,
            anchor: Some(anchor),
            thread: None,
            body: comment,
        },
        _ => {
            let lines = if f.start_line == f.end_line {
                f.start_line.to_string()
            } else {
                format!("{}-{}", f.start_line, f.end_line)
            };
            let text = format!("{}:{lines}: {comment}", printable(&f.path));
            return reviews::add_item_locked(
                shared,
                client,
                pr,
                Origin::Human,
                reviews::NewItem {
                    kind: DraftKind::General,
                    anchor: None,
                    thread: None,
                    body: &text,
                },
            )
            .await;
        }
    };
    reviews::add_item_locked(shared, client, pr, Origin::Human, item).await
}

pub(crate) fn forget(shared: &Shared, pr: &PrRef) {
    {
        let mut table = shared.indexes.table();
        table.ready.retain(|((p, _), _)| p != pr);
        table.failed.remove(pr);
        if let Some(abort) = table.building.remove(pr).and_then(|b| b.abort) {
            abort.abort();
        }
    }
    if let Err(e) = delete_calls(&shared.paths, pr) {
        tracing::warn!(error = %e, pr = %pr, "cannot delete the call graph index");
    }
    tell(shared, pr, CallsIndexState::Idle);
}

pub(crate) async fn shutdown(shared: &Shared) {
    let mut table = shared.indexes.table();
    for building in table.building.values_mut() {
        if let Some(abort) = building.abort.take() {
            abort.abort();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use clusia_harness::testkit::{Script, Turn};
    use clusia_store::save_review;

    use super::*;
    use crate::checks::tests::lab;

    pub(crate) fn pr(n: u64) -> PrRef {
        format!("acme/widgets#{n}").parse().unwrap()
    }

    /// `src/store.rs` at the review's base: a store whose token refreshes, a middleware that asks
    /// for the token and a helper that calls the middleware.
    pub(crate) const STORE_BASE: &str = "\
pub struct TokenStore {}

impl TokenStore {
    pub fn token(&self) -> u32 {
        self.refresh()
    }

    fn refresh(&self) -> u32 {
        1
    }
}

pub fn auth_middleware(s: &TokenStore) -> u32 {
    s.token()
}

pub fn untouched_helper() -> u32 {
    auth_middleware(&TokenStore {})
}
";

    /// The head: line 9 changes.
    pub(crate) const STORE_HEAD: &str = "\
pub struct TokenStore {}

impl TokenStore {
    pub fn token(&self) -> u32 {
        self.refresh()
    }

    fn refresh(&self) -> u32 {
        2
    }
}

pub fn auth_middleware(s: &TokenStore) -> u32 {
    s.token()
}

pub fn untouched_helper() -> u32 {
    auth_middleware(&TokenStore {})
}
";

    pub(crate) const REFRESH: &str = "src/store.rs#TokenStore::refresh#8";
    pub(crate) const TOKEN: &str = "src/store.rs#TokenStore::token#4";
    pub(crate) const MIDDLEWARE: &str = "src/store.rs#auth_middleware#13";
    pub(crate) const HELPER: &str = "src/store.rs#untouched_helper#17";

    pub(crate) fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Writes `files` into `root` (a `None` content deletes the file) and commits them.
    pub(crate) fn commit(root: &Path, files: &[(&str, Option<&str>)], message: &str) -> String {
        for (path, content) in files {
            let file = root.join(path);
            match content {
                Some(text) => {
                    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                    std::fs::write(&file, text).unwrap();
                }
                None => std::fs::remove_file(&file).unwrap(),
            }
        }
        git_in(root, &["add", "-A"]);
        git_in(root, &["commit", "-q", "--allow-empty", "-m", message]);
        git_in(root, &["rev-parse", "HEAD"])
    }

    pub(crate) fn set_review(shared: &Shared, n: u64, base: &str, head: &str) {
        let mut review = reviews::load_stored(shared, &pr(n)).unwrap().unwrap();
        review.base_sha = base.into();
        review.head_sha = head.into();
        save_review(&shared.paths, &review).unwrap();
    }

    /// Makes the worktree of review `n` a repository of two commits and the review's base and
    /// head: `base` files, then `head` changes. Returns (base, head).
    pub(crate) fn repo_with(
        shared: &Shared,
        n: u64,
        base: &[(&str, Option<&str>)],
        head: &[(&str, Option<&str>)],
    ) -> (String, String) {
        let root = shared.paths.worktree_for(&pr(n));
        git_in(&root, &["init", "-q", "-b", "main"]);
        let base_sha = commit(&root, base, "base");
        let head_sha = commit(&root, head, "Refresh twice");
        set_review(shared, n, &base_sha, &head_sha);
        (base_sha, head_sha)
    }

    /// The store at the base; at the head line 9 of the store changes, `README.md` gets a line
    /// and `tool.go` is added.
    pub(crate) fn code_repo(shared: &Shared, n: u64) -> (String, String) {
        repo_with(
            shared,
            n,
            &[
                ("src/store.rs", Some(STORE_BASE)),
                ("README.md", Some("hello\n")),
            ],
            &[
                ("src/store.rs", Some(STORE_HEAD)),
                ("README.md", Some("hello\nmore\n")),
                ("tool.go", Some("package main\n\nfunc main() {}\n")),
            ],
        )
    }

    /// What GitHub says the pull request changed at `head`: line 9 of the store.
    pub(crate) async fn serve_files(shared: &Shared, n: u64, head: &str) {
        let files = vec![clusia_core::FileDiff {
            path: "src/store.rs".into(),
            previous_path: None,
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            patch: Some(
                "@@ -8,3 +8,3 @@\n     fn refresh(&self) -> u32 {\n-        1\n+        2\n     }"
                    .into(),
            ),
        }];
        shared
            .files_cache
            .lock()
            .await
            .insert(pr(n), (head.to_string(), Arc::new(files)));
    }

    /// Asks until the index of the review's head is in memory.
    pub(crate) async fn built(shared: &Arc<Shared>, n: u64) -> Arc<Built> {
        let head = reviews::load_stored(shared, &pr(n))
            .unwrap()
            .unwrap()
            .head_sha;
        wait_ready(shared, &pr(n), &head, Duration::from_secs(30))
            .await
            .expect("the index is built")
    }

    fn lab_with_code() -> (tempfile::TempDir, tempfile::TempDir, Arc<Shared>, String) {
        let (home, fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        let (_base, head) = code_repo(&shared, 1);
        (home, fake, shared, head)
    }

    fn graph_of(out: Outcome) -> (Option<CallGraph>, CallsIndexState, Vec<String>) {
        match out {
            Outcome::Ok(Reply::Calls {
                graph,
                index,
                unsupported,
            }) => (graph, index, unsupported),
            other => panic!("calls, got {other:?}"),
        }
    }

    fn levels(graph: &CallGraph) -> Vec<(String, i8)> {
        let mut levels: Vec<(String, i8)> = graph
            .nodes
            .iter()
            .map(|n| (n.key.clone(), n.level))
            .collect();
        levels.sort();
        levels
    }

    fn refused(out: Outcome) -> ProtocolError {
        match out {
            Outcome::Err(e) => e,
            other => panic!("refused, got {other:?}"),
        }
    }

    fn item_of(out: Outcome) -> clusia_core::DraftItem {
        let Outcome::Ok(Reply::DraftItem(item)) = out else {
            panic!("a draft item, got {out:?}");
        };
        item
    }

    #[tokio::test]
    async fn the_index_builds_off_the_main_loop_and_the_graph_walks_up_to_the_depth() {
        let (_home, _fake, shared, head) = lab_with_code();
        let mut events = shared.events.subscribe();
        assert_eq!(
            ensure_index(&shared, &pr(1)).await,
            CallsIndexState::Building
        );
        let (graph, state, _) = graph_of(get_calls(&shared, &pr(1), None, Some(3), &[]).await);
        assert!(
            graph.is_none(),
            "nothing is drawn before the index is ready"
        );
        assert_eq!(state, CallsIndexState::Building);
        let built = built(&shared, 1).await;
        assert!(!built.index.cut);
        let mut told = Vec::new();
        while told.len() < 2 {
            let (topic, event) = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("an event")
                .expect("the channel stays open");
            if let Event::CallsIndexChanged { state, .. } = event
                && topic == topics::AGENT
            {
                told.push(state);
            }
        }
        assert_eq!(
            told,
            [
                CallsIndexState::Building,
                CallsIndexState::Ready {
                    commit: head.clone(),
                    cut: false
                }
            ]
        );
        let (graph, state, unsupported) =
            graph_of(get_calls(&shared, &pr(1), None, Some(3), &[]).await);
        assert_eq!(
            state,
            CallsIndexState::Ready {
                commit: head,
                cut: false
            }
        );
        assert_eq!(unsupported, ["tool.go"]);
        let graph = graph.expect("a graph");
        assert_eq!(graph.center, REFRESH, "the first changed function");
        assert_eq!(graph.depth, 3);
        assert_eq!(
            levels(&graph),
            [
                (REFRESH.to_string(), 0),
                (TOKEN.to_string(), -1),
                (MIDDLEWARE.to_string(), -2),
                (HELPER.to_string(), -3),
            ]
        );
        let centre = graph.nodes.iter().find(|n| n.key == REFRESH).unwrap();
        assert_eq!(centre.kind, clusia_calls::NodeKind::Changed);
        assert_eq!(centre.signature, "fn refresh(&self) -> u32 {");
        // Centred on the middleware at depth 1: its caller above, its callee below.
        let (graph, _, _) =
            graph_of(get_calls(&shared, &pr(1), Some(MIDDLEWARE), Some(1), &[]).await);
        assert_eq!(
            levels(&graph.expect("a graph")),
            [
                (TOKEN.to_string(), 1),
                (MIDDLEWARE.to_string(), 0),
                (HELPER.to_string(), -1),
            ]
        );
    }

    #[tokio::test]
    async fn one_build_per_head_and_a_ready_index_is_answered_from_memory() {
        let (_home, _fake, shared, head) = lab_with_code();
        assert_eq!(
            ensure_index(&shared, &pr(1)).await,
            CallsIndexState::Building
        );
        assert_eq!(
            ensure_index(&shared, &pr(1)).await,
            CallsIndexState::Building
        );
        built(&shared, 1).await;
        assert_eq!(
            ensure_index(&shared, &pr(1)).await,
            CallsIndexState::Ready {
                commit: head,
                cut: false
            }
        );
        assert_eq!(shared.indexes.table().builds, 1, "one build, not three");
    }

    #[tokio::test]
    async fn the_index_is_stored_and_a_cold_start_reads_it_before_any_parse() {
        let (_home, _fake, shared, head) = lab_with_code();
        built(&shared, 1).await;
        let file = shared.paths.calls_for(&pr(1)).join(format!("{head}.json"));
        assert!(file.is_file(), "written once built");
        // The stored index gains a function no file has: only a read of the cache can know it.
        let mut stored = clusia_store::calls::load_index(&shared.paths, &pr(1), &head).unwrap();
        stored.functions.push(Function {
            id: FunctionId("src/ghost.rs#Ghost::haunt#1".into()),
            qualified: "Ghost::haunt".into(),
            name: "haunt".into(),
            path: "src/ghost.rs".into(),
            start_line: 1,
            end_line: 3,
            signature_lines: 1,
            lang: "rust".into(),
        });
        clusia_store::calls::save_index(&shared.paths, &pr(1), &stored).unwrap();
        shared.indexes.table().ready.clear();
        built(&shared, 1).await;
        let Outcome::Ok(Reply::Functions(hits)) = find_function(&shared, &pr(1), "haunt").await
        else {
            panic!("functions");
        };
        assert_eq!(
            hits.iter()
                .map(|h| h.qualified.as_str())
                .collect::<Vec<_>>(),
            ["Ghost::haunt"]
        );
    }

    #[tokio::test]
    async fn a_corrupt_cache_is_set_aside_and_the_index_rebuilt() {
        let (_home, _fake, shared, head) = lab_with_code();
        let dir = shared.paths.calls_for(&pr(1));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{head}.json")), b"{ not json").unwrap();
        let built = built(&shared, 1).await;
        assert!(built.index.functions.iter().any(|f| f.id.0 == REFRESH));
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n.contains("corrupt")), "{names:?}");
        assert!(
            clusia_store::calls::load_index(&shared.paths, &pr(1), &head).is_some(),
            "the rebuilt index is stored"
        );
    }

    #[tokio::test]
    async fn a_budget_that_runs_out_keeps_what_was_parsed_and_says_cut() {
        let (_home, _fake, shared, head) = lab_with_code();
        let file = shared.paths.calls_for(&pr(1)).join(format!("{head}.json"));
        assert_eq!(
            ensure_with(&shared, &pr(1), Duration::ZERO).await,
            CallsIndexState::Building
        );
        let built = built(&shared, 1).await;
        assert!(built.index.cut);
        assert_eq!(
            ensure_index(&shared, &pr(1)).await,
            CallsIndexState::Ready {
                commit: head,
                cut: true
            }
        );
        let (graph, _, _) = graph_of(get_calls(&shared, &pr(1), None, Some(3), &[]).await);
        assert!(
            graph.is_none(),
            "nothing was parsed, so nothing changed is known"
        );
        assert!(
            !file.exists(),
            "a partial index is not stored, so a restart builds it again"
        );
    }

    #[tokio::test]
    async fn forget_deletes_the_index_and_drops_a_build_that_is_still_running() {
        let (_home, _fake, shared, head) = lab_with_code();
        built(&shared, 1).await;
        let dir = shared.paths.calls_for(&pr(1));
        assert!(dir.is_dir());
        let mut events = shared.events.subscribe();
        forget(&shared, &pr(1));
        assert!(!dir.exists());
        assert!(shared.indexes.ready(&pr(1), &head).is_none());
        assert!(
            std::iter::from_fn(|| events.try_recv().ok()).any(|(_, event)| matches!(
                event,
                Event::CallsIndexChanged {
                    state: CallsIndexState::Idle,
                    ..
                }
            )),
            "the windows hear the index is gone"
        );
        // A build forgotten while it runs stores nothing.
        assert_eq!(
            ensure_index(&shared, &pr(1)).await,
            CallsIndexState::Building
        );
        forget(&shared, &pr(1));
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(!dir.exists());
        assert!(shared.indexes.ready(&pr(1), &head).is_none());
    }

    #[tokio::test]
    async fn a_repository_that_cannot_be_read_fails_and_is_not_retried_at_once() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        // The lab's review names commits "b" and "h" in an empty folder.
        assert_eq!(
            ensure_index(&shared, &pr(1)).await,
            CallsIndexState::Building
        );
        assert!(
            wait_ready(&shared, &pr(1), "h", Duration::from_secs(10))
                .await
                .is_none()
        );
        let CallsIndexState::Failed { message } = ensure_index(&shared, &pr(1)).await else {
            panic!("failed");
        };
        assert!(
            !message.is_empty() && message.chars().count() <= REASON_MAX,
            "{message}"
        );
        assert!(matches!(
            ensure_index(&shared, &pr(1)).await,
            CallsIndexState::Failed { .. }
        ));
        assert_eq!(shared.indexes.table().builds, 1);
        assert!(matches!(
            ensure_index(&shared, &pr(9)).await,
            CallsIndexState::Failed { message } if message.contains("open it first")
        ));
    }

    #[tokio::test]
    async fn the_depth_comes_from_the_config_and_bad_asks_are_refused() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |config| {
            config.diff.calls_depth = 1;
        });
        code_repo(&shared, 1);
        built(&shared, 1).await;
        let (graph, _, _) = graph_of(get_calls(&shared, &pr(1), None, None, &[]).await);
        let graph = graph.expect("a graph");
        assert_eq!(graph.depth, 1);
        assert_eq!(
            levels(&graph),
            [(REFRESH.to_string(), 0), (TOKEN.to_string(), -1)]
        );
        for depth in [0, 6] {
            let e = refused(get_calls(&shared, &pr(1), None, Some(depth), &[]).await);
            assert_eq!(e.code, ErrorCode::BadRequest);
            assert_eq!(
                e.message,
                format!("the depth must be between 1 and 5, got {depth}")
            );
        }
        let e = refused(get_calls(&shared, &pr(1), Some("nope\u{1b}[2J"), None, &[]).await);
        assert_eq!(e.code, ErrorCode::NotFound);
        assert_eq!(
            e.message,
            "there is no function nope\\u{1b}[2J in the call graph"
        );
        let e = refused(get_calls(&shared, &pr(1), None, None, &[1; 65]).await);
        assert_eq!(e.code, ErrorCode::BadRequest);
    }

    #[tokio::test]
    async fn find_and_code_read_the_index_at_its_commit() {
        let (_home, _fake, shared, head) = lab_with_code();
        let Outcome::Ok(Reply::Functions(early)) = find_function(&shared, &pr(1), "refresh").await
        else {
            panic!("functions");
        };
        assert!(early.is_empty(), "nothing before the index is ready");
        built(&shared, 1).await;
        let Outcome::Ok(Reply::Functions(hits)) =
            find_function(&shared, &pr(1), " MIDDLEWARE ").await
        else {
            panic!("functions");
        };
        assert_eq!(
            hits,
            [FunctionHit {
                id: MIDDLEWARE.into(),
                qualified: "auth_middleware".into(),
                path: "src/store.rs".into(),
                line: 13,
                changed: false,
                kind: None,
            }]
        );
        let Outcome::Ok(Reply::Functions(hits)) = find_function(&shared, &pr(1), "e").await else {
            panic!("functions");
        };
        assert_eq!(hits.len(), 4);
        assert_eq!(hits[0].id, REFRESH, "changed first");
        assert!(hits[0].changed);
        assert_eq!(hits[0].kind, Some(ChangeKind::Changed));
        assert!(hits[1..].iter().all(|h| !h.changed && h.kind.is_none()));
        // An empty query lists the changed functions of the pull request.
        let Outcome::Ok(Reply::Functions(changed)) = find_function(&shared, &pr(1), "  ").await
        else {
            panic!("functions");
        };
        assert_eq!(
            changed
                .iter()
                .map(|h| (h.id.as_str(), h.kind))
                .collect::<Vec<_>>(),
            [(REFRESH, Some(ChangeKind::Changed))]
        );
        assert_eq!(
            refused(find_function(&shared, &pr(1), &"x".repeat(201)).await).code,
            ErrorCode::BadRequest
        );
        let Outcome::Ok(Reply::FunctionCode(code)) =
            read_function_code(&shared, &pr(1), REFRESH).await
        else {
            panic!("code");
        };
        assert_eq!(
            (code.path.as_str(), code.commit.as_str(), code.start_line),
            ("src/store.rs", head.as_str(), 8)
        );
        let lines: Vec<(u32, &str, bool)> = code
            .lines
            .iter()
            .map(|l| (l.number, l.text.as_str(), l.changed))
            .collect();
        assert_eq!(
            lines,
            [
                (8, "    fn refresh(&self) -> u32 {", false),
                (9, "        2", true),
                (10, "    }", false),
            ]
        );
        assert_eq!(code.unavailable, None);
        assert_eq!(
            refused(read_function_code(&shared, &pr(1), "nope").await).code,
            ErrorCode::NotFound
        );
    }

    #[tokio::test]
    async fn a_function_at_line_zero_has_no_code_instead_of_a_panic() {
        let (_home, _fake, shared, _head) = lab_with_code();
        let old = built(&shared, 1).await;
        let mut index = old.index.clone();
        index
            .functions
            .iter_mut()
            .find(|f| f.id.0 == REFRESH)
            .expect("the function")
            .start_line = 0;
        shared.indexes.insert(
            &pr(1),
            Arc::new(Built {
                index,
                base: old.base.clone(),
                base_index: old.base_index.clone(),
                resolved: old.resolved.clone(),
                changed: old.changed.clone(),
                hunks: old.hunks.clone(),
                oids: old.oids.clone(),
                signatures: Mutex::new(HashMap::new()),
            }),
        );
        let Outcome::Ok(Reply::FunctionCode(code)) =
            read_function_code(&shared, &pr(1), REFRESH).await
        else {
            panic!("code");
        };
        assert!(code.lines.is_empty());
        assert!(code.unavailable.is_some());
    }

    #[tokio::test]
    async fn a_node_comment_goes_on_the_first_changed_hunk_or_names_its_lines() {
        let (_home, _fake, shared, head) = lab_with_code();
        assert_eq!(
            refused(add_calls_comment(&shared, "t", &pr(1), REFRESH, "Why?").await).code,
            ErrorCode::InvalidState,
            "the index is not ready yet"
        );
        built(&shared, 1).await;
        serve_files(&shared, 1, &head).await;
        let changed =
            item_of(add_calls_comment(&shared, "t", &pr(1), REFRESH, "  Why two?\n").await);
        assert_eq!(
            (changed.kind, changed.origin, changed.body.as_str()),
            (DraftKind::LineComment, Origin::Human, "Why two?")
        );
        let anchor = changed.anchor.expect("an anchor");
        assert_eq!(
            (
                anchor.path.as_str(),
                anchor.start_line,
                anchor.line,
                anchor.side
            ),
            ("src/store.rs", None, 9, Side::Right)
        );
        assert_eq!(anchor.commit, head);
        let untouched =
            item_of(add_calls_comment(&shared, "t", &pr(1), HELPER, "Still right?").await);
        assert_eq!(
            (untouched.kind, untouched.origin, untouched.body.as_str()),
            (
                DraftKind::General,
                Origin::Human,
                "src/store.rs:17-19: Still right?"
            )
        );
        let outside = item_of(
            add_calls_comment(&shared, "t", &pr(1), "outside:tokio::spawn", "Blocking?").await,
        );
        assert_eq!(outside.body, "tokio::spawn: Blocking?");
        assert_eq!(
            refused(add_calls_comment(&shared, "t", &pr(1), REFRESH, " \n").await).code,
            ErrorCode::BadRequest
        );
        assert_eq!(
            refused(add_calls_comment(&shared, "t", &pr(1), "nope", "x").await).code,
            ErrorCode::NotFound
        );
    }

    #[tokio::test]
    async fn a_changed_function_github_cannot_place_gets_a_general_comment() {
        let (_home, _fake, shared, _head) = lab_with_code();
        built(&shared, 1).await;
        // GitHub's files are for another head: the line cannot be checked.
        serve_files(&shared, 1, &"0".repeat(40)).await;
        let item = item_of(add_calls_comment(&shared, "t", &pr(1), REFRESH, "Why?").await);
        assert_eq!(
            (item.kind, item.body.as_str()),
            (DraftKind::General, "src/store.rs:8-10: Why?")
        );
    }

    /// Pull requests the graph cannot draw still answer, and never break.
    #[tokio::test]
    async fn awkward_pull_requests_still_answer() {
        // Only Markdown and TOML.
        let (_h, _f, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        repo_with(
            &shared,
            1,
            &[
                ("README.md", Some("a\n")),
                ("Cargo.toml", Some("[package]\nname = \"x\"\n")),
            ],
            &[
                ("README.md", Some("a\nb\n")),
                ("Cargo.toml", Some("[package]\nname = \"y\"\n")),
            ],
        );
        built(&shared, 1).await;
        let (graph, _, unsupported) = graph_of(get_calls(&shared, &pr(1), None, None, &[]).await);
        assert!(graph.is_none() && unsupported.is_empty());

        // Only Go.
        let (_h, _f, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        repo_with(
            &shared,
            1,
            &[("README.md", Some("a\n"))],
            &[("main.go", Some("package main\n\nfunc main() {}\n"))],
        );
        built(&shared, 1).await;
        let (graph, _, unsupported) = graph_of(get_calls(&shared, &pr(1), None, None, &[]).await);
        assert!(graph.is_none());
        assert_eq!(unsupported, ["main.go"]);

        // Forty changed functions: the graph stays within its caps.
        let (_h, _f, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        let many = |value: u32| {
            (0..40)
                .map(|i| format!("pub fn f{i}() -> u32 {{\n    g()\n}}\n\npub fn g{i}() -> u32 {{ {value} }}\n"))
                .collect::<String>()
                + "pub fn g() -> u32 {\n    0\n}\n"
        };
        let (base_text, head_text) = (many(1), many(2));
        repo_with(
            &shared,
            1,
            &[("src/many.rs", Some(&base_text))],
            &[("src/many.rs", Some(&head_text))],
        );
        built(&shared, 1).await;
        let (graph, _, _) =
            graph_of(get_calls(&shared, &pr(1), Some("src/many.rs#g#201"), Some(5), &[]).await);
        let graph = graph.expect("a graph");
        assert!(graph.nodes.len() <= clusia_calls::MAX_GRAPH_NODES);
        assert!(
            graph
                .nodes
                .iter()
                .any(|n| n.kind == clusia_calls::NodeKind::More)
        );

        // A rename with an edit, and a deleted file.
        let (_h, _f, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        repo_with(
            &shared,
            1,
            &[
                ("src/store.rs", Some(STORE_BASE)),
                ("src/old.rs", Some("pub fn old() {}\n")),
            ],
            &[
                ("src/store.rs", None),
                ("src/token_store.rs", Some(STORE_HEAD)),
                ("src/old.rs", None),
            ],
        );
        let built = built(&shared, 1).await;
        let (graph, _, _) = graph_of(get_calls(&shared, &pr(1), None, Some(3), &[]).await);
        let graph = graph.expect("a graph");
        assert!(
            graph.center.starts_with("src/token_store.rs#"),
            "the moved functions are the head's: {}",
            graph.center
        );
        assert!(
            built
                .changed
                .iter()
                .any(|c| c.kind == ChangeKind::Removed && c.id.0.starts_with("src/old.rs#old#")),
            "{:?}",
            built.changed
        );
        let Outcome::Ok(Reply::FunctionCode(code)) =
            read_function_code(&shared, &pr(1), "src/old.rs#old#1").await
        else {
            panic!("the removed function's code, read at the base");
        };
        assert_eq!(code.lines.len(), 1);
        assert_eq!(code.commit, built.base);
    }

    fn empty_built(commit: &str) -> Arc<Built> {
        let index = |commit: &str| CallIndex {
            commit: commit.into(),
            functions: Vec::new(),
            calls: Vec::new(),
            imports: Vec::new(),
            skipped: Vec::new(),
            unsupported: Vec::new(),
            dependencies: Vec::new(),
            cut: false,
        };
        Arc::new(Built {
            index: index(commit),
            base: "b".into(),
            base_index: index("b"),
            resolved: Resolved {
                edges: Vec::new(),
                unresolved: 0,
            },
            changed: Vec::new(),
            hunks: Vec::new(),
            oids: HashMap::new(),
            signatures: Mutex::new(HashMap::new()),
        })
    }

    #[test]
    fn at_most_eight_indexes_stay_in_memory_and_the_least_recent_leaves_first() {
        let indexes = Indexes::default();
        for n in 1..=8 {
            indexes.insert(&pr(n), empty_built(&format!("c{n}")));
        }
        assert!(
            indexes.ready(&pr(1), "c1").is_some(),
            "a hit makes it recent"
        );
        indexes.insert(&pr(9), empty_built("c9"));
        assert!(
            indexes.ready(&pr(2), "c2").is_none(),
            "the least recent left"
        );
        for n in [1, 3, 4, 5, 6, 7, 8, 9] {
            assert!(indexes.ready(&pr(n), &format!("c{n}")).is_some(), "{n}");
        }
        assert!(
            indexes.ready(&pr(1), "other").is_none(),
            "keyed by commit too"
        );
    }

    #[test]
    fn the_first_changed_hunk_is_cut_to_the_function() {
        let hunks = parse_hunks(
            "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n\
             @@ -2,0 +3,2 @@\n+x\n+y\n@@ -9 +11,4 @@\n-z\n+a\n+b\n+c\n+d\n@@ -20 +23,0 @@\n-q\n",
        );
        let function = |start: u32, end: u32| Function {
            id: FunctionId(format!("src/a.rs#f#{start}")),
            qualified: "f".into(),
            name: "f".into(),
            path: "src/a.rs".into(),
            start_line: start,
            end_line: end,
            signature_lines: 1,
            lang: "rust".into(),
        };
        assert_eq!(first_changed_lines(&hunks, &function(1, 30)), Some((3, 4)));
        assert_eq!(first_changed_lines(&hunks, &function(4, 12)), Some((4, 4)));
        assert_eq!(
            first_changed_lines(&hunks, &function(12, 30)),
            Some((12, 14))
        );
        assert_eq!(
            first_changed_lines(&hunks, &function(16, 30)),
            None,
            "a deletion adds no line"
        );
        let added: Vec<u32> = added_at_head(&hunks, "src/a.rs").into_iter().collect();
        assert_eq!(added, [3, 4, 11, 12, 13, 14]);
        assert!(added_at_head(&hunks, "src/b.rs").is_empty());
    }
}

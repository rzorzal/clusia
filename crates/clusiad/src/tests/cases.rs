//! The changed functions of a review: which are listed, what Clúsia keeps for each in
//! `.clusia/functions/<slug>/`, the reviewer's edits, and the view the clients get.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use clusia_calls::{ChangeKind, Function, FunctionId};
use clusia_core::PrRef;
use clusia_core::functions::{
    CASES_CHANGED, CASES_FIRST, CASES_MAX, Case, CaseInput, CaseOrigin, CaseScope, CaseView,
    CasesAsk, DRIVER_MAX_BYTES, Edits, FUNCTIONS_DIR, FUNCTIONS_MAX, FunctionBlock, FunctionBrief,
    FunctionChange, FunctionLang, FunctionView, FunctionsActivity, FunctionsView, LISTED_MAX,
    StoredFunction, Support, case_hash, check_case_input, check_value, function_hash,
    function_slug, lang_of, valid_slug, view_hash,
};
use clusia_core::tests::TestsRecord;
use clusia_protocol::{CallsIndexState, ErrorCode, Event, Outcome, ProtocolError, Reply, topics};
use clusia_testrun::names::sha256_hex;

use super::env::{self, RecipeState};
use super::{Doing, STOPPING};
use crate::calls::{self, Built};
use crate::reviews;
use crate::sessions;
use crate::state::Shared;

pub(super) const OPEN_FIRST: &str = "Open the review to see its changed functions";
pub(super) const FOLDER_REFUSED: &str = "Clúsia's folder for this function is not a plain folder";
/// How long a request waits for the call index of the head.
pub(super) const INDEX_WAIT: Duration = Duration::from_secs(15);
/// The longest signature shown or written into a prompt.
const SIGNATURE_MAX: usize = 300;
/// The largest `function.json` or `edits.json` read.
const STORED_MAX: usize = 256 * 1024;
const FUNCTION_FILE: &str = "function.json";
const EDITS_FILE: &str = "edits.json";
/// Makes the temporary names of two writers of one process differ.
static TEMPORARIES: AtomicU64 = AtomicU64::new(0);

/// Whether a function is test code, which gets no cases of its own: a file under a test
/// folder, a file named as a test, or a Rust item inside a `tests` or `test` module.
pub(super) fn is_test_code(path: &str, qualified: &str, lang: &str) -> bool {
    let mut parts: Vec<&str> = path.split('/').collect();
    let file = parts.pop().unwrap_or_default();
    if parts
        .iter()
        .any(|p| matches!(*p, "tests" | "test" | "__tests__" | "spec"))
    {
        return true;
    }
    let pieces: Vec<&str> = file.split('.').collect();
    let middle = pieces
        .len()
        .checked_sub(1)
        .map_or(&[][..], |last| &pieces[1.min(last)..last]);
    let named = (file.starts_with("test_") && file.ends_with(".py"))
        || file.ends_with("_test.py")
        || file == "conftest.py"
        || file.ends_with("_test.rs")
        || middle.iter().any(|p| *p == "test" || *p == "spec");
    named || (lang == "rust" && qualified.split("::").any(|s| s == "tests" || s == "test"))
}

/// A changed function the view lists.
pub(super) struct Listed<'a> {
    pub function: &'a Function,
    pub change: FunctionChange,
    pub slug: String,
}

/// The changed functions at head that are not test code, by path then line, one per slug, at
/// most `LISTED_MAX`; and how many more there are.
pub(super) fn listed(built: &Built) -> (Vec<Listed<'_>>, usize) {
    let mut all: Vec<Listed<'_>> = built
        .changed
        .iter()
        .filter(|c| c.kind != ChangeKind::Removed)
        .filter_map(|c| {
            let (function, at_head) = built.locate(&c.id.0)?;
            let change = match c.kind {
                ChangeKind::Changed => FunctionChange::Changed,
                ChangeKind::Signature => FunctionChange::Signature,
                ChangeKind::New => FunctionChange::New,
                ChangeKind::Moved => FunctionChange::Moved,
                ChangeKind::Removed => return None,
            };
            (at_head && !is_test_code(&function.path, &function.qualified, &function.lang)).then(
                || Listed {
                    function,
                    change,
                    slug: function_slug(&function.path, &function.qualified),
                },
            )
        })
        .collect();
    all.sort_by(|a, b| {
        (&a.function.path, a.function.start_line).cmp(&(&b.function.path, b.function.start_line))
    });
    let mut seen = BTreeSet::new();
    all.retain(|l| seen.insert(l.slug.clone()));
    let more = all.len().saturating_sub(LISTED_MAX);
    all.truncate(LISTED_MAX);
    (all, more)
}

/// A signature without the `{` (or, in Python, the `:`) that opens the body, cut to 300
/// characters.
pub(super) fn trim_signature(text: &str, lang: &str) -> String {
    let text = text.trim();
    let end = if lang == "python" { ':' } else { '{' };
    text.strip_suffix(end)
        .unwrap_or(text)
        .trim_end()
        .chars()
        .take(SIGNATURE_MAX)
        .collect()
}

/// A case as it would run: the agent's with the reviewer's edits laid over, or the reviewer's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Effective {
    pub case: Case,
    pub origin: CaseOrigin,
    pub edited: bool,
}

/// The agent's cases in their order with the edits laid over them, then the reviewer's.
pub(super) fn effective(stored: Option<&StoredFunction>, edits: &Edits) -> Vec<Effective> {
    let mut out: Vec<Effective> = Vec::new();
    for case in stored.map(|s| s.cases.as_slice()).unwrap_or_default() {
        let mut case = case.clone();
        let values = edits.values.get(&case.id);
        let name = edits.names.get(&case.id);
        for (key, value) in values.into_iter().flatten() {
            if let Some(slot) = case.values.get_mut(key) {
                slot.clone_from(value);
            }
        }
        if let Some(name) = name {
            case.name.clone_from(name);
        }
        out.push(Effective {
            case,
            origin: CaseOrigin::Agent,
            edited: values.is_some() || name.is_some(),
        });
    }
    out.extend(edits.added.iter().map(|case| Effective {
        case: case.clone(),
        origin: CaseOrigin::Reviewer,
        edited: true,
    }));
    out
}

/// What decides whether a function's cases can run.
pub(super) struct Facts<'a> {
    pub stored: Option<&'a StoredFunction>,
    pub folder_refused: bool,
    /// Why the function's folder could not be opened, when it is not a link.
    pub folder_problem: Option<&'a str>,
    pub no_package: bool,
    /// The signature at head; empty when it could not be read.
    pub signature: &'a str,
    pub cases: usize,
    pub lang: Option<FunctionLang>,
    pub command: Option<&'a str>,
}

/// The first reason that applies, in a fixed order.
pub(super) fn support_of(facts: &Facts<'_>) -> Support {
    if let Some(reason) = facts.stored.and_then(|s| s.unsupported.clone()) {
        return Support::Unsupported { reason };
    }
    if facts.no_package {
        return Support::Unsupported {
            reason: "No Cargo package holds this file".into(),
        };
    }
    if facts.folder_refused {
        return Support::Unsupported {
            reason: FOLDER_REFUSED.into(),
        };
    }
    if let Some(problem) = facts.folder_problem {
        return Support::Unsupported {
            reason: problem.to_string(),
        };
    }
    // A signature that could not be read, then or now, says nothing about a change.
    if let Some(stored) = facts.stored
        && !facts.signature.is_empty()
        && !stored.signature.is_empty()
        && stored.signature != facts.signature
    {
        return Support::Stale {
            reason: "The signature changed since these cases were written".into(),
        };
    }
    if facts.stored.is_none() || facts.cases == 0 {
        return Support::NoCases;
    }
    if facts.lang.is_none() || facts.command.is_none() {
        return Support::NoCommand;
    }
    Support::Ready
}

// Storage. Every access walks `.clusia`, `functions` and the slug one descriptor at a time,
// never following a link, so nothing a pull request swaps in is read or written through.

/// Why a function's folder or file cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Refusal {
    /// A link or something other than a folder or a regular file on the way.
    NotPlain,
    Io(String),
}

fn c_string(text: &[u8]) -> io::Result<CString> {
    CString::new(text).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn descriptor(fd: i32) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by `open` or `openat` and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// An error that says the entry is a link or of another kind, not that it is missing.
fn not_ours(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ELOOP | libc::ENOTDIR | libc::EMLINK)
    )
}

fn refusal(name: &str, error: &io::Error) -> Refusal {
    if not_ours(error) {
        Refusal::NotPlain
    } else {
        Refusal::Io(format!("cannot open {name}: {error}"))
    }
}

/// The worktree itself, as a folder.
fn open_root(worktree: &Path) -> Result<OwnedFd, Refusal> {
    let path = c_string(worktree.as_os_str().as_bytes())
        .map_err(|e| Refusal::Io(format!("cannot open the worktree: {e}")))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    // SAFETY: `path` is a valid C string that `open` only reads.
    descriptor(unsafe { libc::open(path.as_ptr(), flags) })
        .map_err(|e| Refusal::Io(format!("cannot open the worktree: {e}")))
}

fn open_dir_at(dir: &OwnedFd, name: &CString) -> io::Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `dir` is an open descriptor and `name` a valid C string that `openat` only reads.
    descriptor(unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) })
}

/// The folder `name` in `dir`; `None` when it is missing and `create` is false.
fn sub_dir(dir: &OwnedFd, name: &str, create: bool) -> Result<Option<OwnedFd>, Refusal> {
    let c_name = c_string(name.as_bytes()).map_err(|e| refusal(name, &e))?;
    match open_dir_at(dir, &c_name) {
        Ok(fd) => return Ok(Some(fd)),
        Err(e) if e.kind() == io::ErrorKind::NotFound && !create => return Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(refusal(name, &e)),
    }
    // SAFETY: `dir` is an open descriptor and `c_name` a valid C string that `mkdirat` only reads.
    if unsafe { libc::mkdirat(dir.as_raw_fd(), c_name.as_ptr(), 0o755) } != 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::AlreadyExists {
            return Err(refusal(name, &e));
        }
    }
    // Another writer may have put something else there in between; opening it refuses that.
    open_dir_at(dir, &c_name)
        .map(Some)
        .map_err(|e| refusal(name, &e))
}

/// The bytes of the regular file `name` in `dir`, up to `cap`; `None` when it is missing.
fn read_capped(dir: &OwnedFd, name: &str, cap: usize) -> Result<Option<Vec<u8>>, Refusal> {
    let c_name = c_string(name.as_bytes()).map_err(|e| refusal(name, &e))?;
    let flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `dir` is an open descriptor and `c_name` a valid C string that `openat` only reads.
    let fd = match descriptor(unsafe { libc::openat(dir.as_raw_fd(), c_name.as_ptr(), flags) }) {
        Ok(fd) => fd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(refusal(name, &e)),
    };
    let file = File::from(fd);
    let unreadable = |e: io::Error| Refusal::Io(format!("cannot read {name}: {e}"));
    if !file.metadata().map_err(unreadable)?.is_file() {
        return Err(Refusal::NotPlain);
    }
    let mut bytes = Vec::new();
    file.take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() > cap {
        return Err(Refusal::Io(format!(
            "{name} is larger than {} KiB",
            cap / 1024
        )));
    }
    Ok(Some(bytes))
}

/// Writes `bytes` as `name` in `dir`, whole or not at all: a new temporary file no other writer
/// shares, then `renameat` over the name, which replaces a link there without writing through it.
fn write_whole(dir: &OwnedFd, name: &str, bytes: &[u8]) -> io::Result<()> {
    let temp = format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        TEMPORARIES.fetch_add(1, Ordering::Relaxed)
    );
    let c_temp = c_string(temp.as_bytes())?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let written = (|| {
        // SAFETY: `dir` is an open descriptor and `c_temp` a valid C string that `openat` only
        // reads.
        let fd = descriptor(unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c_temp.as_ptr(),
                flags,
                0o644 as libc::c_uint,
            )
        })?;
        let mut file = File::from(fd);
        file.write_all(bytes)?;
        file.sync_all()?;
        let to = c_string(name.as_bytes())?;
        let fd = dir.as_raw_fd();
        // SAFETY: `dir` is an open descriptor and both names are valid C strings that
        // `renameat` only reads.
        if unsafe { libc::renameat(fd, c_temp.as_ptr(), fd, to.as_ptr()) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    })();
    if written.is_err() {
        // SAFETY: `dir` is an open descriptor and `c_temp` a valid C string.
        unsafe { libc::unlinkat(dir.as_raw_fd(), c_temp.as_ptr(), 0) };
    }
    written
}

/// Removes the file `name` from `dir`; a missing one is fine.
fn remove_at(dir: &OwnedFd, name: &str) -> io::Result<()> {
    let c_name = c_string(name.as_bytes())?;
    // SAFETY: `dir` is an open descriptor and `c_name` a valid C string that `unlinkat` only reads.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), c_name.as_ptr(), 0) } == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if e.kind() == io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(e)
    }
}

/// The folder of function `slug`, `.clusia/functions/<slug>`, made when `create`; `None` when
/// it is missing and not made. An invalid slug never reaches a path.
pub(super) fn folder(
    worktree: &Path,
    slug: &str,
    create: bool,
) -> Result<Option<OwnedFd>, Refusal> {
    if !valid_slug(slug) {
        return Err(Refusal::Io("invalid function id".into()));
    }
    let root = open_root(worktree)?;
    let mut dir = root;
    for name in FUNCTIONS_DIR.split('/').chain([slug]) {
        match sub_dir(&dir, name, create)? {
            Some(next) => dir = next,
            None => return Ok(None),
        }
    }
    Ok(Some(dir))
}

/// What is stored for one function.
#[derive(Debug, Default)]
pub(super) struct Stored {
    pub function: Option<StoredFunction>,
    pub edits: Edits,
    /// Its folder or one of its files is not plain: nothing was read.
    pub refused: bool,
    /// Why its folder could not be opened, when that is not a link: nothing was read.
    pub problem: Option<String>,
}

/// Reads what is stored for `slug`. A `function.json` that does not parse is no stored
/// function; edits whose values no longer check against the stored params are dropped.
pub(super) fn load_stored(worktree: &Path, slug: &str) -> Stored {
    let refused = Stored {
        refused: true,
        ..Stored::default()
    };
    let dir = match folder(worktree, slug, false) {
        Ok(Some(dir)) => dir,
        Ok(None) => return Stored::default(),
        Err(Refusal::NotPlain) => return refused,
        Err(Refusal::Io(message)) => {
            tracing::warn!(slug, message, "cannot open the folder of a function");
            return Stored {
                problem: Some(message),
                ..Stored::default()
            };
        }
    };
    let function = match read_capped(&dir, FUNCTION_FILE, STORED_MAX) {
        Ok(Some(bytes)) => match serde_json::from_slice::<StoredFunction>(&bytes) {
            Ok(function) => Some(function),
            Err(e) => {
                tracing::warn!(slug, error = %e, "a function.json could not be read");
                None
            }
        },
        Ok(None) => None,
        Err(Refusal::NotPlain) => return refused,
        Err(Refusal::Io(message)) => {
            tracing::warn!(slug, message, "a function.json could not be read");
            None
        }
    };
    let mut edits = match read_capped(&dir, EDITS_FILE, STORED_MAX) {
        Ok(Some(bytes)) => serde_json::from_slice::<Edits>(&bytes).unwrap_or_else(|e| {
            tracing::warn!(slug, error = %e, "an edits.json could not be read");
            Edits::default()
        }),
        Ok(None) => Edits::default(),
        Err(Refusal::NotPlain) => return refused,
        Err(Refusal::Io(message)) => {
            tracing::warn!(slug, message, "an edits.json could not be read");
            Edits::default()
        }
    };
    let params = function
        .as_ref()
        .map(|f| f.params.as_slice())
        .unwrap_or_default();
    for overlay in edits.values.values_mut() {
        overlay.retain(|key, value| {
            params
                .iter()
                .find(|p| p.name == *key)
                .is_some_and(|p| check_value(p.kind, value).is_ok())
        });
    }
    edits.values.retain(|_, overlay| !overlay.is_empty());
    edits.added.retain(|case| {
        let input = CaseInput {
            id: Some(case.id.clone()),
            name: case.name.clone(),
            values: case.values.clone(),
        };
        check_case_input(params, &input).is_ok()
    });
    Stored {
        function,
        edits,
        refused: false,
        problem: None,
    }
}

fn refusal_text(refusal: Refusal) -> String {
    match refusal {
        Refusal::NotPlain => FOLDER_REFUSED.to_string(),
        Refusal::Io(message) => message,
    }
}

fn writable_folder(worktree: &Path, slug: &str) -> Result<OwnedFd, String> {
    match folder(worktree, slug, true) {
        Ok(Some(dir)) => Ok(dir),
        Ok(None) => Err(format!("cannot make the folder of {slug}")),
        Err(refusal) => Err(refusal_text(refusal)),
    }
}

/// Writes `function.json` and the driver (removing the driver when there is none).
pub(super) fn save_function(
    worktree: &Path,
    slug: &str,
    function: &StoredFunction,
    driver: Option<&str>,
) -> Result<(), String> {
    let dir = writable_folder(worktree, slug)?;
    let driver_file = format!("driver.{}", function.lang.ext());
    match driver {
        Some(text) => write_whole(&dir, &driver_file, text.as_bytes()),
        None => remove_at(&dir, &driver_file),
    }
    .map_err(|e| format!("cannot write {driver_file}: {e}"))?;
    let json = serde_json::to_vec_pretty(function).map_err(|e| e.to_string())?;
    write_whole(&dir, FUNCTION_FILE, &json)
        .map_err(|e| format!("cannot write {FUNCTION_FILE}: {e}"))
}

/// Writes `edits.json`.
pub(super) fn save_edits(worktree: &Path, slug: &str, edits: &Edits) -> Result<(), String> {
    let dir = writable_folder(worktree, slug)?;
    let json = serde_json::to_vec_pretty(edits).map_err(|e| e.to_string())?;
    write_whole(&dir, EDITS_FILE, &json).map_err(|e| format!("cannot write {EDITS_FILE}: {e}"))
}

/// Removes `edits.json`.
pub(super) fn remove_edits(worktree: &Path, slug: &str) -> Result<(), String> {
    match folder(worktree, slug, false) {
        Ok(Some(dir)) => {
            remove_at(&dir, EDITS_FILE).map_err(|e| format!("cannot remove {EDITS_FILE}: {e}"))
        }
        Ok(None) => Ok(()),
        Err(refusal) => Err(refusal_text(refusal)),
    }
}

/// The driver's text; `None` when there is none. A link or a file of another kind at its name
/// is refused without being read.
pub(super) fn read_driver(
    worktree: &Path,
    slug: &str,
    lang: FunctionLang,
) -> Result<Option<String>, String> {
    let dir = match folder(worktree, slug, false) {
        Ok(Some(dir)) => dir,
        Ok(None) => return Ok(None),
        Err(refusal) => return Err(refusal_text(refusal)),
    };
    let name = format!("driver.{}", lang.ext());
    match read_capped(&dir, &name, DRIVER_MAX_BYTES) {
        Ok(Some(bytes)) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| format!("{name} is not UTF-8 text")),
        Ok(None) => Ok(None),
        Err(refusal) => Err(refusal_text(refusal)),
    }
}

/// The driver's path relative to the worktree.
pub(super) fn driver_path(slug: &str, lang: FunctionLang) -> String {
    format!("{FUNCTIONS_DIR}/{slug}/driver.{}", lang.ext())
}

/// The function a block must be for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Target {
    pub slug: String,
    pub qualified: String,
    pub file: String,
    pub lang: FunctionLang,
    /// The signature at head.
    pub signature: String,
    pub head: String,
}

fn input_of(case: &clusia_core::functions::BlockCase) -> CaseInput {
    CaseInput {
        id: None,
        name: case.name.clone(),
        values: case.values.clone(),
    }
}

fn kinds(
    params: &[clusia_core::functions::Param],
) -> Vec<(&str, clusia_core::functions::ParamKind)> {
    params.iter().map(|p| (p.name.as_str(), p.kind)).collect()
}

/// Checks an agent's block for `target` and writes what `ask` says, or nothing and every
/// reason. The caller holds the review lock.
pub(super) fn apply_block(
    worktree: &Path,
    target: &Target,
    block: &serde_json::Value,
    ask: &CasesAsk,
    now: i64,
) -> Result<(), Vec<String>> {
    // Cases past the most a function keeps are dropped before the block is checked, so a long
    // `More` answer still adds what fits.
    let mut block = block.clone();
    if let Some(cases) = block.get_mut("cases").and_then(|c| c.as_array_mut()) {
        cases.truncate(CASES_MAX);
    }
    let block = FunctionBlock::from_json(&block)?;
    if (block.function.as_str(), block.file.as_str())
        != (target.qualified.as_str(), target.file.as_str())
    {
        return Err(vec![format!(
            "The block is for {} in {}, not {} in {}",
            block.function, block.file, target.qualified, target.file
        )]);
    }
    let one = |r: String| vec![r];
    match ask {
        CasesAsk::Fresh => {
            let stored = load_stored(worktree, &target.slug);
            if stored.refused {
                return Err(one(FOLDER_REFUSED.into()));
            }
            if let Some(problem) = stored.problem {
                return Err(one(problem));
            }
            let first = stored.function.as_ref().map_or(1, |f| f.next.max(1));
            let cases: Vec<Case> = block
                .cases
                .iter()
                .take(CASES_FIRST)
                .zip(first..)
                .map(|(case, n)| Case {
                    id: format!("a{n}"),
                    name: case.name.clone(),
                    values: case.values.clone(),
                    expect: case.expect.clone(),
                })
                .collect();
            let next = first + cases.len() as u32;
            let same_params = stored
                .function
                .as_ref()
                .is_some_and(|f| kinds(&f.params) == kinds(&block.params));
            let function = StoredFunction {
                qualified: target.qualified.clone(),
                file: target.file.clone(),
                lang: target.lang,
                signature: target.signature.clone(),
                params: block.params.clone(),
                state_label: block.state_label.clone(),
                unsupported: block.unsupported.clone(),
                driver_hash: block.driver.as_deref().map(|d| sha256_hex(d.as_bytes())),
                cases,
                next,
                written_head: target.head.clone(),
                written_at: now,
            };
            save_function(worktree, &target.slug, &function, block.driver.as_deref())
                .map_err(one)?;
            // Every agent id is new, so no overlay applies any more; the reviewer's cases stay
            // only for the same params. `next` always stays, so their ids are never reused.
            let edits = if same_params {
                Edits {
                    values: BTreeMap::new(),
                    names: BTreeMap::new(),
                    ..stored.edits
                }
            } else {
                Edits {
                    next: stored.edits.next,
                    ..Edits::default()
                }
            };
            if edits == Edits::default() {
                remove_edits(worktree, &target.slug).map_err(one)
            } else {
                save_edits(worktree, &target.slug, &edits).map_err(one)
            }
        }
        CasesAsk::More => {
            let stored = load_stored(worktree, &target.slug);
            if stored.refused {
                return Err(one(FOLDER_REFUSED.into()));
            }
            if let Some(problem) = stored.problem {
                return Err(one(problem));
            }
            let Some(mut function) = stored.function.clone().filter(|f| !f.cases.is_empty()) else {
                return Err(one("There are no cases to add to".into()));
            };
            let reasons: Vec<String> = block
                .cases
                .iter()
                .filter_map(|case| check_case_input(&function.params, &input_of(case)).err())
                .flatten()
                .collect();
            if !reasons.is_empty() {
                return Err(reasons);
            }
            let driver = read_driver(worktree, &target.slug, function.lang).map_err(one)?;
            let room = CASES_MAX.saturating_sub(effective(Some(&function), &stored.edits).len());
            for case in block.cases.iter().take(room) {
                let n = function.next.max(1);
                function.cases.push(Case {
                    id: format!("a{n}"),
                    name: case.name.clone(),
                    values: case.values.clone(),
                    expect: case.expect.clone(),
                });
                function.next = n + 1;
            }
            save_function(worktree, &target.slug, &function, driver.as_deref()).map_err(one)
        }
        CasesAsk::Values { case } => {
            if block.cases.len() != 1 {
                return Err(one("The answer must hold exactly one case".into()));
            }
            let stored = load_stored(worktree, &target.slug);
            if stored.refused {
                return Err(one(FOLDER_REFUSED.into()));
            }
            if let Some(problem) = stored.problem {
                return Err(one(problem));
            }
            let answer = &block.cases[0];
            let mut edits = stored.edits;
            let Some(mut function) = stored.function else {
                return Err(one(format!("No case {case}")));
            };
            let agent = function.cases.iter().position(|c| c.id == *case);
            let reviewer = edits.added.iter().position(|c| c.id == *case);
            if agent.is_none() && reviewer.is_none() {
                return Err(one(format!("No case {case}")));
            }
            check_case_input(&function.params, &input_of(answer))?;
            if let Some(at) = agent {
                let driver = read_driver(worktree, &target.slug, function.lang).map_err(one)?;
                function.cases[at].values = answer.values.clone();
                function.cases[at].expect = answer.expect.clone();
                save_function(worktree, &target.slug, &function, driver.as_deref()).map_err(one)?;
                if edits.values.remove(case).is_some() {
                    save_edits(worktree, &target.slug, &edits).map_err(one)?;
                }
                Ok(())
            } else if let Some(at) = reviewer {
                edits.added[at].values = answer.values.clone();
                edits.added[at].expect = answer.expect.clone();
                save_edits(worktree, &target.slug, &edits).map_err(one)
            } else {
                Ok(())
            }
        }
    }
}

/// After a `Fresh`, drops every stored result of `slug` that could be read as a new case's:
/// all of the agent's, whose cases were all replaced (a lost `function.json` numbers them from
/// `a1` again), and the reviewer's whose case is gone. The caller holds the review lock.
pub(super) fn forget_replaced_results(worktree: &Path, slug: &str, record: &mut TestsRecord) {
    let Some(results) = record.case_results.get_mut(slug) else {
        return;
    };
    let kept: BTreeSet<String> = load_stored(worktree, slug)
        .edits
        .added
        .into_iter()
        .map(|c| c.id)
        .collect();
    results.retain(|id, _| kept.contains(id));
    if results.is_empty() {
        record.case_results.remove(slug);
    }
}

/// One listed function with what is stored for it.
pub(super) struct Item {
    pub brief: FunctionBrief,
    pub stored: Option<StoredFunction>,
    pub edits: Edits,
    pub refused: bool,
    /// Why its folder could not be opened, when that is not a link.
    pub problem: Option<String>,
    /// The recipe's command for its language.
    pub command: Option<String>,
}

/// The view and what it was read from; `items[i]` belongs to `view.functions[i]`.
pub(super) struct Read {
    pub view: FunctionsView,
    pub worktree: Option<PathBuf>,
    /// The problem of a `.clusia/` the pull request ships.
    pub shipped: Option<String>,
    pub items: Vec<Item>,
}

impl Read {
    fn empty(view: FunctionsView) -> Self {
        Read {
            view,
            worktree: None,
            shipped: None,
            items: Vec::new(),
        }
    }
}

/// Waits for the index of `head` in the background, once per review, and tells the view when
/// it is ready or will not be.
fn wait_then_tell(shared: &Arc<Shared>, pr: &PrRef, head: String) {
    let first = shared.tests.with(pr, |entry| {
        !std::mem::replace(&mut entry.functions_waiting, true)
    });
    if !first {
        return;
    }
    let (shared, pr) = (shared.clone(), pr.clone());
    tokio::spawn(async move {
        calls::wait_ready(&shared, &pr, &head, INDEX_WAIT).await;
        shared
            .tests
            .with(&pr, |entry| entry.functions_waiting = false);
        tell_functions(&shared, &pr).await;
    });
}

/// The changed functions of `pr` and what is stored for each.
pub(super) async fn read(shared: &Arc<Shared>, pr: &PrRef) -> Read {
    let review = match super::active(shared, pr) {
        Ok(review) => review,
        Err(_) => {
            let head = reviews::load_stored(shared, pr)
                .ok()
                .flatten()
                .map(|review| review.head_sha)
                .unwrap_or_default();
            let mut view = FunctionsView::new(head);
            view.note = Some(OPEN_FIRST.into());
            return Read::empty(view);
        }
    };
    let head = review.head_sha;
    let mut view = FunctionsView::new(head.clone());
    (view.activity, view.failed) = shared.tests.with(pr, |entry| {
        let activity = entry.activity.as_ref().and_then(|a| {
            if a.doing.is_functions() {
                a.functions.clone()
            } else if a.doing == Doing::Generate {
                Some(FunctionsActivity::Writing {
                    function: None,
                    ask: None,
                })
            } else {
                None
            }
        });
        (activity, entry.cases_failed.clone())
    });
    let ctx = match env::Ctx::new(shared, pr) {
        Ok(ctx) => ctx,
        Err(message) => {
            view.note = Some(message);
            return Read::empty(view);
        }
    };
    let shipped = match shared.tests.with(pr, |entry| entry.shipped.clone()) {
        Some(known) => known,
        None => env::refresh_shipped(shared, &ctx).await,
    };
    if let Some(problem) = shipped {
        view.note = Some(problem.clone());
        return Read {
            view,
            worktree: Some(ctx.worktree),
            shipped: Some(problem),
            items: Vec::new(),
        };
    }
    let built = match shared.indexes.ready(pr, &head) {
        Some(built) => built,
        None => {
            let state = calls::ensure_index(shared, pr).await;
            match (state, shared.indexes.ready(pr, &head)) {
                (_, Some(built)) => built,
                (CallsIndexState::Failed { message }, None) => {
                    view.note = Some(format!(
                        "The changed functions could not be read: {message}"
                    ));
                    return Read::empty(view);
                }
                _ => {
                    view.reading = true;
                    wait_then_tell(shared, pr, head);
                    return Read::empty(view);
                }
            }
        }
    };
    let (listed, more) = listed(&built);
    if more > 0 {
        view.note = Some(format!("{more} more changed functions are not listed"));
    }
    view.unsupported = built.unsupported();
    let ids: Vec<FunctionId> = listed.iter().map(|l| l.function.id.clone()).collect();
    let signatures = calls::signatures_of(shared, pr, &built, &ids).await;
    let record = env::load(shared, pr);
    let spec = match env::recipe_state(&ctx.worktree) {
        RecipeState::Valid { recipe, .. } => recipe.functions,
        _ => None,
    };
    let mut items = Vec::new();
    for l in &listed {
        let f = l.function;
        let lang = lang_of(&f.lang);
        let signature = trim_signature(
            signatures
                .get(&f.id)
                .map(String::as_str)
                .unwrap_or_default(),
            &f.lang,
        );
        let stored = load_stored(&ctx.worktree, &l.slug);
        let command = lang
            .and_then(|lang| spec.as_ref()?.command(lang))
            .map(String::from);
        let cases = effective(stored.function.as_ref(), &stored.edits);
        let support = support_of(&Facts {
            stored: stored.function.as_ref(),
            folder_refused: stored.refused,
            folder_problem: stored.problem.as_deref(),
            no_package: false,
            signature: &signature,
            cases: cases.len(),
            lang,
            command: command.as_deref(),
        });
        let driver_hash = stored.function.as_ref().and_then(|s| s.driver_hash.clone());
        let results = record.case_results.get(&l.slug);
        let cases: Vec<CaseView> = cases
            .into_iter()
            .map(|e| CaseView {
                hash: case_hash(
                    driver_hash.as_deref(),
                    command.as_deref(),
                    &e.case.values,
                    &e.case.expect,
                ),
                result: results.and_then(|r| r.get(&e.case.id)).cloned(),
                id: e.case.id,
                name: e.case.name,
                origin: e.origin,
                edited: e.edited,
                values: e.case.values,
                expect: e.case.expect,
            })
            .collect();
        let params = stored
            .function
            .as_ref()
            .map(|s| s.params.clone())
            .unwrap_or_default();
        let hashes: Vec<(String, String)> = cases
            .iter()
            .map(|c| (c.id.clone(), c.hash.clone()))
            .collect();
        let hash = function_hash(&l.slug, &signature, &params, &support, &hashes);
        let driver = stored
            .function
            .as_ref()
            .filter(|s| s.driver_hash.is_some())
            .map(|s| driver_path(&l.slug, s.lang));
        view.functions.push(FunctionView {
            slug: l.slug.clone(),
            qualified: f.qualified.clone(),
            name: f.name.clone(),
            file: f.path.clone(),
            lang,
            change: l.change,
            signature: signature.clone(),
            start_line: f.start_line,
            diff_lines: calls::first_changed_lines(&built.hunks, f),
            params,
            state_label: stored.function.as_ref().and_then(|s| s.state_label.clone()),
            support,
            cases,
            driver,
            hash,
        });
        items.push(Item {
            brief: FunctionBrief {
                slug: l.slug.clone(),
                qualified: f.qualified.clone(),
                file: f.path.clone(),
                // A function of a language with no driver is never written for; see `write_cases`.
                lang: lang.unwrap_or(FunctionLang::Rust),
                change: l.change,
                signature,
                start_line: f.start_line,
                end_line: f.end_line,
            },
            stored: stored.function,
            edits: stored.edits,
            refused: stored.refused,
            problem: stored.problem,
            command,
        });
    }
    view.hash = view_hash(&view.functions);
    Read {
        view,
        worktree: Some(ctx.worktree),
        shipped: None,
        items,
    }
}

/// The view the clients get.
pub(super) async fn view(shared: &Arc<Shared>, pr: &PrRef) -> FunctionsView {
    read(shared, pr).await.view
}

/// The checks every write and run makes first; the read and the index of the function.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
pub(super) async fn writable(
    shared: &Arc<Shared>,
    pr: &PrRef,
    slug: &str,
) -> Result<(Read, usize), Outcome> {
    if !valid_slug(slug) {
        return Err(reviews::bad_request("invalid function id"));
    }
    super::active(shared, pr)?;
    if shared
        .tests
        .closing
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return Err(reviews::invalid_state(STOPPING));
    }
    let read = read(shared, pr).await;
    if let Some(problem) = &read.shipped {
        return Err(reviews::invalid_state(problem.clone()));
    }
    let Some(at) = read.view.functions.iter().position(|f| f.slug == slug) else {
        return Err(reviews::invalid_state(format!(
            "No changed function {slug} in {pr}"
        )));
    };
    if read.items[at].refused {
        return Err(reviews::invalid_state(FOLDER_REFUSED));
    }
    if let Some(problem) = &read.items[at].problem {
        return Err(reviews::invalid_state(problem.clone()));
    }
    Ok((read, at))
}

/// Whether what the reviewer saw (`seen`, a function's hash or, for `All`, the view's) is
/// still what would run.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn check_seen(
    view: &FunctionsView,
    pr: &PrRef,
    scope: &CaseScope,
    seen: &str,
) -> Result<(), Outcome> {
    let now = match scope {
        CaseScope::All => &view.hash,
        CaseScope::Function { function } | CaseScope::Case { function, .. } => {
            let Some(f) = view.functions.iter().find(|f| f.slug == *function) else {
                return Err(reviews::invalid_state(format!(
                    "No changed function {function} in {pr}"
                )));
            };
            if let CaseScope::Case { case, .. } = scope
                && !f.cases.iter().any(|c| c.id == *case)
            {
                return Err(reviews::invalid_state(format!(
                    "No case {case} in {}",
                    f.qualified
                )));
            }
            &f.hash
        }
    };
    if now == seen {
        Ok(())
    } else {
        Err(reviews::invalid_state(CASES_CHANGED))
    }
}

/// The functions the recipe turn writes cases for: the first `FUNCTIONS_MAX` listed ones of a
/// language with drivers. Waits up to `INDEX_WAIT` for the index of `head`; none without it.
pub(super) async fn briefs(shared: &Arc<Shared>, pr: &PrRef, head: &str) -> Vec<FunctionBrief> {
    let Some(built) = calls::wait_ready(shared, pr, head, INDEX_WAIT).await else {
        return Vec::new();
    };
    let (listed, _) = listed(&built);
    let chosen: Vec<(&Listed<'_>, FunctionLang)> = listed
        .iter()
        .filter_map(|l| lang_of(&l.function.lang).map(|lang| (l, lang)))
        .take(FUNCTIONS_MAX)
        .collect();
    let ids: Vec<FunctionId> = chosen.iter().map(|(l, _)| l.function.id.clone()).collect();
    let signatures = calls::signatures_of(shared, pr, &built, &ids).await;
    chosen
        .into_iter()
        .map(|(l, lang)| {
            let f = l.function;
            FunctionBrief {
                slug: l.slug.clone(),
                qualified: f.qualified.clone(),
                file: f.path.clone(),
                lang,
                change: l.change,
                signature: trim_signature(
                    signatures
                        .get(&f.id)
                        .map(String::as_str)
                        .unwrap_or_default(),
                    &f.lang,
                ),
                start_line: f.start_line,
                end_line: f.end_line,
            }
        })
        .collect()
}

/// Writes the recipe turn's function blocks as fresh cases; answers how many could not be used.
/// The caller holds the review lock.
pub(super) fn adopt_blocks(
    worktree: &Path,
    record: &mut TestsRecord,
    briefs: &[FunctionBrief],
    blocks: &[serde_json::Value],
    head: &str,
    now: i64,
) -> u32 {
    let mut unreadable = 0;
    for block in blocks {
        let named = |key: &str| block.get(key).and_then(|v| v.as_str());
        let Some(brief) = briefs.iter().find(|b| {
            named("function") == Some(b.qualified.as_str())
                && named("file") == Some(b.file.as_str())
        }) else {
            unreadable += 1;
            tracing::warn!(reasons = ?["no listed function matches it"], "a clusia-function block could not be used");
            continue;
        };
        let target = Target {
            slug: brief.slug.clone(),
            qualified: brief.qualified.clone(),
            file: brief.file.clone(),
            lang: brief.lang,
            signature: brief.signature.clone(),
            head: head.to_string(),
        };
        match apply_block(worktree, &target, block, &CasesAsk::Fresh, now) {
            Ok(()) => forget_replaced_results(worktree, &target.slug, record),
            Err(reasons) => {
                unreadable += 1;
                tracing::warn!(reasons = ?reasons, "a clusia-function block could not be used");
            }
        }
    }
    unreadable
}

/// Tells every window the changed functions of `pr`.
pub(super) async fn tell_functions(shared: &Arc<Shared>, pr: &PrRef) {
    let view = view(shared, pr).await;
    shared.publish(
        topics::TESTS,
        Event::FunctionsChanged {
            pr: pr.clone(),
            view: Box::new(view),
        },
    );
}

/// Ends functions activity `id`, keeping `failed` until the next one begins.
pub(super) fn end(shared: &Shared, pr: &PrRef, id: u64, failed: Option<String>) {
    shared.tests.with(pr, |entry| {
        if entry.activity.as_ref().is_some_and(|a| a.id == id) {
            entry.activity = None;
            entry.cases_failed = failed;
        }
    });
}

/// Publishes the view after a write and answers it.
async fn changed(shared: &Arc<Shared>, pr: &PrRef) -> Outcome {
    let view = view(shared, pr).await;
    shared.publish(
        topics::TESTS,
        Event::FunctionsChanged {
            pr: pr.clone(),
            view: Box::new(view.clone()),
        },
    );
    Outcome::Ok(Reply::ChangedFunctions(Box::new(view)))
}

fn write_failed(message: String) -> Outcome {
    if message == FOLDER_REFUSED {
        reviews::invalid_state(message)
    } else {
        Outcome::Err(ProtocolError::new(ErrorCode::Internal, message))
    }
}

/// `GetFunctions`.
pub(crate) async fn functions(shared: &Arc<Shared>, pr: &PrRef) -> Outcome {
    Outcome::Ok(Reply::ChangedFunctions(Box::new(view(shared, pr).await)))
}

/// What an edit works on, read again under the review lock.
struct Editing {
    worktree: PathBuf,
    qualified: String,
    function: StoredFunction,
    edits: Edits,
}

/// Reads the stored function of `slug` again under the lock; refuses one with no cases.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
fn editing(read: &Read, at: usize, slug: &str, name: &str) -> Result<Editing, Outcome> {
    let Some(worktree) = read.worktree.clone() else {
        return Err(reviews::invalid_state(OPEN_FIRST));
    };
    let stored = load_stored(&worktree, slug);
    if stored.refused {
        return Err(reviews::invalid_state(FOLDER_REFUSED));
    }
    if let Some(problem) = stored.problem {
        return Err(reviews::invalid_state(problem));
    }
    let Some(function) = stored.function.filter(|f| !f.cases.is_empty()) else {
        return Err(reviews::invalid_state(format!(
            "{name} has not written cases for this function yet."
        )));
    };
    Ok(Editing {
        worktree,
        qualified: read.view.functions[at].qualified.clone(),
        function,
        edits: stored.edits,
    })
}

/// `SaveCase`: the reviewer's change of a case, or a new case of theirs.
pub(crate) async fn save_case(
    shared: &Arc<Shared>,
    pr: &PrRef,
    function: &str,
    input: CaseInput,
) -> Outcome {
    let (read, at) = match writable(shared, pr, function).await {
        Ok(found) => found,
        Err(out) => return out,
    };
    let name = sessions::name_for(shared, pr).await;
    {
        let _guard = reviews::lock(shared, pr).await;
        let Editing {
            worktree,
            qualified,
            function: stored,
            mut edits,
        } = match editing(&read, at, function, &name) {
            Ok(editing) => editing,
            Err(out) => return out,
        };
        if let Err(reasons) = check_case_input(&stored.params, &input) {
            return reviews::bad_request(reasons.join("; "));
        }
        match input.id.as_deref() {
            Some(id) => {
                if let Some(agent) = stored.cases.iter().find(|c| c.id == id) {
                    let changed: BTreeMap<String, String> = input
                        .values
                        .iter()
                        .filter(|(k, v)| agent.values.get(*k) != Some(*v))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    if changed.is_empty() {
                        edits.values.remove(id);
                    } else {
                        edits.values.insert(id.to_string(), changed);
                    }
                    if input.name == agent.name {
                        edits.names.remove(id);
                    } else {
                        edits.names.insert(id.to_string(), input.name.clone());
                    }
                } else if let Some(own) = edits.added.iter_mut().find(|c| c.id == id) {
                    own.name = input.name.clone();
                    own.values = input.values.clone();
                } else {
                    return reviews::invalid_state(format!("No case {id} in {qualified}"));
                }
            }
            None => {
                if effective(Some(&stored), &edits).len() >= CASES_MAX {
                    return reviews::invalid_state(format!(
                        "This function has {CASES_MAX} cases, the most it keeps."
                    ));
                }
                let n = edits.next.max(1);
                edits.added.push(Case {
                    id: format!("r{n}"),
                    name: input.name.clone(),
                    values: input.values.clone(),
                    expect: Default::default(),
                });
                edits.next = n + 1;
            }
        }
        if let Err(message) = save_edits(&worktree, function, &edits) {
            return write_failed(message);
        }
    }
    changed(shared, pr).await
}

/// `ResetCase`: drops the reviewer's edits of one of the agent's cases.
pub(crate) async fn reset_case(
    shared: &Arc<Shared>,
    pr: &PrRef,
    function: &str,
    case: &str,
) -> Outcome {
    let (read, at) = match writable(shared, pr, function).await {
        Ok(found) => found,
        Err(out) => return out,
    };
    let name = sessions::name_for(shared, pr).await;
    {
        let _guard = reviews::lock(shared, pr).await;
        let Editing {
            worktree,
            qualified,
            function: stored,
            mut edits,
        } = match editing(&read, at, function, &name) {
            Ok(editing) => editing,
            Err(out) => return out,
        };
        if edits.added.iter().any(|c| c.id == case) {
            return reviews::invalid_state(format!("Only {name}'s cases can be reset"));
        }
        if !stored.cases.iter().any(|c| c.id == case) {
            return reviews::invalid_state(format!("No case {case} in {qualified}"));
        }
        edits.values.remove(case);
        edits.names.remove(case);
        if let Err(message) = save_edits(&worktree, function, &edits) {
            return write_failed(message);
        }
    }
    changed(shared, pr).await
}

/// `RemoveCase`: deletes one of the reviewer's cases and its result.
pub(crate) async fn remove_case(
    shared: &Arc<Shared>,
    pr: &PrRef,
    function: &str,
    case: &str,
) -> Outcome {
    let (read, at) = match writable(shared, pr, function).await {
        Ok(found) => found,
        Err(out) => return out,
    };
    let name = sessions::name_for(shared, pr).await;
    {
        let _guard = reviews::lock(shared, pr).await;
        let Editing {
            worktree,
            qualified,
            function: stored,
            mut edits,
        } = match editing(&read, at, function, &name) {
            Ok(editing) => editing,
            Err(out) => return out,
        };
        if stored.cases.iter().any(|c| c.id == case) {
            return reviews::invalid_state("Only your cases can be removed");
        }
        let Some(position) = edits.added.iter().position(|c| c.id == case) else {
            return reviews::invalid_state(format!("No case {case} in {qualified}"));
        };
        edits.added.remove(position);
        if let Err(message) = save_edits(&worktree, function, &edits) {
            return write_failed(message);
        }
    }
    env::update_record(shared, pr, |record| {
        if let Some(results) = record.case_results.get_mut(function) {
            results.remove(case);
            if results.is_empty() {
                record.case_results.remove(function);
            }
        }
    })
    .await;
    changed(shared, pr).await
}

/// `GetDriver`: the driver of one function, read without following a link.
pub(crate) async fn driver(shared: &Arc<Shared>, pr: &PrRef, function: &str) -> Outcome {
    let (read, at) = match writable(shared, pr, function).await {
        Ok(found) => found,
        Err(out) => return out,
    };
    let Some(worktree) = read.worktree.as_deref() else {
        return reviews::invalid_state(OPEN_FIRST);
    };
    let view = &read.view.functions[at];
    let lang = read.items[at].stored.as_ref().map(|s| s.lang).or(view.lang);
    let text = match lang.map(|lang| read_driver(worktree, function, lang)) {
        Some(Ok(Some(text))) => text,
        Some(Err(message)) => return reviews::invalid_state(message),
        Some(Ok(None)) | None => {
            return reviews::invalid_state(format!("{} has no driver yet", view.qualified));
        }
    };
    let lang = lang.unwrap_or(FunctionLang::Rust);
    Outcome::Ok(Reply::Driver(Box::new(
        clusia_core::functions::DriverCode {
            function: function.to_string(),
            path: driver_path(function, lang),
            hash: sha256_hex(text.as_bytes()),
            text,
        },
    )))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::symlink;

    use clusia_core::functions::{
        Case, CaseOrigin, CasesAsk, Edits, Expect, FunctionLang, Param, ParamKind, StoredFunction,
        Support, function_slug,
    };
    use serde_json::json;

    use super::*;

    fn slug() -> String {
        function_slug("src/store.rs", "TokenStore::refresh")
    }

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

    fn values(token: &str, status: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("stored_token".to_string(), token.to_string()),
            ("endpoint_status".to_string(), status.to_string()),
        ])
    }

    fn case(id: &str, name: &str, token: &str, status: &str) -> Case {
        Case {
            id: id.into(),
            name: name.into(),
            values: values(token, status),
            expect: Expect::default(),
        }
    }

    fn stored() -> StoredFunction {
        StoredFunction {
            qualified: "TokenStore::refresh".into(),
            file: "src/store.rs".into(),
            lang: FunctionLang::Rust,
            signature: "pub fn refresh(&self, now: u64) -> Result<u32, String>".into(),
            params: params(),
            state_label: Some("Store".into()),
            unsupported: None,
            driver_hash: Some("d".repeat(64)),
            cases: vec![
                case("a1", "Endpoint fails, old token kept", "old", "500"),
                case("a2", "Valid token, no exchange", "fresh", "200"),
            ],
            next: 3,
            written_head: "h1".into(),
            written_at: 1,
        }
    }

    fn target() -> Target {
        Target {
            slug: slug(),
            qualified: "TokenStore::refresh".into(),
            file: "src/store.rs".into(),
            lang: FunctionLang::Rust,
            signature: "pub fn refresh(&self, now: u64) -> Result<u32, String>".into(),
            head: "h1".into(),
        }
    }

    fn block(cases: serde_json::Value) -> serde_json::Value {
        json!({
            "function": "TokenStore::refresh",
            "file": "src/store.rs",
            "state_label": "Store",
            "params": [
                {"name": "stored_token", "label": "stored token", "kind": "text"},
                {"name": "endpoint_status", "label": "endpoint status", "kind": "int"}
            ],
            "driver": "#[test]\nfn clusia_case() {}\n",
            "cases": cases
        })
    }

    fn two_cases() -> serde_json::Value {
        json!([
            {"name": "Endpoint fails, old token kept",
             "values": {"stored_token": "old", "endpoint_status": "500"},
             "expect": {"returned": "Err(Http(500))", "state": {"token": "old"}}},
            {"name": "Valid token, no exchange",
             "values": {"stored_token": "fresh", "endpoint_status": "200"},
             "expect": {"returned": "Ok(1)"}}
        ])
    }

    fn folder_of(wt: &tempfile::TempDir) -> std::path::PathBuf {
        wt.path().join(".clusia/functions").join(slug())
    }

    #[test]
    fn test_code_is_not_listed() {
        for (path, qualified, lang) in [
            ("tests/api.rs", "api_works", "rust"),
            ("src/test/helpers.py", "make", "python"),
            ("web/__tests__/badge.ts", "renders", "typescript"),
            ("spec/store.js", "works", "javascript"),
            ("app/test_store.py", "test_refresh", "python"),
            ("app/store_test.py", "check", "python"),
            ("app/conftest.py", "fixture", "python"),
            ("src/store_test.rs", "check", "rust"),
            ("web/badge.test.ts", "renders", "typescript"),
            ("web/badge.spec.js", "renders", "javascript"),
            ("src/store.rs", "tests::refresh_works", "rust"),
            ("src/store.rs", "store::test::helper", "rust"),
        ] {
            assert!(is_test_code(path, qualified, lang), "{path} {qualified}");
        }
        for (path, qualified, lang) in [
            ("src/store.rs", "TokenStore::refresh", "rust"),
            ("src/testing.rs", "fake_clock", "rust"),
            ("web/test.ts", "renders", "typescript"),
            ("app/tests.py", "run", "python"),
            ("src/store.rs", "Tests::count", "rust"),
            ("app/store.py", "tests", "python"),
        ] {
            assert!(!is_test_code(path, qualified, lang), "{path} {qualified}");
        }
    }

    #[test]
    fn signatures_lose_their_opening_brace_or_colon() {
        assert_eq!(
            trim_signature("pub fn refresh(&self) -> u32 {", "rust"),
            "pub fn refresh(&self) -> u32"
        );
        assert_eq!(
            trim_signature("def refresh(self, now):", "python"),
            "def refresh(self, now)"
        );
        assert_eq!(
            trim_signature("export function delay(n: number): number {", "typescript"),
            "export function delay(n: number): number"
        );
        assert_eq!(trim_signature("  fn a() { {", "rust"), "fn a() {");
        assert_eq!(
            trim_signature(&"x".repeat(400), "rust").chars().count(),
            300
        );
    }

    #[test]
    fn the_effective_cases_lay_the_edits_over_the_agents() {
        let edits = Edits {
            values: BTreeMap::from([(
                "a1".to_string(),
                BTreeMap::from([("endpoint_status".to_string(), "503".to_string())]),
            )]),
            names: BTreeMap::from([("a2".to_string(), "Fresh token".to_string())]),
            added: vec![case("r1", "New case", "old", "404")],
            next: 2,
        };
        let cases = effective(Some(&stored()), &edits);
        let ids: Vec<&str> = cases.iter().map(|e| e.case.id.as_str()).collect();
        assert_eq!(ids, ["a1", "a2", "r1"]);
        assert_eq!(cases[0].case.values, values("old", "503"));
        assert_eq!(cases[0].case.name, "Endpoint fails, old token kept");
        assert!(cases[0].edited && cases[1].edited && cases[2].edited);
        assert_eq!(cases[1].case.name, "Fresh token");
        assert_eq!(cases[1].case.values, values("fresh", "200"));
        assert_eq!(
            (cases[0].origin, cases[2].origin),
            (CaseOrigin::Agent, CaseOrigin::Reviewer)
        );
        let plain = effective(Some(&stored()), &Edits::default());
        assert!(plain.iter().all(|e| !e.edited));
        assert!(
            effective(None, &edits)
                .iter()
                .all(|e| e.origin == CaseOrigin::Reviewer)
        );
    }

    #[test]
    fn support_takes_the_first_reason_that_applies() {
        let base = stored();
        let sig = base.signature.clone();
        let command = "python3 {driver}";
        let facts = |stored: Option<&StoredFunction>,
                     refused: bool,
                     no_package: bool,
                     sig: &str,
                     cases: usize,
                     command: Option<&str>| {
            support_of(&Facts {
                stored,
                folder_refused: refused,
                folder_problem: None,
                no_package,
                signature: sig,
                cases,
                lang: Some(FunctionLang::Rust),
                command,
            })
        };
        let mut marked = base.clone();
        marked.unsupported =
            Some("Not public: a driver can't call it from outside its crate".into());
        assert_eq!(
            facts(Some(&marked), true, true, "other", 0, None),
            Support::Unsupported {
                reason: "Not public: a driver can't call it from outside its crate".into()
            }
        );
        assert_eq!(
            facts(Some(&base), true, true, &sig, 2, Some(command)),
            Support::Unsupported {
                reason: "No Cargo package holds this file".into()
            }
        );
        assert_eq!(
            facts(Some(&base), true, false, &sig, 2, Some(command)),
            Support::Unsupported {
                reason: FOLDER_REFUSED.into()
            }
        );
        assert_eq!(
            facts(
                Some(&base),
                false,
                false,
                "pub fn refresh(&self) -> u32",
                2,
                Some(command)
            ),
            Support::Stale {
                reason: "The signature changed since these cases were written".into()
            }
        );
        assert_eq!(
            facts(None, false, false, &sig, 0, Some(command)),
            Support::NoCases
        );
        assert_eq!(
            facts(Some(&base), false, false, &sig, 0, Some(command)),
            Support::NoCases
        );
        assert_eq!(
            facts(Some(&base), false, false, &sig, 2, None),
            Support::NoCommand
        );
        assert_eq!(
            facts(Some(&base), false, false, &sig, 2, Some(command)),
            Support::Ready
        );
        assert_eq!(
            facts(Some(&base), false, false, "", 2, Some(command)),
            Support::Ready,
            "an unread signature is not a change"
        );
        let mut unsigned = base.clone();
        unsigned.signature = String::new();
        assert_eq!(
            facts(Some(&unsigned), false, false, &sig, 2, Some(command)),
            Support::Ready,
            "cases written while the signature was unread are not stale"
        );
        assert_eq!(
            support_of(&Facts {
                stored: None,
                folder_refused: false,
                folder_problem: Some("cannot open functions: Permission denied"),
                no_package: false,
                signature: &sig,
                cases: 0,
                lang: Some(FunctionLang::Rust),
                command: Some(command),
            }),
            Support::Unsupported {
                reason: "cannot open functions: Permission denied".into()
            }
        );
    }

    #[test]
    fn a_folder_that_cannot_be_opened_says_why() {
        use std::os::unix::fs::PermissionsExt;
        let wt = tempfile::tempdir().unwrap();
        save_function(wt.path(), &slug(), &stored(), None).unwrap();
        let functions = wt.path().join(".clusia/functions");
        fs::set_permissions(&functions, fs::Permissions::from_mode(0o000)).unwrap();
        let read = load_stored(wt.path(), &slug());
        fs::set_permissions(&functions, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!read.refused && read.function.is_none());
        assert!(
            read.problem
                .as_deref()
                .is_some_and(|p| p.starts_with("cannot open ")),
            "{:?}",
            read.problem
        );
    }

    #[test]
    fn a_symlinked_functions_folder_is_refused_and_never_followed() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(
            outside.path().join("function.json"),
            serde_json::to_vec(&stored()).unwrap(),
        )
        .unwrap();
        let empty_outside = || fs::read_dir(outside.path()).unwrap().count() == 1;

        // `.clusia/functions` is a link.
        let wt = tempfile::tempdir().unwrap();
        fs::create_dir(wt.path().join(".clusia")).unwrap();
        symlink(outside.path(), wt.path().join(".clusia/functions")).unwrap();
        assert_eq!(
            folder(wt.path(), &slug(), true).err(),
            Some(Refusal::NotPlain)
        );
        let read = load_stored(wt.path(), &slug());
        assert!(read.refused && read.function.is_none());
        assert!(save_edits(wt.path(), &slug(), &Edits::default()).is_err());
        assert!(save_function(wt.path(), &slug(), &stored(), Some("x")).is_err());
        assert!(empty_outside(), "nothing was written through the link");

        // The slug folder is a link.
        let wt = tempfile::tempdir().unwrap();
        fs::create_dir_all(wt.path().join(".clusia/functions")).unwrap();
        symlink(outside.path(), folder_of(&wt)).unwrap();
        let read = load_stored(wt.path(), &slug());
        assert!(
            read.refused && read.function.is_none(),
            "the outside file is never read"
        );
        assert!(
            apply_block(
                wt.path(),
                &target(),
                &block(two_cases()),
                &CasesAsk::Fresh,
                5
            )
            .is_err()
        );
        assert!(empty_outside());

        // `function.json` is a link to a valid file.
        let wt = tempfile::tempdir().unwrap();
        fs::create_dir_all(folder_of(&wt)).unwrap();
        symlink(
            outside.path().join("function.json"),
            folder_of(&wt).join("function.json"),
        )
        .unwrap();
        let read = load_stored(wt.path(), &slug());
        assert!(read.refused && read.function.is_none());

        // `.clusia/functions` is a file.
        let wt = tempfile::tempdir().unwrap();
        fs::create_dir(wt.path().join(".clusia")).unwrap();
        fs::write(wt.path().join(".clusia/functions"), "x").unwrap();
        assert_eq!(
            folder(wt.path(), &slug(), true).err(),
            Some(Refusal::NotPlain)
        );

        // A hostile slug never reaches a path.
        let wt = tempfile::tempdir().unwrap();
        for bad in ["../../etc/x", "a/b", ".", "~", "x\0y"] {
            assert!(folder(wt.path(), bad, true).is_err(), "{bad:?}");
        }
        assert!(!wt.path().join(".clusia").exists());
    }

    #[test]
    fn files_are_written_whole_and_read_capped() {
        let wt = tempfile::tempdir().unwrap();
        save_function(
            wt.path(),
            &slug(),
            &stored(),
            Some("#[test]\nfn clusia_case() {}\n"),
        )
        .unwrap();
        assert_eq!(load_stored(wt.path(), &slug()).function, Some(stored()));
        assert_eq!(
            read_driver(wt.path(), &slug(), FunctionLang::Rust)
                .unwrap()
                .as_deref(),
            Some("#[test]\nfn clusia_case() {}\n")
        );
        assert_eq!(
            driver_path(&slug(), FunctionLang::JavaScript),
            format!(".clusia/functions/{}/driver.mjs", slug())
        );
        let mut names: Vec<String> = fs::read_dir(folder_of(&wt))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["driver.rs", "function.json"],
            "no temporary file is left"
        );

        save_function(wt.path(), &slug(), &stored(), None).unwrap();
        assert_eq!(
            read_driver(wt.path(), &slug(), FunctionLang::Rust).unwrap(),
            None
        );

        fs::write(
            folder_of(&wt).join("edits.json"),
            vec![b' '; 256 * 1024 + 1],
        )
        .unwrap();
        assert_eq!(load_stored(wt.path(), &slug()).edits, Edits::default());
        fs::write(folder_of(&wt).join("driver.rs"), vec![b'a'; 32_769]).unwrap();
        assert!(read_driver(wt.path(), &slug(), FunctionLang::Rust).is_err());
        fs::write(folder_of(&wt).join("function.json"), "{not json").unwrap();
        let read = load_stored(wt.path(), &slug());
        assert!(read.function.is_none() && !read.refused);
    }

    #[test]
    fn edits_whose_values_no_longer_check_are_dropped_on_read() {
        let wt = tempfile::tempdir().unwrap();
        save_function(wt.path(), &slug(), &stored(), None).unwrap();
        let edits = Edits {
            values: BTreeMap::from([
                (
                    "a1".to_string(),
                    BTreeMap::from([("endpoint_status".to_string(), "five".to_string())]),
                ),
                (
                    "a2".to_string(),
                    BTreeMap::from([("endpoint_status".to_string(), "503".to_string())]),
                ),
            ]),
            names: BTreeMap::new(),
            added: vec![case("r1", "New case", "old", "x")],
            next: 2,
        };
        save_edits(wt.path(), &slug(), &edits).unwrap();
        let read = load_stored(wt.path(), &slug()).edits;
        assert_eq!(read.values.keys().collect::<Vec<_>>(), ["a2"]);
        assert!(read.added.is_empty());
        assert_eq!(read.next, 2);
    }

    #[test]
    fn fresh_more_and_values_change_the_files_as_the_ask_says() {
        let wt = tempfile::tempdir().unwrap();
        apply_block(
            wt.path(),
            &target(),
            &block(two_cases()),
            &CasesAsk::Fresh,
            7,
        )
        .unwrap();
        let first = load_stored(wt.path(), &slug()).function.unwrap();
        assert_eq!(
            first
                .cases
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            ["a1", "a2"]
        );
        assert_eq!(first.next, 3);
        assert_eq!(first.signature, target().signature);
        assert_eq!(first.written_head, "h1");
        assert_eq!(first.written_at, 7);
        assert_eq!(
            first.driver_hash.as_deref(),
            Some(clusia_testrun::names::sha256_hex(b"#[test]\nfn clusia_case() {}\n").as_str())
        );
        assert_eq!(
            first.cases[0].expect.returned.as_deref(),
            Some("Err(Http(500))")
        );
        assert_eq!(
            first.cases[0].expect.state.get("token").map(String::as_str),
            Some("old")
        );

        let more = block(json!([
            {"name": "Expired token, one exchange",
             "values": {"stored_token": "expired", "endpoint_status": "200"}}
        ]));
        apply_block(wt.path(), &target(), &more, &CasesAsk::More, 8).unwrap();
        let now = load_stored(wt.path(), &slug()).function.unwrap();
        assert_eq!(
            now.cases.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["a1", "a2", "a3"]
        );
        assert_eq!(now.next, 4);

        save_edits(
            wt.path(),
            &slug(),
            &Edits {
                values: BTreeMap::from([(
                    "a1".to_string(),
                    BTreeMap::from([("endpoint_status".to_string(), "503".to_string())]),
                )]),
                names: BTreeMap::from([("a1".to_string(), "Mine".to_string())]),
                added: Vec::new(),
                next: 0,
            },
        )
        .unwrap();
        let answer = block(json!([
            {"name": "ignored",
             "values": {"stored_token": "stale", "endpoint_status": "401"},
             "expect": {"returned": "Err(Http(401))"}}
        ]));
        apply_block(
            wt.path(),
            &target(),
            &answer,
            &CasesAsk::Values { case: "a1".into() },
            9,
        )
        .unwrap();
        let read = load_stored(wt.path(), &slug());
        let a1 = &read.function.as_ref().unwrap().cases[0];
        assert_eq!(a1.values, values("stale", "401"));
        assert_eq!(a1.name, "Endpoint fails, old token kept");
        assert_eq!(a1.expect.returned.as_deref(), Some("Err(Http(401))"));
        assert!(
            !read.edits.values.contains_key("a1"),
            "the values overlay is gone"
        );
        assert_eq!(read.edits.names.get("a1").map(String::as_str), Some("Mine"));

        let many: Vec<serde_json::Value> = (0..30)
            .map(|n| json!({"name": format!("Case {n}"), "values": {"stored_token": "t", "endpoint_status": "200"}}))
            .collect();
        apply_block(
            wt.path(),
            &target(),
            &block(json!(many)),
            &CasesAsk::More,
            10,
        )
        .unwrap();
        assert_eq!(
            load_stored(wt.path(), &slug())
                .function
                .unwrap()
                .cases
                .len(),
            20
        );
    }

    #[test]
    fn fresh_keeps_the_edits_only_when_the_params_are_the_same() {
        let wt = tempfile::tempdir().unwrap();
        apply_block(
            wt.path(),
            &target(),
            &block(two_cases()),
            &CasesAsk::Fresh,
            1,
        )
        .unwrap();
        let edits = Edits {
            values: BTreeMap::from([(
                "a1".to_string(),
                BTreeMap::from([("endpoint_status".to_string(), "503".to_string())]),
            )]),
            names: BTreeMap::new(),
            added: vec![case("r1", "New case", "old", "404")],
            next: 2,
        };
        save_edits(wt.path(), &slug(), &edits).unwrap();
        apply_block(
            wt.path(),
            &target(),
            &block(two_cases()),
            &CasesAsk::Fresh,
            2,
        )
        .unwrap();
        let read = load_stored(wt.path(), &slug());
        assert_eq!(
            read.function
                .unwrap()
                .cases
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            ["a3", "a4"],
            "ids are never reused"
        );
        assert!(
            read.edits.values.is_empty(),
            "overlays of ids that are gone are dropped"
        );
        assert_eq!(read.edits.added, edits.added);

        let mut other = block(two_cases());
        other["params"][1]["kind"] = json!("text");
        apply_block(wt.path(), &target(), &other, &CasesAsk::Fresh, 3).unwrap();
        let edits = load_stored(wt.path(), &slug()).edits;
        assert_eq!(
            edits,
            Edits {
                next: 2,
                ..Edits::default()
            },
            "other params drop every edit but keep the next reviewer id"
        );
    }

    #[test]
    fn a_block_for_another_function_or_with_bad_values_changes_nothing() {
        let wt = tempfile::tempdir().unwrap();
        apply_block(
            wt.path(),
            &target(),
            &block(two_cases()),
            &CasesAsk::Fresh,
            1,
        )
        .unwrap();
        let before = fs::read(folder_of(&wt).join("function.json")).unwrap();

        let mut elsewhere = block(two_cases());
        elsewhere["function"] = json!("TokenStore::exchange");
        let reasons =
            apply_block(wt.path(), &target(), &elsewhere, &CasesAsk::Fresh, 2).unwrap_err();
        assert_eq!(
            reasons,
            [
                "The block is for TokenStore::exchange in src/store.rs, not TokenStore::refresh in src/store.rs"
            ]
        );

        let control = block(json!([
            {"name": "Bell", "values": {"stored_token": "a\u{7}", "endpoint_status": "200"}}
        ]));
        assert!(apply_block(wt.path(), &target(), &control, &CasesAsk::Fresh, 2).is_err());

        let unknown_param = block(json!([
            {"name": "Extra", "values": {"stored_token": "a", "endpoint_status": "200", "now": "1"}}
        ]));
        assert!(apply_block(wt.path(), &target(), &unknown_param, &CasesAsk::More, 2).is_err());

        let two = block(two_cases());
        assert_eq!(
            apply_block(
                wt.path(),
                &target(),
                &two,
                &CasesAsk::Values { case: "a1".into() },
                2
            )
            .unwrap_err(),
            ["The answer must hold exactly one case"]
        );
        let one =
            block(json!([{"name": "x", "values": {"stored_token": "a", "endpoint_status": "1"}}]));
        assert_eq!(
            apply_block(
                wt.path(),
                &target(),
                &one,
                &CasesAsk::Values { case: "a9".into() },
                2
            )
            .unwrap_err(),
            ["No case a9"]
        );
        assert_eq!(
            fs::read(folder_of(&wt).join("function.json")).unwrap(),
            before
        );

        let fresh_wt = tempfile::tempdir().unwrap();
        assert_eq!(
            apply_block(fresh_wt.path(), &target(), &one, &CasesAsk::More, 2).unwrap_err(),
            ["There are no cases to add to"]
        );
        assert!(!fresh_wt.path().join(".clusia").exists());
    }
}

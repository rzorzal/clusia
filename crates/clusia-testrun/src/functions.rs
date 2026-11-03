//! Running one function's driver for one case: the command from the recipe's template, the
//! Cargo package a Rust file belongs to, the Rust driver placed and removed without following
//! links, and the driver's report and status.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use clusia_core::functions::{
    CaseStatus, Expect, NO_RESULT, RESULT_TEXT_MAX, STATE_MAX, StateCheck, valid_slug,
};

use crate::exec::Ended;
use crate::names::sha256_hex;
use crate::recipe::quote_name;

pub const NO_PACKAGE: &str = "No Cargo package holds this file";
/// The largest `Cargo.toml` read while looking for a file's package.
const CARGO_MAX: u64 = 64 * 1024;
/// The longest package name taken from a `Cargo.toml`.
const NAME_MAX: usize = 64;
const TARGET_PREFIX: &str = "clusia_driver_";
const TESTS: &str = "tests";

pub fn function_command(
    template: &str,
    driver: &str,
    package: Option<&str>,
) -> Result<String, String> {
    let quoted_driver =
        quote_name(driver).ok_or_else(|| format!("Clúsia won't run a driver named {driver}"))?;
    let mut out = String::with_capacity(template.len() + driver.len());
    let mut rest = template;
    while let Some(at) = rest.find('{') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        if let Some(after) = tail.strip_prefix("{driver}") {
            out.push_str(&quoted_driver);
            rest = after;
        } else if let Some(after) = tail.strip_prefix("{package}") {
            let package = package.ok_or("The command needs a package")?;
            out.push_str(
                &quote_name(package)
                    .ok_or_else(|| format!("Clúsia won't run a package named {package}"))?,
            );
            rest = after;
        } else {
            out.push('{');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// The folder of the Cargo package that holds `file` (relative to the worktree, `""` for the
/// root) and the package's name. Every folder on the way is opened without following a link,
/// and the walk up never leaves the worktree.
pub fn package_for(worktree: &Path, file: &str) -> Result<(String, String), String> {
    let no_package = || NO_PACKAGE.to_string();
    let parts = plain_parts(file).ok_or_else(no_package)?;
    let folders = &parts[..parts.len() - 1];
    let mut opened = vec![open_root(worktree).map_err(|_| no_package())?];
    for name in folders {
        // A folder that is not there yet (a file the head adds under a new folder, a worktree
        // not checked out) ends the descent; the walk up starts from the deepest one that is.
        match open_dir_at(opened.last().expect("the root is open"), name, false) {
            Ok(Some(dir)) => opened.push(dir),
            Ok(None) => break,
            Err(_) => return Err(no_package()),
        }
    }
    for depth in (0..opened.len()).rev() {
        match package_in(&opened[depth])? {
            Some(name) => return Ok((folders[..depth].join("/"), name)),
            None => continue,
        }
    }
    Err(no_package())
}

/// The package named by the `Cargo.toml` in `dir`; `None` when there is no such file or it has
/// no `[package]` table. Anything else that is not a plain, small, valid manifest ends the
/// search.
fn package_in(dir: &OwnedFd) -> Result<Option<String>, String> {
    let no_package = || NO_PACKAGE.to_string();
    let file = match open_file_at(dir, "Cargo.toml", libc::O_RDONLY | libc::O_NONBLOCK, 0) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(no_package()),
    };
    let meta = file.metadata().map_err(|_| no_package())?;
    if !meta.is_file() || meta.len() > CARGO_MAX {
        return Err(no_package());
    }
    let mut bytes = Vec::new();
    file.take(CARGO_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| no_package())?;
    if bytes.len() as u64 > CARGO_MAX {
        return Err(no_package());
    }
    let text = String::from_utf8(bytes).map_err(|_| no_package())?;
    let table: toml::Table = text.parse().map_err(|_| no_package())?;
    let Some(package) = table.get("package") else {
        return Ok(None);
    };
    let name = package
        .as_table()
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .filter(|n| valid_package_name(n))
        .ok_or_else(no_package)?;
    Ok(Some(name.to_string()))
}

fn valid_package_name(name: &str) -> bool {
    (1..=NAME_MAX).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The name a Rust driver's file and test target take: `clusia_driver_` and the ten hex the
/// slug ends with, or the first ten hex of the slug's sha256 for a slug Clúsia didn't make.
pub fn driver_target(slug: &str) -> String {
    let hex = if valid_slug(slug) {
        slug[slug.len() - 10..].to_string()
    } else {
        sha256_hex(slug.as_bytes())[..10].to_string()
    };
    format!("{TARGET_PREFIX}{hex}")
}

fn valid_target(target: &str) -> bool {
    target.strip_prefix(TARGET_PREFIX).is_some_and(|hex| {
        hex.len() == 10 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Writes `text` as `<package_dir>/tests/<target>.rs` and answers that path, relative to the
/// worktree. Every folder is opened without following a link and `tests` is made when it is
/// missing; a link or a non-folder on the way refuses before anything is written. A file
/// already at that name (a link included, never what it points to) is removed first only when
/// `leftover` says Clúsia placed it; otherwise it is the worktree's own and the driver is
/// refused.
pub fn place_driver(
    worktree: &Path,
    package_dir: &str,
    target: &str,
    text: &[u8],
    leftover: bool,
) -> Result<String, String> {
    if !valid_target(target) {
        return Err(format!("Clúsia won't place a driver named {target}"));
    }
    let folders: Vec<&str> = if package_dir.is_empty() {
        Vec::new()
    } else {
        plain_parts(package_dir)
            .ok_or_else(|| format!("Clúsia won't place the driver in {package_dir}"))?
    };
    let failed = |e: io::Error| format!("Clúsia couldn't place the driver: {e}");
    let mut dir = open_root(worktree).map_err(failed)?;
    let mut walked = Vec::new();
    for name in folders.iter().copied().chain([TESTS]) {
        walked.push(name);
        let here = walked.join("/");
        let refused = || format!("Clúsia won't place the driver: {here} is not a plain folder");
        dir = match open_dir_at(
            &dir,
            name,
            name == TESTS && walked.len() == folders.len() + 1,
        ) {
            Ok(Some(next)) => next,
            Ok(None) => {
                return Err(format!(
                    "Clúsia couldn't place the driver: {here} is missing"
                ));
            }
            Err(e) if not_plain(&e) => return Err(refused()),
            Err(e) => return Err(failed(e)),
        };
    }
    let file_name = format!("{target}.rs");
    walked.push(&file_name);
    let path = walked.join("/");
    if leftover {
        match unlink_at(&dir, &file_name) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(failed(e)),
        }
    }
    // `O_EXCL` fails on any name already there, a link included, without following it.
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL;
    let mut file = match open_file_at(&dir, &file_name, flags, 0o644) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(format!(
                "Clúsia won't place the driver: {path} already exists"
            ));
        }
        Err(e) => return Err(failed(e)),
    };
    if let Err(e) = file.write_all(text).and_then(|()| file.sync_all()) {
        let _ = unlink_at(&dir, &file_name);
        return Err(failed(e));
    }
    Ok(path)
}

/// Removes a file `place_driver` placed, through freshly opened folders that are never links.
/// Only a path of the form `[<dir>/]tests/clusia_driver_<10 hex>.rs` is accepted; a file
/// already gone is not an error.
pub fn remove_driver(worktree: &Path, placed: &str) -> io::Result<()> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a placed driver: {placed}"),
        )
    };
    let parts = plain_parts(placed).ok_or_else(invalid)?;
    let [folders @ .., tests, file] = parts.as_slice() else {
        return Err(invalid());
    };
    let is_driver = file.strip_suffix(".rs").is_some_and(valid_target);
    if *tests != TESTS || !is_driver {
        return Err(invalid());
    }
    let mut dir = open_root(worktree)?;
    for name in folders.iter().chain([tests]) {
        dir = match open_dir_at(&dir, name, false)? {
            Some(next) => next,
            None => return Ok(()),
        };
    }
    match unlink_at(&dir, file) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// What a driver reported on its marked stdout lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriverReport {
    pub returned: Option<String>,
    pub error: Option<String>,
    /// Each name at the place it was first reported, with the last value reported for it.
    pub state: Vec<(String, String)>,
    pub took_ms: Option<u64>,
    pub done: bool,
}

/// The report in `stdout`: only whole lines that start with `<mark> ` count.
pub fn read_report(mark: &str, stdout: &str) -> DriverReport {
    let prefix = format!("{mark} ");
    let mut report = DriverReport::default();
    let mut pieces: Vec<&str> = stdout.split('\n').collect();
    // The last piece has no line break after it, so it is not a whole line.
    pieces.pop();
    for line in pieces {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(line) = line.strip_prefix(&prefix) else {
            continue;
        };
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "returned" => report.returned = Some(cut(rest)),
            "error" => report.error = Some(cut(rest)),
            "took" => {
                if let Ok(ms) = rest.trim().parse::<u64>() {
                    report.took_ms = Some(ms);
                }
            }
            "done" => report.done = true,
            "state" => {
                let (name, value) = rest.split_once(' ').unwrap_or((rest, ""));
                if name.is_empty() {
                    continue;
                }
                let (name, value) = (cut(name), cut(value));
                if let Some(slot) = report.state.iter_mut().find(|(n, _)| *n == name) {
                    slot.1 = value;
                } else if report.state.len() < STATE_MAX {
                    report.state.push((name, value));
                }
            }
            _ => {}
        }
    }
    report
}

fn cut(text: &str) -> String {
    text.chars().take(RESULT_TEXT_MAX).collect()
}

/// The case's status, its state checks and a message that says why, when the reason is not in
/// the report itself. The first rule that applies wins: a stop or a timeout, no report, an
/// error or a failing exit, an unmet expectation, and else a pass.
pub fn case_status(
    report: &DriverReport,
    ended: &Ended,
    exit: Option<i32>,
    expect: &Expect,
) -> (CaseStatus, Vec<StateCheck>, Option<String>) {
    let reported = |name: &str| {
        report
            .state
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    };
    let mut checks: Vec<StateCheck> = expect
        .state
        .iter()
        .map(|(name, expected)| StateCheck {
            name: name.clone(),
            got: reported(name),
            expected: Some(expected.clone()),
        })
        .collect();
    checks.extend(
        report
            .state
            .iter()
            .filter(|(name, _)| !expect.state.contains_key(name))
            .map(|(name, value)| StateCheck {
                name: name.clone(),
                got: Some(value.clone()),
                expected: None,
            }),
    );
    match ended {
        Ended::Stopped => return (CaseStatus::Stopped, checks, None),
        Ended::TimedOut => return (CaseStatus::TimedOut, checks, None),
        Ended::Exited(_) => {}
    }
    if !report.done {
        let code = match exit {
            Some(code) => format!("exit {code}"),
            None => "no exit code".to_string(),
        };
        return (
            CaseStatus::Error,
            checks,
            Some(format!("{NO_RESULT} ({code})")),
        );
    }
    if report.error.is_some() || exit != Some(0) {
        let message = match exit {
            Some(0) => None,
            Some(code) => Some(format!("The driver exited with {code}")),
            None => Some("The driver was ended by a signal".to_string()),
        };
        return (CaseStatus::Failed, checks, message);
    }
    let returned_differs = expect
        .returned
        .as_deref()
        .is_some_and(|want| report.returned.as_deref().map(str::trim) != Some(want.trim()));
    let state_differs = checks
        .iter()
        .any(|c| c.expected.is_some() && c.got != c.expected);
    if returned_differs || state_differs {
        return (CaseStatus::Failed, checks, None);
    }
    (CaseStatus::Passed, checks, None)
}

/// The environment a case's driver reads: the mark, the root, the case id and one
/// `CLUSIA_ARG_<NAME>` per value, in key order. The values travel only here.
pub fn case_env(
    mark: &str,
    root: &str,
    case: &str,
    values: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("CLUSIA_MARK".to_string(), mark.to_string()),
        ("CLUSIA_ROOT".to_string(), root.to_string()),
        ("CLUSIA_CASE".to_string(), case.to_string()),
    ];
    env.extend(values.iter().map(|(name, value)| {
        (
            format!("CLUSIA_ARG_{}", name.to_ascii_uppercase()),
            value.clone(),
        )
    }));
    env
}

/// The components of a relative path, when none of them is empty, `.` or `..`.
fn plain_parts(path: &str) -> Option<Vec<&str>> {
    let parts: Vec<&str> = path.split('/').collect();
    parts
        .iter()
        .all(|p| !p.is_empty() && *p != "." && *p != ".." && !p.contains('\0'))
        .then_some(parts)
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

/// The worktree itself. Its own path is Clúsia's, so only what lies under it is opened
/// without following links.
fn open_root(worktree: &Path) -> io::Result<OwnedFd> {
    let path = c_string(worktree.as_os_str().as_bytes())?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    // SAFETY: `path` is a valid C string that `open` only reads.
    descriptor(unsafe { libc::open(path.as_ptr(), flags) })
}

/// The folder `name` in `dir`, never through a link. `None` when it is missing and `create`
/// is false; with `create`, a missing folder is made first.
fn open_dir_at(dir: &OwnedFd, name: &str, create: bool) -> io::Result<Option<OwnedFd>> {
    let c_name = c_string(name.as_bytes())?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let open = || {
        // SAFETY: `dir` is an open descriptor and `c_name` a valid C string.
        descriptor(unsafe { libc::openat(dir.as_raw_fd(), c_name.as_ptr(), flags) })
    };
    match open() {
        Ok(fd) => Ok(Some(fd)),
        Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
            // SAFETY: `dir` is an open descriptor and `c_name` a valid C string.
            if unsafe { libc::mkdirat(dir.as_raw_fd(), c_name.as_ptr(), 0o755) } != 0 {
                let e = io::Error::last_os_error();
                // Another writer may have made it; opening it below still refuses a link.
                if e.kind() != io::ErrorKind::AlreadyExists {
                    return Err(e);
                }
            }
            open().map(Some)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The file `name` in `dir`, never through a link.
fn open_file_at(dir: &OwnedFd, name: &str, flags: i32, mode: libc::c_uint) -> io::Result<File> {
    let name = c_string(name.as_bytes())?;
    let flags = flags | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `dir` is an open descriptor and `name` a valid C string that `openat` only reads.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, mode) };
    descriptor(fd).map(File::from)
}

/// Removes the entry `name` of `dir` itself; a link is removed, never what it points to.
fn unlink_at(dir: &OwnedFd, name: &str) -> io::Result<()> {
    let name = c_string(name.as_bytes())?;
    // SAFETY: `dir` is an open descriptor and `name` a valid C string.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// An error that says the entry is a link or not a folder, not that it is missing.
fn not_plain(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ELOOP | libc::ENOTDIR | libc::EMLINK)
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use clusia_core::functions::{CaseStatus, Expect, StateCheck};

    use super::*;
    use crate::exec::Ended;

    const MARK: &str = "0123456789abcdef";

    fn expect(returned: Option<&str>, state: &[(&str, &str)]) -> Expect {
        Expect {
            returned: returned.map(String::from),
            state: state
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn function_commands_take_the_names_clusia_made() {
        let rust =
            "cargo test --offline -p {package} --test {driver} -- --exact clusia_case --nocapture";
        assert_eq!(
            function_command(rust, "clusia_driver_0123456789", Some("widgets")).unwrap(),
            "cargo test --offline -p 'widgets' --test 'clusia_driver_0123456789' -- --exact clusia_case --nocapture"
        );
        assert_eq!(
            function_command(
                "python3 {driver}",
                ".clusia/functions/refresh-0123456789/driver.py",
                None
            )
            .unwrap(),
            "python3 '.clusia/functions/refresh-0123456789/driver.py'"
        );
        assert_eq!(
            function_command("node {driver} {driver}", "{package}", Some("x")).unwrap(),
            "node '{package}' '{package}'",
            "a replaced value is never read again"
        );
        assert_eq!(
            function_command(rust, "clusia_driver_0123456789", None).unwrap_err(),
            "The command needs a package"
        );
        assert_eq!(
            function_command("node {driver}", "-rf", None).unwrap_err(),
            "Clúsia won't run a driver named -rf"
        );
    }

    #[test]
    fn the_package_is_the_nearest_cargo_toml_with_a_package() {
        let wt = tempfile::tempdir().unwrap();
        fs::write(
            wt.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        fs::create_dir_all(wt.path().join("crates/store/src/http")).unwrap();
        fs::write(
            wt.path().join("crates/store/Cargo.toml"),
            "[package]\nname = \"store\"\n",
        )
        .unwrap();
        assert_eq!(
            package_for(wt.path(), "crates/store/src/http/retry.rs"),
            Ok(("crates/store".to_string(), "store".to_string()))
        );
        fs::create_dir_all(wt.path().join("tools")).unwrap();
        assert_eq!(
            package_for(wt.path(), "tools/gen.rs"),
            Err(NO_PACKAGE.to_string())
        );
        assert_eq!(
            package_for(wt.path(), "crates/store/src/new/deep/file.rs"),
            Ok(("crates/store".to_string(), "store".to_string())),
            "a folder that is not there yet ends the descent"
        );

        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"widgets\"\n",
        )
        .unwrap();
        assert_eq!(
            package_for(root.path(), "src/store.rs"),
            Ok((String::new(), "widgets".to_string()))
        );
        for bad in ["../x.rs", "/etc/x.rs", "src/../../x.rs", "./src/store.rs"] {
            assert_eq!(
                package_for(root.path(), bad),
                Err(NO_PACKAGE.to_string()),
                "{bad}"
            );
        }
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"wid gets\"\n",
        )
        .unwrap();
        assert_eq!(
            package_for(root.path(), "src/store.rs"),
            Err(NO_PACKAGE.to_string())
        );
        fs::write(root.path().join("Cargo.toml"), vec![b'#'; 64 * 1024 + 1]).unwrap();
        assert_eq!(
            package_for(root.path(), "src/store.rs"),
            Err(NO_PACKAGE.to_string())
        );
    }

    #[test]
    fn a_linked_folder_or_cargo_toml_holds_no_package() {
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(outside.path().join("src")).unwrap();
        fs::write(
            outside.path().join("Cargo.toml"),
            "[package]\nname = \"elsewhere\"\n",
        )
        .unwrap();

        let wt = tempfile::tempdir().unwrap();
        fs::create_dir(wt.path().join("crates")).unwrap();
        symlink(outside.path(), wt.path().join("crates/store")).unwrap();
        assert_eq!(
            package_for(wt.path(), "crates/store/src/lib.rs"),
            Err(NO_PACKAGE.to_string())
        );

        let wt = tempfile::tempdir().unwrap();
        fs::create_dir(wt.path().join("src")).unwrap();
        symlink(
            outside.path().join("Cargo.toml"),
            wt.path().join("Cargo.toml"),
        )
        .unwrap();
        assert_eq!(
            package_for(wt.path(), "src/lib.rs"),
            Err(NO_PACKAGE.to_string())
        );
    }

    #[test]
    fn the_driver_target_comes_from_the_slug() {
        assert_eq!(
            driver_target("tokenstore_refresh-0123456789"),
            "clusia_driver_0123456789"
        );
        let odd = driver_target("../x");
        assert!(odd.starts_with("clusia_driver_") && odd.len() == "clusia_driver_".len() + 10);
        assert!(
            odd["clusia_driver_".len()..]
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
    }

    #[test]
    fn the_driver_is_placed_and_removed_through_plain_folders() {
        let wt = tempfile::tempdir().unwrap();
        fs::create_dir_all(wt.path().join("crates/store")).unwrap();
        let target = "clusia_driver_0123456789";
        let placed = place_driver(
            wt.path(),
            "crates/store",
            target,
            b"#[test]\nfn clusia_case() {}\n",
            false,
        )
        .unwrap();
        assert_eq!(placed, "crates/store/tests/clusia_driver_0123456789.rs");
        assert_eq!(
            fs::read(wt.path().join(&placed)).unwrap(),
            b"#[test]\nfn clusia_case() {}\n"
        );
        // A leftover of an earlier run is replaced.
        let again = place_driver(wt.path(), "crates/store", target, b"// second\n", true).unwrap();
        assert_eq!(fs::read(wt.path().join(&again)).unwrap(), b"// second\n");
        remove_driver(wt.path(), &placed).unwrap();
        assert!(!wt.path().join(&placed).exists());
        remove_driver(wt.path(), &placed).unwrap();
        assert!(
            wt.path().join("crates/store/tests").is_dir(),
            "the folder stays"
        );

        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            place_driver(root.path(), "", target, b"x", false).unwrap(),
            "tests/clusia_driver_0123456789.rs"
        );
        for bad in [
            "clusia_driver_01234",
            "../evil",
            "clusia_driver_0123456789/x",
        ] {
            assert!(
                place_driver(root.path(), "", bad, b"x", false).is_err(),
                "{bad}"
            );
        }
        assert!(place_driver(root.path(), "../x", target, b"x", false).is_err());
        for bad in [
            "src/main.rs",
            "tests/other.rs",
            "../tests/clusia_driver_0123456789.rs",
        ] {
            assert_eq!(
                remove_driver(root.path(), bad).unwrap_err().kind(),
                std::io::ErrorKind::InvalidInput,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_file_clusia_did_not_place_is_never_replaced() {
        let wt = tempfile::tempdir().unwrap();
        let target = "clusia_driver_0123456789";
        let file = wt.path().join("tests/clusia_driver_0123456789.rs");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "// tracked\n").unwrap();
        assert_eq!(
            place_driver(wt.path(), "", target, b"x", false).unwrap_err(),
            "Clúsia won't place the driver: tests/clusia_driver_0123456789.rs already exists"
        );
        assert_eq!(fs::read(&file).unwrap(), b"// tracked\n");
        let outside = tempfile::tempdir().unwrap();
        fs::remove_file(&file).unwrap();
        symlink(outside.path().join("x.rs"), &file).unwrap();
        assert!(place_driver(wt.path(), "", target, b"x", false).is_err());
        assert!(
            fs::symlink_metadata(&file)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!outside.path().join("x.rs").exists());
    }

    #[test]
    fn a_symlinked_tests_folder_refuses_the_rust_driver() {
        let outside = tempfile::tempdir().unwrap();
        let wt = tempfile::tempdir().unwrap();
        fs::create_dir_all(wt.path().join("crates/store")).unwrap();
        symlink(outside.path(), wt.path().join("crates/store/tests")).unwrap();
        let target = "clusia_driver_0123456789";
        assert_eq!(
            place_driver(wt.path(), "crates/store", target, b"x", false).unwrap_err(),
            "Clúsia won't place the driver: crates/store/tests is not a plain folder"
        );
        let linked = tempfile::tempdir().unwrap();
        symlink(outside.path(), linked.path().join("crates")).unwrap();
        assert_eq!(
            place_driver(linked.path(), "crates/store", target, b"x", false).unwrap_err(),
            "Clúsia won't place the driver: crates is not a plain folder"
        );
        let filed = tempfile::tempdir().unwrap();
        fs::write(filed.path().join("tests"), "x").unwrap();
        assert_eq!(
            place_driver(filed.path(), "", target, b"x", false).unwrap_err(),
            "Clúsia won't place the driver: tests is not a plain folder"
        );
        assert_eq!(
            fs::read_dir(outside.path()).unwrap().count(),
            0,
            "nothing written through a link"
        );

        // A removal never follows a link either.
        fs::write(outside.path().join("clusia_driver_0123456789.rs"), "keep").unwrap();
        assert!(
            remove_driver(wt.path(), "crates/store/tests/clusia_driver_0123456789.rs").is_err()
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("clusia_driver_0123456789.rs")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn the_report_reads_only_whole_marked_lines() {
        let stdout = format!(
            "compiling\n{MARK} returned Ok(1)\n{MARK} state token old\n{MARK} state token new\n{MARK} state cache \n\
             {MARK} returned Ok(2)\nffffffffffffffff returned other\n{MARK}returned glued\n{MARK} took 12\n\
             {MARK} took soon\n{MARK} error boom\r\n{MARK} done\n{MARK} state late"
        );
        let report = read_report(MARK, &stdout);
        assert_eq!(
            report,
            DriverReport {
                returned: Some("Ok(2)".into()),
                error: Some("boom".into()),
                state: vec![
                    ("token".into(), "new".into()),
                    ("cache".into(), String::new())
                ],
                took_ms: Some(12),
                done: true,
            }
        );
        let many: String = (0..30).map(|n| format!("{MARK} state s{n} v\n")).collect();
        assert_eq!(read_report(MARK, &many).state.len(), 20);
        let long = format!("{MARK} returned {}\n", "é".repeat(5000));
        assert_eq!(
            read_report(MARK, &long).returned.unwrap().chars().count(),
            4096
        );
    }

    #[test]
    fn the_status_takes_the_first_rule_that_applies() {
        let done =
            |returned: Option<&str>, state: &[(&str, &str)], error: Option<&str>| DriverReport {
                returned: returned.map(String::from),
                error: error.map(String::from),
                state: state
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                took_ms: Some(5),
                done: true,
            };
        let exit0 = Ended::Exited(Some(0));
        let want = expect(Some("Err(Http(500))"), &[("token", "old")]);

        assert_eq!(
            case_status(&done(None, &[], None), &Ended::Stopped, None, &want).0,
            CaseStatus::Stopped
        );
        assert_eq!(
            case_status(&done(None, &[], None), &Ended::TimedOut, None, &want).0,
            CaseStatus::TimedOut
        );
        assert_eq!(
            case_status(
                &DriverReport::default(),
                &Ended::Exited(Some(101)),
                Some(101),
                &want
            ),
            (
                CaseStatus::Error,
                vec![StateCheck {
                    name: "token".into(),
                    got: None,
                    expected: Some("old".into())
                }],
                Some("The driver ended without a result (exit 101)".into())
            )
        );
        assert_eq!(
            case_status(
                &DriverReport::default(),
                &Ended::Exited(None),
                None,
                &Expect::default()
            )
            .2,
            Some("The driver ended without a result (no exit code)".into())
        );
        let (status, _, message) = case_status(
            &done(Some("Err(Http(500))"), &[("token", "old")], None),
            &Ended::Exited(Some(3)),
            Some(3),
            &want,
        );
        assert_eq!(
            (status, message.as_deref()),
            (CaseStatus::Failed, Some("The driver exited with 3"))
        );
        assert_eq!(
            case_status(
                &done(
                    Some("Err(Http(500))"),
                    &[("token", "old")],
                    Some("panicked")
                ),
                &exit0,
                Some(0),
                &want
            )
            .0,
            CaseStatus::Failed
        );
        let (status, state, message) = case_status(
            &done(
                Some(" Err(Http(500)) "),
                &[("token", ""), ("calls", "1")],
                None,
            ),
            &exit0,
            Some(0),
            &want,
        );
        assert_eq!(status, CaseStatus::Failed);
        assert_eq!(message, None);
        assert_eq!(
            state,
            vec![
                StateCheck {
                    name: "token".into(),
                    got: Some(String::new()),
                    expected: Some("old".into())
                },
                StateCheck {
                    name: "calls".into(),
                    got: Some("1".into()),
                    expected: None
                },
            ]
        );
        assert_eq!(
            case_status(
                &done(Some("Err(Http(500))\n"), &[("token", "old")], None),
                &exit0,
                Some(0),
                &want
            )
            .0,
            CaseStatus::Passed
        );
        assert_eq!(
            case_status(
                &done(None, &[("token", "old")], None),
                &exit0,
                Some(0),
                &want
            )
            .0,
            CaseStatus::Failed
        );
        assert_eq!(
            case_status(
                &done(Some("anything"), &[], None),
                &exit0,
                Some(0),
                &Expect::default()
            )
            .0,
            CaseStatus::Passed,
            "nothing expected: a call that returns passes"
        );
    }

    #[test]
    fn the_case_env_names_each_value() {
        let values = [("stored_token", "old"), ("endpoint_status", "500")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(
            case_env(MARK, "/work", "a1", &values),
            [
                ("CLUSIA_MARK", MARK),
                ("CLUSIA_ROOT", "/work"),
                ("CLUSIA_CASE", "a1"),
                ("CLUSIA_ARG_ENDPOINT_STATUS", "500"),
                ("CLUSIA_ARG_STORED_TOKEN", "old"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string()))
        );
    }
}

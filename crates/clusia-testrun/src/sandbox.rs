//! The local runner's `sandbox-exec` profile, the environment a run starts from and the checks
//! around them.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use clusia_core::printable::printable;
use clusia_core::tests::Ecosystem;

use crate::eco::eco_env;
use crate::exec::{CommandSpec, Exec};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxRoots {
    pub worktree: PathBuf,
    pub git_common: PathBuf,
    pub review_tmp: PathBuf,
    pub repo_cache: PathBuf,
    pub data_root: PathBuf,
    pub home: PathBuf,
    pub clone_env: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetMode {
    Offline,
    Online,
}

pub const DENY_HOME: [&str; 18] = [
    ".ssh",
    ".aws",
    ".config/gh",
    ".netrc",
    ".docker/config.json",
    ".kube",
    ".gnupg",
    "Library/Keychains",
    ".git-credentials",
    ".config/git/credentials",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".pypirc",
    ".config/gcloud",
    ".azure",
    ".claude",
    ".claude.json",
    ".codex",
];

pub const HEALTH_PROFILE: &str = "(version 1)(allow default)";
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
pub const ENV_DEPTH: usize = 4;

/// The `DENY_HOME` entries that are files; the rest are folders.
const DENY_FILES: [&str; 8] = [
    ".netrc",
    ".docker/config.json",
    ".git-credentials",
    ".config/git/credentials",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".pypirc",
    ".claude.json",
];

/// Folders a `.env` search never enters: tool output and dependencies, not the project's own.
const ENV_SKIP: [&str; 3] = [".git", "node_modules", "target"];

/// Nothing a run starts may reach LaunchServices, AppleEvents, the pasteboard or launchd, which
/// would act outside its process tree and its sandbox. `osascript -e 'return 1'` sends no
/// AppleEvent, so the two launchers are also refused at exec.
const NO_LAUNCH: &str = "(deny appleevent-send)\n\
                         (deny mach-lookup\n  \
                         (global-name \"com.apple.coreservices.launchservicesd\")\n  \
                         (global-name-regex #\"^com\\.apple\\.lsd\\.\"))\n\
                         (deny mach-lookup\n  (global-name \"com.apple.pasteboard.1\"))\n\
                         (deny mach-lookup\n  (global-name \"com.apple.launchd.peer\"))\n\
                         (deny process-exec\n  (literal \"/usr/bin/osascript\")\n  \
                         (literal \"/usr/bin/open\"))\n";

/// The device nodes a shell and its tools write to. `/dev/tty` is the run's own terminal only;
/// a pattern over `/dev/tty*` would reach the user's other terminals.
const DEV_WRITES: &str = "  (literal \"/dev/null\")\n  (literal \"/dev/dtracehelper\")\n  \
                          (literal \"/dev/tty\")\n  (regex #\"^/dev/fd/\"))\n";

/// Outbound network that reaches no loopback address and no local socket but the resolver's.
/// The `ip4`/`ip6` forms matter: `(remote ip "localhost:*")` is accepted and blocks nothing.
const ONLINE: &str = "(allow network-outbound\n  (remote ip4 \"*:*\")\n  (remote ip6 \"*:*\"))\n\
                      (deny network-outbound\n  (remote ip4 \"localhost:*\")\n  \
                      (remote ip6 \"localhost:*\"))\n\
                      (allow network-outbound\n  (remote unix-socket (path-literal \
                      \"/private/var/run/mDNSResponder\")))\n";

/// The path as an SBPL string, or `None` when it can't be written as one faithfully.
fn sb_string(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    if text.chars().any(char::is_control) {
        return None;
    }
    Some(format!(
        "\"{}\"",
        text.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// One rule line. Roots are checked by `resolve_roots`, so only a deny entry can fail
/// `sb_string`, and it falls back to its nearest clean ancestor so the rule never drops out.
fn rule(kind: &str, path: &Path) -> String {
    match sb_string(path) {
        Some(s) => format!("  ({kind} {s})\n"),
        None => {
            let parent = path
                .ancestors()
                .skip(1)
                .find_map(sb_string)
                .unwrap_or_else(|| "\"/\"".into());
            format!("  (subpath {parent})\n")
        }
    }
}

/// Ends the last rule line of a block with the block's closing parenthesis.
fn close(lines: String) -> String {
    format!("{})\n", lines.trim_end_matches('\n'))
}

/// The real path of a root, refused when the profile could not name it faithfully.
fn real(path: &Path) -> io::Result<PathBuf> {
    let real = std::fs::canonicalize(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    if sb_string(&real).is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("a sandbox path must be UTF-8 without control characters: {real:?}"),
        ));
    }
    Ok(real)
}

/// Creates the review's temp folder and the repo cache, and returns every root at its real path:
/// the sandbox matches real paths only (`/tmp` and `/var` are symlinks into `/private`).
pub fn resolve_roots(roots: SandboxRoots) -> io::Result<SandboxRoots> {
    std::fs::create_dir_all(&roots.review_tmp)?;
    std::fs::create_dir_all(&roots.repo_cache)?;
    Ok(SandboxRoots {
        worktree: real(&roots.worktree)?,
        git_common: real(&roots.git_common)?,
        review_tmp: real(&roots.review_tmp)?,
        repo_cache: real(&roots.repo_cache)?,
        data_root: real(&roots.data_root)?,
        home: real(&roots.home)?,
        clone_env: roots
            .clone_env
            .iter()
            .filter_map(|p| std::fs::canonicalize(p).ok())
            .collect(),
    })
}

/// The profile text for `roots`, which must come from `resolve_roots`.
pub fn profile(roots: &SandboxRoots, mode: NetMode) -> String {
    let mut out = String::from("(version 1)\n(allow default)\n");
    out.push_str(NO_LAUNCH);
    out.push_str("(deny network*)\n");
    if mode == NetMode::Online {
        out.push_str(ONLINE);
    }
    out.push_str("(deny file-write*)\n(allow file-write*\n");
    for root in [&roots.worktree, &roots.review_tmp, &roots.repo_cache] {
        out.push_str(&rule("subpath", root));
    }
    out.push_str(DEV_WRITES);
    out.push_str("(deny file-write*\n");
    let clusia = rule("subpath", &roots.worktree.join(".clusia"));
    out.push_str(&close(
        clusia + &rule("literal", &roots.worktree.join(".git")),
    ));
    out.push_str("(deny file-read*\n");
    out.push_str(&close(rule("subpath", &roots.data_root)));
    // Metadata of the folders above the worktree, so `pwd -P`, `realpath` and git's discovery
    // can walk up through the denied data root.
    out.push_str("(allow file-read-metadata\n");
    let above = [
        roots.data_root.clone(),
        roots.data_root.join("worktrees"),
        roots.data_root.join("tests"),
    ];
    out.push_str(&close(above.iter().map(|p| rule("literal", p)).collect()));
    out.push_str("(allow file-read*\n");
    let readable = [
        &roots.worktree,
        &roots.git_common,
        &roots.review_tmp,
        &roots.repo_cache,
    ];
    out.push_str(&close(
        readable.iter().map(|p| rule("subpath", p)).collect(),
    ));
    let mut denied = String::new();
    for entry in DENY_HOME {
        let folder = !DENY_FILES.contains(&entry);
        let kind = if folder { "subpath" } else { "literal" };
        let path = roots.home.join(entry);
        denied.push_str(&rule(kind, &path));
        let Ok(real) = std::fs::canonicalize(&path) else {
            continue;
        };
        // The sandbox checks real paths, so a symlinked entry is denied where it leads too.
        if real != path {
            denied.push_str(&rule(kind, &real));
        }
        if folder && real.is_dir() {
            denied.push_str(&linked_children(&real));
        }
    }
    for file in &roots.clone_env {
        denied.push_str(&rule("literal", file));
    }
    out.push_str("(deny file-read*\n");
    out.push_str(&close(denied));
    out
}

/// One line per symlink directly inside a denied folder, at the link's real path, in file name
/// order. A link that does not resolve adds nothing.
fn linked_children(folder: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return String::new();
    };
    let mut links: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_symlink()))
        .map(|e| e.path())
        .collect();
    links.sort();
    links
        .iter()
        .filter_map(|link| std::fs::canonicalize(link).ok())
        .map(|real| rule(if real.is_dir() { "subpath" } else { "literal" }, &real))
        .collect()
}

/// The `.env` and `.env.*` files of a clone, down to `ENV_DEPTH`, with the real path of each
/// one that is a symlink. Symlinked folders are never entered.
pub fn clone_env_files(clone: &Path) -> Vec<PathBuf> {
    let clone = std::fs::canonicalize(clone).unwrap_or_else(|_| clone.to_path_buf());
    let mut out = Vec::new();
    walk(&clone, 1, &mut out);
    out.sort();
    out.dedup();
    out
}

/// `dir`'s entries are at `depth`.
fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if meta.is_dir() {
            if depth < ENV_DEPTH && !ENV_SKIP.contains(&name.as_ref()) {
                walk(&path, depth + 1, out);
            }
            continue;
        }
        let is_env = name == ".env" || name.starts_with(".env.");
        if !is_env || !(meta.is_file() || meta.is_symlink()) {
            continue;
        }
        out.push(path.clone());
        if meta.is_symlink()
            && let Ok(real) = std::fs::canonicalize(&path)
            && real.is_file()
        {
            out.push(real);
        }
    }
}

/// Whether a run under `roots` could write `path`, resolved through its nearest existing
/// ancestor. A `..` after that ancestor can't be resolved here and counts as writable, so a
/// caller that refuses writable paths stays on the safe side.
pub fn writable_by_sandbox(roots: &SandboxRoots, path: &Path) -> bool {
    let Some(base) = path.ancestors().find(|a| a.exists()) else {
        return true;
    };
    let Ok(mut real) = std::fs::canonicalize(base) else {
        return true;
    };
    let rest = path.strip_prefix(base).unwrap_or(Path::new(""));
    for part in rest.components() {
        match part {
            Component::Normal(name) => real.push(name),
            Component::CurDir => {}
            _ => return true,
        }
    }
    let within = [&roots.worktree, &roots.review_tmp, &roots.repo_cache]
        .iter()
        .any(|root| real.starts_with(root));
    within
        && !real.starts_with(roots.worktree.join(".clusia"))
        && real != roots.worktree.join(".git")
}

/// Checks that `sandbox_exec` can start a sandbox on this machine, with the message to show when
/// it can't.
pub async fn health(exec: &dyn Exec, sandbox_exec: &Path) -> Result<(), String> {
    const ADVICE: &str = "Install a container engine, or set tests.runner to container.";
    let missing = || {
        format!(
            "sandbox-exec is missing at {}. The local runner needs it; install a container \
             engine, or set tests.runner to container.",
            sandbox_exec.display()
        )
    };
    if !exec.executable(sandbox_exec) {
        return Err(missing());
    }
    let cmd = CommandSpec::new(sandbox_exec)
        .args(["-p", HEALTH_PROFILE, "/usr/bin/true"])
        .clear_env();
    let out = exec
        .output(&cmd, HEALTH_TIMEOUT)
        .await
        .map_err(|_| missing())?;
    if out.code == Some(0) && !out.timed_out {
        return Ok(());
    }
    let why = match (out.timed_out, out.code) {
        (true, _) => "timed out".to_string(),
        (false, Some(code)) => format!("exit {code}"),
        (false, None) => "killed by a signal".to_string(),
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    let why = match stderr.lines().map(str::trim).find(|l| !l.is_empty()) {
        Some(line) if !out.timed_out => format!("{why}: {}", printable(line)),
        _ => why,
    };
    Err(format!(
        "sandbox-exec can't start a sandbox here ({why}). {ADVICE}"
    ))
}

/// The keys `base_env` sets that a recipe may replace, in place.
const RECIPE_MAY_SET: [&str; 3] = ["LANG", "TERM", "CI"];

/// A run's whole environment, built from nothing so the daemon's own never leaks in: the base
/// pairs, the caches of `ecos` under the repo cache, then the recipe's `env`.
pub fn base_env(
    roots: &SandboxRoots,
    path: &OsStr,
    ecos: &[Ecosystem],
    recipe_env: &BTreeMap<String, String>,
) -> Vec<(OsString, OsString)> {
    let cargo_bin = roots.home.join(".cargo/bin");
    let mut path_value = path.to_os_string();
    if !std::env::split_paths(path).any(|p| p == cargo_bin) {
        if !path_value.is_empty() {
            path_value.push(":");
        }
        path_value.push(cargo_bin.as_os_str());
    }
    let mut tmpdir = roots.review_tmp.as_os_str().to_os_string();
    if !tmpdir.as_encoded_bytes().ends_with(b"/") {
        tmpdir.push("/");
    }
    let user = roots.home.file_name().unwrap_or_default();
    let mut env: Vec<(OsString, OsString)> = vec![
        ("PATH".into(), path_value),
        ("HOME".into(), roots.home.clone().into_os_string()),
        ("USER".into(), user.to_os_string()),
        ("LANG".into(), "en_US.UTF-8".into()),
        ("TERM".into(), "dumb".into()),
        ("CI".into(), "true".into()),
        ("TMPDIR".into(), tmpdir),
        ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
    ];
    let cache = roots.repo_cache.to_string_lossy();
    for eco in ecos {
        for (key, value) in eco_env(*eco, &cache) {
            if !env.iter().any(|(k, _)| *k == *key) {
                env.push((key.into(), value.into()));
            }
        }
    }
    for (key, value) in recipe_env {
        if let Some(pair) = env.iter_mut().find(|(k, _)| *k == **key) {
            if RECIPE_MAY_SET.contains(&key.as_str()) {
                pair.1 = value.into();
            }
            continue;
        }
        env.push((key.into(), value.into()));
    }
    env
}

/// `command` under `profile_file`, handed to `sh` as one argument, with exactly `env`.
pub fn local_command(
    sandbox_exec: &Path,
    profile_file: &Path,
    command: &str,
    env: Vec<(OsString, OsString)>,
    cwd: &Path,
) -> CommandSpec {
    let mut cmd = CommandSpec::new(sandbox_exec)
        .arg("-f")
        .arg(profile_file)
        .arg("/bin/sh")
        .arg("-c")
        .arg(command)
        .clear_env()
        .cwd(cwd);
    // The builder appends one pair at a time; assigning keeps the order exactly as given.
    cmd.env = env;
    cmd
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::testkit::{FakeExec, FakeReply};

    use super::*;

    fn roots() -> SandboxRoots {
        SandboxRoots {
            worktree: "/d/worktrees/acme~widgets~7".into(),
            git_common: "/c/widgets/.git".into(),
            review_tmp: "/d/tests/tmp/0123456789ab".into(),
            repo_cache: "/d/tests/cache/acme~widgets".into(),
            data_root: "/d".into(),
            home: "/h".into(),
            clone_env: vec!["/c/widgets/.env".into()],
        }
    }

    /// Rules 1 and 1a: everything before the network block.
    const PREFIX: &str = r##"(version 1)
(allow default)
(deny appleevent-send)
(deny mach-lookup
  (global-name "com.apple.coreservices.launchservicesd")
  (global-name-regex #"^com\.apple\.lsd\."))
(deny mach-lookup
  (global-name "com.apple.pasteboard.1"))
(deny mach-lookup
  (global-name "com.apple.launchd.peer"))
(deny process-exec
  (literal "/usr/bin/osascript")
  (literal "/usr/bin/open"))
(deny network*)
"##;

    const ONLINE_BLOCK: &str = r##"(allow network-outbound
  (remote ip4 "*:*")
  (remote ip6 "*:*"))
(deny network-outbound
  (remote ip4 "localhost:*")
  (remote ip6 "localhost:*"))
(allow network-outbound
  (remote unix-socket (path-literal "/private/var/run/mDNSResponder")))
"##;

    const FILE_RULES: &str = r##"(deny file-write*)
(allow file-write*
  (subpath "/d/worktrees/acme~widgets~7")
  (subpath "/d/tests/tmp/0123456789ab")
  (subpath "/d/tests/cache/acme~widgets")
  (literal "/dev/null")
  (literal "/dev/dtracehelper")
  (literal "/dev/tty")
  (regex #"^/dev/fd/"))
(deny file-write*
  (subpath "/d/worktrees/acme~widgets~7/.clusia")
  (literal "/d/worktrees/acme~widgets~7/.git"))
(deny file-read*
  (subpath "/d"))
(allow file-read-metadata
  (literal "/d")
  (literal "/d/worktrees")
  (literal "/d/tests"))
(allow file-read*
  (subpath "/d/worktrees/acme~widgets~7")
  (subpath "/c/widgets/.git")
  (subpath "/d/tests/tmp/0123456789ab")
  (subpath "/d/tests/cache/acme~widgets"))
(deny file-read*
  (subpath "/h/.ssh")
  (subpath "/h/.aws")
  (subpath "/h/.config/gh")
  (literal "/h/.netrc")
  (literal "/h/.docker/config.json")
  (subpath "/h/.kube")
  (subpath "/h/.gnupg")
  (subpath "/h/Library/Keychains")
  (literal "/h/.git-credentials")
  (literal "/h/.config/git/credentials")
  (literal "/h/.cargo/credentials")
  (literal "/h/.cargo/credentials.toml")
  (literal "/h/.pypirc")
  (subpath "/h/.config/gcloud")
  (subpath "/h/.azure")
  (subpath "/h/.claude")
  (literal "/h/.claude.json")
  (subpath "/h/.codex")
  (literal "/c/widgets/.env"))
"##;

    #[test]
    fn the_offline_profile_is_the_rules_in_order() {
        assert_eq!(
            profile(&roots(), NetMode::Offline),
            format!("{PREFIX}{FILE_RULES}")
        );
    }

    #[test]
    fn the_online_profile_differs_only_in_its_network_block() {
        let online = profile(&roots(), NetMode::Online);
        assert_eq!(online, format!("{PREFIX}{ONLINE_BLOCK}{FILE_RULES}"));
        assert!(
            online.starts_with(PREFIX),
            "launch and pasteboard rules hold online too"
        );
        assert!(
            online.ends_with(FILE_RULES),
            "the deny-list block is identical in both"
        );
        assert!(
            !online.contains("(remote ip \""),
            "the form that does not block loopback"
        );
        assert!(
            !online.contains(r##"#"^/dev/tty"##),
            "no regex over the other terminals"
        );
    }

    #[test]
    fn quotes_and_backslashes_in_paths_are_escaped() {
        let mut roots = roots();
        roots.worktree = PathBuf::from(r#"/d/we"ird\wt"#);
        let text = profile(&roots, NetMode::Offline);
        assert!(text.contains(r#"(subpath "/d/we\"ird\\wt")"#), "{text}");
        assert!(
            text.contains(r#"(subpath "/d/we\"ird\\wt/.clusia")"#),
            "{text}"
        );
    }

    #[test]
    fn an_env_file_with_a_line_break_denies_its_folder_instead() {
        let mut roots = roots();
        roots.clone_env = vec![PathBuf::from("/c/widgets/con\nf/.env")];
        let text = profile(&roots, NetMode::Offline);
        assert!(!text.contains("con\nf"), "{text}");
        assert!(text.ends_with("  (subpath \"/c/widgets\"))\n"), "{text}");
    }

    #[test]
    fn a_symlinked_secret_folder_is_denied_at_both_paths() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().canonicalize().unwrap();
        let real = top.join("elsewhere/aws");
        std::fs::create_dir_all(&real).unwrap();
        let key = top.join("keys/id_ed25519");
        std::fs::create_dir_all(key.parent().unwrap()).unwrap();
        std::fs::write(&key, "key").unwrap();
        let home = top.join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::os::unix::fs::symlink(&real, home.join(".aws")).unwrap();
        std::os::unix::fs::symlink(&key, home.join(".ssh/id_ed25519")).unwrap();
        std::fs::write(home.join(".ssh/known_hosts"), "plain file, no extra line").unwrap();
        let mut roots = roots();
        roots.home = home.clone();
        let text = profile(&roots, NetMode::Offline);
        let follows = |first: String, second: String| {
            let at = text
                .find(&first)
                .unwrap_or_else(|| panic!("{first} in {text}"));
            assert_eq!(
                &text[at + first.len()..at + first.len() + second.len()],
                second
            );
        };
        follows(
            format!("  (subpath \"{}\")\n", home.join(".aws").display()),
            format!("  (subpath \"{}\")\n", real.display()),
        );
        // X34: a symlink inside a denied folder is denied at its target.
        follows(
            format!("  (subpath \"{}\")\n", home.join(".ssh").display()),
            format!("  (literal \"{}\")\n", key.display()),
        );
        assert!(!text.contains("known_hosts"));
    }

    #[test]
    fn resolve_roots_makes_tmp_and_cache_and_returns_real_paths() {
        let dir = tempfile::tempdir().unwrap();
        // A tempdir sits under /var, which is a symlink into /private.
        let top = dir.path().to_path_buf();
        for d in ["data/worktrees/w", "clone/.git", "home"] {
            std::fs::create_dir_all(top.join(d)).unwrap();
        }
        std::fs::write(top.join("clone/.env"), "A=1").unwrap();
        let resolved = resolve_roots(SandboxRoots {
            worktree: top.join("data/worktrees/w"),
            git_common: top.join("clone/.git"),
            review_tmp: top.join("data/tests/tmp/abc"),
            repo_cache: top.join("data/tests/cache/acme~widgets"),
            data_root: top.join("data"),
            home: top.join("home"),
            clone_env: vec![top.join("clone/.env"), top.join("clone/.env.gone")],
        })
        .unwrap();
        let real = |p: &str| top.join(p).canonicalize().unwrap();
        assert_eq!(resolved.worktree, real("data/worktrees/w"));
        assert_eq!(resolved.git_common, real("clone/.git"));
        assert_eq!(resolved.review_tmp, real("data/tests/tmp/abc"));
        assert_eq!(resolved.repo_cache, real("data/tests/cache/acme~widgets"));
        assert_eq!(resolved.data_root, real("data"));
        assert_eq!(resolved.home, real("home"));
        assert!(resolved.review_tmp.is_dir() && resolved.repo_cache.is_dir());
        assert_eq!(resolved.clone_env, [real("clone/.env")]);
    }

    #[test]
    fn resolve_roots_refuses_a_missing_worktree_or_a_control_character() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().to_path_buf();
        std::fs::create_dir_all(top.join("home")).unwrap();
        std::fs::create_dir_all(top.join("bad\nname")).unwrap();
        let base = SandboxRoots {
            worktree: top.join("nowhere"),
            git_common: top.join("home"),
            review_tmp: top.join("tmp"),
            repo_cache: top.join("cache"),
            data_root: top.clone(),
            home: top.join("home"),
            clone_env: vec![],
        };
        let err = resolve_roots(base.clone()).unwrap_err();
        assert!(err.to_string().contains("nowhere"), "{err}");
        let err = resolve_roots(SandboxRoots {
            worktree: top.join("bad\nname"),
            ..base
        })
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn clone_env_files_go_four_levels_deep_and_skip_tool_folders() {
        let dir = tempfile::tempdir().unwrap();
        let clone = dir.path().canonicalize().unwrap().join("clone");
        let outside = clone.parent().unwrap().join("outside.env");
        std::fs::write(&outside, "B=2").unwrap();
        for file in [
            ".env",
            ".env.local",
            "web/.env.production",
            "a/b/c/.env",
            "a/b/c/d/.env",
            "node_modules/pkg/.env",
            ".git/.env",
            "target/.env",
            ".envrc",
            "env",
            "web/.environment",
        ] {
            let path = clone.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "A=1").unwrap();
        }
        std::os::unix::fs::symlink(&outside, clone.join(".env.linked")).unwrap();
        std::os::unix::fs::symlink(clone.join("a"), clone.join("loop")).unwrap();
        let found = clone_env_files(&clone);
        let mut expected = vec![
            clone.join(".env"),
            clone.join(".env.linked"),
            clone.join(".env.local"),
            clone.join("a/b/c/.env"),
            clone.join("web/.env.production"),
            outside,
        ];
        expected.sort();
        assert_eq!(found, expected);
    }

    #[test]
    fn writable_by_sandbox_follows_the_write_rules() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().canonicalize().unwrap();
        for d in [
            "data/worktrees/w/.clusia",
            "data/tests/profiles",
            "clone/.git",
            "home",
        ] {
            std::fs::create_dir_all(top.join(d)).unwrap();
        }
        let roots = resolve_roots(SandboxRoots {
            worktree: top.join("data/worktrees/w"),
            git_common: top.join("clone/.git"),
            review_tmp: top.join("data/tests/tmp/abc"),
            repo_cache: top.join("data/tests/cache/acme~widgets"),
            data_root: top.join("data"),
            home: top.join("home"),
            clone_env: vec![],
        })
        .unwrap();
        assert!(writable_by_sandbox(
            &roots,
            &top.join("data/worktrees/w/src/new.rs")
        ));
        assert!(writable_by_sandbox(
            &roots,
            &top.join("data/tests/tmp/abc/profile.sb")
        ));
        assert!(writable_by_sandbox(
            &roots,
            &top.join("data/tests/cache/acme~widgets/x")
        ));
        assert!(!writable_by_sandbox(
            &roots,
            &top.join("data/worktrees/w/.clusia/recipe.toml")
        ));
        assert!(!writable_by_sandbox(
            &roots,
            &top.join("data/worktrees/w/.git")
        ));
        assert!(!writable_by_sandbox(
            &roots,
            &top.join("data/tests/profiles/abc-offline.sb")
        ));
        assert!(!writable_by_sandbox(&roots, &top.join("clone/.git/HEAD")));
        assert!(!writable_by_sandbox(&roots, &top.join("home/x")));
    }

    #[test]
    fn base_env_starts_from_nothing() {
        let recipe_env = BTreeMap::from([
            ("CARGO_HOME".to_string(), "/evil-cargo".to_string()),
            ("CI".to_string(), "false".to_string()),
            ("LANG".to_string(), "C".to_string()),
            ("PATH".to_string(), "/evil".to_string()),
            ("RUST_BACKTRACE".to_string(), "1".to_string()),
            ("TMPDIR".to_string(), "/x".to_string()),
        ]);
        let env = base_env(
            &roots(),
            OsStr::new("/usr/bin:/bin"),
            &[Ecosystem::Cargo, Ecosystem::Node],
            &recipe_env,
        );
        let env: Vec<(String, String)> = env
            .into_iter()
            .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
            .collect();
        let cache = "/d/tests/cache/acme~widgets";
        let expected: Vec<(String, String)> = [
            ("PATH", "/usr/bin:/bin:/h/.cargo/bin".to_string()),
            ("HOME", "/h".to_string()),
            ("USER", "h".to_string()),
            // X10: the recipe's LANG and CI replace the base pairs in their place.
            ("LANG", "C".to_string()),
            ("TERM", "dumb".to_string()),
            ("CI", "false".to_string()),
            ("TMPDIR", "/d/tests/tmp/0123456789ab/".to_string()),
            ("GIT_OPTIONAL_LOCKS", "0".to_string()),
            ("CARGO_HOME", format!("{cache}/cargo/home")),
            ("CARGO_TARGET_DIR", format!("{cache}/cargo/target")),
            ("npm_config_cache", format!("{cache}/node/npm")),
            ("YARN_CACHE_FOLDER", format!("{cache}/node/yarn")),
            ("npm_config_store_dir", format!("{cache}/node/pnpm-store")),
            ("RUST_BACKTRACE", "1".to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert_eq!(env, expected);
    }

    #[test]
    fn base_env_does_not_add_the_cargo_folder_twice() {
        let env = base_env(
            &roots(),
            OsStr::new("/h/.cargo/bin:/usr/bin"),
            &[],
            &BTreeMap::new(),
        );
        assert_eq!(env[0], ("PATH".into(), "/h/.cargo/bin:/usr/bin".into()));
        let env = base_env(&roots(), OsStr::new(""), &[], &BTreeMap::new());
        assert_eq!(env[0], ("PATH".into(), "/h/.cargo/bin".into()));
    }

    #[test]
    fn local_command_hands_the_command_to_sh_as_one_argument() {
        let command = "cargo test -- --exact 'a b'; echo $HOME";
        let env = vec![(OsString::from("HOME"), OsString::from("/h"))];
        let cmd = local_command(
            Path::new("/usr/bin/sandbox-exec"),
            Path::new("/d/tests/profiles/abc-offline.sb"),
            command,
            env.clone(),
            Path::new("/d/worktrees/acme~widgets~7"),
        );
        assert_eq!(cmd.program, PathBuf::from("/usr/bin/sandbox-exec"));
        assert_eq!(
            cmd.args,
            [
                "-f",
                "/d/tests/profiles/abc-offline.sb",
                "/bin/sh",
                "-c",
                command
            ]
            .map(OsString::from)
        );
        assert!(cmd.clear_env);
        assert_eq!(cmd.env, env);
        assert_eq!(cmd.cwd, Some(PathBuf::from("/d/worktrees/acme~widgets~7")));
    }

    #[tokio::test]
    async fn health_runs_the_minimal_profile() {
        let fake = FakeExec::new();
        fake.executable_at("/usr/bin/sandbox-exec")
            .on("sandbox-exec", &[], FakeReply::ok(""));
        assert_eq!(
            health(&fake, Path::new("/usr/bin/sandbox-exec")).await,
            Ok(())
        );
        let call = fake.calls().remove(0);
        assert_eq!(
            call.args,
            ["-p", "(version 1)(allow default)", "/usr/bin/true"].map(OsString::from)
        );
        assert!(call.clear_env);
    }

    #[tokio::test(start_paused = true)]
    async fn a_broken_sandbox_refuses_with_the_reason() {
        let fake = FakeExec::new();
        fake.executable_at("/usr/bin/sandbox-exec").on(
            "sandbox-exec",
            &[],
            FakeReply::fail(
                71,
                "sandbox-exec: sandbox_apply: Operation not permitted\nmore\n",
            ),
        );
        assert_eq!(
            health(&fake, Path::new("/usr/bin/sandbox-exec")).await,
            Err(
                "sandbox-exec can't start a sandbox here (exit 71: sandbox-exec: sandbox_apply: \
                 Operation not permitted). Install a container engine, or set tests.runner to \
                 container."
                    .to_string()
            )
        );

        let missing = FakeExec::new();
        assert_eq!(
            health(&missing, Path::new("/usr/bin/sandbox-exec")).await,
            Err(
                "sandbox-exec is missing at /usr/bin/sandbox-exec. The local runner needs it; \
                 install a container engine, or set tests.runner to container."
                    .to_string()
            )
        );
        assert!(
            missing.calls().is_empty(),
            "nothing runs when the binary is not there"
        );

        let hangs = FakeExec::new();
        hangs
            .executable_at("/usr/bin/sandbox-exec")
            .on("sandbox-exec", &[], FakeReply::hang());
        let started = tokio::time::Instant::now();
        let err = health(&hangs, Path::new("/usr/bin/sandbox-exec"))
            .await
            .unwrap_err();
        assert!(
            err.starts_with("sandbox-exec can't start a sandbox here (timed out)."),
            "{err}"
        );
        assert_eq!(
            started.elapsed(),
            HEALTH_TIMEOUT,
            "the paused clock: no real wait"
        );
    }
}

//! What a checkout offers to a test run. Every path here comes from the pull request, so
//! symlinks are never followed or listed and a path that is not plain is skipped.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clusia_core::printable::plain_relative;
use clusia_core::tests::{Ecosystem, Engine, Existing, RepoSignals};

use crate::engine::{HostEnv, INFO_TIMEOUT, cli_env};
use crate::exec::{CommandSpec, Exec};
use crate::names::{LABEL_REPO, sha256_hex};

pub const SHIPPED_CLUSIA: &str =
    "This pull request ships its own .clusia/ folder; Clúsia won't use it.";

const LIST_MAX: usize = 50;
const PATH_MAX: usize = 300;
/// Folders below the root that are looked into.
const DEPTH_MAX: usize = 3;
const SKIP_DIRS: [&str; 7] = [
    ".git",
    "node_modules",
    "target",
    ".clusia",
    "vendor",
    ".venv",
    "dist",
];
const LOCKFILES: [&str; 12] = [
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "poetry.lock",
    "uv.lock",
    "Pipfile.lock",
    "requirements.txt",
    "go.sum",
    "Gemfile.lock",
    "gradle.lockfile",
];
const TOOLCHAINS: [&str; 8] = [
    "rust-toolchain.toml",
    "rust-toolchain",
    ".nvmrc",
    ".node-version",
    ".python-version",
    ".tool-versions",
    "go.mod",
    ".ruby-version",
];
const ECOSYSTEMS: [Ecosystem; 6] = [
    Ecosystem::Cargo,
    Ecosystem::Node,
    Ecosystem::Python,
    Ecosystem::Go,
    Ecosystem::Jvm,
    Ecosystem::Ruby,
];
const SIGNAL_FILE_MAX: u64 = 16 * 1024 * 1024;
const LOCAL_DIRS: [&str; 3] = ["node_modules", ".venv", "target"];
const HOST_TOOLS: [&str; 8] = [
    "cargo", "node", "npm", "python3", "go", "java", "ruby", "git",
];

fn is_dockerfile(name: &str) -> bool {
    name == "Dockerfile" || name.starts_with("Dockerfile.") || name.ends_with(".Dockerfile")
}

fn is_compose(name: &str) -> bool {
    let rest = name
        .strip_prefix("docker-compose")
        .or_else(|| name.strip_prefix("compose"));
    let Some(rest) = rest else { return false };
    let Some(stem) = rest
        .strip_suffix(".yml")
        .or_else(|| rest.strip_suffix(".yaml"))
    else {
        return false;
    };
    // `compose.yml`, or `compose.<name>.yml`.
    stem.is_empty() || (stem.starts_with('.') && stem.len() > 1)
}

fn ecosystem_of(name: &str) -> Option<Ecosystem> {
    Some(match name {
        "Cargo.toml" => Ecosystem::Cargo,
        "package.json" => Ecosystem::Node,
        "pyproject.toml" | "requirements.txt" | "setup.py" | "Pipfile" => Ecosystem::Python,
        "go.mod" => Ecosystem::Go,
        "pom.xml" | "build.gradle" | "build.gradle.kts" | "settings.gradle" => Ecosystem::Jvm,
        "Gemfile" => Ecosystem::Ruby,
        _ => return None,
    })
}

#[derive(Default)]
struct Found {
    dockerfiles: BTreeSet<String>,
    compose_files: BTreeSet<String>,
    lockfiles: BTreeSet<String>,
    toolchains: BTreeSet<String>,
    ecosystems: Vec<Ecosystem>,
}

fn walk(root: &Path, rel: &str, depth: usize, found: &mut Found) {
    let dir = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let path = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if !plain_relative(&path, PATH_MAX) {
            continue;
        }
        if meta.is_dir() {
            if depth < DEPTH_MAX && !SKIP_DIRS.contains(&name.as_str()) {
                walk(root, &path, depth + 1, found);
            }
        } else if meta.is_file() {
            if is_dockerfile(&name) {
                found.dockerfiles.insert(path.clone());
            }
            if is_compose(&name) {
                found.compose_files.insert(path.clone());
            }
            if LOCKFILES.contains(&name.as_str()) {
                found.lockfiles.insert(path.clone());
            }
            if TOOLCHAINS.contains(&name.as_str()) {
                found.toolchains.insert(path.clone());
            }
            if let Some(eco) = ecosystem_of(&name)
                && !found.ecosystems.contains(&eco)
            {
                found.ecosystems.push(eco);
            }
        }
    }
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

/// Opens a regular file for reading, never blocking on a FIFO and never following a symlink
/// at the last component: a step that runs PR code may plant either at a path Clúsia reads.
/// The type is checked before the open and again on the open file, which closes the race of
/// a swap in between.
pub(crate) fn open_regular(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let not_a_file = || std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a file");
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(not_a_file());
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(not_a_file());
    }
    Ok(file)
}

fn capped(set: BTreeSet<String>) -> Vec<String> {
    set.into_iter().take(LIST_MAX).collect()
}

pub fn repo_signals(worktree: &Path) -> RepoSignals {
    let mut found = Found::default();
    walk(worktree, "", 0, &mut found);
    let devcontainer = [".devcontainer/devcontainer.json", ".devcontainer.json"]
        .into_iter()
        .find(|rel| is_regular_file(&worktree.join(rel)))
        .map(str::to_string);
    RepoSignals {
        dockerfiles: capped(found.dockerfiles),
        compose_files: capped(found.compose_files),
        devcontainer,
        lockfiles: capped(found.lockfiles),
        toolchains: capped(found.toolchains),
        ecosystems: ECOSYSTEMS
            .into_iter()
            .filter(|e| found.ecosystems.contains(e))
            .collect(),
    }
}

fn digest_of(worktree: &Path, rel: &str) -> String {
    let path = worktree.join(rel);
    if !plain_relative(rel, PATH_MAX) || !is_regular_file(&path) {
        return "missing".to_string();
    }
    let mut bytes = Vec::new();
    match open_regular(&path).and_then(|f| f.take(SIGNAL_FILE_MAX).read_to_end(&mut bytes)) {
        Ok(_) => sha256_hex(&bytes),
        Err(_) => "missing".to_string(),
    }
}

/// A digest of every signal file's path and content, to tell whether what a recipe was made from
/// has changed. A file that is gone hashes as `missing`.
pub fn inputs_hash(worktree: &Path, signals: &RepoSignals) -> String {
    let paths: BTreeSet<&String> = signals
        .dockerfiles
        .iter()
        .chain(&signals.compose_files)
        .chain(&signals.devcontainer)
        .chain(&signals.lockfiles)
        .chain(&signals.toolchains)
        .collect();
    let mut text = String::new();
    for rel in paths {
        text.push_str(&format!("{rel}\0{}\n", digest_of(worktree, rel)));
    }
    sha256_hex(text.as_bytes())
}

async fn engine_lines(
    exec: &dyn Exec,
    engine: &Engine,
    host: &HostEnv,
    args: Vec<OsString>,
    cwd: Option<&Path>,
) -> Vec<String> {
    let cli = Path::new(&engine.cli);
    let mut cmd = CommandSpec::new(cli).args(args);
    cmd.env = cli_env(cli, host);
    cmd.clear_env = true;
    cmd.cwd = cwd.map(Path::to_path_buf);
    match exec.output(&cmd, INFO_TIMEOUT * 5).await {
        Ok(out) if out.code == Some(0) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// What the reviewer's machine already has that a run could use. The engine is asked only when
/// it is running.
pub async fn existing(
    exec: &dyn Exec,
    engine: Option<&Engine>,
    host: &HostEnv,
    worktree: &Path,
    _clone: &Path,
    repo: &str,
) -> Existing {
    let (mut images, mut reviewer_services) = (Vec::new(), Vec::new());
    if let Some(engine) = engine.filter(|e| e.running) {
        let label = format!("label={LABEL_REPO}={repo}");
        images = engine_lines(
            exec,
            engine,
            host,
            [
                "image",
                "ls",
                "--filter",
                &label,
                "--format",
                "{{.Repository}}:{{.Tag}}",
            ]
            .map(OsString::from)
            .to_vec(),
            None,
        )
        .await;
        images.truncate(LIST_MAX);
        // Asks the engine for what is running, so no compose file is ever parsed.
        reviewer_services = engine_lines(
            exec,
            engine,
            host,
            [
                "ps",
                "--filter",
                "label=com.docker.compose.project",
                "--format",
                r#"{{.Label "com.docker.compose.service"}}"#,
            ]
            .map(OsString::from)
            .to_vec(),
            None,
        )
        .await;
        reviewer_services.sort();
        reviewer_services.dedup();
        reviewer_services.truncate(LIST_MAX);
    }
    Existing {
        images,
        reviewer_services,
        local_dirs: LOCAL_DIRS
            .iter()
            .filter(|d| fs::symlink_metadata(worktree.join(d)).is_ok_and(|m| m.is_dir()))
            .map(|d| d.to_string())
            .collect(),
        host_tools: host_tools(exec, host),
    }
}

fn host_tools(exec: &dyn Exec, host: &HostEnv) -> Vec<String> {
    let mut dirs: Vec<PathBuf> = host
        .path
        .as_ref()
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    dirs.push(host.home.join(".cargo/bin"));
    let mut found: Vec<String> = HOST_TOOLS
        .iter()
        .filter(|tool| dirs.iter().any(|d| exec.executable(&d.join(tool))))
        .map(|t| t.to_string())
        .collect();
    found.sort();
    found
}

/// Why this checkout cannot be used for a recipe, if it ships a `.clusia/` of its own: the
/// recipe and the markers belong to Clúsia. `gitdir` is the git folder the daemon verified, so a
/// worktree's own `.git` file (which a run could rewrite) cannot choose the repository, and the
/// `-c` flags keep the worktree's config from naming a program for this host-side call to run.
pub async fn clusia_dir_problem(
    exec: &dyn Exec,
    worktree: &Path,
    gitdir: Option<&Path>,
) -> Option<String> {
    let linked = |rel: &str| fs::symlink_metadata(worktree.join(rel)).is_ok_and(|m| m.is_symlink());
    if linked(".clusia") || linked(".clusia/recipe.toml") {
        return Some(SHIPPED_CLUSIA.to_string());
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    let mut cmd = CommandSpec::new("/usr/bin/git")
        .arg("-C")
        .arg(worktree)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(["ls-files", "-z", "--", ".clusia"])
        .cwd(worktree)
        .clear_env()
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin");
    if let Some(gitdir) = gitdir {
        cmd = cmd.env("GIT_DIR", gitdir);
    }
    let unknown = |why: String| {
        Some(format!(
            "Clúsia could not check whether this pull request ships a .clusia/ folder: {why}"
        ))
    };
    match exec.output(&cmd, Duration::from_secs(10)).await {
        Err(e) => unknown(e.to_string()),
        Ok(out) if out.code != Some(0) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            unknown(stderr.lines().next().unwrap_or("git failed").to_string())
        }
        Ok(out) if !out.stdout.is_empty() => Some(SHIPPED_CLUSIA.to_string()),
        Ok(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use clusia_core::tests::{Ecosystem, EngineKind};

    use super::*;
    use crate::engine::HostEnv;
    use crate::testkit::{FakeExec, FakeReply};

    fn put(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn running_engine() -> Engine {
        Engine {
            kind: EngineKind::Colima,
            cli: "/opt/homebrew/bin/docker".into(),
            endpoint: String::new(),
            context: None,
            running: true,
            start: None,
            compose: None,
        }
    }

    #[test]
    fn signals_are_found_sorted_and_the_noise_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (rel, text) in [
            ("Dockerfile", "FROM rust:1.86\n"),
            ("docker/Dockerfile.test", "FROM rust:1.86\n"),
            ("compose.yaml", "services: {}\n"),
            ("docker-compose.yml", "services: {}\n"),
            (".devcontainer/devcontainer.json", "{}\n"),
            ("Cargo.toml", "[package]\n"),
            ("Cargo.lock", "version = 3\n"),
            ("package.json", "{}\n"),
            ("package-lock.json", "{}\n"),
            ("rust-toolchain.toml", "[toolchain]\n"),
            (".nvmrc", "22\n"),
            ("node_modules/x/Dockerfile", "FROM scratch\n"),
            ("target/Dockerfile", "FROM scratch\n"),
            ("deep/a/b/c/Dockerfile", "FROM scratch\n"),
        ] {
            put(root, rel, text);
        }
        std::os::unix::fs::symlink(root.join("Dockerfile"), root.join("Dockerfile.link")).unwrap();

        let signals = repo_signals(root);
        assert_eq!(
            signals,
            RepoSignals {
                dockerfiles: vec!["Dockerfile".into(), "docker/Dockerfile.test".into()],
                compose_files: vec!["compose.yaml".into(), "docker-compose.yml".into()],
                devcontainer: Some(".devcontainer/devcontainer.json".into()),
                lockfiles: vec!["Cargo.lock".into(), "package-lock.json".into()],
                toolchains: vec![".nvmrc".into(), "rust-toolchain.toml".into()],
                ecosystems: vec![Ecosystem::Cargo, Ecosystem::Node],
            }
        );
    }

    #[test]
    fn manifests_name_the_ecosystems_in_a_fixed_order() {
        let dir = tempfile::tempdir().unwrap();
        for rel in [
            "Gemfile",
            "pom.xml",
            "go.mod",
            "pyproject.toml",
            "package.json",
            "Cargo.toml",
        ] {
            put(dir.path(), rel, "x");
        }
        assert_eq!(
            repo_signals(dir.path()).ecosystems,
            vec![
                Ecosystem::Cargo,
                Ecosystem::Node,
                Ecosystem::Python,
                Ecosystem::Go,
                Ecosystem::Jvm,
                Ecosystem::Ruby,
            ]
        );
        let other = tempfile::tempdir().unwrap();
        put(other.path(), "build.gradle.kts", "x");
        put(other.path(), "requirements.txt", "x");
        assert_eq!(
            repo_signals(other.path()).ecosystems,
            vec![Ecosystem::Python, Ecosystem::Jvm]
        );
    }

    #[test]
    fn a_devcontainer_at_the_root_is_found_and_a_symlinked_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), ".devcontainer.json", "{}");
        assert_eq!(
            repo_signals(dir.path()).devcontainer.as_deref(),
            Some(".devcontainer.json")
        );
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir(other.path().join(".devcontainer")).unwrap();
        std::os::unix::fs::symlink(
            "/etc/hosts",
            other.path().join(".devcontainer/devcontainer.json"),
        )
        .unwrap();
        assert_eq!(repo_signals(other.path()).devcontainer, None);
    }

    #[test]
    fn a_list_is_capped_and_odd_names_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..60 {
            put(
                dir.path(),
                &format!("svc{i:02}/Dockerfile"),
                "FROM scratch\n",
            );
        }
        put(dir.path(), "tab\there/Dockerfile", "FROM scratch\n");
        let signals = repo_signals(dir.path());
        assert_eq!(signals.dockerfiles.len(), 50);
        assert_eq!(signals.dockerfiles[0], "svc00/Dockerfile");
        assert!(signals.dockerfiles.iter().all(|p| !p.contains('\t')));
    }

    #[test]
    fn the_inputs_hash_is_over_sorted_paths_and_content_digests() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "Dockerfile", "FROM rust:1.86\n");
        put(dir.path(), "Cargo.lock", "version = 3\n");
        let signals = repo_signals(dir.path());
        // sha256("Cargo.lock\0<sha256 of its text>\nDockerfile\0<sha256 of its text>\n")
        assert_eq!(
            inputs_hash(dir.path(), &signals),
            "889aa498878f34f4aea0c437839571b1c671fef1337d79b30a257f2437855994"
        );
        let only_dockerfile = RepoSignals {
            dockerfiles: vec!["Dockerfile".into()],
            ..Default::default()
        };
        assert_eq!(
            inputs_hash(dir.path(), &only_dockerfile),
            "8a2fe30e4437d1a3691702e6dead4c3e9e855ef7bd706380213080623e46b1a0"
        );
    }

    #[test]
    fn a_gradle_lockfile_is_a_signal_and_is_hashed() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "gradle.lockfile", "a=1\n");
        let signals = repo_signals(dir.path());
        assert_eq!(signals.lockfiles, vec!["gradle.lockfile".to_string()]);
        let before = inputs_hash(dir.path(), &signals);
        put(dir.path(), "gradle.lockfile", "a=2\n");
        assert_ne!(inputs_hash(dir.path(), &signals), before);
    }

    #[test]
    fn the_inputs_hash_follows_signal_files_only() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "Cargo.lock", "version = 3\n");
        put(dir.path(), "README.md", "one\n");
        let signals = repo_signals(dir.path());
        let before = inputs_hash(dir.path(), &signals);
        put(dir.path(), "README.md", "two\n");
        assert_eq!(
            inputs_hash(dir.path(), &signals),
            before,
            "a file that is no signal"
        );
        put(dir.path(), "Cargo.lock", "version = 4\n");
        assert_ne!(
            inputs_hash(dir.path(), &signals),
            before,
            "a lockfile changed"
        );
    }

    #[test]
    fn a_signal_file_that_vanished_still_hashes() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "Cargo.lock", "version = 3\n");
        let signals = repo_signals(dir.path());
        let before = inputs_hash(dir.path(), &signals);
        std::fs::remove_file(dir.path().join("Cargo.lock")).unwrap();
        let after = inputs_hash(dir.path(), &signals);
        assert_ne!(after, before);
        assert_eq!(after, inputs_hash(dir.path(), &signals), "and it is stable");
    }

    #[test]
    fn a_fifo_or_a_symlink_is_never_opened_as_a_signal_file() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "Cargo.lock", "version = 3\n");
        let signals = repo_signals(dir.path());
        let missing = {
            let gone = tempfile::tempdir().unwrap();
            inputs_hash(gone.path(), &signals)
        };
        std::fs::remove_file(dir.path().join("Cargo.lock")).unwrap();
        let fifo = std::ffi::CString::new(dir.path().join("Cargo.lock").to_str().unwrap()).unwrap();
        // SAFETY: `fifo` is a NUL-terminated path that lives across the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let path = dir.path().join("Cargo.lock");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(open_regular(&path).map(drop).map_err(|e| e.kind()));
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).expect("no block"),
            Err(std::io::ErrorKind::InvalidInput)
        );
        assert_eq!(inputs_hash(dir.path(), &signals), missing);

        put(dir.path(), "real.lock", "version = 3\n");
        std::os::unix::fs::symlink(dir.path().join("real.lock"), dir.path().join("link.lock"))
            .unwrap();
        assert_eq!(
            open_regular(&dir.path().join("link.lock"))
                .map_err(|e| e.kind())
                .err(),
            Some(std::io::ErrorKind::InvalidInput)
        );
        assert!(open_regular(&dir.path().join("real.lock")).is_ok());
    }

    #[tokio::test]
    async fn existing_state_is_read_from_the_engine_the_disk_and_the_path() {
        let worktree = tempfile::tempdir().unwrap();
        for dir in ["node_modules", "target"] {
            std::fs::create_dir(worktree.path().join(dir)).unwrap();
        }
        let clone = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeExec::new());
        fake.executable_at("/opt/homebrew/bin/cargo")
            .executable_at("/opt/x/bin/node");
        fake.on(
            "docker",
            &["image", "ls"],
            FakeReply::ok(
                "clusia/rzorzal-clusia-06d0a3:0123456789abcdef\nclusia/rzorzal-clusia-06d0a3:fedcba9876543210\n",
            ),
        )
        .on("docker", &["ps"], FakeReply::ok("redis\ndb\nredis\n"));
        let host = HostEnv {
            path: Some("/usr/bin:/opt/x/bin".into()),
            home: PathBuf::from("/Users/octo"),
            ..Default::default()
        };
        let existing = existing(
            fake.as_ref(),
            Some(&running_engine()),
            &host,
            worktree.path(),
            clone.path(),
            "rzorzal/clusia",
        )
        .await;
        assert_eq!(
            existing,
            Existing {
                images: vec![
                    "clusia/rzorzal-clusia-06d0a3:0123456789abcdef".into(),
                    "clusia/rzorzal-clusia-06d0a3:fedcba9876543210".into(),
                ],
                reviewer_services: vec!["db".into(), "redis".into()],
                local_dirs: vec!["node_modules".into(), "target".into()],
                host_tools: vec!["cargo".into(), "node".into()],
            }
        );
        let calls = fake.calls();
        let args: Vec<Vec<String>> = calls
            .iter()
            .map(|c| {
                c.args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect()
            })
            .collect();
        assert_eq!(
            args[0],
            [
                "image",
                "ls",
                "--filter",
                "label=io.clusia.repo=rzorzal/clusia",
                "--format",
                "{{.Repository}}:{{.Tag}}"
            ]
        );
        assert_eq!(
            args[1],
            [
                "ps",
                "--filter",
                "label=com.docker.compose.project",
                "--format",
                r#"{{.Label "com.docker.compose.service"}}"#
            ]
        );
        assert!(
            args.iter().all(|a| a[0] != "compose"),
            "no compose file is parsed"
        );
        assert_eq!(calls[1].cwd, None);
        assert!(calls.iter().all(|c| c.clear_env));
    }

    #[tokio::test]
    async fn without_a_running_engine_nothing_is_asked() {
        let worktree = tempfile::tempdir().unwrap();
        let fake = FakeExec::new();
        let host = HostEnv::default();
        let none = existing(
            &fake,
            None,
            &host,
            worktree.path(),
            worktree.path(),
            "acme/widgets",
        )
        .await;
        assert_eq!(none, Existing::default());
        let mut stopped = running_engine();
        stopped.running = false;
        let down = existing(
            &fake,
            Some(&stopped),
            &host,
            worktree.path(),
            worktree.path(),
            "acme/widgets",
        )
        .await;
        assert_eq!(down, Existing::default());
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn a_failing_engine_command_gives_an_empty_list() {
        let worktree = tempfile::tempdir().unwrap();
        let fake = FakeExec::new();
        fake.on("docker", &[], FakeReply::fail(1, "boom"));
        let got = existing(
            &fake,
            Some(&running_engine()),
            &HostEnv::default(),
            worktree.path(),
            worktree.path(),
            "acme/widgets",
        )
        .await;
        assert_eq!(got, Existing::default());
    }

    #[tokio::test]
    async fn a_clean_worktree_ships_no_clusia_folder() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeExec::new();
        fake.on("git", &[], FakeReply::ok(""));
        assert_eq!(clusia_dir_problem(&fake, dir.path(), None).await, None);
        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, PathBuf::from("/usr/bin/git"));
        let worktree = dir.path().to_str().unwrap();
        assert_eq!(
            calls[0].args,
            [
                "-C",
                worktree,
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                "ls-files",
                "-z",
                "--",
                ".clusia",
            ]
            .map(OsString::from)
        );
        assert_eq!(calls[0].cwd.as_deref(), Some(dir.path()));
        assert!(calls[0].clear_env);
        assert!(
            calls[0]
                .env
                .contains(&(OsString::from("GIT_OPTIONAL_LOCKS"), OsString::from("0")))
        );
        assert!(
            calls[0].env.iter().all(|(k, _)| k != "GIT_DIR"),
            "no gitdir given, no GIT_DIR"
        );
    }

    #[tokio::test]
    async fn a_verified_gitdir_is_set_as_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeExec::new();
        fake.on("git", &[], FakeReply::ok(""));
        let gitdir = Path::new("/Users/octo/clones/widgets/.git/worktrees/pr7");
        assert_eq!(
            clusia_dir_problem(&fake, dir.path(), Some(gitdir)).await,
            None
        );
        let call = &fake.calls()[0];
        assert!(
            call.env
                .contains(&(OsString::from("GIT_DIR"), gitdir.as_os_str().to_owned()))
        );
        assert!(
            call.args
                .windows(2)
                .any(|w| w == ["-c", "core.fsmonitor=false"]),
            "a worktree's own config cannot name a program to run"
        );
        assert!(
            call.args
                .windows(2)
                .any(|w| w == ["-c", "core.hooksPath=/dev/null"])
        );
    }

    #[tokio::test]
    async fn a_tracked_clusia_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeExec::new();
        fake.on("git", &[], FakeReply::ok(".clusia/recipe.toml\0"));
        assert_eq!(
            clusia_dir_problem(&fake, dir.path(), None).await.as_deref(),
            Some("This pull request ships its own .clusia/ folder; Clúsia won't use it.")
        );
        assert_eq!(
            SHIPPED_CLUSIA,
            "This pull request ships its own .clusia/ folder; Clúsia won't use it."
        );
    }

    #[tokio::test]
    async fn a_symlinked_clusia_folder_or_recipe_is_refused_before_git_runs() {
        let fake = FakeExec::new();
        let linked = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc", linked.path().join(".clusia")).unwrap();
        assert_eq!(
            clusia_dir_problem(&fake, linked.path(), None)
                .await
                .as_deref(),
            Some(SHIPPED_CLUSIA)
        );

        let recipe = tempfile::tempdir().unwrap();
        std::fs::create_dir(recipe.path().join(".clusia")).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", recipe.path().join(".clusia/recipe.toml"))
            .unwrap();
        assert_eq!(
            clusia_dir_problem(&fake, recipe.path(), None)
                .await
                .as_deref(),
            Some(SHIPPED_CLUSIA)
        );
        assert!(fake.calls().is_empty(), "nothing was run or read");
    }

    #[tokio::test]
    async fn a_git_that_cannot_answer_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeExec::new();
        let missing = clusia_dir_problem(&fake, dir.path(), None).await.unwrap();
        assert!(missing.contains("could not check"), "{missing}");
        fake.on(
            "git",
            &[],
            FakeReply::fail(128, "fatal: not a git repository"),
        );
        let failed = clusia_dir_problem(&fake, dir.path(), None).await.unwrap();
        assert!(
            failed.contains("could not check") && failed.contains("not a git repository"),
            "{failed}"
        );
    }

    #[tokio::test]
    async fn real_git_sees_a_tracked_clusia_file_and_ignores_an_untracked_one() {
        use crate::exec::SystemExec;
        if !Path::new("/usr/bin/git").exists() {
            eprintln!("skipped: /usr/bin/git is not there");
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let git = |dir: &Path, args: &[&str]| {
            // Never the owner's configuration, templates or filters.
            let status = std::process::Command::new("/usr/bin/git")
                .args(args)
                .current_dir(dir)
                .env("HOME", home.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        let shipped = tempfile::tempdir().unwrap();
        git(shipped.path(), &["init", "-q"]);
        put(shipped.path(), ".clusia/recipe.toml", "runner = \"auto\"\n");
        git(shipped.path(), &["add", "-f", ".clusia/recipe.toml"]);
        assert_eq!(
            clusia_dir_problem(&SystemExec, shipped.path(), None)
                .await
                .as_deref(),
            Some(SHIPPED_CLUSIA)
        );

        let clean = tempfile::tempdir().unwrap();
        git(clean.path(), &["init", "-q"]);
        put(clean.path(), ".clusia/recipe.toml", "runner = \"auto\"\n");
        assert_eq!(
            clusia_dir_problem(&SystemExec, clean.path(), None).await,
            None
        );
    }
}

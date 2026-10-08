//! `install.sh` picks Homebrew or a direct build, ends with `clusia install --from`, and
//! only ever runs fakes here: every external program is a script on PATH that records its argv.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../install.sh")
}

const REPO: &str = "https://github.com/rzorzal/clusia.git";

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Sandbox {
        let sandbox = Sandbox {
            dir: tempfile::tempdir().unwrap(),
        };
        for d in ["bin", "home", "tmp", "prefix/libexec/bin"] {
            std::fs::create_dir_all(sandbox.path(d)).unwrap();
        }
        sandbox.fake("uname", r#"echo "${FAKE_UNAME:-Darwin}""#);
        sandbox.fake("open", "");
        sandbox.fake(
            "xcode-select",
            r#"[ "$1" = "-p" ] && [ -z "${FAKE_NO_CLT:-}" ]"#,
        );
        sandbox.fake(
            "git",
            r#"for last; do :; done; mkdir -p "$last"; printf '[workspace]\n' > "$last/Cargo.toml""#,
        );
        sandbox.fake(
            "brew",
            r#"case "$1" in
  --prefix) echo "$FAKE_PREFIX" ;;
  list) [ -n "${FAKE_BREW_INSTALLED:-}" ] ;;
esac"#,
        );
        sandbox.fake(
            "curl",
            r#"cat <<'RUSTUP'
echo "rustup-init $*" >> "$FAKE_LOG"
mkdir -p "$HOME/.cargo/bin"
cp "$FAKE_BIN/cargo-real" "$HOME/.cargo/bin/cargo"
printf 'export PATH="$HOME/.cargo/bin:$PATH"\n' > "$HOME/.cargo/env"
RUSTUP"#,
        );
        sandbox.write_exec(
            "bin/cargo-real",
            &format!(
                "{}\nmkdir -p target/release\ncp \"$FAKE_BIN/clusia-real\" target/release/clusia\n",
                recorder("cargo")
            ),
        );
        sandbox.write_exec(
            "bin/cargo",
            &std::fs::read_to_string(sandbox.path("bin/cargo-real")).unwrap(),
        );
        sandbox.write_exec(
            "bin/clusia-real",
            &format!("#!/bin/bash\n{}\n", recorder("clusia")),
        );
        sandbox.write_exec(
            "prefix/libexec/bin/clusia",
            &format!("#!/bin/bash\n{}\n", recorder("brew-clusia")),
        );
        sandbox
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn write_exec(&self, rel: &str, body: &str) {
        let file = self.path(rel);
        let body = if body.starts_with("#!") {
            body.to_string()
        } else {
            format!("#!/bin/bash\n{body}\n")
        };
        std::fs::write(&file, body).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A fake that records its argv, then runs `behaviour`.
    fn fake(&self, name: &str, behaviour: &str) {
        self.write_exec(
            &format!("bin/{name}"),
            &format!("{}\n{behaviour}", recorder(name)),
        );
    }

    fn remove(&self, name: &str) {
        std::fs::remove_file(self.path(&format!("bin/{name}"))).unwrap();
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let bin = self.path("bin");
        Command::new("bash")
            .arg(script())
            .args(args)
            .env_clear()
            .env("HOME", self.path("home"))
            .env("TMPDIR", self.path("tmp"))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("FAKE_LOG", self.path("log"))
            .env("FAKE_BIN", &bin)
            .env("FAKE_PREFIX", self.path("prefix"))
            .envs(env.iter().copied())
            .output()
            .unwrap()
    }

    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.path("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn recorder(name: &str) -> String {
    format!(r#"echo "{name} $*" >> "$FAKE_LOG""#)
}

fn prefix(sandbox: &Sandbox) -> String {
    sandbox.path("prefix").display().to_string()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn index(log: &[String], line: &str) -> usize {
    log.iter()
        .position(|l| l == line)
        .unwrap_or_else(|| panic!("{line:?} not in {log:#?}"))
}

/// The clone target of a recorded `git clone`.
fn clone_dir(log: &[String]) -> String {
    log.iter()
        .find(|l| l.starts_with("git clone"))
        .and_then(|l| l.rsplit(' ').next())
        .unwrap_or_else(|| panic!("no git clone in {log:#?}"))
        .to_string()
}

#[test]
fn the_script_parses_and_never_asks_for_admin_rights() {
    let mode = std::fs::metadata(script()).unwrap().permissions().mode();
    assert!(mode & 0o111 != 0, "install.sh is not executable");
    assert!(
        Command::new("bash")
            .arg("-n")
            .arg(script())
            .status()
            .unwrap()
            .success()
    );
    let source = std::fs::read_to_string(script()).unwrap();
    assert!(!source.contains("sudo"));
    assert!(!source.contains("Application Support"));
}

#[test]
fn brew_not_installed_installs_the_head_formula_then_clusia_then_opens() {
    let s = Sandbox::new();
    let out = s.run(&[], &[]);
    assert!(out.status.success(), "{}", text(&out));
    let log = s.log();
    let p = prefix(&s);
    let brew = index(&log, "brew install --HEAD rzorzal/clusia/clusia");
    let install = index(&log, &format!("brew-clusia install --from {p}/libexec/bin"));
    let open = index(&log, "open -a Clusia");
    assert!(brew < install && install < open, "{log:#?}");
    assert!(!log.iter().any(|l| l.starts_with("git ")));
}

#[test]
fn brew_already_installed_upgrades_from_head() {
    let s = Sandbox::new();
    let out = s.run(&[], &[("FAKE_BREW_INSTALLED", "1")]);
    assert!(out.status.success(), "{}", text(&out));
    let log = s.log();
    let upgrade = index(&log, "brew upgrade --fetch-HEAD clusia");
    let install = index(
        &log,
        &format!("brew-clusia install --from {}/libexec/bin", prefix(&s)),
    );
    assert!(upgrade < install);
    assert!(!log.iter().any(|l| l.starts_with("brew install")));
}

#[test]
fn without_brew_it_clones_builds_and_installs_from_the_build() {
    let s = Sandbox::new();
    s.remove("brew");
    let out = s.run(&[], &[]);
    assert!(out.status.success(), "{}", text(&out));
    let log = s.log();
    let dir = clone_dir(&log);
    assert!(dir.starts_with(&s.path("tmp").display().to_string()));
    let clone = index(
        &log,
        &format!("git clone --depth 1 --branch main {REPO} {dir}"),
    );
    let build = index(&log, "cargo build --release");
    let install = index(&log, &format!("clusia install --from {dir}/target/release"));
    let open = index(&log, "open -a Clusia");
    assert!(
        clone < build && build < install && install < open,
        "{log:#?}"
    );
    assert!(!Path::new(&dir).exists(), "the temp dir is cleaned up");
    assert!(!log.iter().any(|l| l.starts_with("rustup-init")));
}

#[test]
fn without_command_line_tools_it_asks_for_them_and_stops() {
    let s = Sandbox::new();
    s.remove("brew");
    let out = s.run(&[], &[("FAKE_NO_CLT", "1")]);
    assert!(!out.status.success());
    assert!(text(&out).contains("run me again"), "{}", text(&out));
    let log = s.log();
    index(&log, "xcode-select --install");
    assert!(
        !log.iter().any(|l| l.starts_with("git ")
            || l.contains("install --from")
            || l.starts_with("open"))
    );
}

#[test]
fn without_cargo_it_installs_rust_non_interactively_then_builds() {
    let s = Sandbox::new();
    s.remove("brew");
    s.remove("cargo");
    let out = s.run(&[], &[]);
    assert!(out.status.success(), "{}", text(&out));
    let log = s.log();
    let rustup = index(&log, "rustup-init -y --profile minimal");
    let build = index(&log, "cargo build --release");
    assert!(rustup < build, "{log:#?}");
}

#[test]
fn a_dry_run_prints_the_plan_and_changes_nothing() {
    for (brew, installed) in [(true, false), (true, true), (false, false)] {
        let s = Sandbox::new();
        if !brew {
            s.remove("brew");
            s.remove("cargo");
        }
        let env: &[(&str, &str)] = if installed {
            &[("FAKE_BREW_INSTALLED", "1")]
        } else {
            &[]
        };
        let out = s.run(&["--dry-run"], env);
        assert!(out.status.success(), "{}", text(&out));
        let shown = text(&out);
        assert!(shown.contains("install --from"), "{shown}");
        for line in s.log() {
            let read_only = line == "uname -s"
                || line.starts_with("brew --prefix")
                || line.starts_with("brew list")
                || line.starts_with("xcode-select -p");
            assert!(read_only, "dry run called {line:?}");
        }
        assert_eq!(std::fs::read_dir(s.path("tmp")).unwrap().count(), 0);
    }
}

#[test]
fn the_options_and_their_env_vars_work() {
    let s = Sandbox::new();
    let out = s.run(&["--no-open"], &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!s.log().iter().any(|l| l.starts_with("open")));

    let s = Sandbox::new();
    let out = s.run(&["--no-brew", "--ref", "v1.2.3"], &[]);
    assert!(out.status.success(), "{}", text(&out));
    let log = s.log();
    assert!(
        log.iter()
            .any(|l| l.starts_with("git clone --depth 1 --branch v1.2.3 "))
    );
    assert!(!log.iter().any(|l| l.starts_with("brew install")));

    let s = Sandbox::new();
    let out = s.run(
        &[],
        &[
            ("CLUSIA_NO_BREW", "1"),
            ("CLUSIA_NO_OPEN", "1"),
            ("CLUSIA_REF", "dev"),
        ],
    );
    assert!(out.status.success(), "{}", text(&out));
    let log = s.log();
    assert!(
        log.iter()
            .any(|l| l.starts_with("git clone --depth 1 --branch dev "))
    );
    assert!(!log.iter().any(|l| l.starts_with("open")));

    let s = Sandbox::new();
    let out = s.run(&[], &[("CLUSIA_DRY_RUN", "1")]);
    assert!(out.status.success());
    assert!(!s.log().iter().any(|l| l.starts_with("brew install")));
}

#[test]
fn an_unknown_option_is_refused() {
    let s = Sandbox::new();
    let out = s.run(&["--nope"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(s.log().is_empty());
}

#[test]
fn it_refuses_to_run_off_macos() {
    let s = Sandbox::new();
    let out = s.run(&[], &[("FAKE_UNAME", "Linux")]);
    assert!(!out.status.success());
    assert!(text(&out).contains("macOS"), "{}", text(&out));
    assert!(!s.log().iter().any(|l| l.starts_with("brew")));
}

#[test]
fn a_truncated_download_runs_nothing() {
    let source = std::fs::read_to_string(script()).unwrap();
    let lines: Vec<&str> = source.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.last().copied(), Some(r#"main "$@""#));
    let last = lines.len() - 1;
    let main_at = lines
        .iter()
        .position(|l| l.starts_with("main() {"))
        .unwrap();
    for l in &lines[..last] {
        let top_level = !l.starts_with([' ', '\t', '}', '#']);
        let allowed = l.starts_with("set -")
            || l.contains("() {")
            || l.starts_with("#!")
            || l.contains('=') && !l.contains(' ');
        assert!(!top_level || allowed, "top-level statement above main: {l}");
    }
    assert!(main_at < last);
}

#[test]
fn the_readme_and_the_formula_lead_with_the_one_command() {
    let readme =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../README.md"))
            .unwrap();
    let curl = "curl -fsSL https://raw.githubusercontent.com/rzorzal/clusia/main/install.sh | bash";
    let at = readme.find(curl).expect("the README has the one-liner");
    assert!(at < readme.find("brew install --HEAD").unwrap());
    let formula = include_str!("../../../packaging/homebrew/clusia.rb");
    assert!(formula.contains("install.sh | bash"));
}

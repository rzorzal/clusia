//! Escape attempts against the local runner's profile, under the real `/usr/bin/sandbox-exec`,
//! with a fake home, a fake data root and local listeners. Each refusal has a control where one
//! is safe to run: the same attempt outside the sandbox works.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{MetadataExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use clusia_testrun::exec::{CommandSpec, Exec, Output, SystemExec};
use clusia_testrun::sandbox::{self, NetMode, SandboxRoots};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const GIT: &str = "/usr/bin/git";
const CURL: &str = "/usr/bin/curl";
const PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
const LIMIT: Duration = Duration::from_secs(30);
const RESOLVER: &str = "/private/var/run/mDNSResponder";
const BOTH: [NetMode; 2] = [NetMode::Offline, NetMode::Online];
const ESCAPE_JOB: &str = "io.clusia.escape";

/// A file in each deny-list place the tests cover, and the secret it holds.
const SECRETS: [(&str, &str); 18] = [
    (".ssh/id_test", "ssh-secret"),
    (".aws/credentials", "aws-secret"),
    (".config/gh/hosts.yml", "gh-secret"),
    (".netrc", "netrc-secret"),
    (".docker/config.json", "docker-secret"),
    (".kube/config", "kube-secret"),
    (".gnupg/secring.gpg", "gpg-secret"),
    ("Library/Keychains/login.keychain-db", "keychain-secret"),
    (".git-credentials", "git-credentials-secret"),
    (
        ".config/gcloud/application_default_credentials.json",
        "gcloud-secret",
    ),
    (".claude/.credentials.json", "claude-secret"),
    (".npmrc", "//registry.npmjs.org/:_authToken=npm-secret"),
    (".yarnrc.yml", "npmAuthToken: yarn-secret"),
    (".gem/credentials", ":rubygems_api_key: gem-secret"),
    (".m2/settings.xml", "<password>maven-secret</password>"),
    (".gradle/gradle.properties", "signingPassword=gradle-secret"),
    (
        ".terraform.d/credentials.tfrc.json",
        "{\"token\": \"terraform-secret\"}",
    ),
    (".vault-token", "vault-secret"),
];

struct Lab {
    _dir: tempfile::TempDir,
    top: PathBuf,
    clone: PathBuf,
    roots: SandboxRoots,
    profiles: PathBuf,
}

fn put(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// Single-quoted for `sh`; the lab's paths hold no quote.
fn q(path: &Path) -> String {
    format!("'{}'", path.display())
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Git outside the sandbox, never reading the owner's configuration.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new(GIT)
        .args(["-c", "user.name=octo", "-c", "user.email=octo@example.com"])
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", PATH)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn lab() -> Option<Lab> {
    for tool in [SANDBOX_EXEC, GIT] {
        if !Path::new(tool).exists() {
            eprintln!("skipped: {tool} is not on this machine");
            return None;
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path().canonicalize().unwrap();
    let home = top.join("home");
    for (file, secret) in SECRETS {
        if file.starts_with(".aws/") {
            // `~/.aws` is a symlink: the rule must hold at the real path too.
            put(&top.join("elsewhere/aws/credentials"), secret);
            std::fs::create_dir_all(&home).unwrap();
            symlink(top.join("elsewhere/aws"), home.join(".aws")).unwrap();
        } else {
            put(&home.join(file), secret);
        }
    }
    // A key kept elsewhere and linked from inside a denied folder.
    put(&top.join("keys/id_ed25519"), "ed25519-secret");
    symlink(top.join("keys/id_ed25519"), home.join(".ssh/id_ed25519")).unwrap();
    put(&home.join("notes.txt"), "home-readable");

    let data = top.join("data");
    put(&data.join("reviews/acme~widgets~7.json"), "data-secret");
    let clone = top.join("clone");
    put(&clone.join("README.md"), "clone-readable");
    git(&clone, &["init", "-q", "-b", "main"]);
    git(&clone, &["add", "README.md"]);
    git(&clone, &["commit", "-q", "-m", "first"]);
    put(&clone.join(".env"), "API_KEY=env-secret");
    put(&clone.join("config/.env.local"), "API_KEY=env-local-secret");
    let worktree = data.join("worktrees/acme~widgets~7");
    std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
    git(
        &clone,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "pr7",
            worktree.to_str().unwrap(),
        ],
    );
    put(&worktree.join("src/lib.rs"), "worktree-readable");
    put(
        &worktree.join(".clusia/recipe.toml"),
        "runner = \"local\"\n",
    );
    std::fs::create_dir_all(worktree.join(".clusia/state")).unwrap();
    put(&top.join("outside/existing.txt"), "outside-original");
    let profiles = data.join("tests/profiles");
    std::fs::create_dir_all(&profiles).unwrap();
    let roots = sandbox::resolve_roots(SandboxRoots {
        worktree,
        git_common: clone.join(".git"),
        review_tmp: data.join("tests/tmp/0123456789ab"),
        repo_cache: data.join("tests/cache/acme~widgets"),
        data_root: data,
        home,
        clone_env: sandbox::clone_env_files(&clone),
    })
    .unwrap();
    Some(Lab {
        _dir: dir,
        top,
        clone,
        roots,
        profiles,
    })
}

impl Lab {
    /// `script` under the profile of `mode`, as the local runner starts it.
    async fn run(&self, mode: NetMode, script: &str) -> Output {
        let file = self.profiles.join(format!("{mode:?}.sb"));
        assert!(!sandbox::writable_by_sandbox(&self.roots, &file));
        std::fs::write(&file, sandbox::profile(&self.roots, mode)).unwrap();
        let env = sandbox::base_env(&self.roots, PATH.as_ref(), &[], &BTreeMap::new());
        let cmd = sandbox::local_command(
            Path::new(SANDBOX_EXEC),
            &file,
            script,
            env,
            &self.roots.worktree,
        );
        SystemExec.output(&cmd, LIMIT).await.unwrap()
    }

    /// `script` with the same environment and folder, and no sandbox: the control.
    async fn unsandboxed(&self, script: &str) -> Output {
        let env = sandbox::base_env(&self.roots, PATH.as_ref(), &[], &BTreeMap::new());
        let mut cmd = CommandSpec::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .clear_env()
            .cwd(&self.roots.worktree);
        cmd.env = env;
        SystemExec.output(&cmd, LIMIT).await.unwrap()
    }

    async fn refused(&self, mode: NetMode, script: &str) {
        let out = self.run(mode, script).await;
        assert_ne!(
            out.code,
            Some(0),
            "{mode:?} must refuse: {script}\n{}",
            text(&out.stdout)
        );
        assert!(
            !text(&out.stdout).contains("secret"),
            "{mode:?} leaked: {script}"
        );
    }
}

/// Answers every connection with an HTTP 200 whose body is `ok`.
fn serve(mut stream: impl Read + Write) {
    let mut buf = [0u8; 1024];
    let _ = stream.read(&mut buf);
    let _ = stream.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok");
}

/// A TCP listener on `addr`, or `None` when this machine has no such address.
fn tcp_listener(addr: &str) -> Option<u16> {
    let listener = TcpListener::bind(addr).ok()?;
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || listener.incoming().flatten().for_each(serve));
    Some(port)
}

fn unix_listener(path: &Path) {
    let listener = UnixListener::bind(path).unwrap();
    std::thread::spawn(move || listener.incoming().flatten().for_each(serve));
}

/// Another terminal of this user, if one is open.
fn other_terminal(owner: u32) -> Option<PathBuf> {
    let mut ttys: Vec<PathBuf> = std::fs::read_dir("/dev")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.len() > 4 && n.starts_with("ttys"))
        })
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.uid() == owner))
        .collect();
    ttys.sort();
    ttys.into_iter().next()
}

#[tokio::test]
async fn offline_profile_blocks_dns_tcp_and_unix_sockets() {
    let Some(lab) = lab() else { return };
    let port = tcp_listener("127.0.0.1:0").unwrap();
    let socket = lab.top.join("l.sock");
    unix_listener(&socket);
    let tcp = format!("{CURL} -sS -m 3 http://127.0.0.1:{port}/");
    let unix = format!(
        "{CURL} -sS -m 3 --unix-socket {} http://localhost/",
        q(&socket)
    );
    let resolver =
        format!("{CURL} -sS -m 3 -o /dev/null --unix-socket {RESOLVER} http://localhost/");

    for script in [&tcp, &unix] {
        assert_eq!(
            text(&lab.unsandboxed(script).await.stdout),
            "ok",
            "control: {script}"
        );
    }
    assert_ne!(
        lab.unsandboxed(&resolver).await.code,
        Some(7),
        "control: the resolver answers"
    );

    for script in [&tcp, &unix, &resolver] {
        let out = lab.run(NetMode::Offline, script).await;
        assert_eq!(out.code, Some(7), "{script}: {}", text(&out.stderr));
        assert_eq!(text(&out.stdout), "", "{script}");
    }
    let out = lab
        .run(
            NetMode::Offline,
            &format!("{CURL} -sS -m 5 https://example.com"),
        )
        .await;
    assert_eq!(out.code, Some(6), "no name resolves: {}", text(&out.stderr));
}

#[tokio::test]
async fn writes_stay_in_the_roots_even_through_symlinks() {
    let Some(lab) = lab() else { return };
    let wt = lab.roots.worktree.clone();
    let home = lab.roots.home.clone();
    let outside = lab.top.join("outside");
    let git_file = read(&wt.join(".git"));
    symlink(&outside, wt.join("escape-dir")).unwrap();
    symlink(outside.join("made-by-link.txt"), wt.join("escape-file")).unwrap();
    symlink(home.join("notes.txt"), wt.join("home-notes")).unwrap();
    symlink(wt.join(".clusia"), wt.join("clusia-link")).unwrap();

    for mode in BOTH {
        let allowed = [
            "echo w > written.txt".to_string(),
            format!("echo t > {}/t.txt", q(&lab.roots.review_tmp)),
            format!("echo c > {}/c.txt", q(&lab.roots.repo_cache)),
            "echo d > \"$TMPDIR/tmpdir.txt\"".to_string(),
            "echo n > /dev/null".to_string(),
        ];
        for script in &allowed {
            let out = lab.run(mode, script).await;
            assert_eq!(
                out.code,
                Some(0),
                "{mode:?} {script}: {}",
                text(&out.stderr)
            );
        }
        let refused = [
            "echo x > \"$HOME/written.txt\"".to_string(),
            format!("echo x > {}/written.txt", q(&outside)),
            "echo x > escape-dir/through-dir.txt".to_string(),
            "echo x > escape-file".to_string(),
            "echo x >> home-notes".to_string(),
            "echo x > .clusia/state/x.done".to_string(),
            "echo x > .clusia/recipe.toml".to_string(),
            "echo x > clusia-link/state/y.done".to_string(),
            "mv .clusia moved-clusia".to_string(),
            "rm -rf .clusia".to_string(),
            // The worktree's pointer to its gitdir stays Clúsia's.
            "echo 'gitdir: /tmp/evil' > .git".to_string(),
            "mv .git moved-git".to_string(),
            "rm -f .git".to_string(),
            format!("ln {}/existing.txt hard-outside", q(&outside)),
            "ln .clusia/recipe.toml hard-recipe".to_string(),
            format!("echo x > {}/HEAD", q(&lab.roots.git_common)),
            format!("echo x > {}/new.txt", q(&lab.roots.data_root)),
            format!("echo x > {}/x.sb", q(&lab.profiles)),
        ];
        for script in &refused {
            lab.refused(mode, script).await;
        }
    }

    // `/dev/tty` is writable, the user's other terminals are not.
    let owner = std::fs::metadata(&lab.top).unwrap().uid();
    match other_terminal(owner) {
        Some(tty) => {
            let open = format!("exec 3> {} && echo opened", q(&tty));
            assert_eq!(
                text(&lab.unsandboxed(&open).await.stdout),
                "opened\n",
                "control: {open}"
            );
            for mode in BOTH {
                lab.refused(mode, &open).await;
            }
        }
        None => eprintln!("skipped the /dev/ttys* case: no other terminal of this user is open"),
    }

    assert_eq!(read(&wt.join("written.txt")), "w\n");
    assert_eq!(read(&lab.roots.review_tmp.join("t.txt")), "t\n");
    assert_eq!(read(&lab.roots.review_tmp.join("tmpdir.txt")), "d\n");
    assert_eq!(read(&lab.roots.repo_cache.join("c.txt")), "c\n");
    for gone in [
        home.join("written.txt"),
        outside.join("written.txt"),
        outside.join("through-dir.txt"),
        outside.join("made-by-link.txt"),
        wt.join(".clusia/state/x.done"),
        wt.join(".clusia/state/y.done"),
        wt.join("moved-clusia"),
        wt.join("moved-git"),
        wt.join("hard-outside"),
        wt.join("hard-recipe"),
        lab.roots.data_root.join("new.txt"),
        lab.profiles.join("x.sb"),
    ] {
        assert!(!gone.exists(), "{} must not exist", gone.display());
    }
    assert_eq!(read(&wt.join(".git")), git_file);
    assert_eq!(read(&outside.join("existing.txt")), "outside-original");
    assert_eq!(read(&home.join("notes.txt")), "home-readable");
    assert_eq!(
        read(&wt.join(".clusia/recipe.toml")),
        "runner = \"local\"\n"
    );
    assert_eq!(
        read(&lab.roots.git_common.join("HEAD")),
        "ref: refs/heads/main\n"
    );
}

#[tokio::test]
async fn deny_list_data_root_and_clone_env_files_are_unreadable() {
    let Some(lab) = lab() else { return };
    let home = lab.roots.home.clone();
    let wt = lab.roots.worktree.clone();
    symlink(home.join(".ssh/id_test"), wt.join("key-link")).unwrap();
    symlink(lab.clone.join(".env"), wt.join("env-link")).unwrap();
    let mut secrets: Vec<PathBuf> = SECRETS.iter().map(|(file, _)| home.join(file)).collect();
    secrets.extend([
        lab.top.join("elsewhere/aws/credentials"),
        home.join(".ssh/id_ed25519"),
        lab.top.join("keys/id_ed25519"),
        lab.roots.data_root.join("reviews/acme~widgets~7.json"),
        lab.clone.join(".env"),
        lab.clone.join("config/.env.local"),
        wt.join("key-link"),
        wt.join("env-link"),
    ]);
    for path in &secrets {
        let out = lab.unsandboxed(&format!("cat {}", q(path))).await;
        assert!(
            text(&out.stdout).contains("secret"),
            "control: {}",
            path.display()
        );
    }

    for mode in BOTH {
        for path in &secrets {
            let out = lab.run(mode, &format!("cat {}", q(path))).await;
            assert_ne!(out.code, Some(0), "{mode:?} read {}", path.display());
            assert!(
                !text(&out.stdout).contains("secret"),
                "{mode:?} leaked {}",
                path.display()
            );
            assert!(
                text(&out.stderr).contains("Operation not permitted"),
                "{mode:?} {}: {}",
                path.display(),
                text(&out.stderr)
            );
        }
        for folder in [
            home.join(".ssh"),
            home.join(".aws"),
            home.join(".claude"),
            lab.roots.data_root.clone(),
        ] {
            lab.refused(mode, &format!("ls {}", q(&folder))).await;
        }
        for (path, expected) in [
            (home.join("notes.txt"), "home-readable"),
            (wt.join("src/lib.rs"), "worktree-readable"),
            (lab.clone.join("README.md"), "clone-readable"),
            (lab.roots.git_common.join("HEAD"), "ref: refs/heads/main\n"),
        ] {
            let out = lab.run(mode, &format!("cat {}", q(&path))).await;
            assert_eq!(
                text(&out.stdout),
                expected,
                "{mode:?} must read {}",
                path.display()
            );
        }
    }
}

#[tokio::test]
async fn toolchains_git_and_the_worktree_path_still_work() {
    let Some(lab) = lab() else { return };
    let wt = lab.roots.worktree.display().to_string();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let node = ["/opt/homebrew/bin/node", "/usr/local/bin/node"]
        .into_iter()
        .find(|p| Path::new(p).exists());
    if node.is_none() {
        eprintln!("skipped node --version: node is not installed");
    }
    for mode in BOTH {
        // Tools that walk up from the worktree cross the denied data root.
        for script in ["pwd -P", "realpath ."] {
            let out = lab.run(mode, script).await;
            assert_eq!(
                text(&out.stdout).trim(),
                wt,
                "{mode:?} {script}: {}",
                text(&out.stderr)
            );
        }
        let out = lab
            .run(
                mode,
                &format!("GIT_OPTIONAL_LOCKS=0 {GIT} status --short --branch"),
            )
            .await;
        assert_eq!(
            out.code,
            Some(0),
            "{mode:?} git status: {}",
            text(&out.stderr)
        );
        assert!(
            text(&out.stdout).starts_with("## pr7"),
            "{}",
            text(&out.stdout)
        );
        let mut tools = vec![format!("'{cargo}' --version")];
        tools.extend(node.map(|node| format!("{node} --version")));
        for script in &tools {
            let out = lab.run(mode, script).await;
            assert_eq!(
                out.code,
                Some(0),
                "{mode:?} {script}: {}",
                text(&out.stderr)
            );
        }
    }
}

#[tokio::test]
async fn children_inherit_the_profile() {
    let Some(lab) = lab() else { return };
    let port = tcp_listener("127.0.0.1:0").unwrap();
    let home = lab.roots.home.clone();
    let key = home.join(".ssh/id_test");
    let nested_curl =
        format!("/bin/sh -c '/bin/sh -c \"{CURL} -sS -m 3 http://127.0.0.1:{port}/\"'");
    assert_eq!(
        text(&lab.unsandboxed(&nested_curl).await.stdout),
        "ok",
        "control"
    );

    let scripts = [
        nested_curl,
        "/bin/sh -c 'echo x > \"$HOME/child.txt\"'".to_string(),
        "/bin/sh -c 'cat \"$HOME/.ssh/id_test\"'".to_string(),
        "(sleep 0.2; echo x > \"$HOME/late.txt\") & wait $!".to_string(),
        format!("/usr/bin/env -i /bin/sh -c \"cat '{}'\"", key.display()),
        format!(
            "{SANDBOX_EXEC} -p '(version 1)(allow default)' /bin/cat {}",
            q(&key)
        ),
    ];
    for script in &scripts {
        let out = lab.run(NetMode::Offline, script).await;
        assert_ne!(out.code, Some(0), "{script}");
        let stdout = text(&out.stdout);
        assert!(
            !stdout.contains("ok") && !stdout.contains("secret"),
            "{script}: {stdout}"
        );
    }
    assert!(!home.join("child.txt").exists());
    assert!(!home.join("late.txt").exists());
}

/// A `sleep` started outside any sandbox, killed when dropped.
struct Outsider(std::process::Child);

impl Outsider {
    fn start() -> Self {
        Self(Command::new("/bin/sleep").arg("60").spawn().unwrap())
    }

    fn alive(&mut self) -> bool {
        self.0.try_wait().unwrap().is_none()
    }
}

impl Drop for Outsider {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn signals_reach_only_the_run_itself() {
    let Some(lab) = lab() else { return };
    let mut control = Outsider::start();
    let kill = |pid: u32| format!("/bin/kill -TERM {pid}");
    assert_eq!(
        lab.unsandboxed(&kill(control.0.id())).await.code,
        Some(0),
        "control"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(!control.alive(), "control: the outside sleep was killed");

    let mut outsider = Outsider::start();
    for mode in BOTH {
        let out = lab.run(mode, &kill(outsider.0.id())).await;
        assert_ne!(out.code, Some(0), "{mode:?} signalled an outside process");
        std::thread::sleep(Duration::from_millis(200));
        assert!(outsider.alive(), "{mode:?}: the outside sleep survives");
        // The run still stops its own children.
        let own = "/bin/sleep 60 & /bin/kill -TERM $! && wait $!; echo \"status $?\"";
        let out = lab.run(mode, own).await;
        assert_eq!(
            text(&out.stdout),
            "status 143\n",
            "{mode:?}: {}",
            text(&out.stderr)
        );
    }
}

/// The newest TextEdit process, if any.
fn text_edit() -> String {
    let out = Command::new("/usr/bin/pgrep")
        .args(["-n", "TextEdit"])
        .output()
        .unwrap();
    text(&out.stdout)
}

#[tokio::test]
async fn nothing_launches_outside_the_process_tree() {
    let Some(lab) = lab() else { return };
    // No unsandboxed control here: it would act on the owner's desktop or launchd.
    let before = text_edit();
    let submit = format!("/bin/launchctl submit -l {ESCAPE_JOB} -- /usr/bin/true");
    for mode in BOTH {
        for script in [
            "/usr/bin/open -a TextEdit",
            "/usr/bin/open https://example.com",
            "/usr/bin/osascript -e 'return 1'",
            "/usr/bin/pbpaste",
            "cp /usr/bin/osascript ./osa && ./osa -e 'return 1'",
            submit.as_str(),
        ] {
            let out = lab.run(mode, script).await;
            let _ = Command::new("/bin/launchctl")
                .args(["remove", ESCAPE_JOB])
                .output();
            assert_ne!(out.code, Some(0), "{mode:?} must refuse: {script}");
            assert_eq!(text(&out.stdout), "", "{mode:?} {script} printed something");
        }
    }
    assert_eq!(text_edit(), before, "TextEdit was not started");
    let listed = Command::new("/bin/launchctl")
        .args(["list", ESCAPE_JOB])
        .output()
        .unwrap();
    assert!(!listed.status.success(), "no launchd job is left");
}

#[tokio::test]
async fn online_profile_reaches_no_local_socket() {
    let Some(lab) = lab() else { return };
    let v4 = tcp_listener("127.0.0.1:0").unwrap();
    let any = tcp_listener("0.0.0.0:0").unwrap();
    let socket = lab.top.join("l.sock");
    unix_listener(&socket);
    // (script, whether this Mac may have no route to it at all)
    let mut cases = vec![
        (format!("{CURL} -sS -m 3 http://127.0.0.1:{v4}/"), false),
        (format!("{CURL} -sS -m 3 http://localhost:{v4}/"), false),
        (format!("{CURL} -sS -m 3 http://0.0.0.0:{any}/"), false),
        (
            format!("{CURL} -sS -m 3 -g 'http://[::ffff:127.0.0.1]:{any}/'"),
            false,
        ),
        (format!("{CURL} -sS -m 3 http://127.0.0.2:{any}/"), true),
        (
            format!(
                "{CURL} -sS -m 3 --unix-socket {} http://localhost/",
                q(&socket)
            ),
            false,
        ),
    ];
    match tcp_listener("[::1]:0") {
        Some(v6) => cases.push((format!("{CURL} -sS -m 3 -g 'http://[::1]:{v6}/'"), false)),
        None => eprintln!("skipped the ::1 case: IPv6 loopback is off on this machine"),
    }
    for (script, may_be_unroutable) in &cases {
        let control = text(&lab.unsandboxed(script).await.stdout);
        if *may_be_unroutable && control != "ok" {
            eprintln!(
                "skipped {script}: unreachable outside the sandbox too (lo0 has no such address)"
            );
            continue;
        }
        assert_eq!(control, "ok", "control: {script}");
        let out = lab.run(NetMode::Online, script).await;
        assert_eq!(out.code, Some(7), "{script}: {}", text(&out.stderr));
        assert_eq!(text(&out.stdout), "", "{script}");
    }
    // Still online: the resolver's socket is reachable, unlike in the offline variant.
    let resolver =
        format!("{CURL} -sS -m 3 -o /dev/null --unix-socket {RESOLVER} http://localhost/");
    assert_ne!(lab.run(NetMode::Online, &resolver).await.code, Some(7));
}

#[tokio::test]
async fn online_profile_reaches_the_internet() {
    if std::env::var("CLUSIA_TEST_NETWORK").as_deref() != Ok("1") {
        eprintln!("skipped: set CLUSIA_TEST_NETWORK=1 to reach example.com");
        return;
    }
    let Some(lab) = lab() else { return };
    let script = format!("{CURL} -sS -m 15 -o /dev/null -w '%{{http_code}}' https://example.com");
    assert_eq!(text(&lab.run(NetMode::Online, &script).await.stdout), "200");
    assert_eq!(lab.run(NetMode::Offline, &script).await.code, Some(6));
}

#[tokio::test]
async fn the_real_sandbox_exec_is_healthy() {
    if lab().is_none() {
        return;
    }
    assert_eq!(
        sandbox::health(&SystemExec, Path::new(SANDBOX_EXEC)).await,
        Ok(())
    );
}

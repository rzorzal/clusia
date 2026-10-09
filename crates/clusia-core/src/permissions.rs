//! Which requests a review's rules cover, and the rule a request would add. Pure: the daemon
//! decides, this only reads the request.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// The tools that write a file; their rule is the bare tool name.
const FILE_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// The commands "Allow for this review" can grant: a program and the subcommands it may be
/// allowed with. This allow-list is the gate: any program not in it gets only Allow once, so a
/// new wrapper, interpreter or runner never slips through the way it would past a deny-list.
/// An empty list means the program alone (`make`), with any arguments; otherwise the rule is
/// the program and one listed subcommand (`cargo test`). Every entry lists its subcommands:
/// there is no wildcard. To allow another command, add it here only if no subcommand or
/// argument of it runs other code, deletes outside the worktree, or reaches the network (so
/// `cargo run`, `npm exec`, `git fetch` and `rake` stay out). Arguments that name a path
/// outside the worktree are refused separately, by [`stays_inside`]. `git branch` and
/// `git stash` act on refs every worktree of the repository shares.
const RULE_COMMANDS: &[(&str, &[&str])] = &[
    (
        "cargo",
        &[
            "test", "build", "check", "clippy", "fmt", "nextest", "bench", "doc", "tree",
        ],
    ),
    ("npm", &["test", "run"]),
    ("pnpm", &["test", "run"]),
    ("yarn", &["test", "run"]),
    ("bun", &["test", "run"]),
    ("make", &[]),
    ("go", &["test", "build", "vet"]),
    ("mvn", &["test", "verify", "package", "compile"]),
    ("pytest", &[]),
    ("jest", &[]),
    ("vitest", &[]),
    ("mocha", &[]),
    ("rspec", &[]),
    ("phpunit", &[]),
    ("tox", &[]),
    ("nox", &[]),
    ("swift", &["test", "build"]),
    ("xcodebuild", &["test", "build"]),
    ("dotnet", &["test", "build"]),
    ("mix", &["test", "compile"]),
    ("tsc", &[]),
    ("eslint", &[]),
    ("prettier", &[]),
    ("ruff", &["check", "format"]),
    ("black", &[]),
    ("mypy", &[]),
    ("ninja", &[]),
    ("bazel", &["test", "build"]),
    (
        "git",
        &[
            "status", "diff", "log", "show", "branch", "add", "stash", "checkout", "switch",
            "restore",
        ],
    ),
    ("mkdir", &[]),
    ("touch", &[]),
];

/// Characters that chain, redirect or substitute: a command holding one is more than one
/// command, so only Allow once is offered for it. Any character outside printable ASCII, other
/// than a tab, counts too: the shell does not split on Unicode spaces, and escapes would
/// rewrite what a terminal shows.
const SHELL_SYNTAX: [char; 10] = [';', '|', '&', '<', '>', '$', '`', '\n', '(', ')'];

/// The rule saved for a Bash prefix, in the form Claude Code's `--allowedTools` reads.
pub fn rule_for_bash(prefix: &str) -> String {
    format!("Bash({prefix}:*)")
}

/// The rule saved for a file tool: the bare tool name.
pub fn rule_for_tool(tool: &str) -> String {
    tool.to_string()
}

/// What the request shows the reviewer: the command, the path, or the tool name.
pub fn summary_for(tool: &str, input: &Value) -> String {
    let text = |key: &str| input.get(key).and_then(Value::as_str);
    match tool {
        "Bash" => text("command").unwrap_or(tool),
        t if FILE_TOOLS.contains(&t) => text(path_key(t)).unwrap_or(tool),
        _ => tool,
    }
    .to_string()
}

/// The longest `detail_for` text, in characters.
pub const DETAIL_MAX_CHARS: usize = 2000;

/// How many lines of a new file `detail_for` shows.
const DETAIL_WRITE_LINES: usize = 20;

/// An excerpt of the request the modal shows under the path: for `Edit` the text replaced and
/// the text that replaces it (`old`, a line with `→`, `new`), for `MultiEdit` each pair in
/// turn, for `Write` the first lines of the file. At most [`DETAIL_MAX_CHARS`] characters,
/// the `…` included; `None` for any other tool or an input without those fields.
pub fn detail_for(tool: &str, input: &Value) -> Option<String> {
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    let pair = |v: &Value| {
        Some(format!(
            "{}\n→\n{}",
            text(v, "old_string")?,
            text(v, "new_string")?
        ))
    };
    let detail = match tool {
        "Edit" => pair(input)?,
        "MultiEdit" => input
            .get("edits")?
            .as_array()?
            .iter()
            .filter_map(pair)
            .collect::<Vec<_>>()
            .join("\n\n"),
        "Write" => text(input, "content")?
            .lines()
            .take(DETAIL_WRITE_LINES)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    if detail.is_empty() {
        return None;
    }
    Some(match detail.char_indices().nth(DETAIL_MAX_CHARS) {
        Some(_) => {
            let (cut, _) = detail.char_indices().nth(DETAIL_MAX_CHARS - 1)?;
            format!("{}…", &detail[..cut])
        }
        None => detail,
    })
}

/// The prefix "Allow for this review" would grant, or `None` when only Allow once is safe.
///
/// Bash: only a command in [`RULE_COMMANDS`], as the program alone (`make`) or the program
/// and its subcommand (`cargo test`); anything else, including a flag or a path where the
/// subcommand goes (`cargo -p x test`) or an argument outside the worktree (see
/// [`stays_inside`]), gives no prefix. File tools: the tool name, only for a
/// path inside `worktree`.
pub fn prefix_for(tool: &str, input: &Value, worktree: &Path) -> Option<String> {
    if tool == "Bash" {
        let words = simple_words(input.get("command")?.as_str()?)?;
        return stays_inside(&words)
            .then(|| allowed_prefix(&words))
            .flatten();
    }
    if FILE_TOOLS.contains(&tool)
        && file_path(tool, input).is_some_and(|p| inside_worktree(&p, worktree))
    {
        return Some(rule_for_tool(tool));
    }
    None
}

/// Whether one of `rules` already allows this request.
///
/// A Bash rule covers a command that starts with the rule's words, word for word
/// (`cargo test` covers `cargo test -p x`, not `cargo testx`), and only a single simple
/// command whose arguments stay inside the worktree. A Bash rule that [`prefix_for`] could not
/// have made (`Bash(rm:*)` in an edited state file) covers nothing. A file-tool rule covers its tool for paths inside the
/// worktree. The bare `Bash` is never a rule.
pub fn covers(rules: &BTreeSet<String>, tool: &str, input: &Value, worktree: &Path) -> bool {
    if tool == "Bash" {
        let Some(words) = input
            .get("command")
            .and_then(Value::as_str)
            .and_then(simple_words)
            .filter(|words| stays_inside(words))
        else {
            return false;
        };
        return rules.iter().any(|rule| {
            let Some(prefix) = rule
                .strip_prefix("Bash(")
                .and_then(|r| r.strip_suffix(":*)"))
            else {
                return false;
            };
            let wanted: Vec<&str> = prefix.split(' ').filter(|w| !w.is_empty()).collect();
            allowed_prefix(&wanted).is_some_and(|p| p == wanted.join(" "))
                && words.starts_with(&wanted)
        });
    }
    FILE_TOOLS.contains(&tool)
        && rules.contains(tool)
        && file_path(tool, input).is_some_and(|p| inside_worktree(&p, worktree))
}

/// The words of a single command; `None` for an empty one or one with shell syntax.
fn simple_words(command: &str) -> Option<Vec<&str>> {
    if command.contains(SHELL_SYNTAX)
        || command.contains(|c: char| c != '\t' && !(' '..='~').contains(&c))
    {
        return None;
    }
    let words: Vec<&str> = command
        .split([' ', '\t'])
        .filter(|w| !w.is_empty())
        .collect();
    (!words.is_empty()).then_some(words)
}

/// Whether no word after the program points outside the worktree. Each word is read with its
/// quotes and backslashes removed, as the shell passes it, and checked whole, after its first
/// `=` (`--basetemp=/x`) and, for a short flag, after the flag (`-C/`, `-o/x`): a form that
/// starts at `/` or `~`, or holds a `..` segment, points outside (`...` is not `..`, so
/// `go test ./...` stays). For git, `--output` writes a file anywhere and `--upload-pack`
/// (which git accepts abbreviated down to `--up`) runs a program.
fn stays_inside(words: &[&str]) -> bool {
    let git = words.first() == Some(&"git");
    words.iter().skip(1).all(|word| {
        let word: String = word
            .chars()
            .filter(|c| !matches!(c, '\'' | '"' | '\\'))
            .collect();
        let after_eq = word.split_once('=').map(|(_, value)| value);
        let after_flag = (word.starts_with('-') && !word.starts_with("--"))
            .then(|| word.get(2..))
            .flatten();
        [Some(word.as_str()), after_eq, after_flag]
            .into_iter()
            .flatten()
            .all(|form| {
                !(form.starts_with(['/', '~'])
                    || form.split('/').any(|segment| segment == "..")
                    || git && (form.starts_with("--output") || form.starts_with("--up")))
            })
    })
}

/// The rule prefix [`RULE_COMMANDS`] grants for a command's words, matched exactly (so
/// `Cargo`, `/usr/bin/cargo` and `cargo Test` match nothing).
fn allowed_prefix(words: &[&str]) -> Option<String> {
    let program = *words.first()?;
    let (_, subcommands) = RULE_COMMANDS.iter().find(|(p, _)| *p == program)?;
    if subcommands.is_empty() {
        return Some(program.to_string());
    }
    let sub = *words.get(1)?;
    subcommands
        .contains(&sub)
        .then(|| format!("{program} {sub}"))
}

/// The input field that names the file a file tool writes.
fn path_key(tool: &str) -> &'static str {
    if tool == "NotebookEdit" {
        "notebook_path"
    } else {
        "file_path"
    }
}

fn file_path(tool: &str, input: &Value) -> Option<PathBuf> {
    Some(PathBuf::from(input.get(path_key(tool))?.as_str()?))
}

/// Whether `path` (relative paths start at the worktree) lands inside `worktree` once `..`
/// and every symlink on the way are resolved. A part that does not exist yet is taken as
/// written, so a new file in the worktree is inside.
pub fn inside_worktree(path: &Path, worktree: &Path) -> bool {
    let Some(root) = resolve(worktree) else {
        return false;
    };
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    resolve(&full).is_some_and(|resolved| resolved.starts_with(&root))
}

/// `path` with `.` and `..` applied and each existing symlink replaced by its target, one
/// part at a time (so a `..` after a link steps out of the link's target). `None` for a link
/// that points nowhere.
fn resolve(path: &Path) -> Option<PathBuf> {
    let mut resolved = PathBuf::new();
    for part in path.components() {
        match part {
            Component::RootDir | Component::Prefix(_) => resolved.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                if resolved
                    .symlink_metadata()
                    .is_ok_and(|m| m.file_type().is_symlink())
                {
                    resolved = resolved.canonicalize().ok()?;
                }
            }
        }
    }
    Some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wt() -> PathBuf {
        PathBuf::from("/tmp/acme-widgets")
    }

    fn bash(command: &str) -> Value {
        json!({ "command": command, "description": "d" })
    }

    fn prefix(command: &str) -> Option<String> {
        prefix_for("Bash", &bash(command), &wt())
    }

    fn rules(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|r| r.to_string()).collect()
    }

    #[test]
    fn bash_prefixes_are_the_program_and_a_plain_subcommand() {
        assert_eq!(
            prefix("cargo test -p clusia-core"),
            Some("cargo test".into())
        );
        assert_eq!(prefix("npm run build"), Some("npm run".into()));
        assert_eq!(prefix("git status"), Some("git status".into()));
        assert_eq!(prefix("make"), Some("make".into()));
        assert_eq!(prefix("  cargo   fmt  "), Some("cargo fmt".into()));
        assert_eq!(
            prefix("make clean"),
            Some("make".into()),
            "make takes any args"
        );
        assert_eq!(prefix("make -j4"), Some("make".into()));
        assert_eq!(prefix("touch a.txt"), Some("touch".into()));
        assert_eq!(prefix("mkdir -p out"), Some("mkdir".into()));
        assert_eq!(prefix("pytest -k x"), Some("pytest".into()));
        assert_eq!(prefix("ruff check ."), Some("ruff check".into()));
        assert_eq!(prefix("go test ./..."), Some("go test".into()));
        assert_eq!(prefix("bun run x"), Some("bun run".into()));
    }

    #[test]
    fn only_known_build_and_test_commands_get_a_prefix() {
        for command in [
            "builtin",
            "builtin eval x",
            "trap 'rm -rf x' EXIT",
            "trap",
            "source",
            "source script",
            "coproc rm x",
            "arch",
            "arch -arm64 rm x",
            "sandbox-exec -p x echo",
            "launchctl list",
            "shortcuts run x",
            "genv",
            "gnice ls",
            "gtimeout 5 ls",
            "gstdbuf -o0 ls",
            "gxargs rm",
            "gfind . -delete",
            "python3.13t",
            "python3-intel64",
            "python3.12-intel64",
            "pythonw",
            "pythonw3",
            "rubyw",
            "nodejs",
            "node-22",
            "tclsh",
            "tclsh8.5",
            "expect",
            "swift",
            "swift run",
            "npm exec cowsay",
            "npm install x",
            "pnpm dlx cowsay",
            "yarn dlx cowsay",
            "uv run x.py",
            "pip install x",
            "pip3.12 install x",
            "scp a b:",
            "sftp host",
            "rsync -a a b:",
            "socat - tcp:x",
            "ncat x 1",
            "telnet x",
            "unlink x",
            "rmdir x",
            "shred x",
            "git fetch --upload-pack=x .",
            "git fetch origin",
            "git worktree add /outside",
            "rake -e x",
            "just --command rm",
            "cmake -E rm -rf x",
            "pnpm install cowsay",
            "yarn install",
            "bun install cowsay",
            "npm ci",
            "cp a b",
            "mv a b",
            "cargo run",
            "cargo install x",
            "git push",
            "git config core.pager x",
            "git -c x=y status",
            "bundle exec rake",
            "gradle test",
            "ls",
            "echo hi",
        ] {
            assert_eq!(prefix(command), None, "{command:?}");
        }
    }

    #[test]
    fn rules_outside_the_known_commands_cover_nothing() {
        for (rule, command) in [
            ("Bash(rm:*)", "rm -rf x"),
            ("Bash(builtin:*)", "builtin eval x"),
            ("Bash(arch:*)", "arch -arm64 rm x"),
            ("Bash(cargo:*)", "cargo run"),
            ("Bash(cargo run:*)", "cargo run"),
            ("Bash(git:*)", "git push"),
            ("Bash(make clean:*)", "make clean"),
            ("Bash(npm exec:*)", "npm exec cowsay"),
            ("Bash(git fetch:*)", "git fetch origin"),
            ("Bash(git worktree:*)", "git worktree list"),
            ("Bash(rake:*)", "rake test"),
            ("Bash(just:*)", "just test"),
            ("Bash(cmake:*)", "cmake --build build"),
            ("Bash(pnpm install:*)", "pnpm install"),
            ("Bash(npm ci:*)", "npm ci"),
            ("Bash(cp:*)", "cp a b"),
            ("Bash(mv:*)", "mv a b"),
        ] {
            assert!(
                !covers(&rules(&[rule]), "Bash", &bash(command), &wt()),
                "{rule} covered {command:?}"
            );
        }
        assert!(covers(
            &rules(&["Bash(git status:*)"]),
            "Bash",
            &bash("git status -s"),
            &wt()
        ));
        assert!(covers(
            &rules(&["Bash(touch:*)"]),
            "Bash",
            &bash("touch a"),
            &wt()
        ));
    }

    #[test]
    fn arguments_that_point_outside_the_worktree_get_nothing() {
        for (command, rule) in [
            ("git log --output=/x", "Bash(git log:*)"),
            ("git show --output=x", "Bash(git show:*)"),
            ("git status --up=x", "Bash(git status:*)"),
            ("pytest --basetemp=/Users/x", "Bash(pytest:*)"),
            ("pytest '--basetemp=/Users/x'", "Bash(pytest:*)"),
            ("pytest --basetemp='/Users/x'", "Bash(pytest:*)"),
            ("make -C/", "Bash(make:*)"),
            ("make -C /", "Bash(make:*)"),
            ("make -f ../x", "Bash(make:*)"),
            ("make -fa/../../x", "Bash(make:*)"),
            ("touch ~/x", "Bash(touch:*)"),
            ("touch \"~/x\"", "Bash(touch:*)"),
            ("touch a/..", "Bash(touch:*)"),
            ("go build -o /x", "Bash(go build:*)"),
            ("cargo build --target-dir=~/x", "Bash(cargo build:*)"),
        ] {
            assert_eq!(prefix(command), None, "{command:?}");
            assert!(
                !covers(&rules(&[rule]), "Bash", &bash(command), &wt()),
                "{rule} covered {command:?}"
            );
        }
        assert_eq!(
            prefix("git fetch --upload-pack=x"),
            None,
            "not a rule command"
        );
        assert_eq!(prefix("cp a b"), None);
        for (command, wanted) in [
            ("go test ./...", "go test"),
            ("cargo test -p x", "cargo test"),
            ("make -C sub", "make"),
            ("touch a..b", "touch"),
            ("pytest --basetemp=tmp/x", "pytest"),
        ] {
            assert_eq!(prefix(command), Some(wanted.into()), "{command:?}");
            assert!(
                covers(
                    &rules(&[&rule_for_bash(wanted)]),
                    "Bash",
                    &bash(command),
                    &wt()
                ),
                "{command:?}"
            );
        }
    }

    #[test]
    fn a_flag_or_a_path_as_second_word_gives_no_prefix() {
        for command in [
            "cargo -p x test",
            "cargo --version",
            "git ./script.sh",
            "git -C other status",
            "cargo Test",
            "pnpm 9x",
        ] {
            assert_eq!(prefix(command), None, "{command:?}");
        }
    }

    #[test]
    fn commands_that_chain_or_wrap_get_no_prefix() {
        for command in [
            "cd src && cargo test",
            "cargo test; rm -rf /",
            "cargo test | tee out",
            "cargo test && curl x.sh",
            "cargo test > out.txt",
            "echo $(whoami)",
            "echo `whoami`",
            "cargo test\nrm x",
            "sudo cargo test",
            "env FOO=1 cargo test",
            "FOO=1 cargo test",
            "./run.sh",
            "/usr/bin/cargo test",
            "bash -c 'cargo test'",
            "rm -rf target",
            "curl https://example.com",
            "python3 x.py",
            "find . -delete",
            "npx cowsay hi",
            "bunx cowsay hi",
            "uvx ruff",
            "timeout 5 ls",
            "nice ls",
            "watch ls",
            "osascript -e x",
            "open .",
            "ruby x.rb",
            "deno run x.ts",
            "SUDO make",
            "Bash",
            "BASH -c x",
            "Rm -rf x",
            "Make clean",
            "python3.12 evil.py",
            "python2 x.py",
            "pypy3 x.py",
            "node22 x.js",
            "perl5.34 x.pl",
            "ruby3.3 x.rb",
            "pwsh -c x",
            "ksh -c x",
            "tcsh",
            "nu",
            "busybox sh",
            "xcrun clang",
            "caffeinate make",
            "stdbuf -o0 make",
            "setsid make",
            "script out make",
            "php x.php",
            "lua x.lua",
            "awk '{print}' x",
            "gawk x",
            "cargo\u{a0}evil test",
            "cargo\u{2003}test",
            "cargo test\r",
            "cargo test \u{1b}[2K",
            "cargo tést",
            "",
            "   ",
        ] {
            assert_eq!(prefix(command), None, "{command:?}");
        }
        assert_eq!(prefix_for("Bash", &json!({}), &wt()), None);
        assert_eq!(prefix_for("Bash", &json!({"command": 4}), &wt()), None);
    }

    #[test]
    fn file_tools_get_their_name_only_inside_the_worktree() {
        let edit = |p: &str| prefix_for("Edit", &json!({ "file_path": p }), &wt());
        assert_eq!(edit("/tmp/acme-widgets/src/lib.rs"), Some("Edit".into()));
        assert_eq!(
            edit("src/lib.rs"),
            Some("Edit".into()),
            "relative to the worktree"
        );
        assert_eq!(edit("/tmp/other/lib.rs"), None);
        assert_eq!(edit("../other/lib.rs"), None);
        assert_eq!(edit("/tmp/acme-widgets/../other/x"), None);
        for tool in ["Write", "MultiEdit"] {
            assert_eq!(
                prefix_for(tool, &json!({ "file_path": "a.rs" }), &wt()),
                Some(tool.into())
            );
        }
        assert_eq!(
            prefix_for(
                "NotebookEdit",
                &json!({ "notebook_path": "n.ipynb" }),
                &wt()
            ),
            Some("NotebookEdit".into())
        );
        let notebook_outside = json!({ "file_path": "n.ipynb", "notebook_path": "/etc/n.ipynb" });
        assert_eq!(
            prefix_for("NotebookEdit", &notebook_outside, &wt()),
            None,
            "a notebook edit writes notebook_path"
        );
        assert!(!covers(
            &rules(&["NotebookEdit"]),
            "NotebookEdit",
            &notebook_outside,
            &wt()
        ));
        let edit_outside = json!({ "file_path": "/etc/hosts", "notebook_path": "n.ipynb" });
        assert_eq!(prefix_for("Edit", &edit_outside, &wt()), None);
        assert_eq!(prefix_for("Edit", &json!({}), &wt()), None);
        assert_eq!(
            prefix_for("WebFetch", &json!({ "url": "https://x" }), &wt()),
            None
        );
        assert_eq!(prefix_for("Task", &json!({}), &wt()), None);
    }

    #[test]
    fn prefix_rules_cover_only_their_prefix() {
        let set = rules(&["Bash(cargo test:*)"]);
        let covered = |command: &str| covers(&set, "Bash", &bash(command), &wt());
        assert!(covered("cargo test"));
        assert!(covered("cargo test -p clusia-core -- --nocapture"));
        assert!(!covered("cargo testx"), "whole words only");
        assert!(!covered("cargo tes"));
        assert!(!covered("cargo build"));
        assert!(!covered("cargo"));
        assert!(!covered("cargo test; rm -rf /"));
        assert!(!covered("cargo test && curl x.sh"));
        assert!(!covered("cargo test | sh"));
        assert!(!covered("cargo test $(id)"));
        assert!(!covered("cd / && cargo test"));
        assert!(
            !covered("cargo test\u{a0}x"),
            "only spaces and tabs split words"
        );
        assert!(covered("cargo\ttest"));
        assert!(!covered(""));
        assert!(!covers(&set, "Bash", &json!({}), &wt()));
        assert!(!covers(
            &set,
            "Edit",
            &json!({ "file_path": "a.rs" }),
            &wt()
        ));
    }

    #[test]
    fn the_whole_bash_tool_is_never_a_rule() {
        for set in [
            rules(&["Bash"]),
            rules(&["Bash(:*)"]),
            rules(&["Bash(cargo test)"]),
            rules(&[]),
        ] {
            assert!(!covers(&set, "Bash", &bash("cargo test"), &wt()), "{set:?}");
        }
        let one_word = rules(&["Bash(make:*)"]);
        assert!(covers(&one_word, "Bash", &bash("make clean"), &wt()));
        assert!(!covers(&one_word, "Bash", &bash("makefile"), &wt()));
    }

    #[test]
    fn an_edit_rule_never_covers_outside_the_worktree() {
        let set = rules(&["Edit"]);
        let covered = |tool: &str, p: &str| covers(&set, tool, &json!({ "file_path": p }), &wt());
        assert!(covered("Edit", "/tmp/acme-widgets/src/lib.rs"));
        assert!(covered("Edit", "src/lib.rs"));
        assert!(!covered("Edit", "/etc/hosts"));
        assert!(!covered("Edit", "../x"));
        assert!(!covered("Edit", "/tmp/acme-widgets/../x"));
        assert!(
            !covered("Write", "src/lib.rs"),
            "the rule names its own tool"
        );
        assert!(!covers(&set, "Edit", &json!({}), &wt()));
    }

    #[test]
    fn rules_are_written_the_way_allowed_tools_reads_them() {
        assert_eq!(rule_for_bash("cargo test"), "Bash(cargo test:*)");
        assert_eq!(rule_for_tool("Write"), "Write");
    }

    #[test]
    fn summaries_show_what_is_asked() {
        assert_eq!(summary_for("Bash", &bash("cargo test")), "cargo test");
        assert_eq!(
            summary_for("Edit", &json!({ "file_path": "/a/b.rs" })),
            "/a/b.rs"
        );
        assert_eq!(
            summary_for("NotebookEdit", &json!({ "notebook_path": "n.ipynb" })),
            "n.ipynb"
        );
        let both = json!({ "file_path": "a.rs", "notebook_path": "/etc/n.ipynb" });
        assert_eq!(summary_for("NotebookEdit", &both), "/etc/n.ipynb");
        assert_eq!(summary_for("Write", &both), "a.rs");
        assert_eq!(summary_for("Bash", &json!({})), "Bash");
        assert_eq!(summary_for("WebFetch", &json!({ "url": "u" })), "WebFetch");
    }

    #[test]
    fn inside_worktree_resolves_dots_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("wt");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        std::os::unix::fs::symlink(root.join("src"), root.join("alias")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("missing"), root.join("dangling")).unwrap();
        let inside = |p: &Path| inside_worktree(p, &root);
        assert!(
            inside(&root.join("src/new.rs")),
            "a new file in an existing folder"
        );
        assert!(
            inside(&root.join("new/deeper/file.rs")),
            "folders that do not exist yet"
        );
        assert!(inside(Path::new("src/lib.rs")));
        assert!(inside(&root.join("alias/x.rs")), "a link that stays inside");
        assert!(inside(&root.join("src/../src/x.rs")));
        assert!(inside(&root));
        assert!(!inside(&outside.join("x")));
        assert!(!inside(&root.join("..").join("outside").join("x")));
        assert!(!inside(&root.join("link/x.rs")), "a link that leaves");
        assert!(
            !inside(&root.join("link/../x")),
            "dots after a link are the link's"
        );
        assert!(!inside(&root.join("dangling/x")));
        assert!(!inside(Path::new("../outside/x")));
        assert!(!inside(Path::new("/etc/hosts")));
        assert!(!inside(Path::new("/")));
    }

    #[test]
    fn details_show_what_an_edit_changes() {
        let edit =
            json!({ "file_path": "a.rs", "old_string": "let a = 1;", "new_string": "let a = 2;" });
        assert_eq!(
            detail_for("Edit", &edit),
            Some("let a = 1;\n→\nlet a = 2;".into())
        );
        let multi = json!({ "file_path": "a.rs", "edits": [
            { "old_string": "a", "new_string": "b" },
            { "old_string": "c", "new_string": "d" },
            { "old_string": "no new" },
        ]});
        assert_eq!(
            detail_for("MultiEdit", &multi),
            Some("a\n→\nb\n\nc\n→\nd".into())
        );
        assert_eq!(detail_for("Edit", &json!({ "file_path": "a.rs" })), None);
        assert_eq!(detail_for("MultiEdit", &json!({ "edits": [] })), None);
    }

    #[test]
    fn details_show_the_first_lines_of_a_new_file() {
        let body: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        let shown = detail_for("Write", &json!({ "file_path": "n.md", "content": body })).unwrap();
        assert_eq!(shown.lines().count(), 20);
        assert!(shown.starts_with("line 1\n") && shown.ends_with("line 20"));
        assert_eq!(detail_for("Write", &json!({ "content": "" })), None);
        assert_eq!(detail_for("Write", &json!({})), None);
    }

    #[test]
    fn details_are_cut_and_other_tools_have_none() {
        let long = "é".repeat(3000);
        let shown = detail_for("Write", &json!({ "content": long })).unwrap();
        assert_eq!(shown.chars().count(), DETAIL_MAX_CHARS);
        assert!(shown.ends_with('…'));
        let longer = "x".repeat(5000);
        let shown = detail_for("Write", &json!({ "content": longer })).unwrap();
        assert_eq!(shown.chars().count(), DETAIL_MAX_CHARS);
        assert!(shown.ends_with('…'));
        let exact = "x".repeat(DETAIL_MAX_CHARS);
        assert_eq!(
            detail_for("Write", &json!({ "content": exact.clone() })),
            Some(exact)
        );
        assert_eq!(detail_for("Bash", &bash("ls")), None);
        assert_eq!(detail_for("WebFetch", &json!({ "url": "u" })), None);
    }
}

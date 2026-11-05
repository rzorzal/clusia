//! A scenario run's pieces: the run command checked and rewritten so it holds no value, the
//! values as environment, the read-only run folder that holds the approved bytes, and the marked
//! report lines.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;

use clusia_core::cockpit::{
    COMMAND_MAX, ERRORS_MAX, ErrorLine, Input, OUTPUTS_MAX, REPORT_TEXT_MAX, env_value_of,
    valid_script_name,
};
use clusia_core::printable::printable;

use crate::recipe::{check_text, placeholder_problem};

/// The word of a `{word}` placeholder at the start of `text`: `[a-z_][a-z0-9_]*` in braces.
fn placeholder_at(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('{')?;
    let end = rest.find('}')?;
    let word = &rest[..end];
    let mut bytes = word.bytes();
    let first = bytes.next()?;
    (matches!(first, b'a'..=b'z' | b'_')
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_')))
    .then_some(word)
}

/// The run command's problem, if any: one line of plain text whose placeholders are `{script}`,
/// `{dir}` and the inputs' names, each a shell word of its own outside quotes.
pub fn check_run(run: &str, inputs: &[Input]) -> Result<(), String> {
    if let Some(problem) = check_text("run", run, COMMAND_MAX) {
        return Err(problem);
    }
    if run.contains(['\n', '\r']) {
        return Err("The command must be one line".to_string());
    }
    let mut rest = run;
    while let Some(at) = rest.find('{') {
        let tail = &rest[at..];
        match placeholder_at(tail) {
            Some(word) => {
                let known =
                    word == "script" || word == "dir" || inputs.iter().any(|i| i.name == word);
                if !known {
                    return Err(format!(
                        "The command uses {{{word}}}, which is not an input"
                    ));
                }
                rest = &tail[word.len() + 2..];
            }
            None => rest = &tail[1..],
        }
    }
    let words = ["script", "dir"]
        .into_iter()
        .chain(inputs.iter().map(|i| i.name.as_str()));
    for word in words {
        let placeholder = format!("{{{word}}}");
        if run.contains(&placeholder)
            && let Some(problem) = placeholder_problem("run", run, &placeholder)
        {
            return Err(problem);
        }
    }
    Ok(())
}

/// The command as it runs: every placeholder becomes a quoted reference to an environment
/// variable, in one pass so a value that holds a placeholder is never expanded again.
pub fn rewrite_run(run: &str, inputs: &[Input], first_script: &str) -> Result<String, String> {
    check_run(run, inputs)?;
    if !valid_script_name(first_script) {
        return Err(format!("Clúsia won't run a script named {first_script}"));
    }
    let mut out = String::with_capacity(run.len() + 64);
    let mut rest = run;
    while let Some(at) = rest.find('{') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        match placeholder_at(tail) {
            Some(word) => {
                match word {
                    "script" => out.push_str(&format!("\"$CLUSIA_SCENARIO_DIR/{first_script}\"")),
                    "dir" => out.push_str("\"$CLUSIA_SCENARIO_DIR\""),
                    name => out.push_str(&format!("\"$CLUSIA_ARG_{}\"", name.to_ascii_uppercase())),
                }
                rest = &tail[word.len() + 2..];
            }
            None => {
                out.push('{');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// The environment of a scenario run, after the recipe's: the mark, the root, the run folder and
/// one `CLUSIA_ARG_<NAME>` per input. A value whose name is not an input is dropped.
pub fn scenario_env(
    mark: &str,
    root: &str,
    dir: &str,
    inputs: &[Input],
    values: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("CLUSIA_MARK".to_string(), mark.to_string()),
        ("CLUSIA_ROOT".to_string(), root.to_string()),
        ("CLUSIA_SCENARIO_DIR".to_string(), dir.to_string()),
    ];
    for input in inputs {
        let value = values.get(&input.name).unwrap_or(&input.default);
        env.push((
            format!("CLUSIA_ARG_{}", input.name.to_ascii_uppercase()),
            env_value_of(input.kind, value),
        ));
    }
    env
}

fn invalid(text: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, text.into())
}

/// Makes `dir` (which must not exist) hold `files` read-only, in a read-only folder: the very
/// bytes a run executes, out of reach of anything the run itself writes. A failure removes what
/// was made.
pub fn write_run_folder(dir: &Path, files: &[(String, Vec<u8>)]) -> io::Result<()> {
    if files.is_empty() {
        return Err(invalid("a run folder needs at least one file"));
    }
    let mut seen = BTreeSet::new();
    for (name, _) in files {
        if !valid_script_name(name) {
            return Err(invalid(format!("Clúsia won't run a script named {name}")));
        }
        if !seen.insert(name.as_str()) {
            return Err(invalid(format!("{name} is listed twice")));
        }
    }
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    DirBuilder::new().mode(0o700).create(dir)?;
    let written = write_files(dir, files);
    if written.is_err() {
        let _ = remove_run_folder(dir);
    }
    written
}

fn write_files(dir: &Path, files: &[(String, Vec<u8>)]) -> io::Result<()> {
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    for (name, bytes) in files {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join(name))?;
        file.write_all(bytes)?;
        file.set_permissions(fs::Permissions::from_mode(0o444))?;
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o555))
}

/// Removes a run folder `write_run_folder` made. A missing folder is fine; a link or a file is
/// refused untouched. The folder is opened without following a link and its entries are
/// unlinked through that handle, so nothing outside it is ever reached.
pub fn remove_run_folder(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
        Ok(meta) if !meta.file_type().is_dir() => {
            return Err(invalid(format!("not a run folder: {}", dir.display())));
        }
        Ok(_) => {}
    }
    let handle: File = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)?;
    handle.set_permissions(fs::Permissions::from_mode(0o700))?;
    for entry in fs::read_dir(dir)? {
        let name = CString::new(entry?.file_name().as_bytes())
            .map_err(|_| invalid("a file name holds a NUL"))?;
        // SAFETY: `handle` is an open folder and `name` a NUL-terminated name inside it;
        // `unlinkat` without `AT_REMOVEDIR` removes a file or a link and never follows one.
        let status = unsafe { libc::unlinkat(handle.as_raw_fd(), name.as_ptr(), 0) };
        if status != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    fs::remove_dir(dir)
}

/// What a scenario printed on its marked lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScenarioReport {
    pub outputs: BTreeMap<String, String>,
    pub errors: Vec<ErrorLine>,
}

/// The text after `<mark> ` of one stdout line, without its line end.
pub(crate) fn marked<'a>(mark: &str, line: &'a str) -> Option<&'a str> {
    let line = line.strip_suffix('\n').unwrap_or(line);
    let line = line.strip_suffix('\r').unwrap_or(line);
    line.strip_prefix(mark)?.strip_prefix(' ')
}

fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn output_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= 40
        && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-'))
}

/// Reads one stdout line: `output <name> <value>` and `error <text>`, only when it starts with
/// `<mark> `. Anything else is ignored.
pub fn read_scenario_report(mark: &str, line: &str, at_ms: u64, report: &mut ScenarioReport) {
    let Some(rest) = marked(mark, line) else {
        return;
    };
    if let Some(rest) = rest.strip_prefix("output ") {
        let (name, value) = rest.split_once(' ').unwrap_or((rest, ""));
        if !output_name(name) {
            return;
        }
        if !report.outputs.contains_key(name) && report.outputs.len() >= OUTPUTS_MAX {
            return;
        }
        report
            .outputs
            .insert(name.to_string(), cut(&printable(value), REPORT_TEXT_MAX));
    } else if let Some(text) = rest.strip_prefix("error ") {
        add_error(&mut report.errors, text, at_ms);
    }
}

/// Counts `text` among the distinct error texts, keeping at most `ERRORS_MAX` of them.
pub(crate) fn add_error(errors: &mut Vec<ErrorLine>, text: &str, at_ms: u64) {
    let text = cut(&printable(text), REPORT_TEXT_MAX);
    if text.is_empty() {
        return;
    }
    if let Some(line) = errors.iter_mut().find(|e| e.text == text) {
        line.count = line.count.saturating_add(1);
    } else if errors.len() < ERRORS_MAX {
        errors.push(ErrorLine {
            text,
            count: 1,
            first_ms: at_ms,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use clusia_core::cockpit::{ERRORS_MAX, ErrorLine, Input, InputKind, OUTPUTS_MAX};

    use super::*;

    const MARK: &str = "0123456789abcdef";

    fn input(name: &str, kind: InputKind, default: &str, choices: &[&str]) -> Input {
        Input {
            name: name.into(),
            label: name.into(),
            kind,
            choices: choices.iter().map(|c| c.to_string()).collect(),
            default: default.into(),
        }
    }

    fn inputs() -> Vec<Input> {
        vec![
            input("callers", InputKind::Integer, "2", &[]),
            input("mode", InputKind::OneOf, "fast", &["fast", "slow"]),
        ]
    }

    #[test]
    fn scenario_commands_take_placeholders_as_words_only() {
        let inputs = inputs();
        assert_eq!(
            check_run(
                "python3 {script} --callers {callers} --mode {mode}",
                &inputs
            ),
            Ok(())
        );
        assert_eq!(check_run("cd {dir} && sh {script}", &inputs), Ok(()));
        assert_eq!(
            check_run("python3 -c 'print(1)'", &inputs),
            Ok(()),
            "no placeholder at all"
        );
        for (run, problem) in [
            ("", "run must be between 1 and 2000 characters"),
            ("python3 {script}\nrm -rf ~", "The command must be one line"),
            (
                "python3 {script} {count}",
                "The command uses {count}, which is not an input",
            ),
            (
                "echo ${home}",
                "The command uses {home}, which is not an input",
            ),
            (
                "sh {dir}/run.sh",
                "run must use {dir} as a word of its own, outside quotes",
            ),
            (
                "python3 \"{script}\"",
                "run must use {script} as a word of its own, outside quotes",
            ),
            (
                "python3 {script} --n={callers}",
                "run must use {callers} as a word of its own, outside quotes",
            ),
            (
                "echo ${callers}",
                "run must use {callers} as a word of its own, outside quotes",
            ),
            (
                "python3 {script} '{mode}'",
                "run must use {mode} as a word of its own, outside quotes",
            ),
            (
                "cat <<EOF {mode}",
                "run must use {mode} as a word of its own, outside quotes",
            ),
        ] {
            assert_eq!(check_run(run, &inputs), Err(problem.to_string()), "{run}");
        }
    }

    #[test]
    fn the_rewrite_runs_in_one_pass_and_holds_no_value() {
        let inputs = inputs();
        assert_eq!(
            rewrite_run("python3 {script} --callers {callers} --mode {mode}", &inputs, "run.py"),
            Ok(r#"python3 "$CLUSIA_SCENARIO_DIR/run.py" --callers "$CLUSIA_ARG_CALLERS" --mode "$CLUSIA_ARG_MODE""#.to_string())
        );
        assert_eq!(
            rewrite_run("cd {dir} && sh {script}", &inputs, "run.sh"),
            Ok(r#"cd "$CLUSIA_SCENARIO_DIR" && sh "$CLUSIA_SCENARIO_DIR/run.sh""#.to_string())
        );
        assert_eq!(
            rewrite_run("python3 {script}", &inputs, "../evil.py"),
            Err("Clúsia won't run a script named ../evil.py".to_string())
        );
        assert_eq!(
            rewrite_run("python3 {script} {count}", &inputs, "run.py"),
            Err("The command uses {count}, which is not an input".to_string())
        );
    }

    #[test]
    fn inputs_reach_the_script_only_as_env() {
        let hostile = "$(touch /tmp/pwned)'; rm -rf ~ #";
        let inputs = vec![
            input("callers", InputKind::Integer, "2", &[]),
            input("wait", InputKind::Duration, "200 ms", &[]),
            input("mode", InputKind::OneOf, "fast", &["fast", "slow"]),
            input("note", InputKind::String, "hello", &[]),
        ];
        let values: BTreeMap<String, String> = [
            ("callers", " 4 "),
            ("wait", "30 s"),
            ("note", hostile),
            ("extra", "dropped"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let pairs = |list: &[(&str, &str)]| -> Vec<(String, String)> {
            list.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(
            scenario_env(MARK, "/work", "/clusia-scenario", &inputs, &values),
            pairs(&[
                ("CLUSIA_MARK", MARK),
                ("CLUSIA_ROOT", "/work"),
                ("CLUSIA_SCENARIO_DIR", "/clusia-scenario"),
                ("CLUSIA_ARG_CALLERS", "4"),
                ("CLUSIA_ARG_WAIT", "30000"),
                ("CLUSIA_ARG_MODE", "fast"),
                ("CLUSIA_ARG_NOTE", hostile),
            ])
        );
        let command = rewrite_run("python3 {script} {note}", &inputs, "run.py").unwrap();
        assert!(!command.contains("pwned"), "{command}");
        assert_eq!(
            command,
            r#"python3 "$CLUSIA_SCENARIO_DIR/run.py" "$CLUSIA_ARG_NOTE""#
        );
    }

    #[test]
    fn the_run_folder_holds_read_only_copies_and_goes_without_following_links() {
        let top = tempfile::tempdir().unwrap();
        let folder = top.path().join("runs/acme~widgets~7/1762300000-4-0");
        let files = vec![
            ("run.py".to_string(), b"print('two refreshes')\n".to_vec()),
            ("data.json".to_string(), b"{\"callers\": 2}".to_vec()),
        ];
        write_run_folder(&folder, &files).unwrap();
        for (name, bytes) in &files {
            let path = folder.join(name);
            assert_eq!(&fs::read(&path).unwrap(), bytes);
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o444
            );
        }
        assert_eq!(
            fs::metadata(&folder).unwrap().permissions().mode() & 0o777,
            0o555
        );
        assert_eq!(
            write_run_folder(&folder, &files).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists,
            "a folder is never reused"
        );
        for bad in [
            vec![("../x.py".to_string(), b"x".to_vec())],
            vec![
                ("run.py".to_string(), b"a".to_vec()),
                ("run.py".to_string(), b"b".to_vec()),
            ],
            vec![],
        ] {
            let other = top.path().join("runs/acme~widgets~7/other");
            assert_eq!(
                write_run_folder(&other, &bad).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert!(!other.exists());
        }
        remove_run_folder(&folder).unwrap();
        assert!(!folder.exists());
        remove_run_folder(&folder).unwrap();

        let keep = top.path().join("keep");
        fs::create_dir(&keep).unwrap();
        fs::write(keep.join("a.txt"), "mine").unwrap();
        let link = top.path().join("runs/link");
        symlink(&keep, &link).unwrap();
        assert_eq!(
            remove_run_folder(&link).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(fs::read_to_string(keep.join("a.txt")).unwrap(), "mine");
    }

    #[test]
    fn report_lines_keep_bounded_outputs_and_grouped_errors() {
        let mut report = ScenarioReport::default();
        let mut read = |line: String, at: u64| read_scenario_report(MARK, &line, at, &mut report);
        read(format!("{MARK} output exchanges 1\n"), 10);
        read(format!("{MARK} output exchanges 2\r\n"), 20);
        read(format!("{MARK} output status"), 21);
        read(format!("{MARK} output Bad-Name 3"), 22);
        read("ffffffffffffffff output stolen 1".to_string(), 23);
        read(format!("{MARK}output glued 1"), 24);
        read(format!("{MARK} took 12"), 25);
        read(format!("{MARK} error token expired"), 30);
        read(format!("{MARK} error token expired"), 40);
        read(format!("{MARK} error \u{1b}[31mred"), 50);
        read(format!("{MARK} error "), 55);
        for i in 0..30 {
            read(format!("{MARK} output n{i} {i}"), 60);
            read(format!("{MARK} error e{i}"), 70);
        }
        read(format!("{MARK} error {}", "é".repeat(1000)), 80);
        assert_eq!(report.outputs.len(), OUTPUTS_MAX);
        assert_eq!(report.outputs["exchanges"], "2", "the last value wins");
        assert_eq!(report.outputs["status"], "");
        assert!(report.outputs.contains_key("n17") && !report.outputs.contains_key("n18"));
        assert!(!report.outputs.contains_key("stolen") && !report.outputs.contains_key("Bad-Name"));
        assert_eq!(report.errors.len(), ERRORS_MAX);
        assert_eq!(
            report.errors[..2],
            [
                ErrorLine {
                    text: "token expired".into(),
                    count: 2,
                    first_ms: 30
                },
                ErrorLine {
                    text: "\\u{1b}[31mred".into(),
                    count: 1,
                    first_ms: 50
                },
            ]
        );
        assert_eq!(report.errors.last().unwrap().text, "e17");

        let mut errors = vec![ErrorLine {
            text: "x".into(),
            count: u32::MAX,
            first_ms: 1,
        }];
        add_error(&mut errors, "x", 9);
        assert_eq!(errors[0].count, u32::MAX, "the count saturates");
        let mut long = Vec::new();
        add_error(&mut long, &"é".repeat(1000), 1);
        assert_eq!(long[0].text.chars().count(), 300);
    }
}

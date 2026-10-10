mod agent;
mod cli;
mod install;
mod review;
mod run;
mod spawn;

use std::process::ExitCode;

use clap::Parser;
use clusia_core::Paths;

use crate::run::CliError;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    let home = match cli.home.as_deref().map(std::path::absolute).transpose() {
        Ok(home) => home,
        Err(e) => {
            return fail(
                cli.json,
                &CliError::Other(format!("cannot resolve --home: {e}")),
            );
        }
    };
    let paths = match &home {
        Some(home) => Paths::new(home),
        None => match Paths::from_env() {
            Ok(p) => p,
            Err(e) => return fail(cli.json, &CliError::Other(e.to_string())),
        },
    };
    if let Err(e) = clusia_protocol::launcher::check_socket_path(&paths) {
        return fail(cli.json, &e.into());
    }
    match run::run(&paths, home.as_deref(), cli.command, cli.json).await {
        Ok(out) => {
            // A command that streamed its output already printed everything.
            if !out.json.is_null() {
                if cli.json {
                    println!("{}", out.json);
                } else {
                    println!("{}", shown(&out.human));
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(cli.json, &e),
    }
}

/// An error as the terminal shows it: its message may carry the daemon's or the agent's text.
fn error_line(e: &CliError) -> String {
    shown(&format!("clusia: {e}"))
}

/// What the daemon sends (titles, messages, the agent's log) without anything that could
/// repaint the terminal or disguise what is read.
fn shown(text: &str) -> String {
    agent::streamed(text)
}

fn fail(json: bool, e: &CliError) -> ExitCode {
    if json {
        eprintln!(
            "{}",
            serde_json::json!({ "error": { "kind": e.kind(), "message": e.to_string() } })
        );
    } else {
        eprintln!("{}", error_line(e));
    }
    ExitCode::from(e.exit_code())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_from_the_daemon_cannot_repaint_the_terminal() {
        let e = CliError::Other("no\u{1b}[2K\u{202e} such\nreview".into());
        assert_eq!(error_line(&e), "clusia: no[2K such\nreview");
    }

    #[test]
    fn output_from_the_daemon_cannot_repaint_the_terminal() {
        assert_eq!(shown("a\u{1b}[2K\u{202e}b\n\tc"), "a[2Kb\n\tc");
    }
}

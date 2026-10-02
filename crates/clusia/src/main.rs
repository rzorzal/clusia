mod cli;
mod run;
mod spawn;

use std::process::ExitCode;

use clap::Parser;
use clusia_core::Paths;

use crate::run::CliError;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    let paths = match &cli.home {
        Some(home) => Paths::new(home),
        None => match Paths::from_env() {
            Ok(p) => p,
            Err(e) => return fail(cli.json, &CliError::Other(e.to_string())),
        },
    };
    match run::run(&paths, cli.home.as_deref(), cli.command).await {
        Ok(out) => {
            if cli.json {
                println!("{}", out.json);
            } else {
                println!("{}", out.human);
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(cli.json, &e),
    }
}

fn fail(json: bool, e: &CliError) -> ExitCode {
    if json {
        eprintln!(
            "{}",
            serde_json::json!({ "error": { "kind": e.kind(), "message": e.to_string() } })
        );
    } else {
        eprintln!("clusia: {e}");
    }
    ExitCode::from(e.exit_code())
}

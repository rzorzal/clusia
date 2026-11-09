//! Command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "clusia", version, about = "Clúsia — PR Reviews for Humans")]
pub struct Cli {
    /// Data directory (defaults to $CLUSIA_HOME or ~/Library/Application Support/Clusia).
    #[arg(long, global = true, value_name = "DIR")]
    pub home: Option<PathBuf>,
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start, stop or inspect the background daemon.
    #[command(subcommand)]
    Daemon(DaemonCommand),
    /// Read or change settings.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// List pull requests: assigned to me and mine (both unless filtered).
    Prs {
        /// Only pull requests where my review is requested.
        #[arg(long, conflicts_with = "mine")]
        assigned: bool,
        /// Only pull requests I authored.
        #[arg(long)]
        mine: bool,
    },
    /// Refresh pull requests from GitHub now.
    Sync,
    /// GitHub authentication.
    #[command(subcommand)]
    Auth(AuthCommand),
    /// Prepare a local worktree for a pull request and print its path.
    Worktree {
        /// `owner/repo#number` or a pull request URL.
        pr: String,
    },
    /// Open a pull request for review: refresh, prepare the worktree, show what's new.
    Open {
        /// `owner/repo#number` or a pull request URL.
        pr: String,
    },
    /// Work on a review's draft and publish it.
    #[command(subcommand)]
    Review(ReviewCommand),
    /// Your review activity: heatmap and stats.
    Activity,
    /// Ask the review's agent a question and stream its answer.
    Ask {
        /// `owner/repo#number` or a pull request URL.
        pr: String,
        /// The question; several words need no quotes.
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
    },
    /// The review's agent: show its chat log or stop its turn.
    #[command(subcommand)]
    Agent(AgentCommand),
    /// Run the security and audit checks of a review and print what they found.
    Check {
        /// `owner/repo#number` or a pull request URL.
        pr: String,
        /// Only the security check.
        #[arg(long, conflicts_with = "audit")]
        security: bool,
        /// Only the audit.
        #[arg(long)]
        audit: bool,
    },
    /// List the latest version of a review's diagrams, or draw them with `--draw`.
    Diagrams {
        /// `owner/repo#number` or a pull request URL.
        pr: String,
        /// Plan and draw the diagrams now (one turn of the agent each), and follow the drawing.
        #[arg(long)]
        draw: bool,
    },
    /// List a review's Story: the pull request as ordered steps. `--write` has the agent
    /// write it first (or again, when one exists) and follows the run.
    Story {
        /// `owner/repo#number` or a pull request URL.
        pr: String,
        /// Write the story now (one turn of the agent) and follow it.
        #[arg(long)]
        write: bool,
    },
    /// Print the call graph of a changed function: callers above, callees below.
    Calls {
        /// `owner/repo#number` or a pull request URL.
        pr: String,
        /// The function to centre on, by name; the first changed function when left out.
        function: Option<String>,
        /// How many levels of callers and callees to print, 1 to 5; `diff.calls_depth` when left out.
        #[arg(long)]
        depth: Option<u8>,
    },
    /// Have the agent re-read the draft of a saved review against the new commits of its pull
    /// request, now, and print what it found for each comment.
    Revalidate {
        /// `owner/repo#number` or a pull request URL.
        pr: String,
        /// Run it again when the review already has a finished round; without it that round is
        /// printed as it is.
        #[arg(long)]
        again: bool,
    },
    /// The tests of a review: the environment and the last run, or (in this order) generate the
    /// recipe, approve the setup, list the tests and run them.
    Tests {
        /// `owner/repo#number` or a pull request URL.
        pr: String,
        /// Probe the machine and have the agent write the recipe (one turn of the agent).
        #[arg(long)]
        generate: bool,
        /// Show the setup plan and ask before it runs.
        #[arg(long)]
        setup: bool,
        /// Approve without asking: every step of the setup plan, a scenario with
        /// --approve-scenario, or main's plan with --on-main.
        #[arg(long)]
        yes: bool,
        /// Run the tests and stream the results; the exit code is the run's.
        #[arg(long)]
        run: bool,
        /// Run every test (the default for --run).
        #[arg(long, requires = "run", conflicts_with_all = ["near", "one"])]
        all: bool,
        /// Run only the tests near the change.
        #[arg(long, requires = "run", conflicts_with = "one")]
        near: bool,
        /// Run one test, by name.
        #[arg(long, requires = "run", value_name = "NAME")]
        one: Option<String>,
        /// List the tests the recipe's list command finds.
        #[arg(long)]
        list: bool,
        /// List the changed functions, their cases and the last result of each case.
        #[arg(long)]
        functions: bool,
        /// Run the cases of the changed functions and stream the results; the exit code is the
        /// run's (0 passed, 1 failed, 124 timed out, 130 stopped).
        #[arg(long)]
        run_cases: bool,
        /// With --run-cases: only this function, by id or exact name.
        #[arg(long, requires = "run_cases", value_name = "SLUG_OR_NAME")]
        function: Option<String>,
        /// With --function: only this case.
        #[arg(long, requires = "function", value_name = "ID")]
        case: Option<String>,
        /// List the scenarios and benchmarks with their status, the last comparison and the
        /// verdict.
        #[arg(long)]
        scenarios: bool,
        /// Ask the agent to write scenarios from the diff (one turn of the agent).
        #[arg(long)]
        write_scenarios: bool,
        /// With --write-scenarios: one scenario from this text.
        #[arg(long, requires = "write_scenarios", value_name = "TEXT")]
        describe: Option<String>,
        /// Print a scenario's command and scripts and ask before approving them.
        #[arg(long, value_name = "SLUG")]
        approve_scenario: Option<String>,
        /// Run the approved scenarios and stream the rounds.
        #[arg(long)]
        run_scenarios: bool,
        /// With --run-scenarios or --copilot: this scenario only.
        #[arg(long, value_name = "SLUG", conflicts_with = "bench")]
        scenario: Option<String>,
        /// With --run-scenarios or --bench: compare with main, which is prepared first when
        /// needed.
        #[arg(long)]
        on_main: bool,
        /// Run the benchmark of a function, by id or exact name.
        #[arg(long, value_name = "SLUG_OR_NAME", conflicts_with = "scenario")]
        bench: Option<String>,
        /// With --bench: ask the agent to write the benchmark first (one turn of the agent).
        #[arg(long, requires = "bench", conflicts_with = "copilot")]
        write: bool,
        /// Ask the QA copilot; `explain` explains the item named by --scenario or --bench.
        #[arg(long, value_name = "TEXT")]
        copilot: Option<String>,
        /// Print the suggested comment of a scenario.
        #[arg(long, value_name = "SLUG")]
        as_comment: Option<String>,
    },
    /// The system of a review: start the pull request's code in a container, see it, call it
    /// and stop it.
    #[command(subcommand)]
    System(SystemCommand),
    /// Check a custom command against Clúsia's JSON-lines protocol.
    #[command(subcommand)]
    Harness(HarnessCommand),
    /// Put Clúsia.app, the login agent and the `clusia` command in place.
    Install(InstallArgs),
    /// Remove Clúsia.app, the login agent and the `clusia` link; your data stays.
    Uninstall(UninstallArgs),
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Start clusiad in the background.
    Start,
    /// Ask clusiad to stop.
    Stop,
    /// Show whether clusiad is running.
    Status,
}

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Print the chat log of a review's agent.
    Log { pr: String },
    /// Stop the turn the review's agent is running.
    Stop { pr: String },
}

#[derive(Debug, Subcommand)]
pub enum HarnessCommand {
    /// Run a program through the probe and one short turn, and list what passed. It uses one
    /// turn of the program's model.
    Test {
        /// The program to test; the configured custom command when left out.
        program: Option<String>,
        /// Also ask the program to request a harmless command, to check the permission round
        /// trip.
        #[arg(long)]
        permission: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the whole configuration.
    Show,
    /// Print one value, e.g. `github.poll_interval_secs`.
    Get { key: String },
    /// Change one value.
    Set { key: String, value: String },
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Show which token is used and for whom.
    Status,
    /// Store a personal access token in the Keychain. The token is read from stdin.
    Login,
    /// Remove the stored token.
    Logout,
}

#[derive(Debug, Subcommand)]
pub enum ReviewCommand {
    /// List saved reviews, or show one review's draft.
    Status { pr: Option<String> },
    /// Add a line comment at `path:line` or `path:start-end`.
    Comment {
        pr: String,
        location: String,
        body: String,
        /// Comment on the base side (removed lines).
        #[arg(long)]
        left: bool,
    },
    /// Add a general comment (it goes into the review body).
    Note { pr: String, body: String },
    /// Change a draft item's text.
    Edit {
        pr: String,
        id: String,
        body: String,
    },
    /// Remove a draft item.
    Rm { pr: String, id: String },
    /// Publish the draft as one GitHub review.
    Publish {
        pr: String,
        #[arg(long, value_enum)]
        verdict: VerdictArg,
        #[arg(long, default_value = "")]
        summary: String,
    },
    /// Leave the review (kept if it has comments).
    Close { pr: String },
    /// Throw the review away.
    Discard { pr: String },
}

/// What `clusia system` does; every `pr` is `owner/repo#number` or a pull request URL.
#[derive(Debug, Subcommand)]
pub enum SystemCommand {
    /// Show the system section of the setup plan, ask, set up, and start the system; it stays
    /// until `clusia system stop`, the review closes or the daemon stops.
    Start {
        pr: String,
        /// Approve the setup, the system and its test commands without asking.
        #[arg(long)]
        yes: bool,
    },
    /// Stop the system and wait until it is down.
    Stop { pr: String },
    /// Stop the system and start it again; the plan is not asked again.
    Restart { pr: String },
    /// The state, ports, services, network, start command and endpoints of the system. The
    /// exit code is 0 only while it runs.
    Status { pr: String },
    /// Print the last lines of every process's log, or of one.
    Logs {
        pr: String,
        /// Only this process: `start` or `service:<name>`.
        #[arg(long, value_name = "ID")]
        process: Option<String>,
        /// How many lines of each (at most 2000).
        #[arg(long, default_value_t = 200)]
        lines: u32,
        /// Keep printing new lines until the system stops; Ctrl-C ends it and leaves the system
        /// running.
        #[arg(long)]
        follow: bool,
    },
    /// Send one endpoint to the running system, by the number `status` prints or by its id.
    Send { pr: String, endpoint: String },
    /// Allow or refuse the system outbound network for this review; a running system restarts.
    Network {
        pr: String,
        #[arg(value_enum)]
        state: OnOff,
    },
    /// The endpoints the pull request changes; `--write` has the agent read them again.
    Endpoints {
        pr: String,
        /// Ask the agent to read the endpoints from the diff (one turn of the agent).
        #[arg(long)]
        write: bool,
    },
    /// Ask the agent to fix a system that failed to start or is unhealthy (one turn of the
    /// agent).
    Fix { pr: String },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum VerdictArg {
    Approve,
    RequestChanges,
    Comment,
    Close,
}

#[derive(Debug, clap::Args)]
pub struct InstallArgs {
    /// Print what would be done, and change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Use binaries that are already built: a folder with clusia, clusiad, clusia-app and
    /// clusia-tray. Without it the workspace is built in release mode.
    #[arg(long, value_name = "DIR")]
    pub from: Option<PathBuf>,
    /// The workspace to build (default: the current folder).
    #[arg(long, value_name = "DIR", conflicts_with = "from")]
    pub workspace: Option<PathBuf>,
    /// Where Clusia.app goes (default: /Applications, else ~/Applications).
    #[arg(long, value_name = "DIR")]
    pub applications: Option<PathBuf>,
    /// Where the `clusia` link goes (default: /usr/local/bin, else ~/.local/bin).
    #[arg(long, value_name = "DIR")]
    pub bin_dir: Option<PathBuf>,
    /// Where the login agent's plist goes (default: ~/Library/LaunchAgents). Start at login in
    /// Config changes only the agent in the default folder.
    #[arg(long, value_name = "DIR")]
    pub agents_dir: Option<PathBuf>,
    /// Do not touch launchd or stop the running daemon.
    #[arg(long)]
    pub no_launchctl: bool,
}

#[derive(Debug, clap::Args)]
pub struct UninstallArgs {
    /// Where Clusia.app was put (default: /Applications, else ~/Applications).
    #[arg(long, value_name = "DIR")]
    pub applications: Option<PathBuf>,
    /// Where the `clusia` link was put (default: /usr/local/bin, else ~/.local/bin).
    #[arg(long, value_name = "DIR")]
    pub bin_dir: Option<PathBuf>,
    /// Where the login agent's plist was put (default: ~/Library/LaunchAgents).
    #[arg(long, value_name = "DIR")]
    pub agents_dir: Option<PathBuf>,
    /// Do not touch launchd or stop the running daemon.
    #[arg(long)]
    pub no_launchctl: bool,
}

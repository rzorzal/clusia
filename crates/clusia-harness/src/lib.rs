//! The agent adapter for reviews: the command line of one Claude Code turn, a parser for its
//! `stream-json` output and the suggestions in its answer. It starts no process.

mod check_blocks;
mod command;
mod parse;
mod probe;
mod suggestions;
#[cfg(any(test, feature = "test-kit"))]
pub mod testkit;

pub use check_blocks::extract_check_blocks;
pub use command::{BridgeSpec, ClaudeCode, ROLE_PROMPT, SessionArg, TurnSpec};
pub use parse::{AgentEvent, ParseState, parse_line};
pub use probe::{parse_probe, probe_command};
pub use suggestions::extract_suggestions;

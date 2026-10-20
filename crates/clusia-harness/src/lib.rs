//! The agent adapters for reviews: what differs between Claude Code and Codex, the command
//! line of one Claude Code turn, a parser for its `stream-json` output and the suggestions in
//! its answer. It starts no process.

mod agent;
mod check_blocks;
pub mod codex;
mod codex_command;
mod command;
pub mod custom;
mod diagram_blocks;
mod fenced;
mod parse;
mod probe;
mod revalidation_blocks;
mod suggestions;
#[cfg(any(test, feature = "test-kit"))]
pub mod testkit;

pub use agent::{Agent, version_at_least};
pub use check_blocks::extract_check_blocks;
pub use codex_command::{CodexServerSpec, codex_server_command};
pub use command::{BridgeSpec, ClaudeCode, ROLE_PROMPT, SessionArg, TurnSpec};
pub use diagram_blocks::{
    extract_diagram, extract_keyword_placement, extract_keyword_plan, extract_plan, extract_story,
};
pub use parse::{AgentEvent, ParseState, parse_line};
pub use probe::parse_probe;
pub use revalidation_blocks::{RawVerdict, extract_diagrams_stale, extract_revalidation_blocks};
pub use suggestions::extract_suggestions;

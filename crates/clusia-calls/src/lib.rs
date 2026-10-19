//! Call graphs from source text, pure and deterministic: tree-sitter queries give a
//! `CallIndex` of functions, calls and imports, and calls are resolved by name. The caller
//! brings the file text; nothing here reads a file or runs a process.

mod changes;
mod extract;
mod graph;
mod index;
mod packs;
mod resolve;
mod story_order;

#[cfg(test)]
mod testkit;

pub use changes::{ChangeKind, ChangedFunction, changed_functions, signature_text};
pub use graph::{CallGraph, GraphEdge, GraphNode, NodeKind, find, subgraph};
pub use index::{
    Call, CallIndex, Function, FunctionId, Import, Indexer, Skipped, SourceFile,
    dependencies_from_manifests,
};
pub use packs::{LanguagePack, language_name, pack_for, packs};
pub use resolve::{Resolved, Target, resolve};
pub use story_order::{StaticStep, static_order};

/// Seconds the index builder may take before it keeps what it parsed.
pub const INDEX_BUDGET_SECS: u64 = 10;
/// A larger source file is skipped.
pub const MAX_FILE_BYTES: u64 = 524_288;
/// Source files taken into one index.
pub const MAX_FILES: usize = 20_000;
pub const MAX_DEPTH: u8 = 5;
pub const DEFAULT_DEPTH: u8 = 3;
/// Nodes shown per level of a call graph.
pub const PER_LEVEL: usize = 8;
pub const MAX_GRAPH_NODES: usize = 60;
/// Candidates an ambiguous call links to.
pub const MAX_CANDIDATES: usize = 3;
pub const FIND_MAX: usize = 20;

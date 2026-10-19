//! What the window knows of each open review's call graph: the graph the daemon last drew, the
//! index state, the search hits and the code of the functions it was asked for. The bridge fills
//! it from `CallsTell`s.

use std::collections::{HashMap, HashSet, VecDeque};

use bevy::prelude::*;
use clusia_calls::CallGraph;
use clusia_core::PrRef;
use clusia_protocol::{CallsIndexState, DiagramCode, FunctionHit};

use crate::bridge::CallsTell;

/// How many functions' code the model keeps.
pub const MAX_CODES: usize = 64;

/// What `GetCalls` was asked: the centre (`FunctionId.0`; `None` is the first changed function),
/// the depth and the levels whose +N more was expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallsRequest {
    pub center: Option<String>,
    pub depth: u8,
    pub expand: Vec<i8>,
}

/// One review's call graph.
#[derive(Debug, Clone, PartialEq)]
pub struct CallsModel {
    /// `GetCalls` was asked for.
    pub asked: bool,
    pub graph: Option<CallGraph>,
    pub index: CallsIndexState,
    /// Changed source files whose language has no pack.
    pub unsupported: Vec<String>,
    /// The request that waits for its answer.
    pub asking: Option<CallsRequest>,
    /// The request the graph above answers.
    pub shown: Option<CallsRequest>,
    pub hits: Vec<FunctionHit>,
    /// The search text; hits only count for it.
    pub find: String,
    /// The code `ReadFunctionCode` answered, by `FunctionId.0`.
    pub codes: HashMap<String, DiagramCode>,
    /// Functions whose code was asked for and has not arrived; an answer removes the entry.
    pub requested_code: HashSet<String>,
    code_order: VecDeque<String>,
    /// Why the index failed.
    pub failed: Option<String>,
}

impl Default for CallsModel {
    fn default() -> Self {
        Self {
            asked: false,
            graph: None,
            index: CallsIndexState::Idle,
            unsupported: Vec::new(),
            asking: None,
            shown: None,
            hits: Vec::new(),
            find: String::new(),
            codes: HashMap::new(),
            requested_code: HashSet::new(),
            code_order: VecDeque::new(),
            failed: None,
        }
    }
}

impl CallsModel {
    pub fn apply(&mut self, tell: &CallsTell) {
        match tell {
            CallsTell::Graph {
                request,
                graph,
                index,
                unsupported,
                ..
            } => {
                self.graph = graph.as_deref().cloned();
                self.index = index.clone();
                self.unsupported = unsupported.clone();
                self.shown = Some(request.clone());
                if self.asking.as_ref() == Some(request) {
                    self.asking = None;
                }
                self.failed = match index {
                    CallsIndexState::Failed { message } => Some(message.clone()),
                    _ => None,
                };
                self.asked = true;
            }
            CallsTell::Found { query, hits, .. } => {
                if *query == self.find {
                    self.hits = hits.clone();
                }
            }
            CallsTell::Code { function, code, .. } => {
                self.requested_code.remove(function);
                if self.codes.insert(function.clone(), code.clone()).is_none() {
                    self.code_order.push_back(function.clone());
                }
                while self.code_order.len() > MAX_CODES {
                    if let Some(old) = self.code_order.pop_front() {
                        self.codes.remove(&old);
                    }
                }
            }
            CallsTell::Index { state, .. } => {
                let finished = matches!(
                    (&self.index, state),
                    (_, CallsIndexState::Ready { .. }) | (_, CallsIndexState::Failed { .. })
                );
                self.index = state.clone();
                self.failed = match state {
                    CallsIndexState::Failed { message } => Some(message.clone()),
                    _ => None,
                };
                // A finished index replaces what the graph was drawn from.
                if finished {
                    self.asked = false;
                }
            }
        }
    }
}

/// Every review's call graph.
#[derive(Resource, Debug, Default)]
pub struct Calls(pub HashMap<PrRef, CallsModel>);

impl Calls {
    pub fn apply(&mut self, tell: &CallsTell) {
        self.0.entry(tell.pr().clone()).or_default().apply(tell);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Tell;
    use crate::fixture;
    use clusia_calls::NodeKind;

    fn pr() -> PrRef {
        fixture::demo_pr()
    }

    fn graph_tell() -> CallsTell {
        let Tell::Calls(tell) = fixture::demo_calls_graph(None, 3, &[]) else {
            panic!("a calls tell");
        };
        tell
    }

    #[test]
    fn a_graph_fills_the_model_and_ends_the_request_it_answers() {
        let mut m = CallsModel::default();
        let CallsTell::Graph { request, .. } = graph_tell() else {
            panic!("a graph");
        };
        m.asking = Some(request.clone());
        m.apply(&graph_tell());
        assert!(m.asking.is_none() && m.asked);
        assert_eq!(m.shown.as_ref(), Some(&request));
        let graph = m.graph.as_ref().expect("a graph");
        let centre = graph
            .nodes
            .iter()
            .find(|n| n.key == graph.center)
            .expect("the centre node");
        assert_eq!(centre.kind, NodeKind::Changed);
        assert!(matches!(m.index, CallsIndexState::Ready { cut: false, .. }));
        assert!(m.unsupported.is_empty());
    }

    #[test]
    fn an_answer_to_an_older_request_does_not_end_the_newer_one() {
        let mut m = CallsModel {
            asking: Some(CallsRequest {
                center: None,
                depth: 5,
                expand: vec![],
            }),
            ..CallsModel::default()
        };
        m.apply(&graph_tell());
        assert_eq!(
            m.asking.as_ref().map(|r| r.depth),
            Some(5),
            "depth 3 was answered, 5 waits"
        );
    }

    #[test]
    fn a_search_answer_counts_only_for_the_query_shown() {
        let mut m = CallsModel {
            find: "refr".into(),
            ..CallsModel::default()
        };
        let hits = fixture::demo_find("refr");
        assert!(!hits.is_empty());
        m.apply(&CallsTell::Found {
            pr: pr(),
            query: "ref".into(),
            hits: hits.clone(),
        });
        assert!(m.hits.is_empty(), "an answer to an older query is dropped");
        m.apply(&CallsTell::Found {
            pr: pr(),
            query: "refr".into(),
            hits: hits.clone(),
        });
        assert_eq!(m.hits, hits);
    }

    #[test]
    fn code_is_kept_by_function_and_the_oldest_goes_at_the_cap() {
        let mut m = CallsModel::default();
        let code = fixture::demo_function_code("src/auth/refresh.rs#TokenStore::refresh#38");
        m.requested_code.insert("f0".into());
        m.apply(&CallsTell::Code {
            pr: pr(),
            function: "f0".into(),
            code: code.clone(),
        });
        assert_eq!(m.codes["f0"], code);
        assert!(m.requested_code.is_empty(), "an answer ends the request");
        for i in 1..=MAX_CODES {
            m.apply(&CallsTell::Code {
                pr: pr(),
                function: format!("f{i}"),
                code: code.clone(),
            });
        }
        assert_eq!(m.codes.len(), MAX_CODES);
        assert!(!m.codes.contains_key("f0") && m.codes.contains_key(&format!("f{MAX_CODES}")));
    }

    #[test]
    fn the_index_becoming_ready_reads_the_graph_again() {
        let mut m = CallsModel::default();
        m.apply(&graph_tell());
        m.apply(&CallsTell::Index {
            pr: pr(),
            state: CallsIndexState::Building,
        });
        assert!(m.asked, "starting to build reads nothing");
        m.apply(&CallsTell::Index {
            pr: pr(),
            state: CallsIndexState::Ready {
                commit: "9be1d07".into(),
                cut: true,
            },
        });
        assert!(!m.asked, "a finished index replaces the graph");
        m.apply(&CallsTell::Index {
            pr: pr(),
            state: CallsIndexState::Failed {
                message: "boom".into(),
            },
        });
        assert_eq!(m.failed.as_deref(), Some("boom"));
    }

    #[test]
    fn the_resource_keeps_one_model_per_review() {
        let mut calls = Calls::default();
        calls.apply(&CallsTell::Index {
            pr: pr(),
            state: CallsIndexState::Building,
        });
        assert_eq!(calls.0[&pr()].index, CallsIndexState::Building);
    }
}

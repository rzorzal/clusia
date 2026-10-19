//! `clusia calls`: the call graph of a changed function, as lines.

use std::path::Path;
use std::time::Duration;

use clusia_calls::{CallGraph, GraphNode};
use clusia_core::config::CALLS_DEPTH_RANGE;
use clusia_core::{Paths, PrRef};
use clusia_protocol::{
    CallsIndexState, Client, Command as Request, Event, FunctionHit, Reply, topics,
};

use crate::check::line_of;
use crate::review::parse_pr;
use crate::run::{CliError, Output, connect, unexpected};

/// How long the command waits for the daemon to build the call index.
const INDEX_WAIT: Duration = Duration::from_secs(30);
/// How many unsupported files the no-graph message names.
const UNSUPPORTED_SHOWN: usize = 10;

/// `depth` if it is in `CALLS_DEPTH_RANGE`; the usage error otherwise.
fn check_depth(depth: Option<u8>) -> Result<Option<u8>, CliError> {
    match depth {
        Some(d) if !CALLS_DEPTH_RANGE.contains(&d) => Err(CliError::Usage(format!(
            "--depth must be between {} and {}, got {d}",
            CALLS_DEPTH_RANGE.start(),
            CALLS_DEPTH_RANGE.end()
        ))),
        other => Ok(other),
    }
}

/// The hit to centre on, and the note to give when the choice was a guess.
fn pick<'a>(
    query: &str,
    hits: &'a [FunctionHit],
) -> Result<(&'a FunctionHit, Option<String>), String> {
    let Some(first) = hits.first() else {
        return Err(format!("no function matches \"{}\"", line_of(query)));
    };
    if let Some(exact) = hits.iter().find(|h| h.qualified == query) {
        return Ok((exact, None));
    }
    let note = (hits.len() > 1).then(|| {
        format!(
            "{} functions match; showing {} ({}:{})",
            hits.len(),
            line_of(&first.qualified),
            line_of(&first.path),
            first.line
        )
    });
    Ok((first, note))
}

/// One node as a line: the arrow of its level, its label and, when it has one, its subtitle.
fn node_line(node: &GraphNode) -> String {
    let arrow = match node.level {
        0 => "●".to_string(),
        l if l < 0 => format!("↑{}", -l),
        l => format!("↓{l}"),
    };
    let mut line = format!("{arrow}  {}", line_of(&node.label));
    if !node.sub.is_empty() {
        line.push_str(&format!("  {}", line_of(&node.sub)));
    }
    line
}

/// The lines of a graph: the levels from the top caller down, then what the numbers say.
fn graph_lines(graph: &CallGraph) -> Vec<String> {
    // `sort_by_key` is stable, so the nodes of a level keep the graph's order.
    let mut nodes: Vec<&GraphNode> = graph.nodes.iter().collect();
    nodes.sort_by_key(|n| n.level);
    let mut lines: Vec<String> = nodes.into_iter().map(node_line).collect();
    lines.push(format!(
        "Depth {} · {} changed, {} touched by calls",
        graph.depth, graph.changed, graph.touched
    ));
    match graph.hidden_edges {
        0 => {}
        1 => lines.push("1 call skips a level and is not drawn".into()),
        n => lines.push(format!("{n} calls skip a level and are not drawn")),
    }
    if graph.cut {
        lines.push("Partial: the index stopped after 10 s".into());
    }
    lines
}

/// What the command says when there is no graph.
fn no_graph_lines(unsupported: &[String]) -> Vec<String> {
    let mut lines = vec![
        "No call graph for this pull request: no changed function in a supported language."
            .to_string(),
    ];
    if !unsupported.is_empty() {
        let shown: Vec<String> = unsupported
            .iter()
            .take(UNSUPPORTED_SHOWN)
            .map(|p| line_of(p))
            .collect();
        let mut line = format!("Not supported yet: {}", shown.join(", "));
        if unsupported.len() > UNSUPPORTED_SHOWN {
            line.push_str(&format!(
                " and {} more",
                unsupported.len() - UNSUPPORTED_SHOWN
            ));
        }
        lines.push(line);
    }
    lines
}

/// Waits until the daemon says the index of `pr` changed, or `INDEX_WAIT` passes.
async fn wait_for_change(client: &mut Client, pr: &PrRef) -> Result<(), CliError> {
    let waiting = async {
        loop {
            if let (_, Event::CallsIndexChanged { pr: of, state }) = client.next_event().await?
                && of == *pr
                && !matches!(state, CallsIndexState::Building)
            {
                return Ok::<(), CliError>(());
            }
        }
    };
    tokio::time::timeout(INDEX_WAIT, waiting)
        .await
        .map_err(|_| CliError::Other("the call index took longer than 30 seconds".into()))?
}

/// What `GetCalls` answered once the index was ready.
struct Answer {
    graph: Option<CallGraph>,
    unsupported: Vec<String>,
}

/// `GetCalls`, asked again after each `CallsIndexChanged` for as long as the index is being
/// built. The caller subscribed to the agent topic first, so no ending is missed.
async fn answer(
    client: &mut Client,
    pr: &PrRef,
    center: Option<String>,
    depth: Option<u8>,
) -> Result<Answer, CliError> {
    loop {
        let reply = client
            .request(Request::GetCalls {
                pr: pr.clone(),
                center: center.clone(),
                depth,
                expand: Vec::new(),
            })
            .await?;
        let Reply::Calls {
            graph,
            index,
            unsupported,
        } = reply
        else {
            return Err(unexpected(reply));
        };
        match index {
            CallsIndexState::Building | CallsIndexState::Idle => {
                wait_for_change(client, pr).await?
            }
            CallsIndexState::Failed { message } => {
                return Err(CliError::Other(format!(
                    "the call index could not be built: {}",
                    line_of(&message)
                )));
            }
            CallsIndexState::Ready { .. } => return Ok(Answer { graph, unsupported }),
        }
    }
}

/// Opens the review and has the index built, for a command that needs it. A failure is left
/// for the command's own request to report.
pub(crate) async fn wait_for_index(client: &mut Client, pr: &PrRef) {
    let _ = answer(client, pr, None, None).await;
}

/// Subscribes to the agent topic and opens the review, which prepares the repository.
pub(crate) async fn open(client: &mut Client, pr: &PrRef) -> Result<(), CliError> {
    client
        .request(Request::Subscribe {
            topics: vec![topics::AGENT.into()],
        })
        .await?;
    client
        .request(Request::OpenReview { pr: pr.clone() })
        .await?;
    Ok(())
}

pub(crate) async fn calls(
    paths: &Paths,
    home: Option<&Path>,
    pr: &str,
    function: Option<&str>,
    depth: Option<u8>,
    json: bool,
) -> Result<Output, CliError> {
    let depth = check_depth(depth)?;
    let pr = parse_pr(pr)?;
    let mut client = connect(paths, home).await?;
    open(&mut client, &pr).await?;
    let center = match function {
        None => None,
        Some(query) => {
            // The index must be ready before the daemon can search it.
            wait_for_index(&mut client, &pr).await;
            let reply = client
                .request(Request::FindFunction {
                    pr: pr.clone(),
                    query: query.to_string(),
                })
                .await?;
            let Reply::Functions(hits) = reply else {
                return Err(unexpected(reply));
            };
            let (hit, note) = pick(query, &hits).map_err(CliError::Other)?;
            if let Some(note) = note
                && !json
            {
                eprintln!("{note}");
            }
            Some(hit.id.clone())
        }
    };
    let found = answer(&mut client, &pr, center, depth).await?;
    // `main` prints an output only when its JSON is not `null`, and a missing graph is `null`,
    // so the command prints what it has itself.
    if json {
        let text =
            serde_json::to_string(&found.graph).map_err(|e| CliError::Other(e.to_string()))?;
        println!("{text}");
    } else {
        let lines = match &found.graph {
            Some(graph) => graph_lines(graph),
            None => no_graph_lines(&found.unsupported),
        };
        println!("{}", crate::agent::streamed(&lines.join("\n")));
    }
    Ok(Output {
        human: String::new(),
        json: serde_json::Value::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_calls::{FunctionId, NodeKind};

    fn node(label: &str, sub: &str, kind: NodeKind, level: i8) -> GraphNode {
        GraphNode {
            key: format!("k:{label}"),
            label: label.into(),
            sub: sub.into(),
            kind,
            level,
            function: Some(FunctionId(format!("src/a.rs#{label}#1"))),
            path: Some("src/a.rs".into()),
            line: Some(1),
            signature: String::new(),
            more: 0,
            ambiguous: Vec::new(),
            callers: 0,
        }
    }

    fn graph() -> CallGraph {
        CallGraph {
            center: "k:TokenStore::refresh".into(),
            depth: 3,
            nodes: vec![
                node(
                    "tokio::sleep",
                    "tokio · outside this repo",
                    NodeKind::Outside,
                    1,
                ),
                node(
                    "TokenStore::refresh",
                    "+4 −1 · body changed",
                    NodeKind::Changed,
                    0,
                ),
                node(
                    "TokenStore::token",
                    "not touched · 4 callers",
                    NodeKind::Untouched,
                    -1,
                ),
                node(
                    "auth_middleware",
                    "not touched · 4 callers",
                    NodeKind::Untouched,
                    -2,
                ),
                node("+3 more calls", "", NodeKind::More, 1),
            ],
            edges: Vec::new(),
            hidden_edges: 0,
            breadcrumb: Vec::new(),
            changed: 1,
            touched: 4,
            cut: false,
        }
    }

    #[test]
    fn the_levels_print_from_the_top_caller_to_the_deepest_callee() {
        assert_eq!(
            graph_lines(&graph()),
            [
                "↑2  auth_middleware  not touched · 4 callers",
                "↑1  TokenStore::token  not touched · 4 callers",
                "●  TokenStore::refresh  +4 −1 · body changed",
                "↓1  tokio::sleep  tokio · outside this repo",
                "↓1  +3 more calls",
                "Depth 3 · 1 changed, 4 touched by calls",
            ]
        );
    }

    #[test]
    fn hidden_edges_and_a_cut_index_are_said() {
        let mut g = graph();
        g.hidden_edges = 1;
        g.cut = true;
        let lines = graph_lines(&g);
        assert_eq!(
            lines[lines.len() - 2..],
            [
                "1 call skips a level and is not drawn",
                "Partial: the index stopped after 10 s"
            ]
        );
        g.hidden_edges = 2;
        g.cut = false;
        assert_eq!(
            graph_lines(&g).last().unwrap(),
            "2 calls skip a level and are not drawn"
        );
    }

    #[test]
    fn labels_from_the_repository_are_one_printable_line() {
        let mut g = graph();
        g.nodes[1].label = "evil\u{1b}[2K\nname".into();
        assert!(graph_lines(&g)[2].starts_with("●  evil\\u{1b}[2K name  "));
    }

    #[test]
    fn no_graph_says_so_and_lists_the_files_with_no_language() {
        assert_eq!(
            no_graph_lines(&[]),
            ["No call graph for this pull request: no changed function in a supported language."]
        );
        let many: Vec<String> = (0..12).map(|i| format!("f{i}.go")).collect();
        let lines = no_graph_lines(&many);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[1],
            "Not supported yet: f0.go, f1.go, f2.go, f3.go, f4.go, f5.go, f6.go, f7.go, f8.go, f9.go and 2 more"
        );
        assert_eq!(
            no_graph_lines(&["a.go".into()])[1],
            "Not supported yet: a.go"
        );
    }

    #[test]
    fn the_depth_must_be_in_range_before_anything_connects() {
        assert!(matches!(check_depth(None), Ok(None)));
        assert!(matches!(check_depth(Some(1)), Ok(Some(1))));
        assert!(matches!(check_depth(Some(5)), Ok(Some(5))));
        for bad in [0u8, 6, 200] {
            let Err(CliError::Usage(message)) = check_depth(Some(bad)) else {
                panic!("depth {bad} is refused");
            };
            assert_eq!(
                message,
                format!("--depth must be between 1 and 5, got {bad}")
            );
        }
    }

    fn hit(id: &str, qualified: &str, path: &str, line: u32) -> FunctionHit {
        FunctionHit {
            id: id.into(),
            qualified: qualified.into(),
            path: path.into(),
            line,
            changed: true,
            kind: None,
        }
    }

    #[test]
    fn the_exact_qualified_name_wins_and_otherwise_the_first_hit_is_shown_with_a_note() {
        let hits = [
            hit("src/a.rs#Store::refresh#3", "Store::refresh", "src/a.rs", 3),
            hit("src/b.rs#refresh#9", "refresh", "src/b.rs", 9),
        ];
        let (picked, note) = pick("refresh", &hits).unwrap();
        assert_eq!(picked.id, "src/b.rs#refresh#9");
        assert_eq!(note, None);
        let (picked, note) = pick("Refresh", &hits).unwrap();
        assert_eq!(picked.id, "src/a.rs#Store::refresh#3");
        assert_eq!(
            note.as_deref(),
            Some("2 functions match; showing Store::refresh (src/a.rs:3)")
        );
        let (_, note) = pick("refresh", &hits[..1]).unwrap();
        assert_eq!(note, None, "one hit needs no note");
        assert_eq!(
            pick("ghost\u{1b}", &[]).unwrap_err(),
            "no function matches \"ghost\\u{1b}\""
        );
    }
}

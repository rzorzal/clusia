//! `clusia open`, `clusia review …` and `clusia activity`.

use std::path::Path;

use clusia_core::{DayCount, DraftKind, ItemStatus, PrRef, Side, Verdict};
use clusia_protocol::{
    AnchorInput, Command as Request, Event, LoadStepKind, NewsItem, Reply, StepStatus, topics,
};
use serde_json::json;

use crate::cli::{ReviewCommand, VerdictArg};
use crate::run::{CliError, Output, connect, unexpected, uptime};

pub(crate) fn parse_pr(s: &str) -> Result<PrRef, CliError> {
    s.parse()
        .map_err(|e: clusia_core::PrRefError| CliError::Other(e.to_string()))
}

/// `path:line` or `path:start-end` → (path, start, end).
pub(crate) fn parse_location(s: &str) -> Result<(String, Option<u32>, u32), String> {
    let usage = || format!("expected path:line or path:start-end, got {s:?}");
    let (path, range) = s.rsplit_once(':').ok_or_else(usage)?;
    if path.is_empty() {
        return Err(usage());
    }
    let (start, end) = match range.split_once('-') {
        Some((a, b)) => (
            Some(a.parse::<u32>().map_err(|_| usage())?),
            b.parse::<u32>().map_err(|_| usage())?,
        ),
        None => (None, range.parse::<u32>().map_err(|_| usage())?),
    };
    if end == 0 || start.is_some_and(|s| s == 0 || s > end) {
        return Err(usage());
    }
    Ok((path.to_string(), start, end))
}

fn step_name(step: LoadStepKind) -> &'static str {
    match step {
        LoadStepKind::Repo => "repo",
        LoadStepKind::Branch => "branch",
        LoadStepKind::Pr => "pr",
        LoadStepKind::Agent => "agent",
    }
}

fn news_line(n: &NewsItem) -> String {
    match &n.who {
        Some(who) => format!("  • {} — {} ({})", n.summary, who, n.source),
        None => format!("  • {} ({})", n.summary, n.source),
    }
}

/// Prints the load steps of `pr` that arrived while the open request ran (the daemon sends
/// them before its response, and the client buffers them).
fn print_steps(client: &mut clusia_protocol::Client, pr: &clusia_core::PrRef) {
    for (_, event) in client.take_events() {
        if let Event::LoadStep(s) = event
            && &s.pr == pr
        {
            let mark = match s.status {
                StepStatus::Done => "✓",
                StepStatus::Skipped => "–",
                StepStatus::Failed => "✗",
                StepStatus::Running => continue,
            };
            eprintln!(
                "{mark} {} {}",
                step_name(s.step),
                s.message.unwrap_or_default()
            );
        }
    }
}

pub(crate) async fn open(
    paths: &clusia_core::Paths,
    home: Option<&Path>,
    pr: &str,
) -> Result<Output, CliError> {
    let pr = parse_pr(pr)?;
    let mut client = connect(paths, home).await?;
    client
        .request(Request::Subscribe {
            topics: vec![topics::REVIEWS.into()],
        })
        .await?;
    let result = client.request(Request::OpenReview { pr: pr.clone() }).await;
    print_steps(&mut client, &pr);
    let view = match result {
        Ok(Reply::Review(v)) => *v,
        Ok(other) => return Err(unexpected(other)),
        Err(e) => return Err(e.into()),
    };
    let whats_new = match client
        .request(Request::GetWhatsNew { pr: pr.clone() })
        .await?
    {
        Reply::WhatsNew(items) => items,
        other => return Err(unexpected(other)),
    };
    let had_seen = view.review.last_seen_at.is_some();
    client.request(Request::MarkSeen { pr: pr.clone() }).await?;

    let d = &view.pr;
    let role = match view.role {
        clusia_core::Role::Author => "you are the author",
        clusia_core::Role::Reviewer => "you are reviewing",
    };
    let files = view.files.len();
    let mut lines = vec![
        format!("{} · {} (@{})", pr, d.summary.title, d.summary.author),
        format!(
            "+{} −{} · {} file{} · {role}",
            d.additions,
            d.deletions,
            files,
            if files == 1 { "" } else { "s" }
        ),
    ];
    if let Some(w) = &view.worktree {
        lines.push(format!("worktree: {w}"));
    }
    let items = &view.review.draft.items;
    if !items.is_empty() {
        let moved = items
            .iter()
            .filter(|i| matches!(i.status, ItemStatus::Moved { .. }))
            .count();
        let obsolete = items
            .iter()
            .filter(|i| matches!(i.status, ItemStatus::Obsolete { .. }))
            .count();
        lines.push(format!(
            "draft: {} item{} · {moved} moved · {obsolete} obsolete",
            items.len(),
            if items.len() == 1 { "" } else { "s" }
        ));
    }
    if !whats_new.is_empty() {
        lines.push("what's new:".into());
        lines.extend(whats_new.iter().map(news_line));
    } else if had_seen {
        lines.push("nothing new since you last looked".into());
    }
    Ok(Output {
        human: lines.join("\n"),
        json: json!({ "review": view, "whats_new": whats_new }),
    })
}

fn item_line(item: &clusia_core::DraftItem) -> String {
    let location = match (&item.anchor, &item.thread) {
        (_, Some(t)) if item.kind == DraftKind::Resolve => {
            format!("resolve thread by @{}", t.author)
        }
        (_, Some(t)) => match (&t.path, t.line) {
            (Some(path), Some(line)) => format!("reply to @{} · {path}:{line}", t.author),
            (Some(path), None) => format!("reply to @{} · {path}", t.author),
            _ => format!("reply to @{}", t.author),
        },
        (Some(a), None) => {
            let range = match a.start_line {
                Some(s) => format!("{s}-{}", a.line),
                None => a.line.to_string(),
            };
            let side = match a.side {
                Side::Left => "left",
                Side::Right => "right",
            };
            format!("{}:{range} ({side})", a.path)
        }
        (None, None) => "(general)".to_string(),
    };
    let suffix = match &item.status {
        ItemStatus::Ok => String::new(),
        ItemStatus::Moved {
            from_path,
            from_line,
        } => format!(" [moved from {from_path}:{from_line}]"),
        ItemStatus::Obsolete { reason } => format!(" [obsolete: {reason}]"),
    };
    format!("  {}  {location}  {}{suffix}", item.id, item.body)
        .trim_end()
        .to_string()
}

fn state_name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

pub(crate) async fn run(
    paths: &clusia_core::Paths,
    home: Option<&Path>,
    cmd: ReviewCommand,
) -> Result<Output, CliError> {
    // Validate input before contacting (or starting) the daemon.
    let comment = match &cmd {
        ReviewCommand::Comment { location, .. } => {
            Some(parse_location(location).map_err(CliError::Other)?)
        }
        _ => None,
    };
    let pr = match &cmd {
        ReviewCommand::Status { pr: None } => None,
        ReviewCommand::Status { pr: Some(p) }
        | ReviewCommand::Comment { pr: p, .. }
        | ReviewCommand::Note { pr: p, .. }
        | ReviewCommand::Edit { pr: p, .. }
        | ReviewCommand::Rm { pr: p, .. }
        | ReviewCommand::Publish { pr: p, .. }
        | ReviewCommand::Close { pr: p }
        | ReviewCommand::Discard { pr: p } => Some(parse_pr(p)?),
    };
    let mut client = connect(paths, home).await?;
    match (cmd, pr) {
        (ReviewCommand::Status { .. }, None) => match client.request(Request::ListReviews).await? {
            Reply::Reviews(list) if list.is_empty() => Ok(Output {
                human: "No saved reviews".into(),
                json: json!([]),
            }),
            Reply::Reviews(list) => Ok(Output {
                human: list
                    .iter()
                    .map(|r| {
                        format!(
                            "{}  {}  {} item{}  {}",
                            r.pr,
                            state_name(&r.state),
                            r.items,
                            if r.items == 1 { "" } else { "s" },
                            r.title
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                json: serde_json::to_value(&list).unwrap_or_default(),
            }),
            other => Err(unexpected(other)),
        },
        (ReviewCommand::Status { .. }, Some(pr)) => match client
            .request(Request::GetReview { pr: pr.clone() })
            .await?
        {
            Reply::ReviewFile(r) => {
                let mut lines = vec![format!("{} · {} · {}", r.pr, r.title, state_name(&r.state))];
                if r.draft.items.is_empty() {
                    lines.push("  (no draft items)".into());
                }
                lines.extend(r.draft.items.iter().map(item_line));
                Ok(Output {
                    human: lines.join("\n"),
                    json: serde_json::to_value(&r).unwrap_or_default(),
                })
            }
            other => Err(unexpected(other)),
        },
        (ReviewCommand::Comment { body, left, .. }, Some(pr)) => {
            let (path, start_line, line) = comment.expect("validated above");
            let side = if left { Side::Left } else { Side::Right };
            let anchor = Some(AnchorInput {
                path,
                line,
                start_line,
                side,
            });
            added(
                client
                    .request(Request::AddDraftItem {
                        pr,
                        kind: DraftKind::LineComment,
                        anchor,
                        body,
                        thread: None,
                    })
                    .await?,
            )
        }
        (ReviewCommand::Note { body, .. }, Some(pr)) => added(
            client
                .request(Request::AddDraftItem {
                    pr,
                    kind: DraftKind::General,
                    anchor: None,
                    body,
                    thread: None,
                })
                .await?,
        ),
        (ReviewCommand::Edit { id, body, .. }, Some(pr)) => match client
            .request(Request::UpdateDraftItem { pr, id, body })
            .await?
        {
            Reply::DraftItem(i) => Ok(Output {
                human: format!("{} updated", i.id),
                json: serde_json::to_value(&i).unwrap_or_default(),
            }),
            other => Err(unexpected(other)),
        },
        (ReviewCommand::Rm { id, .. }, Some(pr)) => {
            client
                .request(Request::RemoveDraftItem { pr, id: id.clone() })
                .await?;
            Ok(Output {
                human: format!("{id} removed"),
                json: json!({ "removed": id }),
            })
        }
        (
            ReviewCommand::Publish {
                verdict, summary, ..
            },
            Some(pr),
        ) => {
            let verdict = match verdict {
                VerdictArg::Approve => Verdict::Approve,
                VerdictArg::RequestChanges => Verdict::RequestChanges,
                VerdictArg::Comment => Verdict::Comment,
                VerdictArg::Close => Verdict::ClosePr,
            };
            match client
                .request(Request::Publish {
                    pr,
                    verdict,
                    summary,
                })
                .await?
            {
                Reply::Published(p) => {
                    let mut lines = Vec::new();
                    if let Some(url) = &p.url {
                        lines.push(format!("Published review: {url}"));
                    }
                    if p.closed {
                        lines.push("Closed the pull request".into());
                    }
                    if let Some(e) = &p.close_error {
                        lines.push(format!("Could not close the pull request: {e}"));
                    }
                    if !p.unresolved.is_empty() {
                        lines.push(format!(
                            "Could not resolve {} thread(s); they stay open: {}",
                            p.unresolved.len(),
                            p.unresolved.join(", ")
                        ));
                    }
                    Ok(Output {
                        human: lines.join("\n"),
                        json: serde_json::to_value(&p).unwrap_or_default(),
                    })
                }
                other => Err(unexpected(other)),
            }
        }
        (ReviewCommand::Close { .. }, Some(pr)) => {
            client.request(Request::CloseReview { pr }).await?;
            Ok(Output {
                human: "Review closed (kept if it has comments)".into(),
                json: json!({ "closed": true }),
            })
        }
        (ReviewCommand::Discard { .. }, Some(pr)) => {
            client.request(Request::DiscardReview { pr }).await?;
            Ok(Output {
                human: "Review discarded".into(),
                json: json!({ "discarded": true }),
            })
        }
        (_, None) => Err(CliError::Other("a pull request is required".into())),
    }
}

fn added(reply: Reply) -> Result<Output, CliError> {
    match reply {
        Reply::DraftItem(i) => Ok(Output {
            human: format!("{} added", i.id),
            json: serde_json::to_value(&i).unwrap_or_default(),
        }),
        other => Err(unexpected(other)),
    }
}

/// 7 rows (day within the week) × one column per week, oldest on the left.
pub(crate) fn heatmap_grid(days: &[DayCount]) -> String {
    let weeks = days.len().div_ceil(7);
    let glyph = |count: u32| match count {
        0 => '·',
        1 => '░',
        2 => '▒',
        3 => '▓',
        _ => '█',
    };
    (0..7)
        .map(|row| {
            (0..weeks)
                .map(|col| days.get(col * 7 + row).map_or(' ', |d| glyph(d.count)))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) async fn activity(
    paths: &clusia_core::Paths,
    home: Option<&Path>,
) -> Result<Output, CliError> {
    match connect(paths, home)
        .await?
        .request(Request::GetActivity)
        .await?
    {
        Reply::Activity(s) => {
            let avg = s
                .avg_review_secs
                .map(uptime)
                .unwrap_or_else(|| "—".to_string());
            let human = format!(
                "{}\nReviews published: {} this week · {} total · average time to review: {avg}",
                heatmap_grid(&s.heatmap),
                s.published_this_week,
                s.published_total
            );
            Ok(Output {
                human,
                json: serde_json::to_value(&s).unwrap_or_default(),
            })
        }
        other => Err(unexpected(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_locations() {
        assert_eq!(
            parse_location("src/a.rs:12").unwrap(),
            ("src/a.rs".to_string(), None, 12)
        );
        assert_eq!(
            parse_location("dir:with/colon.rs:3-5").unwrap(),
            ("dir:with/colon.rs".to_string(), Some(3), 5)
        );
        for bad in ["nocolon", "a.rs:", "a.rs:x", "a.rs:0", "a.rs:5-3", ":3"] {
            assert!(parse_location(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn item_lines_name_replies_and_resolves() {
        let mut draft = clusia_core::Draft::default();
        let thread = clusia_core::ThreadRef {
            id: "PRRT_1".into(),
            author: "mona".into(),
            path: Some("src/auth/refresh.rs".into()),
            line: Some(41),
        };
        draft
            .add(DraftKind::Reply, None, Some(thread.clone()), "Agreed.", 1)
            .unwrap();
        draft
            .add(DraftKind::Resolve, None, Some(thread), "", 1)
            .unwrap();
        draft
            .add(DraftKind::General, None, None, "Nice.", 1)
            .unwrap();
        let lines: Vec<String> = draft.items.iter().map(item_line).collect();
        assert_eq!(
            lines,
            [
                "  i1  reply to @mona · src/auth/refresh.rs:41  Agreed.",
                "  i2  resolve thread by @mona",
                "  i3  (general)  Nice.",
            ]
        );
    }

    #[test]
    fn heatmap_grid_shape_and_glyphs() {
        let days: Vec<clusia_core::DayCount> = (0..112)
            .map(|i| clusia_core::DayCount {
                date: format!("d{i}"),
                count: (i % 6) as u32,
            })
            .collect();
        let grid = heatmap_grid(&days);
        let rows: Vec<&str> = grid.lines().collect();
        assert_eq!(rows.len(), 7);
        assert!(rows.iter().all(|r| r.chars().count() == 16));
        assert!(rows[0].starts_with('·'), "day 0 has count 0");
        assert!(grid.contains('█'));
    }
}

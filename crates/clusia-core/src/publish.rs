//! Turning a draft into exactly one GitHub review (spec §5.1, §6.4).

use serde::Serialize;

use crate::draft::{DraftKind, Side};
use crate::review::{Review, Role, Verdict};

/// Body used when a COMMENT / REQUEST_CHANGES review only has inline comments.
pub const DEFAULT_BODY: &str = "See the inline comments.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewPayload {
    pub commit_id: String,
    pub event: String,
    pub body: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<ReviewComment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewComment {
    pub path: String,
    pub body: String,
    pub line: u32,
    pub side: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_side: Option<String>,
}

/// A reply posted inside the review to an existing thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplyPayload {
    /// GraphQL node id of the thread.
    pub thread: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishPlan {
    /// The review to create, if any. Always `Some` when there are replies.
    pub review: Option<ReviewPayload>,
    /// Replies posted inside the review, before it is submitted.
    pub replies: Vec<ReplyPayload>,
    /// Thread node ids to resolve once the review is submitted.
    pub resolves: Vec<String>,
    /// Close the pull request after the review (owner's "Close PR").
    pub close: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublishError {
    #[error("{verdict:?} is not available for your role on this pull request")]
    NotAllowed { verdict: Verdict },
    #[error("requesting changes needs a summary or at least one comment")]
    NeedsContent,
    #[error("nothing to publish: add a comment or a summary")]
    Empty,
}

fn side_name(side: Side) -> String {
    match side {
        Side::Left => "LEFT".to_string(),
        Side::Right => "RIGHT".to_string(),
    }
}

pub fn plan_publish(
    review: &Review,
    verdict: Verdict,
    summary: &str,
    role: Role,
) -> Result<PublishPlan, PublishError> {
    if !verdict.is_allowed_for(role) {
        return Err(PublishError::NotAllowed { verdict });
    }
    let mut body_parts: Vec<String> = Vec::new();
    let summary = summary.trim();
    if !summary.is_empty() {
        body_parts.push(summary.to_string());
    }
    let mut comments = Vec::new();
    let mut replies = Vec::new();
    let mut resolves = Vec::new();
    for item in review.draft.publishable() {
        match (item.kind, &item.anchor, &item.thread) {
            (DraftKind::LineComment, Some(a), _) => comments.push(ReviewComment {
                path: a.path.clone(),
                body: item.body.clone(),
                line: a.line,
                side: side_name(a.side),
                start_line: a.start_line,
                start_side: a.start_line.map(|_| side_name(a.side)),
            }),
            (DraftKind::Reply, _, Some(t)) => replies.push(ReplyPayload {
                thread: t.id.clone(),
                body: item.body.clone(),
            }),
            (DraftKind::Resolve, _, Some(t)) => resolves.push(t.id.clone()),
            _ => body_parts.push(item.body.clone()),
        }
    }
    let mut body = body_parts.join("\n\n---\n\n");
    let has_content = !body.is_empty() || !comments.is_empty() || !replies.is_empty();
    match verdict {
        Verdict::RequestChanges if !has_content => return Err(PublishError::NeedsContent),
        Verdict::Comment if !has_content && resolves.is_empty() => {
            return Err(PublishError::Empty);
        }
        _ => {}
    }
    let event = match verdict {
        Verdict::Approve => "APPROVE",
        Verdict::RequestChanges => "REQUEST_CHANGES",
        Verdict::Comment | Verdict::ClosePr => "COMMENT",
    };
    if body.is_empty()
        && (!comments.is_empty() || !replies.is_empty())
        && verdict != Verdict::Approve
    {
        body = DEFAULT_BODY.to_string();
    }
    let review_payload = (verdict == Verdict::Approve || has_content).then(|| ReviewPayload {
        commit_id: review.head_sha.clone(),
        event: event.to_string(),
        body,
        comments,
    });
    Ok(PublishPlan {
        review: review_payload,
        replies,
        resolves,
        close: verdict == Verdict::ClosePr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draft::{Anchor, ItemStatus, ThreadRef};

    fn review() -> Review {
        Review::new(
            "acme/widgets#7".parse().unwrap(),
            "Fix".into(),
            "b1".into(),
            "h1".into(),
            1,
        )
    }

    fn with_items(r: &mut Review) {
        let a = Anchor {
            path: "src/a.rs".into(),
            line: 12,
            start_line: None,
            side: Side::Right,
            commit: "h1".into(),
        };
        r.draft
            .add(DraftKind::LineComment, Some(a), None, "nit", 1)
            .unwrap();
        let range = Anchor {
            path: "src/b.rs".into(),
            line: 9,
            start_line: Some(7),
            side: Side::Left,
            commit: "b1".into(),
        };
        r.draft
            .add(DraftKind::LineComment, Some(range), None, "why removed?", 1)
            .unwrap();
        r.draft
            .add(DraftKind::General, None, None, "Overall solid.", 1)
            .unwrap();
    }

    #[test]
    fn line_comments_and_body_in_github_shape() {
        let mut r = review();
        with_items(&mut r);
        let plan = plan_publish(
            &r,
            Verdict::RequestChanges,
            " Please fix the nit. ",
            Role::Reviewer,
        )
        .unwrap();
        assert!(!plan.close);
        let json = serde_json::to_string(&plan.review.unwrap()).unwrap();
        assert_eq!(
            json,
            r#"{"commit_id":"h1","event":"REQUEST_CHANGES","body":"Please fix the nit.\n\n---\n\nOverall solid.","comments":[{"path":"src/a.rs","body":"nit","line":12,"side":"RIGHT"},{"path":"src/b.rs","body":"why removed?","line":9,"side":"LEFT","start_line":7,"start_side":"LEFT"}]}"#
        );
    }

    #[test]
    fn obsolete_and_unaccepted_items_are_left_out() {
        let mut r = review();
        with_items(&mut r);
        r.draft.items[0].status = ItemStatus::Obsolete {
            reason: "changed".into(),
        };
        r.draft.items[2].accepted = false;
        let payload = plan_publish(&r, Verdict::Comment, "", Role::Reviewer)
            .unwrap()
            .review
            .unwrap();
        assert_eq!(payload.comments.len(), 1);
        assert_eq!(payload.body, DEFAULT_BODY);
    }

    #[test]
    fn author_cannot_approve() {
        assert_eq!(
            plan_publish(&review(), Verdict::Approve, "", Role::Author),
            Err(PublishError::NotAllowed {
                verdict: Verdict::Approve
            })
        );
        assert_eq!(
            plan_publish(&review(), Verdict::RequestChanges, "x", Role::Author),
            Err(PublishError::NotAllowed {
                verdict: Verdict::RequestChanges
            })
        );
        assert!(plan_publish(&review(), Verdict::ClosePr, "", Role::Reviewer).is_err());
    }

    #[test]
    fn content_rules() {
        assert_eq!(
            plan_publish(&review(), Verdict::RequestChanges, " ", Role::Reviewer),
            Err(PublishError::NeedsContent)
        );
        assert_eq!(
            plan_publish(&review(), Verdict::Comment, "", Role::Reviewer),
            Err(PublishError::Empty)
        );
        let approve = plan_publish(&review(), Verdict::Approve, "", Role::Reviewer)
            .unwrap()
            .review
            .unwrap();
        assert_eq!(
            serde_json::to_string(&approve).unwrap(),
            r#"{"commit_id":"h1","event":"APPROVE","body":""}"#
        );
    }

    #[test]
    fn owner_close_with_and_without_content() {
        let plan = plan_publish(&review(), Verdict::ClosePr, "", Role::Author).unwrap();
        assert_eq!(
            plan,
            PublishPlan {
                review: None,
                replies: vec![],
                resolves: vec![],
                close: true
            }
        );
        let plan = plan_publish(
            &review(),
            Verdict::ClosePr,
            "Superseded by #8",
            Role::Author,
        )
        .unwrap();
        assert!(plan.close);
        let payload = plan.review.unwrap();
        assert_eq!(
            (payload.event.as_str(), payload.body.as_str()),
            ("COMMENT", "Superseded by #8")
        );
    }

    fn thread(id: &str) -> ThreadRef {
        ThreadRef {
            id: id.into(),
            author: "mona".into(),
            path: Some("src/a.rs".into()),
            line: Some(41),
        }
    }

    fn reply(r: &mut Review, id: &str, body: &str) {
        r.draft
            .add(DraftKind::Reply, None, Some(thread(id)), body, 1)
            .unwrap();
    }

    fn resolve(r: &mut Review, id: &str) {
        r.draft
            .add(DraftKind::Resolve, None, Some(thread(id)), "", 1)
            .unwrap();
    }

    #[test]
    fn replies_and_resolves_are_split_out() {
        let mut r = review();
        with_items(&mut r);
        reply(&mut r, "PRRT_1", "Clock skew, agreed.");
        resolve(&mut r, "PRRT_1");
        resolve(&mut r, "PRRT_2");
        let plan = plan_publish(&r, Verdict::Comment, "", Role::Reviewer).unwrap();
        assert_eq!(
            plan.replies,
            vec![ReplyPayload {
                thread: "PRRT_1".into(),
                body: "Clock skew, agreed.".into()
            }]
        );
        assert_eq!(plan.resolves, vec!["PRRT_1", "PRRT_2"]);
        let payload = plan.review.unwrap();
        assert_eq!(payload.comments.len(), 2);
        assert_eq!(
            payload.body, "Overall solid.",
            "replies stay out of the body"
        );
    }

    #[test]
    fn replies_alone_need_a_review_with_the_default_body() {
        let mut r = review();
        reply(&mut r, "PRRT_1", "Agreed.");
        for verdict in [Verdict::Comment, Verdict::RequestChanges] {
            let plan = plan_publish(&r, verdict, "", Role::Reviewer).unwrap();
            let payload = plan.review.expect("replies need a pending review");
            assert_eq!(payload.body, DEFAULT_BODY);
            assert!(payload.comments.is_empty());
            assert_eq!(plan.replies.len(), 1);
        }
        let approve = plan_publish(&r, Verdict::Approve, "", Role::Reviewer)
            .unwrap()
            .review
            .unwrap();
        assert_eq!(approve.body, "", "an approval needs no body");
    }

    #[test]
    fn resolves_alone() {
        let mut r = review();
        resolve(&mut r, "PRRT_1");
        assert_eq!(
            plan_publish(&r, Verdict::Comment, "", Role::Reviewer),
            Ok(PublishPlan {
                review: None,
                replies: vec![],
                resolves: vec!["PRRT_1".into()],
                close: false
            })
        );
        assert_eq!(
            plan_publish(&r, Verdict::RequestChanges, "", Role::Reviewer),
            Err(PublishError::NeedsContent)
        );
        let approve = plan_publish(&r, Verdict::Approve, "", Role::Reviewer).unwrap();
        assert!(approve.review.is_some());
        assert_eq!(approve.resolves, vec!["PRRT_1"]);
        let summary = plan_publish(&r, Verdict::Comment, "Thanks!", Role::Reviewer).unwrap();
        assert_eq!(summary.review.unwrap().body, "Thanks!");
    }

    #[test]
    fn author_may_reply_and_resolve_then_close() {
        let mut r = review();
        reply(&mut r, "PRRT_1", "Fixed in the next PR.");
        resolve(&mut r, "PRRT_1");
        let plan = plan_publish(&r, Verdict::ClosePr, "", Role::Author).unwrap();
        assert!(plan.close);
        assert_eq!(plan.review.unwrap().event, "COMMENT");
        assert_eq!((plan.replies.len(), plan.resolves.len()), (1, 1));
    }

    #[test]
    fn reply_payload_wire_shape() {
        assert_eq!(
            serde_json::to_string(&ReplyPayload {
                thread: "PRRT_1".into(),
                body: "ok".into()
            })
            .unwrap(),
            r#"{"thread":"PRRT_1","body":"ok"}"#
        );
    }
}

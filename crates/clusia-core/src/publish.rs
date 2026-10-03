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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishPlan {
    /// The review to create, if any.
    pub review: Option<ReviewPayload>,
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
    for item in review.draft.publishable() {
        match (item.kind, &item.anchor) {
            (DraftKind::LineComment, Some(a)) => comments.push(ReviewComment {
                path: a.path.clone(),
                body: item.body.clone(),
                line: a.line,
                side: side_name(a.side),
                start_line: a.start_line,
                start_side: a.start_line.map(|_| side_name(a.side)),
            }),
            _ => body_parts.push(item.body.clone()),
        }
    }
    let mut body = body_parts.join("\n\n---\n\n");
    let has_content = !body.is_empty() || !comments.is_empty();
    match verdict {
        Verdict::RequestChanges if !has_content => return Err(PublishError::NeedsContent),
        Verdict::Comment if !has_content => return Err(PublishError::Empty),
        _ => {}
    }
    let event = match verdict {
        Verdict::Approve => "APPROVE",
        Verdict::RequestChanges => "REQUEST_CHANGES",
        Verdict::Comment | Verdict::ClosePr => "COMMENT",
    };
    if body.is_empty() && !comments.is_empty() && verdict != Verdict::Approve {
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
        close: verdict == Verdict::ClosePr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draft::{Anchor, ItemStatus};

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
            .add(DraftKind::LineComment, Some(a), "nit", 1)
            .unwrap();
        let range = Anchor {
            path: "src/b.rs".into(),
            line: 9,
            start_line: Some(7),
            side: Side::Left,
            commit: "b1".into(),
        };
        r.draft
            .add(DraftKind::LineComment, Some(range), "why removed?", 1)
            .unwrap();
        r.draft
            .add(DraftKind::General, None, "Overall solid.", 1)
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
}

//! The reviewer's draft: everything that will go into one GitHub review.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// The base version (removed lines).
    Left,
    /// The head version (added and unchanged lines).
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub path: String,
    /// Last line of the commented range (GitHub's `line`).
    pub line: u32,
    /// First line of a multi-line comment (GitHub's `start_line`), on the same side.
    pub start_line: Option<u32>,
    pub side: Side,
    /// The commit the line numbers refer to: head SHA for `Right`, base SHA for `Left`.
    pub commit: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftKind {
    LineComment,
    /// Goes into the review body.
    General,
    /// A reply to someone else's review thread, posted inside the review.
    Reply,
    /// Marks a review thread to resolve after the review is submitted. No text.
    Resolve,
}

/// The GitHub review thread a reply or resolve points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRef {
    /// GraphQL node id (`PRRT_…`).
    pub id: String,
    /// Display context: who started the thread and where.
    pub author: String,
    pub path: Option<String>,
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Human,
    Audit,
    Security,
    Diagram,
    /// A comment the review agent suggested; it counts once the human accepts it.
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ItemStatus {
    Ok,
    Moved { from_path: String, from_line: u32 },
    Obsolete { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftItem {
    pub id: String,
    pub kind: DraftKind,
    pub origin: Origin,
    pub anchor: Option<Anchor>,
    /// Replies and resolves only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadRef>,
    pub body: String,
    pub status: ItemStatus,
    /// Agent-originated items only count once the human accepts them.
    pub accepted: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DraftError {
    #[error("the comment is empty")]
    EmptyBody,
    #[error("a line comment needs a file and a line")]
    MissingAnchor,
    #[error("a general comment cannot point at a line")]
    UnexpectedAnchor,
    #[error("invalid line range {start}..{end}")]
    InvalidRange { start: u32, end: u32 },
    #[error("there is no draft item {0}")]
    NoSuchItem(String),
    #[error("a reply or resolve needs a thread")]
    MissingThread,
    #[error("only replies and resolves point at a thread")]
    UnexpectedThread,
    #[error("this thread is already marked to resolve")]
    AlreadyResolving,
    #[error("a resolve has no text to edit")]
    NotEditable,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Draft {
    pub items: Vec<DraftItem>,
    /// Ids are never reused, even after removals.
    #[serde(default)]
    pub next_id: u64,
}

impl Draft {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Adds a human-written item and returns it.
    ///
    /// Line comments need an anchor, replies and resolves a thread; a resolve has no text and
    /// is accepted once per thread.
    pub fn add(
        &mut self,
        kind: DraftKind,
        anchor: Option<Anchor>,
        thread: Option<ThreadRef>,
        body: &str,
        now: i64,
    ) -> Result<&DraftItem, DraftError> {
        let body = if kind == DraftKind::Resolve {
            ""
        } else {
            body.trim()
        };
        if kind != DraftKind::Resolve && body.is_empty() {
            return Err(DraftError::EmptyBody);
        }
        match (kind, &anchor, &thread) {
            (DraftKind::LineComment, None, _) => return Err(DraftError::MissingAnchor),
            (DraftKind::LineComment | DraftKind::General, _, Some(_)) => {
                return Err(DraftError::UnexpectedThread);
            }
            (DraftKind::General | DraftKind::Reply | DraftKind::Resolve, Some(_), _) => {
                return Err(DraftError::UnexpectedAnchor);
            }
            (DraftKind::Reply | DraftKind::Resolve, _, None) => {
                return Err(DraftError::MissingThread);
            }
            _ => {}
        }
        if let (DraftKind::Resolve, Some(t)) = (kind, &thread)
            && self.items.iter().any(|i| {
                i.kind == DraftKind::Resolve && i.thread.as_ref().is_some_and(|x| x.id == t.id)
            })
        {
            return Err(DraftError::AlreadyResolving);
        }
        if let Some(a) = &anchor {
            let start = a.start_line.unwrap_or(a.line);
            if a.line == 0 || start == 0 || start > a.line {
                return Err(DraftError::InvalidRange { start, end: a.line });
            }
        }
        self.next_id += 1;
        self.items.push(DraftItem {
            id: format!("i{}", self.next_id),
            kind,
            origin: Origin::Human,
            anchor,
            thread,
            body: body.to_string(),
            status: ItemStatus::Ok,
            accepted: true,
            created_at: now,
        });
        Ok(&self.items[self.items.len() - 1])
    }

    pub fn get(&self, id: &str) -> Option<&DraftItem> {
        self.items.iter().find(|i| i.id == id)
    }

    /// Replaces an item's text. A resolve has none to edit.
    pub fn update_body(&mut self, id: &str, body: &str) -> Result<(), DraftError> {
        let item = self
            .items
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| DraftError::NoSuchItem(id.to_string()))?;
        if item.kind == DraftKind::Resolve {
            return Err(DraftError::NotEditable);
        }
        let body = body.trim();
        if body.is_empty() {
            return Err(DraftError::EmptyBody);
        }
        item.body = body.to_string();
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Result<DraftItem, DraftError> {
        let index = self
            .items
            .iter()
            .position(|i| i.id == id)
            .ok_or_else(|| DraftError::NoSuchItem(id.to_string()))?;
        Ok(self.items.remove(index))
    }

    /// Items that go into the published review: accepted and not obsolete.
    pub fn publishable(&self) -> impl Iterator<Item = &DraftItem> {
        self.items
            .iter()
            .filter(|i| i.accepted && !matches!(i.status, ItemStatus::Obsolete { .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(line: u32, start: Option<u32>) -> Anchor {
        Anchor {
            path: "src/a.rs".into(),
            line,
            start_line: start,
            side: Side::Right,
            commit: "h1".into(),
        }
    }

    #[test]
    fn add_assigns_sequential_ids_and_trims() {
        let mut d = Draft::default();
        let first = d
            .add(
                DraftKind::LineComment,
                Some(anchor(3, None)),
                None,
                "  nit: rename  ",
                10,
            )
            .unwrap()
            .clone();
        assert_eq!(
            (first.id.as_str(), first.body.as_str(), first.accepted),
            ("i1", "nit: rename", true)
        );
        assert_eq!(first.status, ItemStatus::Ok);
        assert_eq!(
            d.add(DraftKind::General, None, None, "overall fine", 11)
                .unwrap()
                .id,
            "i2"
        );
        assert!(!d.is_empty());
    }

    #[test]
    fn add_validates() {
        let mut d = Draft::default();
        assert_eq!(
            d.add(DraftKind::General, None, None, "   ", 1).unwrap_err(),
            DraftError::EmptyBody
        );
        assert_eq!(
            d.add(DraftKind::LineComment, None, None, "x", 1)
                .unwrap_err(),
            DraftError::MissingAnchor
        );
        assert_eq!(
            d.add(DraftKind::General, Some(anchor(1, None)), None, "x", 1)
                .unwrap_err(),
            DraftError::UnexpectedAnchor
        );
        assert_eq!(
            d.add(
                DraftKind::LineComment,
                Some(anchor(3, Some(5))),
                None,
                "x",
                1
            )
            .unwrap_err(),
            DraftError::InvalidRange { start: 5, end: 3 }
        );
        assert!(
            d.add(DraftKind::LineComment, Some(anchor(0, None)), None, "x", 1)
                .is_err()
        );
        assert!(d.is_empty());
    }

    #[test]
    fn ids_are_never_reused_after_removal() {
        let mut d = Draft::default();
        d.add(DraftKind::General, None, None, "a", 1).unwrap();
        d.remove("i1").unwrap();
        assert_eq!(
            d.add(DraftKind::General, None, None, "b", 1).unwrap().id,
            "i2"
        );
        assert_eq!(
            d.remove("i9").unwrap_err(),
            DraftError::NoSuchItem("i9".into())
        );
    }

    #[test]
    fn update_and_publishable() {
        let mut d = Draft::default();
        d.add(DraftKind::General, None, None, "a", 1).unwrap();
        d.add(DraftKind::General, None, None, "b", 1).unwrap();
        d.update_body("i1", " edited ").unwrap();
        assert_eq!(d.get("i1").unwrap().body, "edited");
        assert_eq!(d.update_body("i1", " ").unwrap_err(), DraftError::EmptyBody);
        d.items[1].status = ItemStatus::Obsolete {
            reason: "gone".into(),
        };
        let ids: Vec<_> = d.publishable().map(|i| i.id.clone()).collect();
        assert_eq!(ids, vec!["i1"]);
    }

    fn thread(id: &str) -> ThreadRef {
        ThreadRef {
            id: id.into(),
            author: "mona".into(),
            path: Some("src/a.rs".into()),
            line: Some(41),
        }
    }

    #[test]
    fn replies_and_resolves_point_at_a_thread() {
        let mut d = Draft::default();
        let reply = d
            .add(
                DraftKind::Reply,
                None,
                Some(thread("PRRT_1")),
                " Agreed. ",
                5,
            )
            .unwrap()
            .clone();
        assert_eq!(
            (reply.body.as_str(), reply.thread, reply.anchor),
            ("Agreed.", Some(thread("PRRT_1")), None)
        );
        let resolve = d
            .add(
                DraftKind::Resolve,
                None,
                Some(thread("PRRT_1")),
                "ignored",
                6,
            )
            .unwrap()
            .clone();
        assert_eq!((resolve.id.as_str(), resolve.body.as_str()), ("i2", ""));
        assert_eq!(
            d.add(DraftKind::Resolve, None, Some(thread("PRRT_2")), "", 7)
                .unwrap()
                .id,
            "i3"
        );
    }

    #[test]
    fn reply_and_resolve_rules() {
        let mut d = Draft::default();
        assert_eq!(
            d.add(DraftKind::Reply, None, None, "x", 1).unwrap_err(),
            DraftError::MissingThread
        );
        assert_eq!(
            d.add(DraftKind::Resolve, None, None, "", 1).unwrap_err(),
            DraftError::MissingThread
        );
        assert_eq!(
            d.add(DraftKind::Reply, None, Some(thread("PRRT_1")), "  ", 1)
                .unwrap_err(),
            DraftError::EmptyBody
        );
        assert_eq!(
            d.add(
                DraftKind::Reply,
                Some(anchor(3, None)),
                Some(thread("PRRT_1")),
                "x",
                1
            )
            .unwrap_err(),
            DraftError::UnexpectedAnchor
        );
        assert_eq!(
            d.add(
                DraftKind::LineComment,
                Some(anchor(3, None)),
                Some(thread("PRRT_1")),
                "x",
                1
            )
            .unwrap_err(),
            DraftError::UnexpectedThread
        );
        assert_eq!(
            d.add(DraftKind::General, None, Some(thread("PRRT_1")), "x", 1)
                .unwrap_err(),
            DraftError::UnexpectedThread
        );
        assert!(d.is_empty());
        d.add(DraftKind::Resolve, None, Some(thread("PRRT_1")), "", 1)
            .unwrap();
        assert_eq!(
            d.add(DraftKind::Resolve, None, Some(thread("PRRT_1")), "", 2)
                .unwrap_err(),
            DraftError::AlreadyResolving
        );
        d.add(
            DraftKind::Reply,
            None,
            Some(thread("PRRT_1")),
            "and a reply",
            3,
        )
        .unwrap();
        assert_eq!(d.items.len(), 2, "a reply next to a resolve is fine");
    }

    #[test]
    fn a_resolve_has_no_text_to_edit() {
        let mut d = Draft::default();
        d.add(DraftKind::Resolve, None, Some(thread("PRRT_1")), "", 1)
            .unwrap();
        assert_eq!(
            d.update_body("i1", "text").unwrap_err(),
            DraftError::NotEditable
        );
        assert_eq!(
            d.update_body("i1", "").unwrap_err(),
            DraftError::NotEditable
        );
        assert_eq!(
            d.update_body("i9", "text").unwrap_err(),
            DraftError::NoSuchItem("i9".into())
        );
        d.remove("i1").unwrap();
        assert!(d.is_empty(), "a resolve is undone by removing it");
    }

    #[test]
    fn old_review_files_have_no_thread() {
        let json = r#"{"id":"i1","kind":"general","origin":"human","anchor":null,"body":"b",
            "status":{"status":"ok"},"accepted":true,"created_at":1}"#;
        let item: DraftItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.thread, None);
        let back = serde_json::to_string(&item).unwrap();
        assert!(!back.contains("thread"), "{back}");
    }

    #[test]
    fn wire_names() {
        assert_eq!(serde_json::to_string(&Side::Right).unwrap(), r#""right""#);
        assert_eq!(
            serde_json::to_string(&DraftKind::LineComment).unwrap(),
            r#""line_comment""#
        );
        assert_eq!(
            serde_json::to_string(&[DraftKind::Reply, DraftKind::Resolve]).unwrap(),
            r#"["reply","resolve"]"#
        );
        assert_eq!(
            serde_json::to_string(&thread("PRRT_1")).unwrap(),
            r#"{"id":"PRRT_1","author":"mona","path":"src/a.rs","line":41}"#
        );
        assert_eq!(
            serde_json::to_string(&ItemStatus::Moved {
                from_path: "a".into(),
                from_line: 3
            })
            .unwrap(),
            r#"{"status":"moved","from_path":"a","from_line":3}"#
        );
    }

    #[test]
    fn agent_origin_has_its_own_wire_name() {
        assert_eq!(serde_json::to_string(&Origin::Agent).unwrap(), r#""agent""#);
        assert_eq!(
            serde_json::from_str::<Origin>(r#""human""#).unwrap(),
            Origin::Human
        );
    }
}

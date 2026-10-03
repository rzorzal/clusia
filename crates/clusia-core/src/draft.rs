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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Human,
    Audit,
    Security,
    Diagram,
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
    pub fn add(
        &mut self,
        kind: DraftKind,
        anchor: Option<Anchor>,
        body: &str,
        now: i64,
    ) -> Result<&DraftItem, DraftError> {
        let body = body.trim();
        if body.is_empty() {
            return Err(DraftError::EmptyBody);
        }
        match (kind, &anchor) {
            (DraftKind::LineComment, None) => return Err(DraftError::MissingAnchor),
            (DraftKind::General, Some(_)) => return Err(DraftError::UnexpectedAnchor),
            _ => {}
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

    pub fn update_body(&mut self, id: &str, body: &str) -> Result<(), DraftError> {
        let body = body.trim();
        if body.is_empty() {
            return Err(DraftError::EmptyBody);
        }
        let item = self
            .items
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| DraftError::NoSuchItem(id.to_string()))?;
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
            d.add(DraftKind::General, None, "overall fine", 11)
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
            d.add(DraftKind::General, None, "   ", 1).unwrap_err(),
            DraftError::EmptyBody
        );
        assert_eq!(
            d.add(DraftKind::LineComment, None, "x", 1).unwrap_err(),
            DraftError::MissingAnchor
        );
        assert_eq!(
            d.add(DraftKind::General, Some(anchor(1, None)), "x", 1)
                .unwrap_err(),
            DraftError::UnexpectedAnchor
        );
        assert_eq!(
            d.add(DraftKind::LineComment, Some(anchor(3, Some(5))), "x", 1)
                .unwrap_err(),
            DraftError::InvalidRange { start: 5, end: 3 }
        );
        assert!(
            d.add(DraftKind::LineComment, Some(anchor(0, None)), "x", 1)
                .is_err()
        );
        assert!(d.is_empty());
    }

    #[test]
    fn ids_are_never_reused_after_removal() {
        let mut d = Draft::default();
        d.add(DraftKind::General, None, "a", 1).unwrap();
        d.remove("i1").unwrap();
        assert_eq!(d.add(DraftKind::General, None, "b", 1).unwrap().id, "i2");
        assert_eq!(
            d.remove("i9").unwrap_err(),
            DraftError::NoSuchItem("i9".into())
        );
    }

    #[test]
    fn update_and_publishable() {
        let mut d = Draft::default();
        d.add(DraftKind::General, None, "a", 1).unwrap();
        d.add(DraftKind::General, None, "b", 1).unwrap();
        d.update_body("i1", " edited ").unwrap();
        assert_eq!(d.get("i1").unwrap().body, "edited");
        assert_eq!(d.update_body("i1", " ").unwrap_err(), DraftError::EmptyBody);
        d.items[1].status = ItemStatus::Obsolete {
            reason: "gone".into(),
        };
        let ids: Vec<_> = d.publishable().map(|i| i.id.clone()).collect();
        assert_eq!(ids, vec!["i1"]);
    }

    #[test]
    fn wire_names() {
        assert_eq!(serde_json::to_string(&Side::Right).unwrap(), r#""right""#);
        assert_eq!(
            serde_json::to_string(&DraftKind::LineComment).unwrap(),
            r#""line_comment""#
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
}

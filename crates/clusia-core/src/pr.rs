//! Pull request identity and summaries, shared by the daemon and every client.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PrRef {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PrRefError {
    #[error("expected owner/repo#number or a pull request URL, got {0:?}")]
    Invalid(String),
}

fn valid_owner(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 39
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn valid_repo(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl PrRef {
    pub fn new(
        owner: impl Into<String>,
        repo: impl Into<String>,
        number: u64,
    ) -> Result<Self, PrRefError> {
        let (owner, repo) = (owner.into(), repo.into());
        if valid_owner(&owner) && valid_repo(&repo) && number > 0 {
            Ok(Self {
                owner,
                repo,
                number,
            })
        } else {
            Err(PrRefError::Invalid(format!("{owner}/{repo}#{number}")))
        }
    }

    /// `owner/repo`.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// File-system key `owner~repo~number`. `~` cannot appear in owners or repo names.
    pub fn file_key(&self) -> String {
        format!("{}~{}~{}", self.owner, self.repo, self.number)
    }

    /// Inverse of [`PrRef::file_key`].
    pub fn from_file_key(key: &str) -> Option<PrRef> {
        let mut parts = key.split('~');
        let (owner, repo, number) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        PrRef::new(owner, repo, number.parse().ok()?).ok()
    }
}

impl fmt::Display for PrRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}#{}", self.owner, self.repo, self.number)
    }
}

impl FromStr for PrRef {
    type Err = PrRefError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || PrRefError::Invalid(s.to_string());
        let t = s.trim();
        if let Some(rest) = t
            .strip_prefix("https://")
            .or_else(|| t.strip_prefix("http://"))
        {
            // host/owner/repo/pull/number[/...]
            let path = rest.split(['?', '#']).next().unwrap_or("");
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() >= 5 && parts[3] == "pull" {
                let number = parts[4].parse().map_err(|_| invalid())?;
                return PrRef::new(parts[1], parts[2], number).map_err(|_| invalid());
            }
            return Err(invalid());
        }
        let (slug, number) = t.rsplit_once('#').ok_or_else(invalid)?;
        let (owner, repo) = slug.split_once('/').ok_or_else(invalid)?;
        let number = number.parse().map_err(|_| invalid())?;
        PrRef::new(owner, repo, number).map_err(|_| invalid())
    }
}

impl Serialize for PrRef {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PrRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrFilter {
    /// Review requested from me.
    Assigned,
    /// Authored by me.
    Mine,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrSummary {
    pub pr: PrRef,
    pub title: String,
    pub author: String,
    pub url: String,
    pub draft: bool,
    /// As GitHub reports it (RFC 3339).
    pub updated_at: String,
    pub comments: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrDetail {
    pub summary: PrSummary,
    pub base_ref: String,
    pub head_ref: String,
    pub base_sha: String,
    pub head_sha: String,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    /// Clone URL of the base repository.
    pub clone_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(s: &str) -> PrRef {
        s.parse().unwrap()
    }

    #[test]
    fn parses_short_form() {
        assert_eq!(
            pr("acme/widgets#7"),
            PrRef::new("acme", "widgets", 7).unwrap()
        );
        assert_eq!(pr(" my-org/repo.name_x#123 ").number, 123);
    }

    #[test]
    fn parses_urls() {
        let want = PrRef::new("acme", "widgets", 7).unwrap();
        for url in [
            "https://github.com/acme/widgets/pull/7",
            "https://github.com/acme/widgets/pull/7/",
            "https://github.com/acme/widgets/pull/7/files",
            "https://github.com/acme/widgets/pull/7?w=1",
            "https://github.com/acme/widgets/pull/7#discussion_r1",
            "https://ghe.corp.example/acme/widgets/pull/7",
            "http://github.com/acme/widgets/pull/7",
        ] {
            assert_eq!(pr(url), want, "{url}");
        }
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            "",
            "acme/widgets",
            "acme/widgets#0",
            "acme/widgets#x",
            "acme/wid/gets#1",
            "acme/..#1",
            "https://github.com/acme/widgets/issues/7",
            "https://github.com/acme",
        ] {
            assert!(bad.parse::<PrRef>().is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn display_slug_and_file_key() {
        let p = pr("acme/widgets#7");
        assert_eq!(p.to_string(), "acme/widgets#7");
        assert_eq!(p.slug(), "acme/widgets");
        assert_eq!(p.file_key(), "acme~widgets~7");
    }

    #[test]
    fn owners_may_contain_underscores() {
        assert_eq!(pr("octo_corp/widgets#3").owner, "octo_corp");
    }

    #[test]
    fn file_key_round_trips() {
        for s in ["acme/widgets#7", "octo_corp/my.repo-x#12"] {
            let p = pr(s);
            assert_eq!(PrRef::from_file_key(&p.file_key()), Some(p));
        }
        assert_eq!(PrRef::from_file_key("acme__widgets__7"), None);
        assert_eq!(PrRef::from_file_key("a~b~0"), None);
    }

    #[test]
    fn file_key_is_unambiguous() {
        assert_ne!(pr("a-b/c#1").file_key(), pr("a/b-c#1").file_key());
    }

    #[test]
    fn serializes_as_a_string() {
        assert_eq!(
            serde_json::to_string(&pr("acme/widgets#7")).unwrap(),
            r#""acme/widgets#7""#
        );
        assert_eq!(
            serde_json::from_str::<PrRef>(r#""acme/widgets#7""#).unwrap(),
            pr("acme/widgets#7")
        );
        assert!(serde_json::from_str::<PrRef>(r#""nope""#).is_err());
        assert_eq!(
            serde_json::to_string(&PrFilter::Assigned).unwrap(),
            r#""assigned""#
        );
    }
}

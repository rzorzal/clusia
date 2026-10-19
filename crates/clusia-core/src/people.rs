//! People on a pull request: which logins are safe to turn into links, where a person's profile
//! and picture live, and the public fields of a profile card.

use serde::{Deserialize, Serialize};

/// The longest GitHub login.
const MAX_LOGIN: usize = 39;
/// Pixels of the avatar asked for; the largest place it is drawn is 48 logical pixels.
const AVATAR_SIDE: u32 = 96;

/// The public fields of a GitHub user, as the hover card shows them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub login: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub company: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
}

/// Whether `login` is an ordinary user name: letters, digits and hyphens only. Bots
/// (`name[bot]`), empty names and anything that could change a URL are not.
pub fn is_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= MAX_LOGIN
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The browser address of `login`'s profile on `host`; `None` for a name that is not a login.
pub fn profile_url(host: &str, login: &str) -> Option<String> {
    is_login(login).then(|| format!("https://{host}/{login}"))
}

/// The picture to show before the profile is known. Only github.com serves avatars from a
/// fixed address; other hosts give the address in the profile, so they start with initials.
pub fn avatar_url(host: &str, login: &str) -> Option<String> {
    (host.eq_ignore_ascii_case("github.com") && is_login(login))
        .then(|| format!("https://avatars.githubusercontent.com/{login}?s={AVATAR_SIDE}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_logins_become_links() {
        for ok in ["octo", "mona-lisa", "Hubot2", "a"] {
            assert!(is_login(ok), "{ok}");
        }
        for bad in [
            "",
            "dependabot[bot]",
            "a/b",
            "a b",
            "a?x=1",
            "..",
            "ünï",
            &"x".repeat(40),
        ] {
            assert!(!is_login(bad), "{bad}");
        }
    }

    #[test]
    fn profile_urls_follow_the_host() {
        assert_eq!(
            profile_url("github.com", "octo").as_deref(),
            Some("https://github.com/octo")
        );
        assert_eq!(
            profile_url("ghe.example.com", "mona").as_deref(),
            Some("https://ghe.example.com/mona")
        );
        assert_eq!(profile_url("github.com", "a/b"), None);
    }

    #[test]
    fn avatars_start_from_github_com_only() {
        assert_eq!(
            avatar_url("github.com", "octo").as_deref(),
            Some("https://avatars.githubusercontent.com/octo?s=96")
        );
        assert_eq!(avatar_url("ghe.example.com", "octo"), None);
        assert_eq!(avatar_url("github.com", "x[bot]"), None);
    }

    #[test]
    fn a_profile_leaves_out_what_is_empty() {
        let p = Profile {
            login: "octo".into(),
            name: Some("Octo Cat".into()),
            company: None,
            bio: None,
            avatar_url: None,
        };
        let text = serde_json::to_string(&p).unwrap();
        assert_eq!(text, r#"{"login":"octo","name":"Octo Cat"}"#);
        assert_eq!(serde_json::from_str::<Profile>(&text).unwrap(), p);
    }
}

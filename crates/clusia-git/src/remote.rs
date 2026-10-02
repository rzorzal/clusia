//! Which repository a remote URL points at.

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoId {
    pub host: String,
    pub owner: String,
    pub repo: String,
}

impl RepoId {
    /// Lowercases everything: GitHub hosts, owners and repo names are case-insensitive.
    pub fn new(host: &str, owner: &str, repo: &str) -> Self {
        Self {
            host: host.to_ascii_lowercase(),
            owner: owner.to_ascii_lowercase(),
            repo: repo.to_ascii_lowercase(),
        }
    }
}

/// `scheme://[user@]host[:port]/owner/repo[.git]` or scp-like `[user@]host:owner/repo[.git]`.
pub fn parse_remote(url: &str) -> Option<RepoId> {
    let url = url.trim();
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?.split(':').next()?;
        (host, path)
    } else {
        let (user_host, path) = url.split_once(':')?;
        if user_host.contains('/') {
            return None;
        }
        (user_host.rsplit('@').next()?, path)
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    if host.is_empty() || owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(RepoId::new(host, owner, repo))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_remote_accepts_common_forms() {
        let want = RepoId::new("github.com", "acme", "widgets");
        for url in [
            "git@github.com:acme/widgets.git",
            "git@github.com:Acme/Widgets",
            "https://github.com/acme/widgets",
            "https://github.com/acme/widgets.git",
            "https://github.com/acme/widgets/",
            "https://user@github.com/acme/widgets.git",
            "ssh://git@github.com/acme/widgets.git",
            "ssh://git@github.com:22/acme/widgets.git",
            "git://github.com/acme/widgets",
            "  https://GitHub.com/acme/widgets.git\n",
        ] {
            assert_eq!(parse_remote(url), Some(want.clone()), "{url}");
        }
        assert_eq!(
            parse_remote("git@ghe.corp.example:team/app.git"),
            Some(RepoId::new("ghe.corp.example", "team", "app"))
        );
    }

    #[test]
    fn parse_remote_rejects_others() {
        for url in [
            "",
            "/local/path/repo",
            "../relative/repo",
            "file:///tmp/origin.git",
            "https://github.com/acme",
            "https://gitlab.com/group/sub/repo.git",
            "git@github.com:acme",
        ] {
            assert_eq!(parse_remote(url), None, "{url:?}");
        }
    }
}

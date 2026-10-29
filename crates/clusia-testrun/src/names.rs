//! The names Clúsia gives to engine objects. Docker wants image, volume and project names in
//! lowercase, and a review's file key holds `~`, so every name is derived from a slug.

use clusia_core::tests::Ecosystem;
use sha2::{Digest, Sha256};

pub const LABEL_REVIEW: &str = "io.clusia.review";
pub const LABEL_REPO: &str = "io.clusia.repo";
pub const LABEL_KIND: &str = "io.clusia.kind";

const SLUG_BODY_MAX: usize = 48;

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `owner-repo` flattened to `[a-z0-9-]`, then six hex of the hash of the original text, so two
/// names that flatten alike (`a.b`, `a-b`) still get different slugs.
pub fn repo_slug(owner: &str, repo: &str) -> String {
    let mut body = String::new();
    for c in format!("{owner}-{repo}")
        .chars()
        .flat_map(char::to_lowercase)
    {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            body.push(c);
        } else if !body.ends_with('-') {
            body.push('-');
        }
    }
    body.truncate(SLUG_BODY_MAX);
    let body = body.trim_matches('-');
    let hash = sha256_hex(format!("{owner}/{repo}").as_bytes());
    format!("{body}-{}", &hash[..6])
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(16)]
}

pub fn image_ref(slug: &str, hash: &str) -> String {
    format!("clusia/{slug}:{}", short(hash))
}

pub fn base_ref(slug: &str, hash: &str) -> String {
    format!("clusia/{slug}-base:{}", short(hash))
}

fn eco_name(eco: Ecosystem) -> &'static str {
    match eco {
        Ecosystem::Cargo => "cargo",
        Ecosystem::Node => "node",
        Ecosystem::Python => "python",
        Ecosystem::Go => "go",
        Ecosystem::Jvm => "jvm",
        Ecosystem::Ruby => "ruby",
    }
}

pub fn cache_volume(slug: &str, eco: Ecosystem) -> String {
    format!("clusia-cache-{slug}-{}", eco_name(eco))
}

pub fn compose_project(slug: &str, number: u64) -> String {
    format!("clusia-{slug}-{number}")
}

pub fn run_name(slug: &str, number: u64, seq: u32) -> String {
    format!("clusia-run-{slug}-{number}-{seq}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelKind {
    Run,
    Image,
    Cache,
    Stack,
    Probe,
}

impl LabelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LabelKind::Run => "run",
            LabelKind::Image => "image",
            LabelKind::Cache => "cache",
            LabelKind::Stack => "stack",
            LabelKind::Probe => "probe",
        }
    }
}

/// What marks an engine object as Clúsia's, so cleanup deletes by label and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Labels {
    pub review: String,
    pub repo: String,
    pub kind: LabelKind,
}

impl Labels {
    pub fn pairs(&self) -> [(&'static str, String); 3] {
        [
            (LABEL_REVIEW, self.review.clone()),
            (LABEL_REPO, self.repo.clone()),
            (LABEL_KIND, self.kind.as_str().to_string()),
        ]
    }

    /// The `--label key=value` flags.
    pub fn args(&self) -> Vec<String> {
        self.pairs()
            .into_iter()
            .flat_map(|(k, v)| ["--label".to_string(), format!("{k}={v}")])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use clusia_core::tests::Ecosystem;

    use super::*;

    #[test]
    fn a_slug_is_lowercase_with_the_hash_of_the_original_name() {
        assert_eq!(repo_slug("rzorzal", "clusia"), "rzorzal-clusia-06d0a3");
        assert_eq!(repo_slug("acme", "widgets"), "acme-widgets-d782c8");
        assert_eq!(
            repo_slug("octo", "Hello_World.rs"),
            "octo-hello-world-rs-0a4dad"
        );
        assert_eq!(repo_slug("octo", "hubot"), "octo-hubot-ed10ca");
    }

    #[test]
    fn two_names_that_flatten_alike_get_different_slugs() {
        assert_ne!(repo_slug("octo", "a.b"), repo_slug("octo", "a-b"));
    }

    #[test]
    fn a_long_name_is_cut_before_the_hash() {
        let slug = repo_slug("octo", &"x".repeat(200));
        let (body, hash) = slug.rsplit_once('-').unwrap();
        assert_eq!(hash.len(), 6);
        assert_eq!(body, format!("octo-{}", "x".repeat(43)));
        assert_eq!(body.len(), 48);
    }

    #[test]
    fn references_follow_the_rulings() {
        let slug = "rzorzal-clusia-06d0a3";
        let hash = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            image_ref(slug, hash),
            "clusia/rzorzal-clusia-06d0a3:0123456789abcdef"
        );
        assert_eq!(
            base_ref(slug, hash),
            "clusia/rzorzal-clusia-06d0a3-base:0123456789abcdef"
        );
        assert_eq!(
            cache_volume(slug, Ecosystem::Cargo),
            "clusia-cache-rzorzal-clusia-06d0a3-cargo"
        );
        assert_eq!(
            cache_volume(slug, Ecosystem::Node),
            "clusia-cache-rzorzal-clusia-06d0a3-node"
        );
        assert_eq!(
            cache_volume(slug, Ecosystem::Python),
            "clusia-cache-rzorzal-clusia-06d0a3-python"
        );
        assert_eq!(
            cache_volume(slug, Ecosystem::Go),
            "clusia-cache-rzorzal-clusia-06d0a3-go"
        );
        assert_eq!(
            cache_volume(slug, Ecosystem::Jvm),
            "clusia-cache-rzorzal-clusia-06d0a3-jvm"
        );
        assert_eq!(
            cache_volume(slug, Ecosystem::Ruby),
            "clusia-cache-rzorzal-clusia-06d0a3-ruby"
        );
        assert_eq!(compose_project(slug, 7), "clusia-rzorzal-clusia-06d0a3-7");
        assert_eq!(run_name(slug, 7, 2), "clusia-run-rzorzal-clusia-06d0a3-7-2");
    }

    #[test]
    fn labels_are_flags_and_pairs() {
        let labels = Labels {
            review: "acme~widgets~12".into(),
            repo: "acme/widgets".into(),
            kind: LabelKind::Run,
        };
        assert_eq!(
            labels.args(),
            [
                "--label",
                "io.clusia.review=acme~widgets~12",
                "--label",
                "io.clusia.repo=acme/widgets",
                "--label",
                "io.clusia.kind=run",
            ]
        );
        assert_eq!(
            labels.pairs(),
            [
                ("io.clusia.review", "acme~widgets~12".to_string()),
                ("io.clusia.repo", "acme/widgets".to_string()),
                ("io.clusia.kind", "run".to_string()),
            ]
        );
        let kinds = [
            (LabelKind::Run, "run"),
            (LabelKind::Image, "image"),
            (LabelKind::Cache, "cache"),
            (LabelKind::Stack, "stack"),
            (LabelKind::Probe, "probe"),
        ];
        for (kind, text) in kinds {
            assert_eq!(kind.as_str(), text);
        }
    }

    #[test]
    fn sha256_hex_is_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}

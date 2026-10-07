//! Rules for images and GIFs shown in comments: which hosts may be fetched, what counts as an
//! image, and how a fetched file is named in the cache. Pure; the daemon does the fetching.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The largest body the daemon accepts for one image.
pub const MAX_MEDIA_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Png,
    Gif,
    Jpeg,
}

impl MediaKind {
    /// The file extension a cached file of this kind gets.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Gif => "gif",
            Self::Jpeg => "jpg",
        }
    }
}

/// The kind of image `bytes` starts as, judged by its magic bytes only.
pub fn sniff(bytes: &[u8]) -> Option<MediaKind> {
    if bytes.starts_with(b"\x89PNG") {
        Some(MediaKind::Png)
    } else if bytes.starts_with(b"GIF8") {
        Some(MediaKind::Gif)
    } else if bytes.starts_with(b"\xFF\xD8\xFF") {
        Some(MediaKind::Jpeg)
    } else {
        None
    }
}

fn is_label(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Whether media may be fetched from `host` (a bare host name, no port). `extra` holds more
/// exact host names, which tests use to reach a local server.
pub fn allowed_host(host: &str, extra: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    if extra.iter().any(|e| e.eq_ignore_ascii_case(&host)) {
        return true;
    }
    if matches!(
        host.as_str(),
        "github.com" | "camo.githubusercontent.com" | "github.githubassets.com"
    ) {
        return true;
    }
    if let Some(sub) = host.strip_suffix(".githubusercontent.com") {
        return sub.split('.').all(is_label);
    }
    if let Some(sub) = host
        .strip_prefix("github-production-user-asset-")
        .and_then(|rest| rest.strip_suffix(".s3.amazonaws.com"))
    {
        return is_label(sub);
    }
    if let Some(label) = host.strip_suffix(".giphy.com") {
        return label
            .strip_prefix("media")
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()));
    }
    false
}

/// Whether a request to `host` may carry the GitHub token.
pub fn needs_token(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "github.com" | "api.github.com"
    )
}

/// The cache name of `url`: the SHA-256 of the URL without its fragment and without a `jwt`
/// query value, in lower-case hex. Signed attachment URLs change their `jwt` on every request
/// but name the same file.
pub fn cache_key(url: &str) -> String {
    let url = url.split('#').next().unwrap_or(url);
    let stripped = match url.split_once('?') {
        None => url.to_string(),
        Some((base, query)) => {
            let kept: Vec<&str> = query
                .split('&')
                .filter(|pair| pair.split('=').next() != Some("jwt"))
                .collect();
            if kept.is_empty() {
                base.to_string()
            } else {
                format!("{base}?{}", kept.join("&"))
            }
        }
    };
    let digest = Sha256::digest(stripped.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_reads_magic_bytes_only() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some(MediaKind::Png));
        assert_eq!(sniff(b"GIF89a...."), Some(MediaKind::Gif));
        assert_eq!(sniff(b"GIF87a...."), Some(MediaKind::Gif));
        assert_eq!(sniff(b"\xFF\xD8\xFF\xE0...."), Some(MediaKind::Jpeg));
        assert_eq!(sniff(b"<svg xmlns=\"http://www.w3.org/2000/svg\">"), None);
        assert_eq!(sniff(b"\x00\x00\x00\x18ftypmp42"), None);
        assert_eq!(sniff(b""), None);
        assert_eq!(sniff(b"\x89PN"), None, "a truncated header is not a PNG");
    }

    #[test]
    fn kinds_have_extensions_and_wire_names() {
        assert_eq!(MediaKind::Jpeg.extension(), "jpg");
        assert_eq!(MediaKind::Png.extension(), "png");
        assert_eq!(MediaKind::Gif.extension(), "gif");
        assert_eq!(
            serde_json::to_string(&MediaKind::Jpeg).unwrap(),
            r#""jpeg""#
        );
    }

    #[test]
    fn the_allow_list_is_exact() {
        for ok in [
            "github.com",
            "GitHub.com",
            "user-images.githubusercontent.com",
            "private-user-images.githubusercontent.com",
            "camo.githubusercontent.com",
            "github.githubassets.com",
            "github-production-user-asset-6210df.s3.amazonaws.com",
            "media.giphy.com",
            "media0.giphy.com",
            "media3.giphy.com",
        ] {
            assert!(allowed_host(ok, &[]), "{ok}");
        }
        for bad in [
            "",
            "evil.com",
            "githubusercontent.com",
            "evilgithubusercontent.com",
            "github.com.evil.com",
            "x.githubusercontent.com.evil.com",
            ".githubusercontent.com",
            "a..githubusercontent.com",
            "raw.github.com",
            "s3.amazonaws.com",
            "github-production-user-asset-.s3.amazonaws.com",
            "github-production-user-asset-x.evil.s3.amazonaws.com",
            "media.giphy.com.evil.com",
            "mediax.giphy.com",
            "i.giphy.com",
            "giphy.com",
            "127.0.0.1",
        ] {
            assert!(!allowed_host(bad, &[]), "{bad}");
        }
        assert!(allowed_host("127.0.0.1", &["127.0.0.1".into()]));
        assert!(!allowed_host("127.0.0.2", &["127.0.0.1".into()]));
    }

    #[test]
    fn only_github_gets_the_token() {
        assert!(needs_token("github.com"));
        assert!(needs_token("API.github.com"));
        for other in [
            "user-images.githubusercontent.com",
            "camo.githubusercontent.com",
            "media.giphy.com",
            "github-production-user-asset-6210df.s3.amazonaws.com",
            "evil.com",
        ] {
            assert!(!needs_token(other), "{other}");
        }
    }

    #[test]
    fn cache_keys_ignore_jwt_and_fragments() {
        let base = "https://github.com/user-attachments/assets/abc";
        let a = cache_key(&format!("{base}?jwt=one"));
        let b = cache_key(&format!("{base}?jwt=two#frag"));
        assert_eq!(a, b);
        assert_eq!(a, cache_key(base));
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(cache_key(&format!("{base}?v=1&jwt=x")), cache_key(base));
        assert_eq!(
            cache_key(&format!("{base}?v=1&jwt=x")),
            cache_key(&format!("{base}?v=1"))
        );
        assert_ne!(cache_key(base), cache_key("https://github.com/other"));
        assert_eq!(
            cache_key("https://x.test/a"),
            "d53531fcb1bcf3ac90fd53c8486a2943e5391f8473815294971716b064ac11ae"
        );
    }
}

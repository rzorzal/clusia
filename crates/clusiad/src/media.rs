//! `FetchMedia`: downloads an image into the media cache and answers with the cached file.
//!
//! Only https, only the hosts `clusia_core::media` allows (or, when the user allows images
//! from other sites, any public web address), at most five redirects (each hop is
//! checked again), 10 MiB, and only PNG, GIF or JPEG by magic bytes. The GitHub token goes to
//! GitHub's own hosts and nowhere else: redirects are followed here, hop by hop, never by the
//! HTTP client, so a hop to another host starts a request without credentials.

use std::future::Future;
use std::net::{IpAddr, Ipv6Addr};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use clusia_core::media::{MAX_MEDIA_BYTES, MediaKind, allowed_host, cache_key, needs_token, sniff};
use clusia_protocol::{ErrorCode, MediaFile, Outcome, ProtocolError, Reply};
use clusia_store::atomic::write_atomic;
use reqwest::{StatusCode, Url, header};

use crate::state::Shared;
use crate::sync;

const MAX_REDIRECTS: usize = 5;
const TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
const KINDS: [MediaKind; 3] = [MediaKind::Png, MediaKind::Gif, MediaKind::Jpeg];

/// The one HTTP client for media and Giphy: it never follows a redirect by itself.
pub(crate) fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("clusia/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TOTAL_TIMEOUT)
            .build()
            .expect("a plain HTTP client builds")
    })
}

fn refused(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::Refused, message)
}

pub(crate) async fn fetch(shared: &Shared, url: &str) -> Outcome {
    let result = match tokio::time::timeout(TOTAL_TIMEOUT, fetch_inner(shared, url)).await {
        Ok(r) => r,
        Err(_) => Err(ProtocolError::new(
            ErrorCode::Offline,
            "the image took too long to download",
        )),
    };
    match result {
        Ok(file) => Outcome::Ok(Reply::Media(file)),
        Err(e) => Outcome::Err(e),
    }
}

/// Names that only mean something on the user's own network.
const LOCAL_SUFFIXES: [&str; 4] = [".local", ".localhost", ".internal", ".lan"];

/// Whether `url` names an ordinary public web site: a dotted domain name (no IP address, no
/// single-label or local name) on the default https port.
fn public_web_address(url: &Url) -> bool {
    let Some(domain) = url.domain() else {
        return false;
    };
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    domain.contains('.')
        && !LOCAL_SUFFIXES.iter().any(|s| domain.ends_with(s))
        && url.port().is_none_or(|p| p == 443)
}

/// Whether `url` may be requested. Listed hosts need https; so does any other host once the
/// user allows images from other sites, and then only a public web address qualifies. Plain
/// http is accepted only for the hosts the options name (tests).
fn check(shared: &Shared, url: &Url, external: bool) -> Result<(), ProtocolError> {
    let host = url.host_str().unwrap_or("");
    let extra = &shared.media_extra_hosts;
    let listed = allowed_host(host, extra);
    if !listed && !external {
        return Err(refused(format!("images from {host} are not loaded")));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(refused("image addresses with credentials are not loaded"));
    }
    let named = extra.iter().any(|e| e.eq_ignore_ascii_case(host));
    let local = named || (!listed && shared.media_allow_local);
    if url.scheme() != "https" && !(local && url.scheme() == "http") {
        return Err(refused("only https images are loaded"));
    }
    if !listed && !shared.media_allow_local && !public_web_address(url) {
        return Err(refused(format!("images from {host} are not loaded")));
    }
    Ok(())
}

/// Whether `ip` belongs to this machine or its local network: loopback, private, link-local,
/// unique-local, shared (carrier-grade NAT), unspecified, broadcast or multicast.
fn is_local_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_local_address(IpAddr::V4(v4)),
            None => {
                v6 == Ipv6Addr::LOCALHOST
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || v6.is_unique_local()
                    || v6.is_unicast_link_local()
            }
        },
    }
}

/// Refuses `host` unless every address it resolves to is public. `resolve` answers the
/// addresses of a name and port (the real one asks the system resolver).
async fn require_public<F, Fut>(host: &str, port: u16, resolve: F) -> Result<(), ProtocolError>
where
    F: FnOnce(String, u16) -> Fut,
    Fut: Future<Output = std::io::Result<Vec<IpAddr>>>,
{
    let addresses = resolve(host.to_string(), port).await.unwrap_or_default();
    if addresses.is_empty() {
        return Err(ProtocolError::new(
            ErrorCode::Offline,
            format!("could not find {host}"),
        ));
    }
    if addresses.iter().any(|&ip| is_local_address(ip)) {
        return Err(refused(format!(
            "images from {host} are not loaded (it points at this computer or a local network)"
        )));
    }
    Ok(())
}

async fn system_resolver(host: String, port: u16) -> std::io::Result<Vec<IpAddr>> {
    Ok(tokio::net::lookup_host((host, port))
        .await?
        .map(|a| a.ip())
        .collect())
}

/// Before a request to a host outside the allow-list, makes sure the name does not lead to
/// this machine or the local network.
async fn check_resolved(shared: &Shared, url: &Url) -> Result<(), ProtocolError> {
    let host = url.host_str().unwrap_or("");
    if shared.media_allow_local || allowed_host(host, &shared.media_extra_hosts) {
        return Ok(());
    }
    let port = url.port_or_known_default().unwrap_or(443);
    require_public(host, port, system_resolver).await
}

/// Whether a request to `host` carries the GitHub token. An overridden GitHub API address
/// stands in for GitHub itself.
fn gets_token(shared: &Shared, host: &str) -> bool {
    needs_token(host)
        || shared
            .github_api
            .as_deref()
            .and_then(|api| Url::parse(api).ok())
            .and_then(|api| api.host_str().map(|h| h.eq_ignore_ascii_case(host)))
            .unwrap_or(false)
}

fn cached(dir: &std::path::Path, key: &str) -> Option<MediaFile> {
    KINDS.iter().find_map(|&kind| {
        let path = dir.join(format!("{key}.{}", kind.extension()));
        let meta = std::fs::metadata(&path).ok().filter(|m| m.is_file())?;
        Some(MediaFile {
            path: path.display().to_string(),
            kind,
            bytes: meta.len(),
        })
    })
}

async fn fetch_inner(shared: &Shared, url: &str) -> Result<MediaFile, ProtocolError> {
    let mut current = Url::parse(url)
        .map_err(|_| ProtocolError::new(ErrorCode::BadRequest, "that is not a web address"))?;
    let external = shared.config.read().await.media.load_external_images;
    check(shared, &current, external)?;
    let dir = shared.paths.media_dir();
    let key = cache_key(url);
    if let Some(file) = cached(&dir, &key) {
        return Ok(file);
    }
    let mut token: Option<Option<String>> = None;
    let mut redirects = 0;
    let mut response = loop {
        check_resolved(shared, &current).await?;
        let host = current.host_str().unwrap_or("").to_string();
        let mut request = http().get(current.clone());
        if gets_token(shared, &host) {
            if token.is_none() {
                token = Some(match sync::github_client(shared).await {
                    Ok(Some(gh)) => Some(gh.token().secret().to_string()),
                    _ => None,
                });
            }
            if let Some(Some(secret)) = &token {
                request = request.bearer_auth(secret);
            }
        }
        let response = request.send().await.map_err(|e| {
            ProtocolError::new(
                ErrorCode::Offline,
                format!("could not download the image: {}", e.without_url()),
            )
        })?;
        if !response.status().is_redirection() {
            break response;
        }
        redirects += 1;
        if redirects > MAX_REDIRECTS {
            return Err(refused("the image address redirects too many times"));
        }
        let next = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| current.join(v).ok())
            .ok_or_else(|| refused("the image address redirects nowhere"))?;
        check(shared, &next, external)?;
        current = next;
    };
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Err(ProtocolError::new(ErrorCode::NotFound, "the image is gone"));
    }
    if !status.is_success() {
        return Err(refused(format!(
            "the image host answered HTTP {}",
            status.as_u16()
        )));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_MEDIA_BYTES)
    {
        return Err(refused("the image is larger than 10 MiB"));
    }
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| {
        ProtocolError::new(
            ErrorCode::Offline,
            format!("the download broke off: {}", e.without_url()),
        )
    })? {
        if body.len() as u64 + chunk.len() as u64 > MAX_MEDIA_BYTES {
            return Err(refused("the image is larger than 10 MiB"));
        }
        body.extend_from_slice(&chunk);
    }
    let kind = sniff(&body).ok_or_else(|| refused("only PNG, GIF and JPEG images are shown"))?;
    let path: PathBuf = dir.join(format!("{key}.{}", kind.extension()));
    write_atomic(&path, &body).map_err(|e| {
        ProtocolError::new(
            ErrorCode::Internal,
            format!("could not save the image: {e}"),
        )
    })?;
    Ok(MediaFile {
        path: path.display().to_string(),
        kind,
        bytes: body.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(url: &str) -> bool {
        public_web_address(&Url::parse(url).unwrap())
    }

    #[test]
    fn only_public_web_addresses_count_as_other_sites() {
        assert!(public("https://images.example.com/a.png"));
        assert!(public("https://images.example.com:443/a.png"));
        assert!(public("https://example.org./a.png"));
        for bad in [
            "https://127.0.0.1/a.png",
            "https://[::1]/a.png",
            "https://192.168.1.10/a.png",
            "https://localhost/a.png",
            "https://printer/a.png",
            "https://nas.local/a.png",
            "https://app.localhost/a.png",
            "https://wiki.internal/a.png",
            "https://images.example.com:8443/a.png",
        ] {
            assert!(!public(bad), "{bad}");
        }
    }

    fn stub(
        addresses: &[&str],
    ) -> impl FnOnce(String, u16) -> std::future::Ready<std::io::Result<Vec<IpAddr>>> {
        let parsed: Vec<IpAddr> = addresses.iter().map(|a| a.parse().unwrap()).collect();
        move |_, _| std::future::ready(Ok(parsed))
    }

    #[test]
    fn local_addresses_are_recognised() {
        for local in [
            "127.0.0.1",
            "127.8.8.8",
            "10.0.0.5",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.10.10",
            "0.0.0.0",
            "0.1.2.3",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fd00::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_local_address(local.parse().unwrap()), "{local}");
        }
        for public in [
            "8.8.8.8",
            "93.184.216.34",
            "172.32.0.1",
            "100.128.0.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_local_address(public.parse().unwrap()), "{public}");
        }
    }

    #[tokio::test]
    async fn a_public_name_that_points_at_a_local_address_is_refused() {
        assert!(
            require_public(
                "images.example.com",
                443,
                stub(&["93.184.216.34", "2606:4700::1"])
            )
            .await
            .is_ok()
        );
        for addresses in [
            &["127.0.0.1"][..],
            &["8.8.8.8", "127.0.0.1"][..],
            &["192.168.0.20"][..],
            &["fd12:3456::1"][..],
            &["fe80::1"][..],
            &["::"][..],
            &["::ffff:10.1.1.1"][..],
        ] {
            let err = require_public("sneaky.example.com", 443, stub(addresses))
                .await
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Refused, "{addresses:?}");
            assert!(err.message.contains("sneaky.example.com"));
        }
    }

    #[tokio::test]
    async fn a_name_that_does_not_resolve_is_offline_not_refused() {
        let empty = require_public("gone.example.com", 443, stub(&[]))
            .await
            .unwrap_err();
        assert_eq!(empty.code, ErrorCode::Offline);
        let failed = require_public("gone.example.com", 443, |_, _| {
            std::future::ready(Err(std::io::Error::other("no such host")))
        })
        .await
        .unwrap_err();
        assert_eq!(failed.code, ErrorCode::Offline);
    }
}

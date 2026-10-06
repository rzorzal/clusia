//! Giphy for the GIF picker. The API key lives in the secret store under the account `giphy`;
//! searches run here so the window never holds the key or talks to Giphy.

use std::time::Duration;

use clusia_protocol::{
    ErrorCode, Event, GifItem, GifPage, GiphyKeyStatus, Outcome, ProtocolError, Reply, topics,
};
use reqwest::StatusCode;
use serde_json::Value;

use crate::media::http;
use crate::state::Shared;

const KEY_ACCOUNT: &str = "giphy";
const PAGE_SIZE: u32 = 24;
const MAX_QUERY_CHARS: usize = 200;
const TIMEOUT: Duration = Duration::from_secs(10);

fn fail(code: ErrorCode, message: impl Into<String>) -> Outcome {
    Outcome::Err(ProtocolError::new(code, message))
}

pub(crate) async fn set_key(shared: &Shared, key: &str) -> Outcome {
    let key = key.trim();
    if key.is_empty() {
        return fail(ErrorCode::BadRequest, "the key is empty");
    }
    if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return fail(
            ErrorCode::BadRequest,
            "the key has spaces or line breaks in it",
        );
    }
    if let Err(e) = shared.secrets.set(KEY_ACCOUNT, key) {
        return fail(ErrorCode::Internal, format!("could not store the key: {e}"));
    }
    shared.publish(topics::CONFIG, Event::GiphyKeyChanged);
    Outcome::Ok(Reply::Ack)
}

/// Whether a key is stored. The key itself is never part of an answer.
pub(crate) fn key_status(shared: &Shared) -> Outcome {
    match shared.secrets.get(KEY_ACCOUNT) {
        Ok(found) => Outcome::Ok(Reply::GiphyKeyStatus(GiphyKeyStatus {
            configured: found.is_some_and(|k| !k.trim().is_empty()),
        })),
        Err(e) => fail(ErrorCode::Internal, format!("could not read the key: {e}")),
    }
}

pub(crate) async fn clear_key(shared: &Shared) -> Outcome {
    if let Err(e) = shared.secrets.delete(KEY_ACCOUNT) {
        return fail(
            ErrorCode::Internal,
            format!("could not remove the key: {e}"),
        );
    }
    shared.publish(topics::CONFIG, Event::GiphyKeyChanged);
    Outcome::Ok(Reply::Ack)
}

/// A dimension Giphy sends as a number or as a string.
fn dimension(v: &Value) -> u32 {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0) as u32
}

fn item_from(v: &Value) -> Option<GifItem> {
    let images = v.get("images")?;
    let small = images.get("fixed_width_small")?;
    Some(GifItem {
        id: v.get("id")?.as_str()?.to_string(),
        title: v
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        preview_url: small.get("url")?.as_str()?.to_string(),
        url: images.get("downsized")?.get("url")?.as_str()?.to_string(),
        width: dimension(small.get("width")?),
        height: dimension(small.get("height")?),
    })
}

/// The page a Giphy answer describes. Entries without both renditions are left out.
fn page_from(body: &Value, offset: u32) -> Option<GifPage> {
    let data = body.get("data")?.as_array()?;
    let next = offset.saturating_add(data.len() as u32);
    let more = match body
        .pointer("/pagination/total_count")
        .and_then(Value::as_u64)
    {
        Some(total) => u64::from(next) < total,
        None => data.len() as u32 >= PAGE_SIZE,
    };
    Some(GifPage {
        items: data.iter().filter_map(item_from).collect(),
        next_offset: (more && !data.is_empty()).then_some(next),
    })
}

pub(crate) async fn search(shared: &Shared, query: &str, offset: u32) -> Outcome {
    let key = match shared.secrets.get(KEY_ACCOUNT) {
        Ok(Some(k)) if !k.trim().is_empty() => k.trim().to_string(),
        Ok(_) => {
            return fail(
                ErrorCode::NotConfigured,
                "add a Giphy key in Config › Media to search GIFs",
            );
        }
        Err(e) => return fail(ErrorCode::Internal, format!("could not read the key: {e}")),
    };
    let query = query.trim();
    if query.chars().count() > MAX_QUERY_CHARS {
        return fail(ErrorCode::BadRequest, "the search is too long");
    }
    let mut params = vec![
        ("api_key", key),
        ("limit", PAGE_SIZE.to_string()),
        ("offset", offset.to_string()),
        ("rating", "pg-13".to_string()),
    ];
    let route = if query.is_empty() {
        "/v1/gifs/trending"
    } else {
        params.push(("q", query.to_string()));
        "/v1/gifs/search"
    };
    let url = format!("{}{route}", shared.giphy_api.trim_end_matches('/'));
    let response = match http().get(url).query(&params).timeout(TIMEOUT).send().await {
        Ok(r) => r,
        Err(e) => {
            return fail(
                ErrorCode::Offline,
                format!("could not reach Giphy: {}", e.without_url()),
            );
        }
    };
    match response.status() {
        s if s.is_success() => {}
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            return fail(ErrorCode::Unauthorized, "Giphy rejected the key");
        }
        StatusCode::TOO_MANY_REQUESTS => {
            return fail(
                ErrorCode::RateLimited,
                "Giphy is limiting this key; try again in a while",
            );
        }
        s => {
            return fail(
                ErrorCode::Upstream,
                format!("Giphy answered HTTP {}", s.as_u16()),
            );
        }
    }
    let page = response
        .json::<Value>()
        .await
        .ok()
        .and_then(|body| page_from(&body, offset));
    match page {
        Some(page) => Outcome::Ok(Reply::Gifs(page)),
        None => fail(
            ErrorCode::Upstream,
            "Giphy sent an answer Clúsia cannot read",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_at_the_top_of_the_offset_range_does_not_overflow() {
        let body = serde_json::json!({"data": [{}, {}], "pagination": {"total_count": 10}});
        let page = page_from(&body, u32::MAX).unwrap();
        assert_eq!(page.next_offset, None);
    }
}

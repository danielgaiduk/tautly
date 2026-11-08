//! Helpers shared by the provider adapters.

use std::time::{Duration, SystemTime};

use reqwest::header::HeaderMap;

use crate::{Message, ProviderError, Quota};

pub(crate) fn header_u64(h: &HeaderMap, name: &str) -> Option<u64> {
    h.get(name)?.to_str().ok()?.trim().parse().ok()
}

/// The `retry-after` header in seconds, if present.
pub(crate) fn retry_after(h: &HeaderMap) -> Option<Duration> {
    header_u64(h, "retry-after").map(Duration::from_secs)
}

/// Consecutive messages with the same role joined into one (content concatenated in order).
/// The engine's working-state tail is a separate user message after tool results or the
/// prompt; APIs and chat templates that need alternating roles get a single turn instead.
pub(crate) fn merge_same_role(messages: &[Message]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for m in messages {
        match out.last_mut() {
            Some(last) if last.role == m.role => last.content.extend(m.content.iter().cloned()),
            _ => out.push(m.clone()),
        }
    }
    out
}

pub(crate) fn transient(msg: impl Into<String>) -> ProviderError {
    ProviderError::Transient {
        status: None,
        retry_after: None,
        msg: msg.into(),
    }
}

/// The error body's message, or `HTTP <status>` when it has none.
pub(crate) fn http_msg(status: u16, msg: String) -> String {
    if msg.is_empty() {
        format!("HTTP {status}")
    } else {
        msg
    }
}

/// The error for an HTTP error status, the same for every adapter. `overflow` tells
/// whether a 400's message reads as a context overflow.
pub(crate) fn map_http_status(
    status: u16,
    retry_after: Option<Duration>,
    msg: String,
    overflow: impl FnOnce(&str) -> bool,
) -> ProviderError {
    let msg = http_msg(status, msg);
    match status {
        429 | 500..=504 => ProviderError::Transient {
            status: Some(status),
            retry_after,
            msg,
        },
        401 | 403 => ProviderError::Auth(msg),
        400 if overflow(&msg) => ProviderError::ContextOverflow(msg),
        400..=499 => ProviderError::InvalidRequest(msg),
        _ => ProviderError::Other(format!("HTTP {status}: {msg}")),
    }
}

/// Parse a Go-style duration such as `1s`, `6m0s` or `20ms`.
fn parse_go_duration(s: &str) -> Option<Duration> {
    let mut rest = s.trim();
    if rest.is_empty() {
        return None;
    }
    let mut secs = 0f64;
    let is_num = |c: char| c.is_ascii_digit() || c == '.';
    while !rest.is_empty() {
        let n_end = rest.find(|c| !is_num(c))?;
        let n: f64 = rest[..n_end].parse().ok()?;
        rest = &rest[n_end..];
        let u_end = rest.find(is_num).unwrap_or(rest.len());
        let scale = match &rest[..u_end] {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 1e-3,
            "us" | "\u{b5}s" => 1e-6,
            "ns" => 1e-9,
            _ => return None,
        };
        secs += n * scale;
        rest = &rest[u_end..];
    }
    Duration::try_from_secs_f64(secs).ok()
}

fn header_duration(h: &HeaderMap, name: &str) -> Option<Duration> {
    parse_go_duration(h.get(name)?.to_str().ok()?)
}

pub(crate) fn openai_quota_from_headers(h: &HeaderMap) -> Option<Quota> {
    let now = SystemTime::now();
    let quota = Quota {
        remaining_requests: header_u64(h, "x-ratelimit-remaining-requests"),
        remaining_tokens: header_u64(h, "x-ratelimit-remaining-tokens"),
        // Quota is fully restored once the later of the resets has passed.
        reset_at: [
            header_duration(h, "x-ratelimit-reset-requests"),
            header_duration(h, "x-ratelimit-reset-tokens"),
        ]
        .into_iter()
        .flatten()
        .max()
        .and_then(|d| now.checked_add(d)),
    };
    (quota != Quota::default()).then_some(quota)
}

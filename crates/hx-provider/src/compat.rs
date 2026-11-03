//! OpenAI-compatible Chat Completions adapter (Ollama, vLLM, LM Studio, ...).
//!
//! No reasoning parameter is sent because there is no portable field for it; servers that
//! stream reasoning text (`reasoning_content` or `reasoning`) still have it surfaced.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::sse::try_events;
use crate::util::{merge_same_role, openai_quota_from_headers, retry_after, transient};
use crate::{
    Block, CanonicalRequest, Catalog, Content, Provider, ProviderError, ProviderEvent, RawUsage,
    Role, StopReason,
};

#[derive(Clone)]
pub struct CompatProvider {
    client: reqwest::Client,
    api_key: Option<String>,
    base_url: Url,
    catalog: Arc<Catalog>,
}

impl CompatProvider {
    /// `base_url` must include any version prefix the server needs (for example `/v1`).
    pub fn new(api_key: Option<String>, base_url: Url, catalog: Arc<Catalog>) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client with rustls builds");
        Self {
            client,
            api_key,
            base_url,
            catalog,
        }
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/chat/completions",
            self.base_url.as_str().trim_end_matches('/')
        )
    }
}

fn messages(req: &CanonicalRequest) -> Vec<Value> {
    let system = req
        .system
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) => Some(t.as_str()),
            Block::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut out = Vec::new();
    if !system.is_empty() {
        out.push(json!({"role": "system", "content": system}));
    }
    for m in &merge_same_role(&req.messages) {
        if m.role == Role::Assistant {
            let mut text = String::new();
            let mut calls = Vec::new();
            for c in &m.content {
                match c {
                    Content::Text(t) => text.push_str(t),
                    Content::ToolUse { id, name, input } => calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": input.to_string()},
                    })),
                    Content::ToolResult { .. }
                    | Content::Reasoning { .. }
                    | Content::RedactedReasoning { .. }
                    | Content::Image { .. } => {}
                }
            }
            if text.is_empty() && calls.is_empty() {
                continue;
            }
            let mut msg = json!({
                "role": "assistant",
                "content": if text.is_empty() { Value::Null } else { text.into() },
            });
            if !calls.is_empty() {
                msg["tool_calls"] = calls.into();
            }
            out.push(msg);
        } else {
            // Tool messages must directly follow the assistant calls they answer.
            for c in &m.content {
                if let Content::ToolResult { id, output, .. } = c {
                    out.push(json!({"role": "tool", "tool_call_id": id, "content": output}));
                }
            }
            let has_image = m.content.iter().any(|c| matches!(c, Content::Image { .. }));
            let content: Value = if has_image {
                m.content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text(t) => Some(json!({"type": "text", "text": t})),
                        Content::Image { media_type, data } => Some(json!({
                            "type": "image_url",
                            "image_url": {"url": format!("data:{media_type};base64,{}", STANDARD.encode(data))},
                        })),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .into()
            } else {
                m.content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n")
                    .into()
            };
            if content.as_str().is_some_and(str::is_empty) {
                continue;
            }
            out.push(json!({"role": "user", "content": content}));
        }
    }
    out
}

/// Build the Chat Completions request body.
pub fn to_wire(req: &CanonicalRequest) -> Value {
    let mut body = json!({
        "model": req.model.model(),
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_tokens": req.max_output,
        "messages": messages(req),
    });
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.schema}}))
            .collect();
    }
    body
}

fn error_parts(body: &str) -> (Option<String>, String) {
    match serde_json::from_str::<Value>(body) {
        Ok(v) => {
            let err = &v["error"];
            // Ollama and some proxies send `{"error": "message"}`.
            let msg = err["message"]
                .as_str()
                .or_else(|| err.as_str())
                .unwrap_or(body);
            (err["code"].as_str().map(String::from), msg.to_string())
        }
        Err(_) => (None, body.to_string()),
    }
}

fn is_overflow(code: Option<&str>, msg: &str) -> bool {
    let lower = msg.to_lowercase();
    code == Some("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("context window")
}

fn map_http_error(status: u16, retry_after: Option<Duration>, body: &str) -> ProviderError {
    let (code, msg) = error_parts(body);
    let msg = if msg.is_empty() {
        format!("HTTP {status}")
    } else {
        msg
    };
    match status {
        429 | 500..=504 => ProviderError::Transient {
            status: Some(status),
            retry_after,
            msg,
        },
        401 | 403 => ProviderError::Auth(msg),
        400 if is_overflow(code.as_deref(), &msg) => ProviderError::ContextOverflow(msg),
        400..=499 => ProviderError::InvalidRequest(msg),
        _ => ProviderError::Other(format!("HTTP {status}: {msg}")),
    }
}

fn map_stream_error(v: &Value) -> ProviderError {
    let err = &v["error"];
    let msg = err["message"]
        .as_str()
        .or_else(|| err.as_str())
        .unwrap_or("stream error")
        .to_string();
    match err["type"].as_str() {
        Some("server_error" | "rate_limit_exceeded") => transient(msg),
        _ => ProviderError::Other(msg),
    }
}

fn stop_reason(finish: &str, has_calls: bool) -> StopReason {
    match finish {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        // Some servers report `stop` even when the turn ends in tool calls.
        "stop" if has_calls => StopReason::ToolUse,
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        other => StopReason::Other(other.to_string()),
    }
}

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    args: String,
}

/// Per-response state machine turning Chat Completions SSE payloads into canonical events.
#[derive(Default)]
struct StreamState {
    calls: BTreeMap<u64, Call>,
    /// Key of the call the latest delta belongs to (used when deltas carry no `index`).
    current: Option<u64>,
    has_calls: bool,
    finish: Option<String>,
    usage: Option<RawUsage>,
}

impl StreamState {
    fn flush_calls(&mut self, out: &mut Vec<ProviderEvent>) {
        // The flushed calls are gone; a later index-less delta must not continue one.
        self.current = None;
        for (index, c) in std::mem::take(&mut self.calls) {
            self.has_calls = true;
            out.push(ProviderEvent::ToolCall {
                id: if c.id.is_empty() {
                    format!("call_{index}")
                } else {
                    c.id
                },
                name: c.name,
                input_json: if c.args.trim().is_empty() {
                    "{}".into()
                } else {
                    c.args
                },
            });
        }
    }

    fn handle(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        let v: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Other(format!("invalid SSE payload: {e}")))?;
        if !v["error"].is_null() {
            return Err(map_stream_error(&v));
        }
        let mut out = Vec::new();
        let u = &v["usage"];
        if u.is_object() {
            let get = |x: &Value, k: &str| x[k].as_u64().unwrap_or(0);
            let cached = get(&u["prompt_tokens_details"], "cached_tokens");
            self.usage = Some(RawUsage {
                // `prompt_tokens` includes cached tokens; report them separately.
                input: get(u, "prompt_tokens").saturating_sub(cached),
                output: get(u, "completion_tokens"),
                cache_read: cached,
                cache_write: 0,
            });
        }
        let choice = &v["choices"][0];
        let delta = &choice["delta"];
        for key in ["reasoning_content", "reasoning"] {
            if let Some(t) = delta[key].as_str().filter(|t| !t.is_empty()) {
                out.push(ProviderEvent::ReasoningDelta(t.into()));
                break;
            }
        }
        if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            out.push(ProviderEvent::TextDelta(t.into()));
        }
        for tc in delta["tool_calls"].as_array().into_iter().flatten() {
            let id = tc["id"].as_str().filter(|s| !s.is_empty());
            let key = match tc["index"].as_u64() {
                Some(i) => i,
                // Without an index, a delta with a new id starts a new call; anything else
                // continues the current one.
                None => match self.current.filter(|k| {
                    self.calls
                        .get(k)
                        .is_some_and(|c| id.is_none_or(|id| c.id.is_empty() || c.id == id))
                }) {
                    Some(k) => k,
                    None => self.calls.keys().next_back().map_or(0, |k| k + 1),
                },
            };
            self.current = Some(key);
            let call = self.calls.entry(key).or_default();
            if let Some(id) = tc["id"].as_str().filter(|s| !s.is_empty()) {
                call.id = id.into();
            }
            if let Some(n) = tc["function"]["name"].as_str().filter(|s| !s.is_empty()) {
                call.name = n.into();
            }
            if let Some(a) = tc["function"]["arguments"].as_str() {
                call.args.push_str(a);
            }
        }
        if let Some(f) = choice["finish_reason"].as_str() {
            self.flush_calls(&mut out);
            self.finish = Some(f.to_string());
        }
        Ok(out)
    }

    /// Events for the end of the stream (`[DONE]` or the connection closing); `None` when the
    /// stream was cut short before the choice finished.
    fn finish(&mut self, done_seen: bool) -> Option<Vec<ProviderEvent>> {
        if self.finish.is_none() && !done_seen {
            return None;
        }
        let mut out = Vec::new();
        self.flush_calls(&mut out);
        if let Some(u) = self.usage {
            out.push(ProviderEvent::Usage(u));
        }
        let finish = self.finish.take().unwrap_or_else(|| "stop".into());
        out.push(ProviderEvent::Done {
            stop: stop_reason(&finish, self.has_calls),
        });
        Some(out)
    }
}

impl Provider for CompatProvider {
    fn name(&self) -> &str {
        "compat"
    }

    fn stream(
        &self,
        mut req: CanonicalRequest,
        cancel: CancellationToken,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        if self.catalog.contains(&req.model) {
            req.max_output = req
                .max_output
                .min(self.catalog.get(&req.model).caps.max_output);
        }
        if req.system.iter().any(|b| matches!(b, Block::Image { .. })) {
            return Box::pin(futures::stream::once(async {
                Err(ProviderError::InvalidRequest(
                    "images are not supported in the system prompt".into(),
                ))
            }));
        }
        let mut builder = self
            .client
            .post(self.endpoint())
            .header("content-type", "application/json")
            .body(to_wire(&req).to_string());
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }

        Box::pin(async_stream::stream! {
            let sent = tokio::select! {
                () = cancel.cancelled() => return,
                r = builder.send() => r,
            };
            let resp = match sent {
                Ok(r) => r,
                Err(e) => {
                    yield Err(if e.is_builder() { ProviderError::Other(e.to_string()) } else { transient(format!("request failed: {e}")) });
                    return;
                }
            };
            let status = resp.status();
            if !status.is_success() {
                let retry_after = retry_after(resp.headers());
                let body = resp.text().await.unwrap_or_default();
                yield Err(map_http_error(status.as_u16(), retry_after, &body));
                return;
            }
            if let Some(q) = openai_quota_from_headers(resp.headers()) {
                yield Ok(ProviderEvent::Quota(q));
            }

            let mut events = std::pin::pin!(try_events(resp.bytes_stream()));
            let mut state = StreamState::default();
            loop {
                let next = tokio::select! {
                    () = cancel.cancelled() => return,
                    n = events.next() => n,
                };
                let ev = match next {
                    Some(Ok(ev)) => Some(ev),
                    Some(Err(e)) => {
                        yield Err(e);
                        return;
                    }
                    None => None,
                };
                let done = ev.as_ref().is_none_or(|e| e.data.trim() == "[DONE]");
                if done {
                    // [DONE] and a closed connection both end the stream.
                    let eof = ev.is_none();
                    match state.finish(!eof) {
                        Some(out) => {
                            for e in out {
                                yield Ok(e);
                            }
                        }
                        None => yield Err(transient("stream ended before finish_reason")),
                    }
                    return;
                }
                match state.handle(&ev.expect("event present").data) {
                    Ok(out) => {
                        for e in out {
                            yield Ok(e);
                        }
                    }
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                }
            }
        })
    }
}

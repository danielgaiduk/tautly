//! Anthropic Messages API adapter.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::{Engine, engine::general_purpose::STANDARD};
use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::sse::try_events;
use crate::util::{header_u64, retry_after, transient};
use crate::{
    Block, CachePoint, CanonicalRequest, Catalog, Content, Effort, Provider, ProviderError,
    ProviderEvent, Quota, RawUsage, Reasoning, Role, StopReason,
};

const API_VERSION: &str = "2023-06-01";

#[derive(Clone)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: Url,
    catalog: Arc<Catalog>,
}

impl AnthropicProvider {
    pub fn new(api_key: String, base_url: Url, catalog: Arc<Catalog>) -> Self {
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
            "{}/v1/messages",
            self.base_url.as_str().trim_end_matches('/')
        )
    }
}

fn cache_control() -> Value {
    json!({"type": "ephemeral"})
}

fn mark_last(blocks: &mut [Value]) {
    if let Some(Value::Object(o)) = blocks.last_mut() {
        o.insert("cache_control".into(), cache_control());
    }
}

fn effort_name(e: &Effort) -> &'static str {
    match e {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
    }
}

fn content_to_wire(c: &Content) -> Option<Value> {
    Some(match c {
        Content::Text(t) => json!({"type": "text", "text": t}),
        Content::Image { media_type, data } => {
            json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": STANDARD.encode(data)}})
        }
        Content::ToolUse { id, name, input } => {
            json!({"type": "tool_use", "id": id, "name": name, "input": input})
        }
        Content::ToolResult {
            id,
            output,
            is_error,
        } => {
            json!({"type": "tool_result", "tool_use_id": id, "content": output, "is_error": is_error})
        }
        // Thinking blocks are only valid when sent back with their signature.
        Content::Reasoning {
            text,
            signature: Some(sig),
        } => json!({"type": "thinking", "thinking": text, "signature": sig}),
        Content::Reasoning {
            signature: None, ..
        } => return None,
        Content::RedactedReasoning { data } => json!({"type": "redacted_thinking", "data": data}),
    })
}

/// Lets adaptive requests set `thinking.block_binding` (preserved thinking).
const BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";
/// Thinking between tool calls for budget-style (older) models.
const INTERLEAVED_BETA: &str = "interleaved-thinking-2025-05-14";

/// A budget below `max_tokens` with room for the answer (half of it), or none when that
/// is under the API's 1024 minimum.
fn thinking_budget(n: u32, max_output: u32) -> Option<u32> {
    Some(n.min(max_output / 2)).filter(|b| *b >= 1_024)
}

/// The `anthropic-beta` values a request needs: the binding controls with adaptive
/// thinking (its `block_binding`), interleaved thinking with a budget and tools.
pub fn beta_headers(req: &CanonicalRequest) -> Vec<&'static str> {
    match &req.reasoning {
        Some(Reasoning::Effort(_)) => vec![BINDING_BETA],
        Some(Reasoning::Budget(n))
            if !req.tools.is_empty() && thinking_budget(*n, req.max_output).is_some() =>
        {
            vec![INTERLEAVED_BETA]
        }
        _ => Vec::new(),
    }
}

/// Build the Messages API request body.
pub fn to_wire(req: &CanonicalRequest) -> Value {
    let mut system: Vec<Value> = req
        .system
        .iter()
        .map(|b| match b {
            Block::Text(t) => json!({"type": "text", "text": t}),
            Block::Image { media_type, data } => {
                json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": STANDARD.encode(data)}})
            }
        })
        .collect();
    let mut tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.schema}))
        .collect();
    let mut messages: Vec<Value> = req
        .messages
        .iter()
        .map(|m| {
            let mut blocks: Vec<Value> = m
                .content
                .iter()
                // The API accepts images in user messages only.
                .filter(|c| !(m.role == Role::Assistant && matches!(c, Content::Image { .. })))
                .filter_map(content_to_wire)
                .collect();
            // tool_result blocks must precede any other content in a user message.
            blocks.sort_by_key(|b| b["type"] != "tool_result");
            json!({"role": if m.role == Role::Assistant { "assistant" } else { "user" }, "content": blocks})
        })
        .collect();

    for point in &req.cache_breakpoints {
        match point {
            CachePoint::AfterSystem => mark_last(&mut system),
            CachePoint::AfterTools => mark_last(&mut tools),
            CachePoint::AfterMessage(i) => {
                if let Some(blocks) = messages
                    .get_mut(*i)
                    .and_then(|m| m["content"].as_array_mut())
                {
                    mark_last(blocks);
                }
            }
        }
    }

    let mut body = json!({
        "model": req.model.model(),
        "max_tokens": req.max_output,
        "stream": true,
        "messages": messages,
    });
    if !system.is_empty() {
        body["system"] = system.into();
    }
    if !tools.is_empty() {
        body["tools"] = tools.into();
    }
    // No `temperature` or forced `tool_choice` is ever sent: both conflict with thinking.
    match &req.reasoning {
        // Older models: a fixed budget (no history check runs, so no block_binding).
        Some(Reasoning::Budget(n)) => {
            if let Some(budget) = thinking_budget(*n, req.max_output) {
                body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
            }
        }
        // Current models reject `budget_tokens`: adaptive thinking, depth set by effort.
        // A thinking block an earlier history edit (GC, compaction, a pin fold)
        // invalidated is dropped by the API instead of failing the request.
        Some(Reasoning::Effort(e)) => {
            body["thinking"] = json!({
                "type": "adaptive",
                "display": "summarized",
                "block_binding": {"prefix_mismatch_behavior": "drop_block"},
            });
            body["output_config"] = json!({"effort": effort_name(e)});
        }
        None => {}
    }
    body
}

fn header_time(h: &HeaderMap, name: &str) -> Option<SystemTime> {
    humantime::parse_rfc3339(h.get(name)?.to_str().ok()?.trim()).ok()
}

fn quota_from_headers(h: &HeaderMap) -> Option<Quota> {
    let quota = Quota {
        remaining_requests: header_u64(h, "anthropic-ratelimit-requests-remaining"),
        remaining_tokens: header_u64(h, "anthropic-ratelimit-tokens-remaining"),
        // Quota is fully restored once the later of the resets has passed.
        reset_at: [
            header_time(h, "anthropic-ratelimit-requests-reset"),
            header_time(h, "anthropic-ratelimit-tokens-reset"),
        ]
        .into_iter()
        .flatten()
        .max(),
    };
    (quota != Quota::default()).then_some(quota)
}

fn error_parts(body: &str) -> (Option<String>, String) {
    match serde_json::from_str::<Value>(body) {
        Ok(v) => {
            let err = &v["error"];
            (
                err["type"].as_str().map(String::from),
                err["message"].as_str().unwrap_or(body).to_string(),
            )
        }
        Err(_) => (None, body.to_string()),
    }
}

fn map_http_error(status: u16, retry_after: Option<Duration>, body: &str) -> ProviderError {
    let (kind, msg) = error_parts(body);
    let msg = if msg.is_empty() {
        format!("HTTP {status}")
    } else {
        msg
    };
    let lower = msg.to_lowercase();
    let transient = || ProviderError::Transient {
        status: Some(status),
        retry_after,
        msg: msg.clone(),
    };
    match status {
        429 | 500..=504 | 529 => transient(),
        _ if kind.as_deref() == Some("overloaded_error") => transient(),
        401 | 403 => ProviderError::Auth(msg),
        413 => ProviderError::ContextOverflow(msg),
        400 if [
            "prompt is too long",
            "input is too long",
            "exceeds the maximum context",
        ]
        .iter()
        .any(|p| lower.contains(p)) =>
        {
            ProviderError::ContextOverflow(msg)
        }
        400..=499 => ProviderError::InvalidRequest(msg),
        _ => ProviderError::Other(format!("HTTP {status}: {msg}")),
    }
}

fn map_stream_error(v: &Value) -> ProviderError {
    let err = &v["error"];
    let msg = err["message"]
        .as_str()
        .unwrap_or("stream error")
        .to_string();
    match err["type"].as_str() {
        Some("overloaded_error" | "api_error") => ProviderError::Transient {
            status: None,
            retry_after: None,
            msg,
        },
        Some(t) => ProviderError::Other(format!("{t}: {msg}")),
        None => ProviderError::Other(msg),
    }
}

fn stop_reason(s: Option<&str>) -> StopReason {
    match s {
        Some("end_turn") | None => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("stop_sequence") => StopReason::StopSequence,
        Some("refusal") => StopReason::Refusal,
        Some(other) => StopReason::Other(other.to_string()),
    }
}

fn merge_usage(usage: &mut RawUsage, v: &Value) {
    let get = |k: &str| v[k].as_u64();
    if let Some(n) = get("input_tokens") {
        usage.input = n;
    }
    if let Some(n) = get("output_tokens") {
        usage.output = n;
    }
    if let Some(n) = get("cache_read_input_tokens") {
        usage.cache_read = n;
    }
    if let Some(n) = get("cache_creation_input_tokens") {
        usage.cache_write = n;
    }
}

struct ToolBlock {
    id: String,
    name: String,
    json: String,
}

/// Per-response state machine turning Anthropic SSE payloads into canonical events.
#[derive(Default)]
struct StreamState {
    usage: RawUsage,
    stop: Option<String>,
    tools: HashMap<u64, ToolBlock>,
}

impl StreamState {
    /// Returns the events for one SSE payload; `Ok(true)` in the flag means the message ended.
    fn handle(&mut self, data: &str) -> Result<(Vec<ProviderEvent>, bool), ProviderError> {
        let v: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Other(format!("invalid SSE payload: {e}")))?;
        let index = v["index"].as_u64().unwrap_or(0);
        let mut out = Vec::new();
        match v["type"].as_str().unwrap_or_default() {
            // Reported at once (cumulative; the total at the end replaces it), so a stream
            // that breaks off still says what its input cost.
            "message_start" => {
                merge_usage(&mut self.usage, &v["message"]["usage"]);
                out.push(ProviderEvent::Usage(self.usage));
            }
            "content_block_start" => {
                let b = &v["content_block"];
                if b["type"] == "redacted_thinking" {
                    // Encrypted reasoning arrives whole; it must be sent back unchanged.
                    out.push(ProviderEvent::RedactedReasoning(
                        b["data"].as_str().unwrap_or_default().into(),
                    ));
                } else if b["type"] == "tool_use" {
                    let s = |k: &str| b[k].as_str().unwrap_or_default().to_string();
                    self.tools.insert(
                        index,
                        ToolBlock {
                            id: s("id"),
                            name: s("name"),
                            json: String::new(),
                        },
                    );
                }
            }
            "content_block_delta" => {
                let d = &v["delta"];
                match d["type"].as_str().unwrap_or_default() {
                    "text_delta" => out.push(ProviderEvent::TextDelta(
                        d["text"].as_str().unwrap_or_default().into(),
                    )),
                    "thinking_delta" => out.push(ProviderEvent::ReasoningDelta(
                        d["thinking"].as_str().unwrap_or_default().into(),
                    )),
                    "input_json_delta" => {
                        if let Some(t) = self.tools.get_mut(&index) {
                            t.json
                                .push_str(d["partial_json"].as_str().unwrap_or_default());
                        }
                    }
                    "signature_delta" => out.push(ProviderEvent::ReasoningSignature(
                        d["signature"].as_str().unwrap_or_default().into(),
                    )),
                    // Unknown delta types carry nothing the canonical stream models.
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(t) = self.tools.remove(&index) {
                    let input_json = if t.json.trim().is_empty() {
                        "{}".to_string()
                    } else {
                        t.json
                    };
                    out.push(ProviderEvent::ToolCall {
                        id: t.id,
                        name: t.name,
                        input_json,
                    });
                }
            }
            "message_delta" => {
                if let Some(s) = v["delta"]["stop_reason"].as_str() {
                    self.stop = Some(s.to_string());
                }
                merge_usage(&mut self.usage, &v["usage"]);
            }
            "message_stop" => {
                out.push(ProviderEvent::Usage(self.usage));
                out.push(ProviderEvent::Done {
                    stop: stop_reason(self.stop.as_deref()),
                });
                return Ok((out, true));
            }
            "error" => return Err(map_stream_error(&v)),
            // ping and unknown future event types are ignored.
            _ => {}
        }
        Ok((out, false))
    }
}

impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
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
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json");
        let betas = beta_headers(&req);
        if !betas.is_empty() {
            builder = builder.header("anthropic-beta", betas.join(","));
        }
        let builder = builder.body(to_wire(&req).to_string());

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
            if let Some(q) = quota_from_headers(resp.headers()) {
                yield Ok(ProviderEvent::Quota(q));
            }

            let mut events = std::pin::pin!(try_events(resp.bytes_stream()));
            let mut state = StreamState::default();
            loop {
                let next = tokio::select! {
                    () = cancel.cancelled() => return,
                    n = events.next() => n,
                };
                let Some(item) = next else {
                    yield Err(transient("stream ended before message_stop"));
                    return;
                };
                let ev = match item {
                    Ok(ev) => ev,
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                };
                match state.handle(&ev.data) {
                    Ok((out, finished)) => {
                        for e in out {
                            yield Ok(e);
                        }
                        if finished {
                            return;
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

//! OpenAI Responses API adapter.
//!
//! Requests are stateless: `store: false`, the full input is sent every time, and
//! `include: ["reasoning.encrypted_content"]` makes reasoning items come back encrypted so
//! they can be replayed. Each reasoning output item (`id`, `encrypted_content`, `summary`)
//! is surfaced as a [`ProviderEvent::RedactedReasoning`] whose data is that item as JSON,
//! stored as [`Content::RedactedReasoning`], and sent back as a `reasoning` input item in
//! its original position. `previous_response_id` is never used: the engine's per-request
//! working-state tail means a stored response could not be continued anyway, and nothing
//! stays on OpenAI's side. Automatic prefix caching still applies to the repeated prefix.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::sse::try_events;
use crate::util::{openai_quota_from_headers, retry_after, transient};
use crate::{
    Block, CanonicalRequest, Catalog, Content, Effort, Message, Provider, ProviderError,
    ProviderEvent, RawUsage, Reasoning, Role, StopReason,
};

#[derive(Clone)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: Url,
    catalog: Arc<Catalog>,
}

impl OpenAiProvider {
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
            "{}/v1/responses",
            self.base_url.as_str().trim_end_matches('/')
        )
    }
}

fn effort_name(e: &Effort) -> &'static str {
    match e {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
    }
}

fn budget_effort(n: u32) -> &'static str {
    match n {
        0..=4_096 => "low",
        4_097..=12_288 => "medium",
        _ => "high",
    }
}

fn message_item(role: &str, part: &str, text: &str) -> Value {
    json!({"role": role, "content": [{"type": part, "text": text}]})
}

fn tool_call_item(id: &str, name: &str, input: &Value) -> Value {
    json!({"type": "function_call", "call_id": id, "name": name, "arguments": input.to_string()})
}

/// The `reasoning` input item stored in `data` (see the module doc), if it is one. Opaque
/// reasoning from other providers is not.
fn reasoning_item(data: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(data).ok()?;
    (v["type"] == "reasoning" && v["encrypted_content"].is_string()).then_some(v)
}

/// Convert canonical messages into Responses `input` items. Plain reasoning text is dropped
/// (it is only a summary); opaque reasoning items are replayed in place.
fn input_items(messages: &[Message]) -> Vec<Value> {
    let mut items = Vec::new();
    for m in messages {
        if m.role == Role::Assistant {
            let mut text = String::new();
            let flush = |text: &mut String, items: &mut Vec<Value>| {
                if !text.is_empty() {
                    items.push(message_item("assistant", "output_text", text));
                    text.clear();
                }
            };
            for c in &m.content {
                match c {
                    Content::Text(t) => text.push_str(t),
                    Content::ToolUse { id, name, input } => {
                        flush(&mut text, &mut items);
                        items.push(tool_call_item(id, name, input));
                    }
                    Content::RedactedReasoning { data } => {
                        if let Some(item) = reasoning_item(data) {
                            flush(&mut text, &mut items);
                            items.push(item);
                        }
                    }
                    Content::ToolResult { .. }
                    | Content::Reasoning { .. }
                    | Content::Image { .. } => {}
                }
            }
            flush(&mut text, &mut items);
        } else {
            // Tool outputs must directly follow the calls they answer, so they go first.
            for c in &m.content {
                if let Content::ToolResult { id, output, .. } = c {
                    items.push(
                        json!({"type": "function_call_output", "call_id": id, "output": output}),
                    );
                }
            }
            let parts: Vec<Value> = m
                .content
                .iter()
                .filter_map(|c| match c {
                    Content::Text(t) => Some(json!({"type": "input_text", "text": t})),
                    Content::Image { media_type, data } => Some(json!({
                        "type": "input_image",
                        "image_url": format!("data:{media_type};base64,{}", STANDARD.encode(data)),
                    })),
                    _ => None,
                })
                .collect();
            if !parts.is_empty() {
                let role = if m.role == Role::System {
                    "system"
                } else {
                    "user"
                };
                items.push(json!({"role": role, "content": parts}));
            }
        }
    }
    items
}

fn build_body(req: &CanonicalRequest, input: &[Value]) -> Value {
    let instructions = req
        .system
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) => Some(t.as_str()),
            Block::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut body = json!({
        "model": req.model.model(),
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "max_output_tokens": req.max_output,
        "input": input,
    });
    if !instructions.is_empty() {
        body["instructions"] = instructions.into();
    }
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.schema}))
            .collect();
    }
    match &req.reasoning {
        Some(Reasoning::Effort(e)) => {
            body["reasoning"] = json!({"effort": effort_name(e), "summary": "auto"})
        }
        Some(Reasoning::Budget(n)) => {
            body["reasoning"] = json!({"effort": budget_effort(*n), "summary": "auto"})
        }
        None => {}
    }
    body
}

/// Build the Responses API request body from all of `req.messages`.
pub fn to_wire(req: &CanonicalRequest) -> Value {
    build_body(req, &input_items(&req.messages))
}

fn error_parts(body: &str) -> (Option<String>, String) {
    match serde_json::from_str::<Value>(body) {
        Ok(v) => {
            let err = &v["error"];
            (
                err["code"].as_str().map(String::from),
                err["message"].as_str().unwrap_or(body).to_string(),
            )
        }
        Err(_) => (None, body.to_string()),
    }
}

fn is_overflow(code: Option<&str>, msg: &str) -> bool {
    code == Some("context_length_exceeded") || msg.to_lowercase().contains("maximum context length")
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

fn map_stream_error(code: Option<&str>, msg: &str) -> ProviderError {
    match code {
        Some("server_error" | "rate_limit_exceeded") => transient(msg),
        Some(c) => ProviderError::Other(format!("{c}: {msg}")),
        None => ProviderError::Other(msg.to_string()),
    }
}

struct Call {
    id: String,
    name: String,
    args: String,
}

/// Per-response state machine turning Responses SSE payloads into canonical events.
#[derive(Default)]
struct StreamState {
    calls: HashMap<u64, Call>,
    emitted: HashSet<u64>,
    has_calls: bool,
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

impl StreamState {
    fn emit_call(&mut self, index: u64, call: Call, out: &mut Vec<ProviderEvent>) {
        if !self.emitted.insert(index) {
            return;
        }
        self.has_calls = true;
        let input_json = if call.args.trim().is_empty() {
            "{}".to_string()
        } else {
            call.args
        };
        out.push(ProviderEvent::ToolCall {
            id: call.id,
            name: call.name,
            input_json,
        });
    }

    fn finish(&mut self, resp: &Value) -> Vec<ProviderEvent> {
        let u = &resp["usage"];
        let total_in = u["input_tokens"].as_u64().unwrap_or(0);
        let cached = u["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0);
        let usage = RawUsage {
            // `input_tokens` includes cached tokens; report them separately.
            input: total_in.saturating_sub(cached),
            output: u["output_tokens"].as_u64().unwrap_or(0),
            cache_read: cached,
            cache_write: 0,
        };
        let stop = if resp["status"] == "incomplete"
            && resp["incomplete_details"]["reason"] == "max_output_tokens"
        {
            StopReason::MaxTokens
        } else if self.has_calls {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        };
        vec![ProviderEvent::Usage(usage), ProviderEvent::Done { stop }]
    }

    /// Returns the events for one SSE payload; `Some` means the response ended.
    fn handle(&mut self, data: &str) -> Result<(Vec<ProviderEvent>, bool), ProviderError> {
        let v: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Other(format!("invalid SSE payload: {e}")))?;
        let index = v["output_index"].as_u64().unwrap_or(0);
        let mut out = Vec::new();
        match v["type"].as_str().unwrap_or_default() {
            "response.output_item.added" => {
                let item = &v["item"];
                if item["type"] == "function_call" {
                    self.calls.insert(
                        index,
                        Call {
                            id: s(item, "call_id"),
                            name: s(item, "name"),
                            args: s(item, "arguments"),
                        },
                    );
                }
            }
            "response.output_text.delta" => {
                out.push(ProviderEvent::TextDelta(s(&v, "delta")));
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                out.push(ProviderEvent::ReasoningDelta(s(&v, "delta")))
            }
            "response.function_call_arguments.delta" => {
                if let Some(c) = self.calls.get_mut(&index) {
                    c.args.push_str(v["delta"].as_str().unwrap_or_default());
                }
            }
            "response.function_call_arguments.done" => {
                if let Some(mut c) = self.calls.remove(&index) {
                    if let Some(a) = v["arguments"].as_str() {
                        c.args = a.to_string();
                    }
                    self.emit_call(index, c, &mut out);
                }
            }
            "response.output_item.done" => {
                let item = &v["item"];
                match item["type"].as_str().unwrap_or_default() {
                    "function_call" => {
                        let tracked = self.calls.remove(&index);
                        let pick = |k: &str, fallback: Option<&str>| {
                            let x = s(item, k);
                            if x.is_empty() {
                                fallback.unwrap_or_default().to_string()
                            } else {
                                x
                            }
                        };
                        let call = Call {
                            id: pick("call_id", tracked.as_ref().map(|c| c.id.as_str())),
                            name: pick("name", tracked.as_ref().map(|c| c.name.as_str())),
                            args: pick("arguments", tracked.as_ref().map(|c| c.args.as_str())),
                        };
                        self.emit_call(index, call, &mut out);
                    }
                    "reasoning" if item["encrypted_content"].is_string() => {
                        let replay = json!({
                            "type": "reasoning",
                            "id": item["id"],
                            "encrypted_content": item["encrypted_content"],
                            "summary": item.get("summary").cloned().unwrap_or_else(|| json!([])),
                        });
                        out.push(ProviderEvent::RedactedReasoning(replay.to_string()));
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                return Ok((self.finish(&v["response"]), true));
            }
            "response.failed" => {
                let e = &v["response"]["error"];
                return Err(map_stream_error(
                    e["code"].as_str(),
                    e["message"].as_str().unwrap_or("response failed"),
                ));
            }
            "error" => {
                let e = if v["error"].is_object() {
                    &v["error"]
                } else {
                    &v
                };
                return Err(map_stream_error(
                    e["code"].as_str(),
                    e["message"].as_str().unwrap_or("stream error"),
                ));
            }
            // Unknown and bookkeeping events carry nothing the canonical stream models.
            _ => {}
        }
        Ok((out, false))
    }
}

impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
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
        let this = self.clone();
        let body = to_wire(&req);

        Box::pin(async_stream::stream! {
            let resp = {
                let builder = this
                    .client
                    .post(this.endpoint())
                    .bearer_auth(&this.api_key)
                    .header("content-type", "application/json")
                    .body(body.to_string());
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
                    let text = resp.text().await.unwrap_or_default();
                    yield Err(map_http_error(status.as_u16(), retry_after, &text));
                    return;
                }
                resp
            };
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
                let Some(item) = next else {
                    yield Err(transient("stream ended before response.completed"));
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

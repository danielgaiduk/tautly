//! Google Gemini `streamGenerateContent` adapter.
//!
//! Gemini matches a function response to its call by function name rather than by id, and
//! returns each function call complete in a single chunk without an id. Ids are therefore
//! generated here (`call_<n>`), and the name of each call is remembered while building the
//! request so that tool results can be sent back under the right name.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::sse::try_events;
use crate::util::{retry_after, transient};
use crate::{
    Block, CanonicalRequest, Catalog, Content, Effort, Provider, ProviderError, ProviderEvent,
    RawUsage, Reasoning, Role, StopReason,
};

#[derive(Clone)]
pub struct GoogleProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: Url,
    catalog: Arc<Catalog>,
    next_call: Arc<AtomicU64>,
}

impl GoogleProvider {
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
            next_call: Arc::default(),
        }
    }

    fn endpoint(&self, model: &str) -> String {
        format!(
            "{}/v1beta/models/{model}:streamGenerateContent?alt=sse",
            self.base_url.as_str().trim_end_matches('/')
        )
    }
}

fn effort_budget(e: &Effort) -> u32 {
    match e {
        Effort::Low => 1_024,
        Effort::Medium => 8_192,
        Effort::High => 24_576,
    }
}

/// Build the `streamGenerateContent` request body.
pub fn to_wire(req: &CanonicalRequest) -> Value {
    let system: Vec<Value> = req
        .system
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) => Some(json!({"text": t})),
            Block::Image { .. } => None,
        })
        .collect();

    // ToolUse id -> function name, updated in message order so that reused ids resolve to the
    // most recent call.
    let mut names: HashMap<&str, &str> = HashMap::new();
    let mut contents: Vec<Value> = Vec::new();
    for m in &req.messages {
        let mut responses = Vec::new();
        let mut parts = Vec::new();
        for c in &m.content {
            match c {
                Content::Text(t) => parts.push(json!({"text": t})),
                Content::Image { media_type, data } => parts.push(
                    json!({"inlineData": {"mimeType": media_type, "data": STANDARD.encode(data)}}),
                ),
                Content::ToolUse { id, name, input } => {
                    names.insert(id, name);
                    parts.push(json!({"functionCall": {"name": name, "args": input}}));
                }
                Content::ToolResult { id, output, .. } => {
                    let name = names.get(id.as_str()).copied().unwrap_or(id.as_str());
                    responses.push(json!({"functionResponse": {"name": name, "response": {"content": output}}}));
                }
                // Thought signatures are not modelled; thoughts are not sent back.
                Content::Reasoning { .. } => {}
            }
        }
        // Function responses must directly follow the calls they answer, so they go first.
        responses.append(&mut parts);
        if !responses.is_empty() {
            let role = if m.role == Role::Assistant {
                "model"
            } else {
                "user"
            };
            contents.push(json!({"role": role, "parts": responses}));
        }
    }

    let mut generation = json!({"maxOutputTokens": req.max_output});
    let budget = match &req.reasoning {
        Some(Reasoning::Budget(n)) => Some(*n),
        Some(Reasoning::Effort(e)) => Some(effort_budget(e)),
        None => None,
    };
    if let Some(n) = budget {
        generation["thinkingConfig"] = json!({"thinkingBudget": n, "includeThoughts": true});
    }

    let mut body = json!({"contents": contents, "generationConfig": generation});
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts": system});
    }
    if !req.tools.is_empty() {
        let decls: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "parameters": t.schema}))
            .collect();
        body["tools"] = json!([{"functionDeclarations": decls}]);
    }
    body
}

fn error_parts(body: &str) -> (Option<String>, String) {
    match serde_json::from_str::<Value>(body) {
        Ok(v) => {
            let err = &v["error"];
            (
                err["status"].as_str().map(String::from),
                err["message"].as_str().unwrap_or(body).to_string(),
            )
        }
        Err(_) => (None, body.to_string()),
    }
}

fn is_overflow(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("exceeds the maximum number of tokens") || lower.contains("input token count")
}

fn map_http_error(status: u16, retry_after: Option<Duration>, body: &str) -> ProviderError {
    let (_, msg) = error_parts(body);
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
        400 if is_overflow(&msg) => ProviderError::ContextOverflow(msg),
        400..=499 => ProviderError::InvalidRequest(msg),
        _ => ProviderError::Other(format!("HTTP {status}: {msg}")),
    }
}

fn map_stream_error(v: &Value) -> ProviderError {
    let (status, msg) = error_parts(&v.to_string());
    match status.as_deref() {
        Some("UNAVAILABLE" | "INTERNAL" | "RESOURCE_EXHAUSTED" | "DEADLINE_EXCEEDED") => {
            transient(msg)
        }
        Some(s) if !s.is_empty() => ProviderError::Other(format!("{s}: {msg}")),
        _ => ProviderError::Other(msg),
    }
}

fn stop_reason(finish: &str, has_calls: bool) -> StopReason {
    match finish {
        "STOP" if has_calls => StopReason::ToolUse,
        "STOP" => StopReason::EndTurn,
        "MAX_TOKENS" => StopReason::MaxTokens,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY" => {
            StopReason::Refusal
        }
        other => StopReason::Other(other.to_string()),
    }
}

fn usage_from(u: &Value) -> RawUsage {
    let get = |k: &str| u[k].as_u64().unwrap_or(0);
    let cached = get("cachedContentTokenCount");
    RawUsage {
        // `promptTokenCount` includes cached tokens; report them separately.
        input: get("promptTokenCount").saturating_sub(cached),
        output: get("candidatesTokenCount") + get("thoughtsTokenCount"),
        cache_read: cached,
        cache_write: 0,
    }
}

/// Per-response state machine turning Gemini SSE chunks into canonical events.
struct StreamState {
    next_call: Arc<AtomicU64>,
    usage: Option<RawUsage>,
    finish: Option<String>,
    has_calls: bool,
}

impl StreamState {
    fn handle(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        let v: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Other(format!("invalid SSE payload: {e}")))?;
        if v["error"].is_object() {
            return Err(map_stream_error(&v));
        }
        let mut out = Vec::new();
        if v["usageMetadata"].is_object() {
            self.usage = Some(usage_from(&v["usageMetadata"]));
        }
        // A blocked prompt yields no candidates, only a block reason.
        if v["promptFeedback"]["blockReason"].is_string() {
            self.finish.get_or_insert_with(|| "SAFETY".into());
        }
        let cand = &v["candidates"][0];
        for part in cand["content"]["parts"].as_array().into_iter().flatten() {
            if let Some(call) = part["functionCall"].as_object() {
                self.has_calls = true;
                let id = format!("call_{}", self.next_call.fetch_add(1, Ordering::Relaxed));
                let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
                out.push(ProviderEvent::ToolCall {
                    id,
                    name: call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    input_json: args.to_string(),
                });
            } else if let Some(t) = part["text"].as_str() {
                out.push(if part["thought"] == true {
                    ProviderEvent::ReasoningDelta(t.into())
                } else {
                    ProviderEvent::TextDelta(t.into())
                });
            }
        }
        if let Some(f) = cand["finishReason"].as_str() {
            self.finish = Some(f.to_string());
        }
        Ok(out)
    }

    /// Events for the end of the stream; `None` when the stream was cut short.
    fn finish(&mut self) -> Option<Vec<ProviderEvent>> {
        let finish = self.finish.take()?;
        let mut out = Vec::new();
        if let Some(u) = self.usage {
            out.push(ProviderEvent::Usage(u));
        }
        out.push(ProviderEvent::Done {
            stop: stop_reason(&finish, self.has_calls),
        });
        Some(out)
    }
}

impl Provider for GoogleProvider {
    fn name(&self) -> &str {
        "google"
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
        let builder = self
            .client
            .post(self.endpoint(req.model.model()))
            .header("x-goog-api-key", &self.api_key)
            .header("content-type", "application/json")
            .body(to_wire(&req).to_string());
        let next_call = self.next_call.clone();

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

            let mut events = std::pin::pin!(try_events(resp.bytes_stream()));
            let mut state = StreamState { next_call, usage: None, finish: None, has_calls: false };
            loop {
                let next = tokio::select! {
                    () = cancel.cancelled() => return,
                    n = events.next() => n,
                };
                let Some(item) = next else {
                    match state.finish() {
                        Some(out) => {
                            for e in out {
                                yield Ok(e);
                            }
                        }
                        None => yield Err(transient("stream ended without a finishReason")),
                    }
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

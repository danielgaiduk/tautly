//! `web_fetch`: GET an http(s) URL through the sandbox and return it as text. HTML is
//! converted to markdown; other text is returned as-is; binary content is an error.

use async_trait::async_trait;
use htmd::HtmlToMarkdown;
use hx_provider::{Capabilities, ToolSpec};
use hx_sandbox::{Effect, HttpResponse, Sandbox, Url};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cap::{OUTPUT_CAP, cap_text};
use crate::spec::{decode_text, parse_args, tool_spec};
use crate::{Tool, ToolCx, ToolError, ToolResult};

pub struct WebFetchTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    url: String,
    /// Reserved: answered by the side model once that exists; ignored until then.
    #[serde(default)]
    #[allow(dead_code)]
    question: Option<String>,
}

/// How a response body is presented.
enum Kind {
    Html,
    Text,
    Binary(String),
}

fn content_type(resp: &HttpResponse) -> Option<String> {
    resp.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
}

fn classify(resp: &HttpResponse) -> Kind {
    let Some(ct) = content_type(resp).filter(|c| !c.is_empty()) else {
        // No declared type: sniff.
        let head = String::from_utf8_lossy(&resp.body[..resp.body.len().min(512)]);
        let head = head.trim_start().to_ascii_lowercase();
        return if head.starts_with("<!doctype html") || head.starts_with("<html") {
            Kind::Html
        } else if decode_text(&resp.body).is_ok() {
            Kind::Text
        } else {
            Kind::Binary("unknown".into())
        };
    };
    match ct.as_str() {
        "text/html" | "application/xhtml+xml" => Kind::Html,
        c if c.starts_with("text/")
            || c.ends_with("+json")
            || c.ends_with("+xml")
            || matches!(
                c,
                "application/json"
                    | "application/xml"
                    | "application/javascript"
                    | "application/x-yaml"
                    | "application/yaml"
                    | "application/toml"
            ) =>
        {
            Kind::Text
        }
        _ => Kind::Binary(ct),
    }
}

fn html_to_markdown(html: &str) -> Result<String, String> {
    HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "noscript", "head"])
        .build()
        .convert(html)
        .map_err(|e| e.to_string())
}

fn parse_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|e| format!("invalid url `{raw}`: {e}"))?;
    match url.scheme() {
        "http" | "https" if url.host().is_some() => Ok(url),
        "http" | "https" => Err(format!("invalid url `{raw}`: no host")),
        other => Err(format!(
            "unsupported scheme `{other}`: web_fetch only fetches http and https URLs"
        )),
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        tool_spec(
            "web_fetch",
            "Fetch an http(s) URL. HTML pages are converted to markdown; other text is returned \
             as-is; binary content is refused. Long pages are cut in the middle.",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "http:// or https:// URL."},
                    "question": {"type": "string", "description": "Optional question about the page (reserved: the page text is returned either way)."}
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        )
    }

    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        let _: Args = parse_args(args)?;
        Ok(Effect::ReadOnly)
    }

    async fn run(&self, args: Value, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
        let a: Args = match parse_args(&args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let url = match parse_url(&a.url) {
            Ok(u) => u,
            Err(e) => return ToolResult::error(e),
        };
        let resp = tokio::select! {
            () = cx.cancel().cancelled() => return ToolResult::error("cancelled"),
            r = sb.http_get(&url) => match r {
                Ok(r) => r,
                Err(e) => return ToolResult::error(format!("{url}: {e}")),
            },
        };
        if !(200..300).contains(&resp.status) {
            return ToolResult::error(format!("{url}: HTTP {}", resp.status));
        }
        let text = match classify(&resp) {
            Kind::Binary(ct) => {
                return ToolResult::error(format!("{url}: binary content ({ct}); not fetched"));
            }
            Kind::Text | Kind::Html if resp.body.iter().take(8192).any(|&b| b == 0) => {
                return ToolResult::error(format!("{url}: binary content; not fetched"));
            }
            Kind::Text => String::from_utf8_lossy(&resp.body).into_owned(),
            Kind::Html => {
                let html = String::from_utf8_lossy(&resp.body).into_owned();
                // Conversion is CPU-bound and the page may be megabytes.
                match tokio::task::spawn_blocking(move || html_to_markdown(&html)).await {
                    Ok(Ok(md)) => md.trim().to_string(),
                    Ok(Err(e)) => return ToolResult::error(format!("{url}: {e}")),
                    Err(e) => return ToolResult::error(format!("{url}: {e}")),
                }
            }
        };
        ToolResult::ok(cap_text(&text, OUTPUT_CAP).0)
    }
}

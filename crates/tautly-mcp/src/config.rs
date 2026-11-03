//! The `[mcp]` configuration, where a server comes from, and the Claude-style JSON
//! (`mcpServers`) that `.mcp.json` and `tautly mcp import` read.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer};
use serde_json::Value;
use url::Url;

use crate::text::{head_tail, printable};

/// Of a long repository variable or header value, an approval prompt shows this many
/// bytes from the start…
pub const LAUNCH_VALUE_HEAD: usize = 120;
/// …and this many from the end, with the value's length.
pub const LAUNCH_VALUE_TAIL: usize = 60;

/// Provider key variables no server ever sees (spec 7.11.10); the engine adds every
/// configured `api_key_env`.
pub const PROVIDER_KEY_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "OPENROUTER_API_KEY",
];

/// `[mcp]`. Every key but `servers` is user-only; the project layer's `servers` are kept
/// apart by the engine's configuration loader, since they need approval.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct McpConfig {
    pub enabled: bool,
    /// Per tool call.
    pub timeout_s: u64,
    /// How long the first request waits for servers that are still starting.
    pub startup_timeout_ms: u64,
    /// Tools used per server.
    pub max_tools: usize,
    /// Read the repository root's `.mcp.json` (its servers need approval).
    pub import_mcp_json: bool,
    pub servers: BTreeMap<String, ServerSpec>,
}

impl Default for McpConfig {
    fn default() -> Self {
        McpConfig {
            enabled: true,
            timeout_s: 120,
            startup_timeout_ms: 5000,
            max_tools: 50,
            import_mcp_json: true,
            servers: BTreeMap::new(),
        }
    }
}

/// How to reach one server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerSpec {
    /// A child process speaking MCP over its stdin and stdout.
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        /// Relative to the workspace root; default the root.
        cwd: Option<PathBuf>,
    },
    /// Streamable HTTP.
    Http {
        url: Url,
        /// Values may use `${VAR}` in the user's configuration (expanded at start).
        headers: BTreeMap<String, String>,
    },
}

/// Where a server was declared. Only `User` and `Client` are trusted; the others are
/// repository content and need the user's approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ServerSource {
    /// `[mcp.servers]` of the user's configuration.
    User,
    /// `[mcp.servers]` of the project's `.tautly/config.toml`.
    Project,
    /// The repository root's `.mcp.json`.
    McpJson,
    /// Given by the client (an editor over ACP).
    Client,
}

impl ServerSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ServerSource::User => "user",
            ServerSource::Project => "project",
            ServerSource::McpJson => "mcp_json",
            ServerSource::Client => "client",
        }
    }

    /// Declared by the repository, so it needs an approval.
    pub fn is_repository(self) -> bool {
        matches!(self, ServerSource::Project | ServerSource::McpJson)
    }
}

impl fmt::Display for ServerSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ServerSpec {
    /// `stdio` or `http`.
    pub fn transport(&self) -> &'static str {
        match self {
            ServerSpec::Stdio { .. } => "stdio",
            ServerSpec::Http { .. } => "http",
        }
    }

    /// What starting it means, for an approval prompt or a listing: the command line (or
    /// URL) and its variables (or headers). A repository server's values are shown: they
    /// are repository content and they decide what runs (`NODE_OPTIONS`, `PATH`). A long
    /// one shows its first [`LAUNCH_VALUE_HEAD`] and last [`LAUNCH_VALUE_TAIL`] bytes and its
    /// length, so nothing hides behind padding. A trusted server's values may be the user's secrets,
    /// so only the names are shown. Control and invisible characters are escaped
    /// ([`printable`]), so the text cannot redraw itself in a terminal.
    pub fn launch(&self, source: ServerSource) -> String {
        let show_values = source.is_repository();
        let vars = |what: &str, map: &BTreeMap<String, String>| {
            if map.is_empty() {
                return String::new();
            }
            let items: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    if show_values {
                        let shown =
                            head_tail(&printable(v), LAUNCH_VALUE_HEAD, LAUNCH_VALUE_TAIL, v.len());
                        format!("{}={shown}", printable(k))
                    } else {
                        printable(k)
                    }
                })
                .collect();
            format!(" ({what}: {})", items.join(", "))
        };
        match self {
            ServerSpec::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                let argv: Vec<String> = std::iter::once(command)
                    .chain(args)
                    .map(|a| quote(a))
                    .collect();
                let cwd = cwd
                    .as_ref()
                    .map(|c| format!(" (cwd: {})", printable(&c.to_string_lossy())))
                    .unwrap_or_default();
                format!("stdio: {}{cwd}{}", argv.join(" "), vars("env", env))
            }
            ServerSpec::Http { url, headers } => format!(
                "http: {}{}",
                printable(url.as_str()),
                vars("headers", headers)
            ),
        }
    }

    fn check(&self) -> Result<(), String> {
        match self {
            ServerSpec::Stdio { command, .. } if command.trim().is_empty() => {
                Err("`command` must not be empty".into())
            }
            ServerSpec::Http { url, .. } if !matches!(url.scheme(), "http" | "https") => {
                Err(format!("`url` must be http or https, got {url}"))
            }
            _ => Ok(()),
        }
    }
}

/// One `[mcp.servers.<name>]` table as written: either `command` (with `args`, `env`,
/// `cwd`) or `url` (with `headers`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpec {
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    cwd: Option<PathBuf>,
    url: Option<Url>,
    headers: Option<BTreeMap<String, String>>,
}

impl<'de> Deserialize<'de> for ServerSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = RawSpec::deserialize(d)?;
        let spec = match (raw.command, raw.url) {
            (Some(command), None) => {
                if raw.headers.is_some() {
                    return Err(serde::de::Error::custom(
                        "`headers` belongs to an http server (`url`), not a `command`",
                    ));
                }
                ServerSpec::Stdio {
                    command,
                    args: raw.args.unwrap_or_default(),
                    env: raw.env.unwrap_or_default(),
                    cwd: raw.cwd,
                }
            }
            (None, Some(url)) => {
                if raw.args.is_some() || raw.env.is_some() || raw.cwd.is_some() {
                    return Err(serde::de::Error::custom(
                        "`args`, `env` and `cwd` belong to a `command`, not an http server",
                    ));
                }
                ServerSpec::Http {
                    url,
                    headers: raw.headers.unwrap_or_default(),
                }
            }
            (Some(_), Some(_)) => {
                return Err(serde::de::Error::custom(
                    "a server has either `command` or `url`, not both",
                ));
            }
            (None, None) => {
                return Err(serde::de::Error::custom(
                    "a server needs `command` (stdio) or `url` (http)",
                ));
            }
        };
        spec.check().map_err(serde::de::Error::custom)?;
        Ok(spec)
    }
}

/// The servers of a Claude-style JSON document (`{"mcpServers": {name: {command, args,
/// env} | {type: "http", url, headers}}}`), plus one line per entry that was skipped
/// (unsupported transport, bad shape). An error when the document is not such JSON.
pub fn parse_mcp_json(text: &str) -> Result<(BTreeMap<String, ServerSpec>, Vec<String>), String> {
    let doc: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    let Some(doc) = doc.as_object() else {
        return Err("not a JSON object".into());
    };
    let mut servers = BTreeMap::new();
    let mut skipped = Vec::new();
    let Some(entries) = doc.get("mcpServers") else {
        return Ok((servers, skipped));
    };
    let Some(entries) = entries.as_object() else {
        return Err("`mcpServers` is not an object".into());
    };
    for (name, entry) in entries {
        match json_spec(entry) {
            Ok(spec) => {
                servers.insert(name.clone(), spec);
            }
            Err(why) => skipped.push(format!("MCP server `{name}` skipped: {why}")),
        }
    }
    Ok((servers, skipped))
}

/// One `mcpServers` entry.
fn json_spec(entry: &Value) -> Result<ServerSpec, String> {
    let obj = entry.as_object().ok_or("not an object")?;
    let string = |key: &str| -> Result<Option<String>, String> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(format!("`{key}` is not a string")),
        }
    };
    let map = |key: &str| -> Result<BTreeMap<String, String>, String> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(BTreeMap::new()),
            Some(Value::Object(m)) => m
                .iter()
                .map(|(k, v)| match v {
                    Value::String(s) => Ok((k.clone(), s.clone())),
                    _ => Err(format!("`{key}.{k}` is not a string")),
                })
                .collect(),
            Some(_) => Err(format!("`{key}` is not an object")),
        }
    };
    let http = |kind: Option<&str>| {
        matches!(
            kind,
            None | Some("http" | "streamable-http" | "streamable_http")
        )
    };
    let kind = string("type")?;
    let spec = match (kind.as_deref(), string("command")?, string("url")?) {
        (Some("sse"), ..) => {
            return Err("the SSE transport (`type: sse`) is not supported".into());
        }
        (None | Some("stdio"), Some(command), None) => {
            let args = match obj.get("args") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|v| v.as_str().map(str::to_string))
                    .collect::<Option<Vec<_>>>()
                    .ok_or("`args` holds something other than strings")?,
                Some(_) => return Err("`args` is not an array".into()),
            };
            ServerSpec::Stdio {
                command,
                args,
                env: map("env")?,
                cwd: string("cwd")?.map(PathBuf::from),
            }
        }
        (kind, None, Some(url)) if http(kind) => ServerSpec::Http {
            url: url.parse().map_err(|e| format!("bad `url`: {e}"))?,
            headers: map("headers")?,
        },
        (Some(other), ..) if other != "stdio" && !http(Some(other)) => {
            return Err(format!("unknown transport `{other}`"));
        }
        _ => return Err("needs `command` (stdio) or `url` (http)".into()),
    };
    spec.check()?;
    Ok(spec)
}

/// `s` as one shell word, for display, with control and invisible characters escaped.
fn quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", printable(s).replace('\'', r"'\''"))
    }
}

/// `existing` (a user `config.toml`, comments and layout kept) with `servers` added under
/// `[mcp.servers.<name>]`. A name the file already has is left alone and listed in the
/// second value.
pub fn import_into_toml(
    existing: &str,
    servers: &BTreeMap<String, ServerSpec>,
) -> Result<(String, Vec<String>), String> {
    use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, value};

    let mut doc: DocumentMut = existing.parse().map_err(|e| format!("{e}"))?;
    let implicit = || {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    };
    let mcp = doc
        .entry("mcp")
        .or_insert_with(implicit)
        .as_table_mut()
        .ok_or("`mcp` is not a table")?;
    let table = mcp
        .entry("servers")
        .or_insert_with(implicit)
        .as_table_mut()
        .ok_or("`mcp.servers` is not a table")?;
    let strings = |m: &BTreeMap<String, String>| {
        InlineTable::from_iter(m.iter().map(|(k, v)| (k.as_str(), v.as_str())))
    };
    let mut kept = Vec::new();
    for (name, spec) in servers {
        if table.contains_key(name) {
            kept.push(name.clone());
            continue;
        }
        let mut t = Table::new();
        match spec {
            ServerSpec::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                t["command"] = value(command.as_str());
                if !args.is_empty() {
                    t["args"] = value(Array::from_iter(args.iter().map(String::as_str)));
                }
                if !env.is_empty() {
                    t["env"] = value(strings(env));
                }
                if let Some(cwd) = cwd {
                    t["cwd"] = value(cwd.to_string_lossy().as_ref());
                }
            }
            ServerSpec::Http { url, headers } => {
                t["url"] = value(url.as_str());
                if !headers.is_empty() {
                    t["headers"] = value(strings(headers));
                }
            }
        }
        table.insert(name, Item::Table(t));
    }
    Ok((doc.to_string(), kept))
}

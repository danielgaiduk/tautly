//! The MCP servers of one session: started in the background, their tools offered once
//! ready, stopped at shutdown.
//!
//! Every server connects with `rmcp`'s `Auto` lifecycle: `server/discover` (MCP
//! 2026-07-28), falling back to the `initialize` handshake for older servers. The client
//! declares no capabilities and refuses `roots/list`, elicitation and sampling requests.
//! A `notifications/tools/list_changed` (through a `subscriptions/listen` subscription
//! for 2026-07-28 servers, directly for older ones) fetches the tool list again; the
//! engine picks the new list up at its next user-turn start.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

#[allow(deprecated)]
use rmcp::model::{
    ClientCapabilities, ClientConfig, ElicitRequestParams, ElicitResult,
    ElicitationCreateRequestMethod, Implementation, ListRootsRequestMethod, ListRootsResult,
    ProtocolVersion, ServerNotification, SubscriptionFilter,
};
use rmcp::service::{NotificationContext, RequestContext, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, ClientLifecycleMode, ErrorData, Peer, RoleClient};
use serde::{Deserialize, Serialize};
use tautly_sandbox::{Effect, Sandbox, SpawnSpec};
use tautly_tools::{Tool, group_alive};
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::{McpConfig, ServerSource, ServerSpec};
use crate::names;
use crate::tool::{DESCRIPTION_CAP, McpTool, RemoteTool, SCHEMA_CAP, truncate};

/// Longest a server may take to connect and list its tools before it is `Failed` (an
/// `npx` server may first download itself).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(120);
/// At shutdown, between SIGTERM and SIGKILL of a stdio server's process group.
const TERM_GRACE: Duration = Duration::from_millis(500);
/// Bytes of a stdio server's stderr kept for the message of a failed start.
const STDERR_TAIL: usize = 2048;
/// How long closing a session (an HTTP session delete, a transport flush) may take.
const CLOSE_GRACE: Duration = Duration::from_secs(1);

/// Where a server is (spec 7.11.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpState {
    Starting,
    Ready,
    Failed,
    Skipped,
    AwaitingApproval,
    /// A state this version does not know.
    #[serde(other)]
    Unknown,
}

/// One server as [`McpHub::status`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub name: String,
    pub source: ServerSource,
    pub state: McpState,
    /// Tools offered (after the per-server cap and skipped schemas).
    pub tools: u32,
    pub message: Option<String>,
}

/// A live connection to one server.
pub(crate) struct Connection {
    pub server: String,
    pub peer: Peer<RoleClient>,
    /// What a call does, for the policy: `Exec{argv}` (stdio) or `Network{host}` (HTTP).
    pub effect: Effect,
    /// A stdio server: a local process that may change files.
    pub may_write: bool,
    service: Mutex<Option<RunningService<RoleClient, Handler>>>,
    process: Mutex<Option<Child>>,
    pgid: Option<u32>,
    groups: Groups,
}

impl Connection {
    /// Close the session and end the server's process group: SIGTERM, a grace period,
    /// SIGKILL for what is left; the server process is reaped.
    async fn close(&self) {
        let service = lock(&self.service).take();
        if let Some(s) = service {
            let _ = tokio::time::timeout(CLOSE_GRACE, s.cancel()).await;
        }
        let child = lock(&self.process).take();
        if let Some(mut child) = child {
            end_group(self.pgid, &mut child, &self.groups).await;
        }
    }
}

/// The process groups of the stdio servers started and not yet ended. The server process
/// (the group leader) stays unreaped until its group is ended, so a group id here cannot
/// have been reused by another process.
type Groups = Arc<Mutex<Vec<u32>>>;

/// End `child`'s process group and reap it; the group is then forgotten.
async fn end_group(pgid: Option<u32>, child: &mut Child, groups: &Groups) {
    if let Some(g) = pgid {
        killpg(g, libc::SIGTERM);
    }
    if tokio::time::timeout(TERM_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
    }
    if let Some(g) = pgid.filter(|&g| group_alive(g)) {
        killpg(g, libc::SIGKILL);
    }
    let _ = child.wait().await;
    if let Some(g) = pgid {
        lock(groups).retain(|&x| x != g);
    }
}

fn killpg(pgid: u32, signal: i32) {
    let Ok(pgid) = i32::try_from(pgid) else {
        return;
    };
    if pgid > 1 {
        // SAFETY: killpg only sends a signal; the group is one this hub started.
        unsafe {
            libc::killpg(pgid, signal);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Server {
    source: ServerSource,
    state: McpState,
    message: Option<String>,
    conn: Option<Arc<Connection>>,
    tools: Vec<RemoteTool>,
}

impl Server {
    fn new(source: ServerSource, state: McpState, message: Option<String>) -> Self {
        Server {
            source,
            state,
            message,
            conn: None,
            tools: Vec::new(),
        }
    }
}

#[derive(Default)]
struct HubState {
    servers: BTreeMap<String, Server>,
    warnings: Vec<String>,
    closed: bool,
}

struct Inner {
    sb: Arc<dyn Sandbox>,
    cfg: McpConfig,
    strip_env: Vec<String>,
    state: Mutex<HubState>,
    changes: watch::Sender<u64>,
    shutdown: CancellationToken,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Ended at shutdown (or drop) whatever state their server reached.
    groups: Groups,
}

impl Inner {
    /// SIGKILL every started process group that still has members.
    fn kill_groups(&self) {
        for &g in lock(&self.groups).iter() {
            if group_alive(g) {
                killpg(g, libc::SIGKILL);
            }
        }
    }

    fn changed(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    fn set(&self, name: &str, source: ServerSource, state: McpState, message: Option<String>) {
        {
            let mut st = lock(&self.state);
            let server = st
                .servers
                .entry(name.to_string())
                .or_insert_with(|| Server::new(source, state, None));
            server.source = source;
            server.state = state;
            server.message = message;
        }
        self.changed();
    }

    fn warn(&self, w: String) {
        lock(&self.state).warnings.push(w);
        self.changed();
    }

    fn spawn(self: &Arc<Self>, f: impl Future<Output = ()> + Send + 'static) {
        let shutdown = self.shutdown.clone();
        let task = tokio::spawn(async move {
            tokio::select! {
                () = f => {}
                () = shutdown.cancelled() => {}
            }
        });
        let mut tasks = lock(&self.tasks);
        tasks.retain(|t| !t.is_finished());
        tasks.push(task);
    }

    /// Fetch `server`'s tool list again and keep it if the server is still the one
    /// connected.
    async fn relist(self: Arc<Self>, server: String) {
        let conn = lock(&self.state)
            .servers
            .get(&server)
            .and_then(|s| s.conn.clone());
        let Some(conn) = conn else {
            return;
        };
        match tokio::time::timeout(CONNECT_TIMEOUT, conn.peer.list_all_tools()).await {
            Ok(Ok(tools)) => {
                let (tools, warnings) = select_tools(&server, tools, self.cfg.max_tools);
                {
                    let mut st = lock(&self.state);
                    let Some(s) = st.servers.get_mut(&server) else {
                        return;
                    };
                    if !s.conn.as_ref().is_some_and(|c| Arc::ptr_eq(c, &conn)) {
                        return;
                    }
                    s.tools = tools;
                    st.warnings.extend(warnings);
                }
                self.changed();
            }
            Ok(Err(e)) => self.warn(format!(
                "MCP server `{server}`: listing its tools again failed: {e}"
            )),
            Err(_) => self.warn(format!(
                "MCP server `{server}`: listing its tools again timed out"
            )),
        }
    }

    /// Start one server: connect, list its tools, and report it `Ready` (or `Failed`).
    async fn run_server(self: Arc<Self>, name: String, spec: ServerSpec, source: ServerSource) {
        self.set(&name, source, McpState::Starting, None);
        let started = tokio::time::timeout(CONNECT_TIMEOUT, self.connect(&name, &spec, source))
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "did not start within {} s",
                    CONNECT_TIMEOUT.as_secs()
                ))
            });
        let (conn, tools) = match started {
            Ok(ok) => ok,
            Err(why) => {
                self.set(&name, source, McpState::Failed, Some(why));
                return;
            }
        };
        let (tools, warnings) = select_tools(&name, tools, self.cfg.max_tools);
        let kept = {
            let mut st = lock(&self.state);
            if st.closed {
                false
            } else {
                let server = st
                    .servers
                    .entry(name.clone())
                    .or_insert_with(|| Server::new(source, McpState::Starting, None));
                server.state = McpState::Ready;
                server.message = None;
                server.conn = Some(Arc::clone(&conn));
                server.tools = tools;
                st.warnings.extend(warnings);
                true
            }
        };
        if !kept {
            conn.close().await;
            return;
        }
        self.changed();
        self.subscribe(name, conn);
    }

    /// For a 2026-07-28 server that announces tool list changes, open the subscription
    /// they arrive on. Older servers send them to [`Handler`] directly.
    fn subscribe(self: &Arc<Self>, name: String, conn: Arc<Connection>) {
        let Some(info) = conn.peer.peer_info() else {
            return;
        };
        let announces = info
            .capabilities
            .tools
            .as_ref()
            .and_then(|t| t.list_changed)
            .unwrap_or(false);
        if !announces || info.protocol_version.has_initialize() {
            return;
        }
        let me = Arc::downgrade(self);
        self.spawn(async move {
            let filter = SubscriptionFilter::builder().tools_list_changed().build();
            let Ok(mut sub) = conn.peer.listen(filter).await else {
                return;
            };
            drop(conn);
            while let Ok(Some(n)) = sub.next().await {
                if matches!(n, ServerNotification::ToolListChangedNotification(_)) {
                    let Some(inner) = me.upgrade() else {
                        return;
                    };
                    inner.relist(name.clone()).await;
                }
            }
        });
    }

    async fn connect(
        self: &Arc<Self>,
        name: &str,
        spec: &ServerSpec,
        source: ServerSource,
    ) -> Result<(Arc<Connection>, Vec<rmcp::model::Tool>), String> {
        let handler = Handler {
            hub: Arc::downgrade(self),
            server: name.to_string(),
        };
        let lifecycle = ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            legacy_version: Some(ProtocolVersion::V_2025_11_25),
        };
        match spec {
            ServerSpec::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                let argv: Vec<String> = std::iter::once(command).chain(args).cloned().collect();
                let mut child = self
                    .sb
                    .spawn_stdio(SpawnSpec {
                        argv: argv.clone(),
                        cwd: cwd.clone().unwrap_or_else(|| PathBuf::from(".")),
                        env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                        // Removed after `env` is applied: no server sees a provider key.
                        env_remove: self.strip_env.clone(),
                        new_process_group: true,
                    })
                    .await
                    .map_err(|e| format!("could not start `{command}`: {e}"))?;
                let pgid = child.id();
                // Tracked from now on, so a shutdown during the handshake still ends it.
                lock(&self.groups).extend(pgid);
                let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
                    end_group(pgid, &mut child, &self.groups).await;
                    return Err("the sandbox gave the server no stdin or stdout".into());
                };
                let stderr_tail = Arc::new(Mutex::new(Vec::new()));
                if let Some(mut stderr) = child.stderr.take() {
                    let tail = Arc::clone(&stderr_tail);
                    self.spawn(async move {
                        let mut buf = [0u8; 4096];
                        while let Ok(n @ 1..) = stderr.read(&mut buf).await {
                            let mut t = lock(&tail);
                            t.extend_from_slice(&buf[..n]);
                            let excess = t.len().saturating_sub(STDERR_TAIL);
                            t.drain(..excess);
                        }
                    });
                }
                let served =
                    rmcp::serve_client_with_lifecycle(handler, (stdout, stdin), lifecycle).await;
                let service = match served {
                    Ok(s) => s,
                    Err(e) => {
                        end_group(pgid, &mut child, &self.groups).await;
                        let tail = String::from_utf8_lossy(&lock(&stderr_tail))
                            .trim()
                            .to_string();
                        let tail = if tail.is_empty() {
                            String::new()
                        } else {
                            format!("; stderr: {}", truncate(&tail, STDERR_TAIL))
                        };
                        return Err(format!("handshake failed: {e}{tail}"));
                    }
                };
                let conn = Arc::new(Connection {
                    server: name.to_string(),
                    peer: service.peer().clone(),
                    effect: Effect::Exec { argv },
                    may_write: true,
                    service: Mutex::new(Some(service)),
                    process: Mutex::new(Some(child)),
                    pgid,
                    groups: Arc::clone(&self.groups),
                });
                match conn.peer.list_all_tools().await {
                    Ok(tools) => Ok((conn, tools)),
                    Err(e) => {
                        conn.close().await;
                        Err(format!("listing its tools failed: {e}"))
                    }
                }
            }
            ServerSpec::Http { url, headers } => {
                let host = url
                    .host_str()
                    .ok_or("the URL has no host")?
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .trim_end_matches('.')
                    .to_ascii_lowercase();
                let mut custom = HashMap::new();
                for (k, v) in headers {
                    let v = if matches!(source, ServerSource::User | ServerSource::Client) {
                        expand_env(v)
                    } else {
                        v.clone()
                    };
                    let k = http::HeaderName::try_from(k.as_str())
                        .map_err(|e| format!("bad header name `{k}`: {e}"))?;
                    let v = http::HeaderValue::try_from(v.as_str())
                        .map_err(|e| format!("bad value for header `{k}`: {e}"))?;
                    custom.insert(k, v);
                }
                let config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
                    .custom_headers(custom);
                let transport = StreamableHttpClientTransport::from_config(config);
                let service = rmcp::serve_client_with_lifecycle(handler, transport, lifecycle)
                    .await
                    .map_err(|e| format!("could not connect to {url}: {e}"))?;
                let conn = Arc::new(Connection {
                    server: name.to_string(),
                    peer: service.peer().clone(),
                    effect: Effect::Network { host },
                    may_write: false,
                    service: Mutex::new(Some(service)),
                    process: Mutex::new(None),
                    pgid: None,
                    groups: Arc::clone(&self.groups),
                });
                match conn.peer.list_all_tools().await {
                    Ok(tools) => Ok((conn, tools)),
                    Err(e) => {
                        conn.close().await;
                        Err(format!("listing its tools failed: {e}"))
                    }
                }
            }
        }
    }
}

/// `${VAR}` in `v` replaced by the variable's value (empty when unset).
fn expand_env(v: &str) -> String {
    let mut out = String::new();
    let mut rest = v;
    while let Some(start) = rest.find("${") {
        let Some(len) = rest[start + 2..].find('}') else {
            break;
        };
        out.push_str(&rest[..start]);
        let name = &rest[start + 2..start + 2 + len];
        out.push_str(&std::env::var(name).unwrap_or_default());
        rest = &rest[start + 3 + len..];
    }
    out.push_str(rest);
    out
}

/// The tools of `server` within the caps, by name: a schema over [`SCHEMA_CAP`] is skipped
/// and descriptions are cut to [`DESCRIPTION_CAP`]; past `max_tools` the rest are left
/// out. Each skipped or cut tool list gives a warning.
fn select_tools(
    server: &str,
    mut listed: Vec<rmcp::model::Tool>,
    max_tools: usize,
) -> (Vec<RemoteTool>, Vec<String>) {
    listed.sort_by(|a, b| a.name.cmp(&b.name));
    listed.dedup_by(|a, b| a.name == b.name);
    let offered = listed.len();
    let mut warnings = Vec::new();
    let mut kept = Vec::new();
    for t in listed {
        let schema = serde_json::Value::Object((*t.input_schema).clone());
        let size = serde_json::to_vec(&schema).map_or(usize::MAX, |b| b.len());
        if size > SCHEMA_CAP {
            warnings.push(format!(
                "MCP server `{server}`: tool `{}` skipped: its input schema is {size} bytes, \
                 over the {SCHEMA_CAP} byte limit",
                t.name
            ));
            continue;
        }
        kept.push(RemoteTool {
            name: t.name.to_string(),
            description: truncate(t.description.as_deref().unwrap_or(""), DESCRIPTION_CAP),
            schema,
        });
    }
    if kept.len() > max_tools {
        warnings.push(format!(
            "MCP server `{server}` offers {offered} tools; only the first {max_tools} by name \
             are used"
        ));
        kept.truncate(max_tools);
    }
    (kept, warnings)
}

/// The client side of one connection.
pub(crate) struct Handler {
    hub: Weak<Inner>,
    server: String,
}

// Roots are deprecated (SEP-2577); they are refused here like the rest of the unsupported
// client features.
#[allow(deprecated)]
impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("tautly", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        Err(ErrorData::method_not_found::<ListRootsRequestMethod>())
    }

    async fn create_elicitation(
        &self,
        _request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        Err(ErrorData::method_not_found::<ElicitationCreateRequestMethod>())
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        if let Some(inner) = self.hub.upgrade() {
            let server = self.server.clone();
            let task = Arc::clone(&inner).relist(server);
            inner.spawn(task);
        }
    }
}

/// The servers of a session.
pub struct McpHub {
    inner: Arc<Inner>,
}

impl McpHub {
    /// Start `servers` in the background and return at once; see [`McpHub::status`].
    /// Stdio servers run through `sb` in their own process group, with the parent
    /// environment minus `strip_env` (even where a server's `env` sets one), plus their
    /// `env`. HTTP header values from trusted sources (`User`, `Client`) expand `${VAR}`.
    pub async fn start(
        servers: Vec<(String, ServerSpec, ServerSource)>,
        sb: Arc<dyn Sandbox>,
        cfg: &McpConfig,
        strip_env: &[String],
    ) -> McpHub {
        let (changes, _) = watch::channel(0);
        let hub = McpHub {
            inner: Arc::new(Inner {
                sb,
                cfg: McpConfig {
                    servers: BTreeMap::new(),
                    ..cfg.clone()
                },
                strip_env: strip_env.to_vec(),
                state: Mutex::default(),
                changes,
                shutdown: CancellationToken::new(),
                tasks: Mutex::default(),
                groups: Arc::default(),
            }),
        };
        for (name, spec, source) in servers {
            hub.add(name, spec, source);
        }
        hub
    }

    /// Start one more server in the background (for example once the user approved it).
    pub fn add(&self, name: String, spec: ServerSpec, source: ServerSource) {
        if lock(&self.inner.state).closed {
            return;
        }
        self.inner.set(&name, source, McpState::Starting, None);
        let task = Arc::clone(&self.inner).run_server(name, spec, source);
        self.inner.spawn(task);
    }

    /// Record a server that is not started (`Skipped`, `AwaitingApproval`).
    pub fn note(&self, name: &str, source: ServerSource, state: McpState, message: Option<String>) {
        self.inner.set(name, source, state, message);
    }

    /// The tools of every ready server, sorted by name, names distinct
    /// ([`names::assign`]).
    pub fn ready_tools(&self) -> Vec<Arc<dyn Tool>> {
        let st = lock(&self.inner.state);
        let mut found: Vec<(&str, &RemoteTool, &Arc<Connection>)> = Vec::new();
        for (name, server) in &st.servers {
            if let (McpState::Ready, Some(conn)) = (server.state, &server.conn) {
                found.extend(server.tools.iter().map(|t| (name.as_str(), t, conn)));
            }
        }
        let pairs: Vec<(&str, &str)> = found
            .iter()
            .map(|(s, t, _)| (*s, t.name.as_str()))
            .collect();
        let timeout = Duration::from_secs(self.inner.cfg.timeout_s);
        let mut tools: Vec<McpTool> = names::assign(&pairs)
            .into_iter()
            .zip(&found)
            .map(|(name, (_, remote, conn))| McpTool {
                name,
                remote: (*remote).clone(),
                conn: Arc::clone(conn),
                timeout,
            })
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
            .into_iter()
            .map(|t| Arc::new(t) as Arc<dyn Tool>)
            .collect()
    }

    /// Every server, sorted by name.
    pub fn status(&self) -> Vec<ServerStatus> {
        lock(&self.inner.state)
            .servers
            .iter()
            .map(|(name, s)| ServerStatus {
                name: name.clone(),
                source: s.source,
                state: s.state,
                tools: u32::try_from(s.tools.len()).unwrap_or(u32::MAX),
                message: s.message.clone(),
            })
            .collect()
    }

    /// Changes whenever a status, a tool list or the warnings change.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.inner.changes.subscribe()
    }

    /// Warnings since the last call (skipped tools, capped tool lists).
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut lock(&self.inner.state).warnings)
    }

    /// Wait until no server is `Starting`, at most `timeout`; true if none is.
    pub async fn wait_started(&self, timeout: Duration) -> bool {
        let starting = || {
            lock(&self.inner.state)
                .servers
                .values()
                .any(|s| s.state == McpState::Starting)
        };
        let mut changes = self.changes();
        let wait = async {
            while starting() {
                if changes.changed().await.is_err() {
                    break;
                }
            }
        };
        let _ = tokio::time::timeout(timeout, wait).await;
        !starting()
    }

    /// Close every connection and end the stdio servers' process groups. Servers still
    /// starting are stopped; nothing starts afterwards.
    pub async fn shutdown(&self) {
        let conns: Vec<Arc<Connection>> = {
            let mut st = lock(&self.inner.state);
            st.closed = true;
            st.servers
                .values_mut()
                .filter_map(|s| {
                    s.tools.clear();
                    s.conn.take()
                })
                .collect()
        };
        futures::future::join_all(conns.iter().map(|c| c.close())).await;
        // Servers still starting: their tasks hold their (unreaped) processes, so the
        // groups are theirs until the tasks are stopped.
        self.inner.kill_groups();
        self.inner.shutdown.cancel();
        let tasks: Vec<_> = std::mem::take(&mut *lock(&self.inner.tasks));
        for t in tasks {
            let _ = t.await;
        }
        self.inner.changed();
    }
}

/// Dropped without [`McpHub::shutdown`]: every server's process group is killed at once.
impl Drop for McpHub {
    fn drop(&mut self) {
        self.inner.kill_groups();
        self.inner.shutdown.cancel();
        for t in lock(&self.inner.tasks).iter() {
            t.abort();
        }
    }
}

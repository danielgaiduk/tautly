//! The MCP servers of one session: started in the background, their tools offered once
//! ready, stopped at shutdown.
//!
//! Every server connects with `rmcp`'s `Auto` lifecycle: `server/discover` (MCP
//! 2026-07-28), falling back to the `initialize` handshake for older servers. The client
//! declares no capabilities and refuses `roots/list`, elicitation and sampling requests.
//! A `notifications/tools/list_changed` (through a `subscriptions/listen` subscription
//! for 2026-07-28 servers, directly for older ones) fetches the tool list again, one
//! listing at a time per server however many notifications arrive; the engine picks the
//! new list up at its next user-turn start. A server that exits or drops its connection
//! is `Failed` and its tools are no longer offered.
//!
//! Bounds: connecting (and the first listing) takes at most the connect timeout (120 s),
//! after which the server's process group is killed; a listing stops after
//! [`MAX_PAGES`] pages or `4 × max_tools` tools; at most [`MAX_TOTAL_TOOLS`] tools are
//! offered across all servers; at most [`MAX_HUB_WARNINGS`] distinct warnings are kept.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

#[allow(deprecated)]
use rmcp::model::{
    ClientCapabilities, ClientConfig, ElicitRequestParams, ElicitResult,
    ElicitationCreateRequestMethod, Implementation, ListRootsRequestMethod, ListRootsResult,
    PaginatedRequestParams, ProtocolVersion, ServerNotification, SubscriptionFilter,
};
use rmcp::service::{NotificationContext, RequestContext, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, ClientLifecycleMode, ErrorData, Peer, RoleClient, ServiceError};
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
use crate::text::{printable, strip_control, strip_control_json};
use crate::tool::{DESCRIPTION_CAP, McpTool, RemoteTool, SCHEMA_CAP, truncate};

/// Most MCP tools offered in one request, across all servers (by name).
pub const MAX_TOTAL_TOOLS: usize = 200;
/// Most pages of one tool listing.
pub const MAX_PAGES: usize = 20;
/// Distinct warnings a hub keeps; past this one line says the rest are not shown.
pub const MAX_HUB_WARNINGS: usize = 100;
/// Default longest a server may take to connect and list its tools before it is `Failed`
/// (an `npx` server may first download itself).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(120);
/// At shutdown, between SIGTERM and SIGKILL of a stdio server's process group.
const TERM_GRACE: Duration = Duration::from_millis(500);
/// Bytes of a stdio server's stderr kept for the message of a failure.
const STDERR_TAIL: usize = 2048;
/// How long closing a session (an HTTP session delete, a transport flush) may take.
const CLOSE_GRACE: Duration = Duration::from_secs(1);
/// A tool-list change is listed this long after it is announced, so a burst of
/// announcements costs one listing (the engine only looks at the next user turn anyway).
const RELIST_DEBOUNCE: Duration = Duration::from_millis(250);
/// How often [`end_group`] looks whether the leader has exited.
const EXIT_POLL: Duration = Duration::from_millis(10);
/// How often a ready server is checked for having exited or disconnected.
const WATCH_EVERY: Duration = Duration::from_millis(200);

/// Where a server is (spec 7.11.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpState {
    /// Being started or initialized.
    Starting,
    /// Connected; its tools are offered.
    Ready,
    /// Could not start, or exited or disconnected.
    Failed,
    /// Not started: a repository server that was not approved, or was denied.
    Skipped,
    /// A repository server waiting for the user's approval.
    AwaitingApproval,
    /// A state this version does not know.
    #[serde(other)]
    Unknown,
}

/// One server as [`McpHub::status`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    /// The server's configured name.
    pub name: String,
    /// Where it was declared.
    pub source: ServerSource,
    /// Where it is now.
    pub state: McpState,
    /// Tools offered (after the per-server cap and skipped schemas).
    pub tools: u32,
    /// Why it failed or was skipped, if so.
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
    /// The last [`STDERR_TAIL`] bytes of a stdio server's stderr.
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl Connection {
    /// Close the session and end the server's process group (see [`end_group`]); the
    /// server's exit status, when it was a process.
    async fn close(&self) -> Option<ExitStatus> {
        let service = lock(&self.service).take();
        if let Some(s) = service {
            let _ = tokio::time::timeout(CLOSE_GRACE, s.cancel()).await;
        }
        let child = lock(&self.process).take();
        match child {
            Some(mut child) => end_group(self.pgid, &mut child, &self.groups).await,
            None => None,
        }
    }

    /// Whether the server is gone: its process exited (seen without reaping it, so its
    /// group id stays ours until the group is killed), or the transport closed.
    fn gone(&self) -> bool {
        self.pgid.is_some_and(leader_exited) || self.peer.is_transport_closed()
    }

    /// The stderr tail, escaped, for a failure message.
    fn stderr_note(&self) -> String {
        stderr_note(&self.stderr)
    }
}

fn stderr_note(tail: &Mutex<Vec<u8>>) -> String {
    let text = String::from_utf8_lossy(&lock(tail)).trim().to_string();
    if text.is_empty() {
        String::new()
    } else {
        format!("; stderr: {}", printable(&truncate(&text, STDERR_TAIL)))
    }
}

/// The process groups of the stdio servers started and not yet ended. The server process
/// (the group leader) stays unreaped until its group is ended, so a group id here cannot
/// have been reused by another process.
type Groups = Arc<Mutex<Vec<u32>>>;

/// End `child`'s process group, then reap the leader; the group is then forgotten.
///
/// SIGTERM to the group, up to [`TERM_GRACE`] for the leader to exit (watched without
/// reaping it), then SIGKILL to the group, and only then the reap. Until the reap the
/// leader, alive or a zombie, holds its pid, so the group id cannot have been given to
/// another process when it is signalled.
async fn end_group(pgid: Option<u32>, child: &mut Child, groups: &Groups) -> Option<ExitStatus> {
    let Some(g) = pgid else {
        let _ = child.start_kill();
        return child.wait().await.ok();
    };
    killpg(g, libc::SIGTERM);
    let deadline = tokio::time::Instant::now() + TERM_GRACE;
    while !leader_exited(g) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(EXIT_POLL).await;
    }
    killpg(g, libc::SIGKILL);
    let status = child.wait().await.ok();
    lock(groups).retain(|&x| x != g);
    status
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
    /// The stdio server's process group, from its spawn on.
    pgid: Option<u32>,
    /// A listing is running; `dirty` asks for one more after it.
    relisting: bool,
    dirty: bool,
}

impl Server {
    fn new(source: ServerSource, state: McpState, message: Option<String>) -> Self {
        Server {
            source,
            state,
            message,
            conn: None,
            tools: Vec::new(),
            pgid: None,
            relisting: false,
            dirty: false,
        }
    }
}

#[derive(Default)]
struct HubState {
    servers: BTreeMap<String, Server>,
    warnings: Vec<String>,
    /// Every warning ever kept, so a repeated one is not shown again.
    warned: HashSet<String>,
    closed: bool,
}

impl HubState {
    /// Keep `w` unless it was seen before or the cap is reached; true when kept.
    fn warn(&mut self, w: String) -> bool {
        if self.warned.contains(&w) {
            return false;
        }
        if self.warned.len() >= MAX_HUB_WARNINGS {
            let last = "further MCP warnings are not shown".to_string();
            if self.warned.insert(last.clone()) {
                self.warnings.push(last);
                return true;
            }
            return false;
        }
        self.warned.insert(w.clone());
        self.warnings.push(w);
        true
    }
}

struct Inner {
    sb: Arc<dyn Sandbox>,
    cfg: McpConfig,
    strip_env: Vec<String>,
    /// Set in every stdio server's environment, over the server's own `env`.
    set_env: Vec<(String, String)>,
    connect_timeout: Duration,
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

    /// SIGKILL `server`'s process group and forget it. Called while the task that holds
    /// its (unreaped) process still runs, so the group id is still the server's.
    fn kill_server_group(&self, server: &str) {
        let pgid = lock(&self.state).servers.get(server).and_then(|s| s.pgid);
        if let Some(g) = pgid {
            if group_alive(g) {
                killpg(g, libc::SIGKILL);
            }
            lock(&self.groups).retain(|&x| x != g);
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
        if lock(&self.state).warn(w) {
            self.changed();
        }
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

    /// List `server`'s tools again, coalesced: one task per server lists after
    /// [`RELIST_DEBOUNCE`]; requests while it waits are folded in, and requests while it
    /// lists ask for one more round.
    fn request_relist(self: &Arc<Self>, server: String) {
        {
            let mut st = lock(&self.state);
            let Some(s) = st.servers.get_mut(&server) else {
                return;
            };
            if s.relisting {
                s.dirty = true;
                return;
            }
            s.relisting = true;
        }
        let me = Arc::clone(self);
        self.spawn(async move {
            loop {
                // Notifications arriving meanwhile are folded into this listing.
                tokio::time::sleep(RELIST_DEBOUNCE).await;
                if let Some(s) = lock(&me.state).servers.get_mut(&server) {
                    s.dirty = false;
                }
                me.relist(&server).await;
                let again = {
                    let mut st = lock(&me.state);
                    let Some(s) = st.servers.get_mut(&server) else {
                        return;
                    };
                    let again = std::mem::take(&mut s.dirty);
                    s.relisting = again;
                    again
                };
                if !again {
                    return;
                }
            }
        });
    }

    /// Fetch `server`'s tool list again and keep it if the server is still the one
    /// connected.
    async fn relist(&self, server: &str) {
        let conn = lock(&self.state)
            .servers
            .get(server)
            .and_then(|s| s.conn.clone());
        let Some(conn) = conn else {
            return;
        };
        let listed = tokio::time::timeout(
            self.connect_timeout,
            list_tools(&conn.peer, server, self.cfg.max_tools),
        )
        .await;
        match listed {
            Ok(Ok((tools, cut))) => {
                let (tools, mut warnings) = select_tools(server, tools, self.cfg.max_tools);
                warnings.extend(cut);
                {
                    let mut st = lock(&self.state);
                    let Some(s) = st.servers.get_mut(server) else {
                        return;
                    };
                    if !s.conn.as_ref().is_some_and(|c| Arc::ptr_eq(c, &conn)) {
                        return;
                    }
                    s.tools = tools;
                    for w in warnings {
                        st.warn(w);
                    }
                }
                self.changed();
            }
            Ok(Err(e)) => self.warn(format!(
                "MCP server `{}`: listing its tools again failed: {}",
                printable(server),
                printable(&e.to_string())
            )),
            Err(_) => self.warn(format!(
                "MCP server `{}`: listing its tools again timed out",
                printable(server)
            )),
        }
    }

    /// Start one server: connect, list its tools, and report it `Ready` (or `Failed`).
    async fn run_server(self: Arc<Self>, name: String, spec: ServerSpec, source: ServerSource) {
        self.set(&name, source, McpState::Starting, None);
        let mut connect = Box::pin(self.connect(&name, &spec, source));
        let started = tokio::select! {
            r = &mut connect => r,
            () = tokio::time::sleep(self.connect_timeout) => {
                // The pinned handshake still holds the process: its group is still ours.
                self.kill_server_group(&name);
                Err(format!(
                    "did not start within {} s",
                    self.connect_timeout.as_secs_f32()
                ))
            }
        };
        drop(connect);
        let (conn, tools, cut) = match started {
            Ok(ok) => ok,
            Err(why) => {
                self.set(&name, source, McpState::Failed, Some(why));
                return;
            }
        };
        let (tools, mut warnings) = select_tools(&name, tools, self.cfg.max_tools);
        warnings.extend(cut);
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
                for w in warnings {
                    st.warn(w);
                }
                true
            }
        };
        if !kept {
            conn.close().await;
            return;
        }
        self.changed();
        self.subscribe(name.clone(), Arc::clone(&conn));
        let me = Arc::clone(&self);
        self.spawn(me.watch(name, conn));
    }

    /// Report a ready server that exits or disconnects as `Failed` and stop offering its
    /// tools (the engine drops them at the next user-turn start).
    async fn watch(self: Arc<Self>, name: String, conn: Arc<Connection>) {
        while !conn.gone() {
            tokio::time::sleep(WATCH_EVERY).await;
        }
        let gone = if conn.may_write {
            "the server exited or closed its stdio"
        } else {
            "the server closed the connection"
        };
        let current = {
            let mut st = lock(&self.state);
            let closed = st.closed;
            match st.servers.get_mut(&name) {
                Some(s) if !closed && s.conn.as_ref().is_some_and(|c| Arc::ptr_eq(c, &conn)) => {
                    s.state = McpState::Failed;
                    s.message = Some(format!("{gone}{}", conn.stderr_note()));
                    s.conn = None;
                    s.tools.clear();
                    true
                }
                _ => false,
            }
        };
        if !current {
            return;
        }
        self.changed();
        // What the server left in its group goes with it: the leader is not reaped yet
        // (it was seen exiting with WNOWAIT), so the group id is still ours to signal.
        if let Some(g) = conn.pgid {
            killpg(g, libc::SIGKILL);
        }
        let status = conn.close().await;
        if let Some(status) = status {
            let mut st = lock(&self.state);
            if let Some(s) = st.servers.get_mut(&name)
                && s.state == McpState::Failed
                && s.conn.is_none()
            {
                s.message = Some(format!(
                    "the server exited ({status}){}",
                    conn.stderr_note()
                ));
            }
        }
        self.changed();
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
                    inner.request_relist(name.clone());
                }
            }
        });
    }

    async fn connect(
        self: &Arc<Self>,
        name: &str,
        spec: &ServerSpec,
        source: ServerSource,
    ) -> Result<Connected, String> {
        let handler = Handler {
            hub: Arc::downgrade(self),
            server: name.to_string(),
        };
        let lifecycle = ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            legacy_version: Some(ProtocolVersion::V_2025_11_25),
        };
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let conn = match spec {
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
                        env: env
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            // Over the server's own values: `set_env` wins.
                            .chain(self.set_env.iter().cloned())
                            .collect(),
                        // Removed after `env` is applied: no server sees a provider key.
                        env_remove: self.strip_env.clone(),
                        new_process_group: true,
                    })
                    .await
                    .map_err(|e| {
                        format!(
                            "could not start `{}`: {}",
                            printable(command),
                            printable(&e.to_string())
                        )
                    })?;
                let pgid = child.id();
                // Tracked from now on, so a shutdown or a connect timeout during the
                // handshake still ends it.
                lock(&self.groups).extend(pgid);
                if let Some(s) = lock(&self.state).servers.get_mut(name) {
                    s.pgid = pgid;
                }
                let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
                    end_group(pgid, &mut child, &self.groups).await;
                    return Err("the sandbox gave the server no stdin or stdout".into());
                };
                if let Some(mut pipe) = child.stderr.take() {
                    let tail = Arc::clone(&stderr);
                    self.spawn(async move {
                        let mut buf = [0u8; 4096];
                        while let Ok(n @ 1..) = pipe.read(&mut buf).await {
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
                        return Err(format!(
                            "handshake failed: {}{}",
                            printable(&e.to_string()),
                            stderr_note(&stderr)
                        ));
                    }
                };
                Arc::new(Connection {
                    server: name.to_string(),
                    peer: service.peer().clone(),
                    effect: Effect::Exec { argv },
                    may_write: true,
                    service: Mutex::new(Some(service)),
                    process: Mutex::new(Some(child)),
                    pgid,
                    groups: Arc::clone(&self.groups),
                    stderr,
                })
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
                    // `${VAR}` only from the user's (or the client's) own configuration: a
                    // repository must not send the user's environment anywhere.
                    let v = if matches!(source, ServerSource::User | ServerSource::Client) {
                        expand_env(v)
                    } else {
                        v.clone()
                    };
                    let k = http::HeaderName::try_from(k.as_str())
                        .map_err(|e| format!("bad header name `{}`: {e}", printable(k)))?;
                    let v = http::HeaderValue::try_from(v.as_str())
                        .map_err(|e| format!("bad value for header `{k}`: {e}"))?;
                    custom.insert(k, v);
                }
                let config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
                    .custom_headers(custom);
                let transport = StreamableHttpClientTransport::with_client(http_client()?, config);
                let service = rmcp::serve_client_with_lifecycle(handler, transport, lifecycle)
                    .await
                    .map_err(|e| {
                        format!(
                            "could not connect to {}: {}",
                            printable(url.as_str()),
                            printable(&e.to_string())
                        )
                    })?;
                Arc::new(Connection {
                    server: name.to_string(),
                    peer: service.peer().clone(),
                    effect: Effect::Network { host },
                    may_write: false,
                    service: Mutex::new(Some(service)),
                    process: Mutex::new(None),
                    pgid: None,
                    groups: Arc::clone(&self.groups),
                    stderr,
                })
            }
        };
        match list_tools(&conn.peer, name, self.cfg.max_tools).await {
            Ok((tools, cut)) => Ok((conn, tools, cut)),
            Err(e) => {
                let why = format!(
                    "listing its tools failed: {}{}",
                    printable(&e.to_string()),
                    conn.stderr_note()
                );
                conn.close().await;
                Err(why)
            }
        }
    }
}

/// A connection, its listed tools, and a warning when the listing was cut short.
type Connected = (Arc<Connection>, Vec<rmcp::model::Tool>, Option<String>);

/// The reqwest client for streamable HTTP: the workspace's TLS setup (rustls on `ring`, the
/// webpki roots, HTTP/1.1), no redirects (a server must not move the session to another
/// host), and no idle connection pool (rmcp's advice for its transport).
fn http_client() -> Result<reqwest::Client, String> {
    tautly_sandbox::http_client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|e| format!("HTTP client setup failed: {e}"))
}

/// The tool list, following cursors for at most [`MAX_PAGES`] pages and `4 × max_tools`
/// tools; when cut short, the warning that says so.
async fn list_tools(
    peer: &Peer<RoleClient>,
    server: &str,
    max_tools: usize,
) -> Result<(Vec<rmcp::model::Tool>, Option<String>), ServiceError> {
    let limit = max_tools.saturating_mul(4).max(1);
    let mut tools = Vec::new();
    let mut cursor = None;
    for page in 1..=MAX_PAGES {
        let r = peer
            .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
            .await?;
        tools.extend(r.tools);
        cursor = r.next_cursor;
        if cursor.is_none() {
            return Ok((tools, None));
        }
        if tools.len() >= limit || page == MAX_PAGES {
            let note = format!(
                "MCP server `{}`: stopped listing tools after {page} pages ({} tools); the \
                 rest are not used",
                printable(server),
                tools.len()
            );
            return Ok((tools, Some(note)));
        }
    }
    Ok((tools, None))
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

/// The tools of `server` within the caps, by name: a schema over [`SCHEMA_CAP`] is skipped;
/// a description is marked `[MCP server <name>]`, stripped of control characters and cut
/// to [`DESCRIPTION_CAP`]; past `max_tools` the rest are left out. Each skipped or cut
/// tool list gives a warning.
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
    let prefix = format!("[MCP server {}] ", strip_control(server));
    for t in listed {
        let mut schema = serde_json::Value::Object((*t.input_schema).clone());
        // Descriptions and titles inside the schema reach the model as well.
        strip_control_json(&mut schema);
        let size = serde_json::to_vec(&schema).map_or(usize::MAX, |b| b.len());
        if size > SCHEMA_CAP {
            warnings.push(format!(
                "MCP server `{}`: tool `{}` skipped: its input schema is {size} bytes, over \
                 the {SCHEMA_CAP} byte limit",
                printable(server),
                printable(&t.name)
            ));
            continue;
        }
        let description = format!(
            "{prefix}{}",
            strip_control(t.description.as_deref().unwrap_or(""))
        );
        kept.push(RemoteTool {
            name: t.name.to_string(),
            description: truncate(&description, DESCRIPTION_CAP),
            schema,
        });
    }
    if kept.len() > max_tools {
        warnings.push(format!(
            "MCP server `{}` offers {offered} tools; only the first {max_tools} by name are \
             used",
            printable(server)
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
            inner.request_relist(self.server.clone());
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
        McpHub::start_with_connect_timeout(servers, sb, cfg, strip_env, CONNECT_TIMEOUT).await
    }

    /// [`McpHub::start`], with `set_env` set in every stdio server's environment over the
    /// server's own `env` (the engine sets `PYTHONPYCACHEPREFIX` to a private directory,
    /// so a Python server that honours it does not read the repository's `__pycache__`).
    pub async fn start_with_env(
        servers: Vec<(String, ServerSpec, ServerSource)>,
        sb: Arc<dyn Sandbox>,
        cfg: &McpConfig,
        strip_env: &[String],
        set_env: &[(String, String)],
    ) -> McpHub {
        McpHub::build(servers, sb, cfg, strip_env, set_env, CONNECT_TIMEOUT)
    }

    /// [`McpHub::start`] with another limit on connecting (default 120 s).
    pub async fn start_with_connect_timeout(
        servers: Vec<(String, ServerSpec, ServerSource)>,
        sb: Arc<dyn Sandbox>,
        cfg: &McpConfig,
        strip_env: &[String],
        connect_timeout: Duration,
    ) -> McpHub {
        McpHub::build(servers, sb, cfg, strip_env, &[], connect_timeout)
    }

    fn build(
        servers: Vec<(String, ServerSpec, ServerSource)>,
        sb: Arc<dyn Sandbox>,
        cfg: &McpConfig,
        strip_env: &[String],
        set_env: &[(String, String)],
        connect_timeout: Duration,
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
                set_env: set_env.to_vec(),
                connect_timeout,
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
    /// ([`names::assign`]); at most [`MAX_TOTAL_TOOLS`] (the first by name).
    pub fn ready_tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<McpTool> = {
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
            names::assign(&pairs)
                .into_iter()
                .zip(&found)
                .map(|(name, (_, remote, conn))| McpTool {
                    name,
                    remote: (*remote).clone(),
                    conn: Arc::clone(conn),
                    timeout,
                })
                .collect()
        };
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        if tools.len() > MAX_TOTAL_TOOLS {
            self.inner.warn(format!(
                "MCP servers offer {} tools in all; only the first {MAX_TOTAL_TOOLS} by name \
                 are used",
                tools.len()
            ));
            tools.truncate(MAX_TOTAL_TOOLS);
        }
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

    /// Warnings since the last call (skipped tools, capped tool lists), each distinct one
    /// once, at most `MAX_HUB_WARNINGS` in the hub's life.
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

/// Whether the process `pid` (our child) has exited, without reaping it
/// (`waitid(P_PID, pid, WEXITED | WNOHANG | WNOWAIT)`): a zombie still holds its pid, so
/// its process group id cannot be reused while the group is signalled.
fn leader_exited(pid: u32) -> bool {
    let id = libc::id_t::from(pid);
    // SAFETY: an all-zero `siginfo_t` is a valid out-parameter for `waitid`.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` outlives the call; WNOWAIT leaves the child waitable.
    let r = unsafe {
        libc::waitid(
            libc::P_PID,
            id,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    // With WNOHANG and no exited child, `si_pid` stays 0.
    r == 0 && siginfo_pid(&info) != 0
}

#[cfg(target_os = "linux")]
fn siginfo_pid(info: &libc::siginfo_t) -> libc::pid_t {
    // SAFETY: `waitid` filled in the child fields.
    unsafe { info.si_pid() }
}

#[cfg(not(target_os = "linux"))]
fn siginfo_pid(info: &libc::siginfo_t) -> libc::pid_t {
    info.si_pid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exit_is_detected_without_reaping() {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "exit 3"])
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let end = std::time::Instant::now() + Duration::from_secs(10);
        while !leader_exited(pid) {
            assert!(std::time::Instant::now() < end, "exit not seen");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Still a zombie: not reaped, so the pid (and group id) is still taken.
        // SAFETY: signal 0 only checks that the process exists.
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);
        assert!(leader_exited(pid), "asking again does not reap either");
        assert_eq!(child.wait().await.unwrap().code(), Some(3));
        // SAFETY: as above.
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    }
}

//! A session's MCP servers (spec 7.11.10): which ones start, the approval of
//! repository-declared ones, status events, and the tool set taken at each user-turn
//! start.
//!
//! Servers come from the user's configuration and the client (trusted), and from the
//! project layer's `[mcp.servers]` and the repository root's `.mcp.json` (repository
//! content). A trusted server shadows a repository one of the same name, with a warning.
//! A repository server starts only with a remembered approval
//! (`<data home>/mcp-approvals.json`, keyed by [`tautly_mcp::approvals::key`]), or with
//! `approve_mcp` (this session only, nothing remembered). Otherwise an interactive client
//! is asked (`McpServerApprovalRequested`; an approval is remembered, a denial lasts for
//! the session) and a headless session skips the server.
//!
//! Servers start in the background. The tool set changes only at a user-turn start
//! (`Session::refresh_mcp_tools`): the first waits up to `startup_timeout_ms` for
//! servers still starting; a server ready later, or a changed tool list, joins at the next
//! one, with a warning that the cached prefix breaks once.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use tautly_mcp::approvals::approval_key;
use tautly_mcp::config::parse_mcp_json;
use tautly_mcp::names::{MAX_SERVER_NAME, is_valid_server_name, sanitize};
use tautly_mcp::{
    APPROVALS_FILE, ApprovalStore, McpConfig, McpHub, McpState, PROVIDER_KEY_VARS, ServerSource,
    ServerSpec, printable,
};
use tautly_provider::ToolSpec;
use tautly_sandbox::{Effect, Sandbox};
use tautly_tools::{Tool, registry};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::approvals::{Approvals, Verdict};
use crate::checkpoint::Point;
use crate::config::Config;
use crate::protocol::Event;
use crate::record::{ApprovalKind, SessionRecord};
use crate::session::{EventSink, Session};

/// Every MCP tool's name starts with this (`mcp__<server>__<tool>`).
pub(crate) const MCP_TOOL_PREFIX: &str = "mcp__";
/// The repository root's Claude-style server list.
const MCP_JSON: &str = ".mcp.json";
/// A larger `.mcp.json` is not read.
const MAX_MCP_JSON_BYTES: u64 = 256 * 1024;
/// Repository-declared servers considered per session (the rest are dropped with a
/// warning).
const MAX_REPO_SERVERS: usize = 32;
/// Planning warnings shown before the rest are summarized.
const MAX_WARNINGS: usize = 20;
/// Longest planning (reading `.mcp.json` and the approvals) may take at session start.
const PLAN_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs before a call that may change files the engine does not see change: every `bash`
/// and stdio MCP call (spec 7.11.11), right after the engine's checkpoint of the work tree
/// (`checkpoint.rs`). The default does nothing.
#[async_trait]
pub trait PreExecHook: Send + Sync {
    /// `tool` is about to run as part of step `turn`.
    async fn before_exec(&self, tool: &str, turn: u32);
}

/// The default [`PreExecHook`]: nothing.
pub struct NoPreExec;

#[async_trait]
impl PreExecHook for NoPreExec {
    async fn before_exec(&self, _tool: &str, _turn: u32) {}
}

/// What the engine builder knows about MCP, for [`McpSession::start`].
pub(crate) struct McpSetup {
    pub config: McpConfig,
    pub project: BTreeMap<String, ServerSpec>,
    pub client: Vec<(String, ServerSpec)>,
    pub approve_mcp: bool,
    pub data_home: PathBuf,
    /// Provider key variables no server may see.
    pub strip_env: Vec<String>,
}

impl McpSetup {
    pub fn new(
        config: &Config,
        client: Vec<(String, ServerSpec)>,
        approve_mcp: bool,
        data_home: &Path,
    ) -> Self {
        McpSetup {
            config: config.mcp.clone(),
            project: config.project_mcp_servers.clone(),
            client,
            approve_mcp,
            data_home: data_home.to_path_buf(),
            strip_env: provider_key_vars(config),
        }
    }
}

/// The default provider key variables plus every configured `api_key_env`: stripped from
/// MCP servers and from bash commands.
pub(crate) fn provider_key_vars(config: &Config) -> Vec<String> {
    let p = &config.providers;
    let mut vars: Vec<String> = PROVIDER_KEY_VARS.iter().map(|s| s.to_string()).collect();
    let configured = [&p.anthropic, &p.openai, &p.google]
        .into_iter()
        .flatten()
        .filter_map(|e| e.api_key_env.clone())
        .chain(p.compat.iter().filter_map(|c| c.api_key_env.clone()));
    for v in configured {
        if !vars.contains(&v) {
            vars.push(v);
        }
    }
    vars
}

/// Whether a planned server may start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// The user's or the client's own server.
    Trusted,
    /// A repository server the user approved before (remembered in the data home).
    Remembered,
    /// A repository server started for this run only (`approve_mcp`); nothing is saved.
    ThisRun,
    /// A repository server nobody approved: asked about (interactive) or skipped.
    Pending,
}

/// One server a session would know about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedServer {
    pub name: String,
    pub spec: ServerSpec,
    pub source: ServerSource,
    pub approval: Approval,
    /// The approval key of a repository server ([`tautly_mcp::approvals::key`]).
    pub key: Option<String>,
    /// An approval may be remembered (false: a file its command line names cannot be
    /// hashed, so it is approved for the run only; see
    /// [`tautly_mcp::approvals::ApprovalKey`]).
    pub rememberable: bool,
}

/// The servers of a session, by name, and what was noticed on the way (shadowed servers,
/// an unreadable `.mcp.json`).
#[derive(Debug, Clone, Default)]
pub struct McpPlan {
    pub servers: Vec<PlannedServer>,
    pub warnings: Vec<String>,
}

/// Which servers a session on `sandbox`'s root has, where each comes from and whether it
/// may start: the engine starts a session from this, and `tautly mcp list` / `doctor`
/// show it. `config` is the user's `[mcp]` (its `servers` trusted), `project` the project
/// layer's servers, `client` the client's (trusted); the repository root's `.mcp.json` is
/// read when `import_mcp_json` is on. A trusted server shadows a repository one of the
/// same name, and a project server one of `.mcp.json`. Empty when MCP is disabled.
pub async fn plan_servers(
    config: &McpConfig,
    project: &BTreeMap<String, ServerSpec>,
    client: &[(String, ServerSpec)],
    approve_mcp: bool,
    data_home: &Path,
    sandbox: &dyn Sandbox,
) -> McpPlan {
    let mut plan = McpPlan::default();
    if !config.enabled {
        return plan;
    }
    let mut repo: Vec<(String, ServerSpec, ServerSource)> = project
        .iter()
        .map(|(n, s)| (n.clone(), s.clone(), ServerSource::Project))
        .collect();
    if config.import_mcp_json {
        let (servers, warnings) = read_mcp_json(sandbox).await;
        plan.warnings.extend(warnings);
        let declared: HashSet<String> = project.keys().map(|n| sanitize(n)).collect();
        for (name, spec) in servers {
            if declared.contains(&sanitize(&name)) {
                plan.warnings.push(format!(
                    "MCP server `{}` of {MCP_JSON} is shadowed by the project config's server \
                     of the same name",
                    printable(&name)
                ));
            } else {
                repo.push((name, spec, ServerSource::McpJson));
            }
        }
    }
    // A repository server's name must not let its tools pass for another server's
    // (`gh__x` would get tools `mcp__gh__x__…`, which the user's `mcp__gh__*` rules cover).
    repo.retain(|(name, _, source)| {
        let ok = is_valid_server_name(name);
        if !ok {
            plan.warnings.push(format!(
                "MCP server `{}` declared by the repository ({source}) is not a valid server \
                 name (letters, digits and `-`, parts joined by single `_`, at most \
                 {MAX_SERVER_NAME} characters); it is ignored",
                printable(name)
            ));
        }
        ok
    });
    let mut trusted: BTreeMap<String, (ServerSpec, ServerSource)> = config
        .servers
        .iter()
        .map(|(n, s)| (n.clone(), (s.clone(), ServerSource::User)))
        .collect();
    for (name, spec) in client {
        if trusted.contains_key(name) {
            plan.warnings.push(format!(
                "MCP server `{}` from the client replaces the user config's server of the \
                 same name",
                printable(name)
            ));
        }
        trusted.insert(name.clone(), (spec.clone(), ServerSource::Client));
    }
    // Compared as tool names see them: `my.srv` (user) and `my_srv` (repository) are the
    // same `mcp__my_srv__` prefix.
    let by_tool_name: HashMap<String, ServerSource> = trusted
        .iter()
        .map(|(n, (_, source))| (sanitize(n), *source))
        .collect();
    repo.retain(
        |(name, _, source)| match by_tool_name.get(&sanitize(name)) {
            Some(by) => {
                plan.warnings.push(format!(
                    "MCP server `{}` declared by the repository ({source}) is shadowed by the {by} \
                 server of the same name; the {by} one is used",
                    printable(name)
                ));
                false
            }
            None => true,
        },
    );
    // A repository cannot flood the user with approval requests (or the session with
    // processes).
    repo.sort_by(|a, b| a.0.cmp(&b.0));
    if repo.len() > MAX_REPO_SERVERS {
        plan.warnings.push(format!(
            "the repository declares {} MCP servers; only the first {MAX_REPO_SERVERS} by name \
             are considered",
            repo.len()
        ));
        repo.truncate(MAX_REPO_SERVERS);
    }
    if plan.warnings.len() > MAX_WARNINGS {
        let more = plan.warnings.len() - MAX_WARNINGS;
        plan.warnings.truncate(MAX_WARNINGS);
        plan.warnings
            .push(format!("and {more} more MCP configuration warnings"));
    }
    for (name, (spec, source)) in trusted {
        plan.servers.push(PlannedServer {
            name,
            spec,
            source,
            approval: Approval::Trusted,
            key: None,
            rememberable: false,
        });
    }
    if !repo.is_empty() {
        let path = data_home.join(APPROVALS_FILE);
        let root = sandbox.root().to_path_buf();
        // Keys read the files the command lines name (bounded), so off the async threads.
        let keyed = tokio::task::spawn_blocking(move || {
            let keyed: Vec<_> = repo
                .into_iter()
                .map(|(name, spec, source)| {
                    let k = approval_key(&root, &name, &spec);
                    (name, spec, source, k)
                })
                .collect();
            (ApprovalStore::open(&path), keyed)
        })
        .await;
        let Ok((store, keyed)) = keyed else {
            plan.warnings.push(
                "repository-declared MCP servers not started: their approvals could not be \
                 checked"
                    .into(),
            );
            plan.servers.sort_by(|a, b| a.name.cmp(&b.name));
            return plan;
        };
        for (name, spec, source, k) in keyed {
            let approval = if k.rememberable && store.is_approved(&k.key) {
                Approval::Remembered
            } else if approve_mcp {
                Approval::ThisRun
            } else {
                Approval::Pending
            };
            plan.servers.push(PlannedServer {
                name,
                spec,
                source,
                approval,
                key: Some(k.key),
                rememberable: k.rememberable,
            });
        }
    }
    plan.servers.sort_by(|a, b| a.name.cmp(&b.name));
    plan
}

/// A server's status as last told to the client.
type Told = HashMap<String, (McpState, u32, Option<String>)>;

/// The servers of one session and the tasks that report on them.
pub(crate) struct McpSession {
    hub: Arc<McpHub>,
    startup_timeout: Duration,
    /// The first user turn has waited for the servers.
    waited: bool,
    /// Approval requests not answered yet (by request id), announced again at every
    /// user-turn start until they are: one the client missed would otherwise wait unseen
    /// for the whole session.
    pending: Arc<Mutex<Vec<(String, Event)>>>,
    /// The statuses the client has been sent; any other is sent at the next turn start.
    told: Arc<Mutex<Told>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for McpSession {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl McpSession {
    /// Decide which servers start, ask about or skip repository ones, and start the rest
    /// in the background, for session `owner`. None when MCP is off or no server is
    /// declared. Each question is logged (`ApprovalRequested`) before it is shown; its
    /// answer is the logged `Approve`/`Deny` command.
    pub async fn start(
        setup: McpSetup,
        owner: &Session,
        approvals: Approvals,
    ) -> Option<McpSession> {
        let sandbox = Arc::clone(&owner.sandbox);
        let events = owner.events.clone();
        let planned = tokio::time::timeout(
            PLAN_TIMEOUT,
            plan_servers(
                &setup.config,
                &setup.project,
                &setup.client,
                setup.approve_mcp,
                &setup.data_home,
                sandbox.as_ref(),
            ),
        )
        .await;
        let Ok(plan) = planned else {
            events.try_emit(Event::Warning {
                message: format!(
                    "MCP servers not started: reading their configuration took longer than {} s",
                    PLAN_TIMEOUT.as_secs()
                ),
            });
            return None;
        };
        for w in plan.warnings {
            events.try_emit(Event::Warning {
                message: printable(&w),
            });
        }
        if plan.servers.is_empty() {
            return None;
        }
        let root = sandbox.root().to_path_buf();
        let store_path = setup.data_home.join(APPROVALS_FILE);
        let mut start = Vec::new();
        let mut ask = Vec::new();
        let mut skip = Vec::new();
        for s in plan.servers {
            match (s.approval, s.key) {
                (Approval::Pending, Some(k)) if approvals.is_interactive() => {
                    ask.push((s.name, s.spec, s.source, k, s.rememberable));
                }
                (Approval::Pending, _) => skip.push((s.name, s.source)),
                _ => start.push((s.name, s.spec, s.source)),
            }
        }

        let hub = Arc::new(
            McpHub::start(start, Arc::clone(&sandbox), &setup.config, &setup.strip_env).await,
        );
        let told: Arc<Mutex<Told>> = Arc::default();
        let mut session = McpSession {
            hub: Arc::clone(&hub),
            startup_timeout: Duration::from_millis(setup.config.startup_timeout_ms),
            waited: false,
            pending: Arc::default(),
            told: Arc::clone(&told),
            tasks: vec![tokio::spawn(report(Arc::clone(&hub), events.clone(), told))],
        };
        for (name, source) in skip {
            hub.note(
                &name,
                source,
                McpState::Skipped,
                Some(
                    "repository-declared server not approved: approve it in an interactive \
                     session, or run with --approve-mcp to start it this time"
                        .into(),
                ),
            );
        }
        for (name, spec, source, k, rememberable) in ask {
            let Some((request_id, rx)) = approvals.open() else {
                continue;
            };
            hub.note(&name, source, McpState::AwaitingApproval, None);
            // What runs once approved is this `spec`, exactly as shown and keyed.
            let mut launch = spec.launch(source);
            if !rememberable {
                launch.push_str(
                    " (approved for this run only: a file its command line names cannot be \
                     hashed)",
                );
            }
            owner
                .log_approval(SessionRecord::ApprovalRequested {
                    request_id: request_id.clone(),
                    kind: ApprovalKind::McpServer,
                    call_id: None,
                    reason: format!(
                        "repository-declared MCP server `{}` ({source}) wants to start: {launch}",
                        printable(&name)
                    ),
                })
                .await;
            let request = Event::McpServerApprovalRequested {
                request_id: request_id.clone(),
                server: printable(&name),
                source: source.as_str().to_string(),
                launch,
            };
            lock(&session.pending).push((request_id.clone(), request.clone()));
            // Sent again at each user-turn start while unanswered, so a full channel now
            // does not lose it.
            events.try_send(request);
            let hub = Arc::clone(&hub);
            let pending = Arc::clone(&session.pending);
            let (store_path, root, events) = (store_path.clone(), root.clone(), events.clone());
            session.tasks.push(tokio::spawn(async move {
                let verdict = rx.await.unwrap_or(Verdict::Denied { reason: None });
                lock(&pending).retain(|(id, _)| *id != request_id);
                match verdict {
                    Verdict::Approved if !rememberable => hub.add(name, spec, source),
                    Verdict::Approved => {
                        let (n, r) = (name.clone(), root.clone());
                        let saved = tokio::task::spawn_blocking(move || {
                            ApprovalStore::open(&store_path).approve(&k, &r, &n)
                        })
                        .await;
                        if !matches!(saved, Ok(Ok(()))) {
                            events.try_emit(Event::Warning {
                                message: format!(
                                    "the approval of MCP server `{}` could not be saved; it \
                                     is approved for this session only",
                                    printable(&name)
                                ),
                            });
                        }
                        hub.add(name, spec, source);
                    }
                    Verdict::Denied { .. } => hub.note(
                        &name,
                        source,
                        McpState::Skipped,
                        Some("denied for this session".into()),
                    ),
                }
            }));
        }
        Some(session)
    }

    /// The MCP tools for a user turn that is starting, and whether it is the session's
    /// first (which waits up to the start-up timeout for servers still starting).
    pub async fn tools_for_turn(&mut self) -> (Vec<Arc<dyn Tool>>, bool) {
        let first = !std::mem::replace(&mut self.waited, true);
        if first {
            self.hub.wait_started(self.startup_timeout).await;
        }
        (self.hub.ready_tools(), first)
    }

    /// What to tell the client at a user-turn start: every unanswered approval request,
    /// and every status it has not been sent (a full channel dropped it). The statuses are
    /// counted as told.
    fn announcements(&self) -> Vec<Event> {
        let mut out: Vec<Event> = lock(&self.pending)
            .iter()
            .map(|(_, ev)| ev.clone())
            .collect();
        let mut told = lock(&self.told);
        for s in self.hub.status() {
            let now = (s.state, s.tools, s.message.clone());
            if told.get(&s.name) != Some(&now) {
                out.push(status_event(
                    &s.name,
                    s.state,
                    s.tools,
                    s.message.as_deref(),
                ));
                told.insert(s.name, now);
            }
        }
        out
    }

    pub async fn shutdown(&self) {
        self.hub.shutdown().await;
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn status_event(server: &str, state: McpState, tools: u32, message: Option<&str>) -> Event {
    Event::McpServerStatus {
        server: printable(server),
        state,
        tools,
        message: message.map(printable),
    }
}

impl Session {
    /// At a user-turn start: repeat unanswered approval requests and missed statuses, then
    /// offer the MCP tools that are ready now (the first turn waits for servers still
    /// starting). A different tool set than the last turn's is announced, since it breaks
    /// the cached prompt prefix once.
    pub(crate) async fn refresh_mcp_tools(&mut self) {
        let Some(told) = self.mcp.as_ref().map(McpSession::announcements) else {
            return;
        };
        for ev in told {
            self.emit(ev).await;
        }
        let Some(mcp) = self.mcp.as_mut() else {
            return;
        };
        let (extra, first) = mcp.tools_for_turn().await;
        let tools = registry(&self.info.caps, extra);
        let specs: Vec<ToolSpec> = tools.iter().map(|t| t.spec(&self.info.caps)).collect();
        if specs != self.specs && !first {
            let mcp_names = |s: &[ToolSpec]| -> Vec<String> {
                s.iter()
                    .filter(|t| t.name.starts_with(MCP_TOOL_PREFIX))
                    .map(|t| t.name.clone())
                    .collect()
            };
            let (now, before) = (mcp_names(&specs), mcp_names(&self.specs));
            let added = now.iter().filter(|n| !before.contains(n)).count();
            let removed = before.iter().filter(|n| !now.contains(n)).count();
            self.warn(format!(
                "MCP tools changed ({added} added, {removed} removed, the others may have \
                 new descriptions); the cached prompt prefix is rebuilt once for this turn"
            ))
            .await;
        }
        self.tools = tools;
        self.specs = specs;
    }

    /// Before a call that may change files unseen (`bash`, a stdio MCP call) of step
    /// `turn`: a checkpoint of the work tree (spec 7.11.11; a cancel abandons it), then the
    /// pre-exec hook. Inside the call's barrier, so its time counts in the call's.
    pub(crate) async fn before_exec(
        &self,
        tool: &str,
        effect: &Effect,
        turn: u32,
        cancel: &CancellationToken,
    ) {
        let command = tool == "bash" || tool.starts_with(MCP_TOOL_PREFIX);
        if command && matches!(effect, Effect::Exec { .. }) {
            self.git_checkpoint(Point::BeforeExec, turn, Some(cancel))
                .await;
            self.pre_exec.before_exec(tool, turn).await;
        }
    }
}

/// Emit `McpServerStatus` for every state change and a `Warning` for every hub warning,
/// for as long as the hub lives. A status the channel had no room for is not counted as
/// told, so the next user-turn start sends it.
async fn report(hub: Arc<McpHub>, events: EventSink, told: Arc<Mutex<Told>>) {
    let mut changes = hub.changes();
    loop {
        for w in hub.take_warnings() {
            events.try_emit(Event::Warning {
                message: printable(&w),
            });
        }
        for s in hub.status() {
            let now = (s.state, s.tools, s.message.clone());
            let mut told = lock(&told);
            if told.get(&s.name) != Some(&now)
                && events.try_send(status_event(
                    &s.name,
                    s.state,
                    s.tools,
                    s.message.as_deref(),
                ))
            {
                told.insert(s.name, now);
            }
        }
        if changes.changed().await.is_err() {
            return;
        }
    }
}

/// The servers of the repository root's `.mcp.json`, with a warning per problem. The path
/// is resolved through the sandbox (a symlink out of the workspace is refused), then
/// opened once without blocking and read only if that handle is a regular file, at most
/// [`MAX_MCP_JSON_BYTES`] (a FIFO or device in a checkout cannot hang the session).
async fn read_mcp_json(sb: &dyn Sandbox) -> (BTreeMap<String, ServerSpec>, Vec<String>) {
    let none = || (BTreeMap::new(), Vec::new());
    let not_read = |why: String| (BTreeMap::new(), vec![format!("{MCP_JSON} not read: {why}")]);
    let full = match sb.resolve(Path::new(MCP_JSON)) {
        Ok(p) => p,
        Err(e) => {
            return if std::fs::symlink_metadata(sb.root().join(MCP_JSON)).is_ok() {
                not_read(e.to_string())
            } else {
                none()
            };
        }
    };
    let read = tokio::task::spawn_blocking(move || read_bounded(&full)).await;
    let text = match read {
        Ok(Ok(Some(text))) => text,
        Ok(Ok(None)) => return none(),
        Ok(Err(why)) => return not_read(why),
        Err(e) => return not_read(e.to_string()),
    };
    match parse_mcp_json(&text) {
        Ok((servers, skipped)) => (
            servers,
            skipped
                .into_iter()
                .map(|w| format!("{MCP_JSON}: {w}"))
                .collect(),
        ),
        Err(e) => not_read(e),
    }
}

/// The text of `path`: None when it does not exist; an error unless it is a regular file
/// of at most [`MAX_MCP_JSON_BYTES`].
fn read_bounded(path: &Path) -> Result<Option<String>, String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let f = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if !f.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("it is not a regular file".into());
    }
    let mut bytes = Vec::new();
    f.take(MAX_MCP_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_MCP_JSON_BYTES {
        return Err(format!("larger than {MAX_MCP_JSON_BYTES} bytes"));
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

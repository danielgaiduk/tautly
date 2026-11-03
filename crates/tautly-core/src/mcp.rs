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

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tautly_mcp::approvals::key;
use tautly_mcp::config::parse_mcp_json;
use tautly_mcp::{
    APPROVALS_FILE, ApprovalStore, McpConfig, McpHub, McpState, PROVIDER_KEY_VARS, ServerSource,
    ServerSpec,
};
use tautly_provider::ToolSpec;
use tautly_sandbox::{Effect, Sandbox};
use tautly_tools::{Tool, registry};
use tokio::task::JoinHandle;

use crate::approvals::{Approvals, Verdict};
use crate::config::Config;
use crate::protocol::Event;
use crate::session::{EventSink, Session};

/// Every MCP tool's name starts with this (`mcp__<server>__<tool>`).
const MCP_TOOL_PREFIX: &str = "mcp__";
/// The repository root's Claude-style server list.
const MCP_JSON: &str = ".mcp.json";
/// A larger `.mcp.json` is not read.
const MAX_MCP_JSON_BYTES: u64 = 256 * 1024;
/// Repository-declared servers considered per session (the rest are dropped with a
/// warning).
const MAX_REPO_SERVERS: usize = 32;
/// Planning warnings shown before the rest are summarized.
const MAX_WARNINGS: usize = 20;

/// Runs before a call that may change files the engine does not see change: every stdio
/// MCP call (spec 7.11.11). Git checkpoints (Task 47) snapshot the work tree here; the
/// default does nothing.
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

/// The default provider key variables plus every configured `api_key_env`.
fn provider_key_vars(config: &Config) -> Vec<String> {
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
        for (name, spec) in servers {
            if project.contains_key(&name) {
                plan.warnings.push(format!(
                    "MCP server `{name}` of {MCP_JSON} is shadowed by the project config's \
                     server of the same name"
                ));
            } else {
                repo.push((name, spec, ServerSource::McpJson));
            }
        }
    }
    let mut trusted: BTreeMap<String, (ServerSpec, ServerSource)> = config
        .servers
        .iter()
        .map(|(n, s)| (n.clone(), (s.clone(), ServerSource::User)))
        .collect();
    for (name, spec) in client {
        if trusted.contains_key(name) {
            plan.warnings.push(format!(
                "MCP server `{name}` from the client replaces the user config's server of the \
                 same name"
            ));
        }
        trusted.insert(name.clone(), (spec.clone(), ServerSource::Client));
    }
    repo.retain(|(name, _, source)| match trusted.get(name) {
        Some((_, by)) => {
            plan.warnings.push(format!(
                "MCP server `{name}` declared by the repository ({source}) is shadowed by the \
                 {by} server of the same name; the {by} one is used"
            ));
            false
        }
        None => true,
    });
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
        });
    }
    if !repo.is_empty() {
        let path = data_home.join(APPROVALS_FILE);
        let store = tokio::task::spawn_blocking(move || ApprovalStore::open(&path))
            .await
            .ok();
        let root = sandbox.root();
        for (name, spec, source) in repo {
            let k = key(root, &name, &spec);
            let approval = if store.as_ref().is_some_and(|s| s.is_approved(&k)) {
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
                key: Some(k),
            });
        }
    }
    plan.servers.sort_by(|a, b| a.name.cmp(&b.name));
    plan
}

/// The servers of one session and the tasks that report on them.
pub(crate) struct McpSession {
    hub: Arc<McpHub>,
    startup_timeout: Duration,
    /// The first user turn has waited for the servers.
    waited: bool,
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
    /// in the background. None when MCP is off or no server is declared.
    pub async fn start(
        setup: McpSetup,
        sandbox: Arc<dyn Sandbox>,
        events: EventSink,
        approvals: Approvals,
    ) -> Option<McpSession> {
        let plan = plan_servers(
            &setup.config,
            &setup.project,
            &setup.client,
            setup.approve_mcp,
            &setup.data_home,
            sandbox.as_ref(),
        )
        .await;
        for w in plan.warnings {
            events.try_emit(Event::Warning { message: w });
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
                    ask.push((s.name, s.spec, s.source, k));
                }
                (Approval::Pending, _) => skip.push((s.name, s.source)),
                _ => start.push((s.name, s.spec, s.source)),
            }
        }

        let hub = Arc::new(
            McpHub::start(start, Arc::clone(&sandbox), &setup.config, &setup.strip_env).await,
        );
        let mut session = McpSession {
            hub: Arc::clone(&hub),
            startup_timeout: Duration::from_millis(setup.config.startup_timeout_ms),
            waited: false,
            tasks: vec![tokio::spawn(report(Arc::clone(&hub), events.clone()))],
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
        for (name, spec, source, k) in ask {
            let Some((request_id, rx)) = approvals.open() else {
                continue;
            };
            hub.note(&name, source, McpState::AwaitingApproval, None);
            // Not dropped lightly: unseen, the request would wait for the whole session.
            // What runs once approved is this `spec`, exactly as shown and keyed.
            events
                .send_or_drop(Event::McpServerApprovalRequested {
                    request_id,
                    server: name.clone(),
                    source: source.as_str().to_string(),
                    launch: spec.launch(),
                })
                .await;
            let hub = Arc::clone(&hub);
            let (store_path, root, events) = (store_path.clone(), root.clone(), events.clone());
            session.tasks.push(tokio::spawn(async move {
                match rx.await.unwrap_or(Verdict::Denied { reason: None }) {
                    Verdict::Approved => {
                        let (n, r) = (name.clone(), root.clone());
                        let saved = tokio::task::spawn_blocking(move || {
                            ApprovalStore::open(&store_path).approve(&k, &r, &n)
                        })
                        .await;
                        if !matches!(saved, Ok(Ok(()))) {
                            events.try_emit(Event::Warning {
                                message: format!(
                                    "the approval of MCP server `{name}` could not be saved; \
                                     it is approved for this session only"
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

    pub async fn shutdown(&self) {
        self.hub.shutdown().await;
    }
}

impl Session {
    /// At a user-turn start: offer the MCP tools that are ready now (the first turn waits
    /// for servers still starting). A different tool set than the last turn's is
    /// announced, since it breaks the cached prompt prefix once.
    pub(crate) async fn refresh_mcp_tools(&mut self) {
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

    /// Before a call that may change files unseen: a stdio MCP call.
    pub(crate) async fn before_exec(&self, tool: &str, effect: &Effect, turn: u32) {
        if tool.starts_with(MCP_TOOL_PREFIX) && matches!(effect, Effect::Exec { .. }) {
            self.pre_exec.before_exec(tool, turn).await;
        }
    }
}

/// Emit `McpServerStatus` for every state change and a `Warning` for every hub warning,
/// for as long as the hub lives.
async fn report(hub: Arc<McpHub>, events: EventSink) {
    let mut changes = hub.changes();
    let mut last: HashMap<String, (McpState, u32, Option<String>)> = HashMap::new();
    loop {
        for w in hub.take_warnings() {
            events.try_emit(Event::Warning { message: w });
        }
        for s in hub.status() {
            let now = (s.state, s.tools, s.message.clone());
            if last.get(&s.name) != Some(&now) {
                events.try_emit(Event::McpServerStatus {
                    server: s.name.clone(),
                    state: s.state,
                    tools: s.tools,
                    message: s.message.clone(),
                });
                last.insert(s.name, now);
            }
        }
        if changes.changed().await.is_err() {
            return;
        }
    }
}

/// The servers of the repository root's `.mcp.json`, through the sandbox (so a symlink
/// out of the workspace is refused), with a warning per problem.
async fn read_mcp_json(sb: &dyn Sandbox) -> (BTreeMap<String, ServerSpec>, Vec<String>) {
    let path = Path::new(MCP_JSON);
    let Ok(meta) = sb.metadata(path).await else {
        return (BTreeMap::new(), Vec::new());
    };
    if meta.len > MAX_MCP_JSON_BYTES {
        return (
            BTreeMap::new(),
            vec![format!(
                "{MCP_JSON} not read: larger than {MAX_MCP_JSON_BYTES} bytes"
            )],
        );
    }
    let text = match sb.read(path).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => return (BTreeMap::new(), vec![format!("{MCP_JSON} not read: {e}")]),
    };
    match parse_mcp_json(&text) {
        Ok((servers, skipped)) => (
            servers,
            skipped
                .into_iter()
                .map(|w| format!("{MCP_JSON}: {w}"))
                .collect(),
        ),
        Err(e) => (BTreeMap::new(), vec![format!("{MCP_JSON} not read: {e}")]),
    }
}

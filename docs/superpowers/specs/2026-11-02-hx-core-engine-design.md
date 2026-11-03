# Tautly — Core Engine Design (Sub-project 1)

- **Date:** 2026-11-02
- **Status:** Draft for review
- **Name:** Tautly (crates `tautly-*`, binary `tautly`); renamed from the working name `hx`

## 1. Intent

### 1.1 Goal
An open-source, provider-neutral agentic coding harness that becomes the go-to tool for agentic coders, winning on **performance and reliability**, with extensibility as the underlying architecture.

### 1.2 Decisions made
| Topic | Decision |
|---|---|
| Model | Open-source product, permissive license (Apache-2.0) |
| Wedge | Performance + reliability |
| v1 surfaces | Daily-driver line REPL, headless/JSON-RPC mode and an ACP bridge for editors, all on one engine (7.11); the full-screen TUI is sub-project 3 |
| Providers | Native Anthropic, OpenAI, Google adapters + generic OpenAI-compatible |
| Team | Solo developer + coding agents |
| Language / shape | Rust, library-first engine, thin clients in-process |

### 1.3 Assumptions
- Platforms: macOS and Linux in v1; Windows (WSL first) after.
- Distribution: single static binary, released with cargo-dist (Homebrew tap, `curl | sh` installer, `cargo binstall`, GitHub release binaries; 7.11.3).
- No vendor subscription OAuth reuse — API keys only (avoids ToS/blocking risk seen in 2026).

### 1.4 Why this can win (from market research, Nov 2026)
- Codex CLI is fast (Rust) but OpenAI-centric; OpenCode is provider-neutral but has documented memory runaways (10–15 GB+), orphaned processes and version-to-version breakage; pi is minimal and transparent but has no sandbox; Claude Code is closed and single-vendor.
- Gaps this design targets: lowest tokens per solved task (code index, diff re-reads, context GC, output filters, code-anchored memory, cheap side-model routing), bounded memory and instant start, flicker-free TUI, sandbox covering *all* tools, transparent context with constraints that survive compaction, model-adaptive and measured edit tools, live cost/quota with hard budgets, open session format, standards interop (AGENTS.md, SKILL.md, MCP, ACP).

### 1.5 Program decomposition
| # | Sub-project | Covered here? |
|---|---|---|
| 1 | Core engine (loop, providers, tools, context, sessions, protocol) | **Yes** |
| 2 | Sandbox + permission enforcement (Seatbelt, Landlock/bubblewrap) | Interface + stub only; git checkpoints make bash side effects undoable (7.11.11) |
| 3 | TUI client (ratatui, synchronized output) | Daily-driver line REPL only (7.11.8) |
| 4 | Extensions (MCP, hooks, skills, ACP bridge) | Tools-only MCP client (7.11.10) and ACP bridge (7.11.9); hooks, skills and MCP resources/prompts/sampling: no |
| 5 | Eval & benchmark program | `tautly eval` runner, starter set and the public benchmark kit (9.3) |

**Scope change (user-approved 2026-11-03):** v1 gains receipts and a savings meter, zero-config first run with `doctor`/`auth add` and installers, diffs in approvals and results, mid-session model switch, `CLAUDE.md` fallback, an evidence footer, the daily-driver REPL, the ACP bridge, a tools-only MCP client and git checkpoints before bash (all in 7.11). OpenTelemetry export is removed from v1 (4.6). A second approved change (2026-11-03) adds the time-travel core: session-format hooks, rewind as fork, the session tree, fork at a step in an isolated worktree, playback, compare and bisect (6.5, 7.12).

**Roadmap after v1:**
1. A minimal OS sandbox for `Exec` (sub-project 2: Seatbelt on macOS, bubblewrap on Linux; writes confined to the root and temp, network on/off).
2. Trust.
3. Time travel (7.12.8).
4. Resilience & safety.
5. Economics.

Then the remaining sub-projects. The public launch ships the reproducible benchmark (9.3). OpenTelemetry export is post-v1.

Post-v1, out of scope for all of the above: multi-agent orchestration, deterministic replay, team policy-as-code, plugin marketplaces.

### 1.6 Success criteria for this sub-project
- Cold start to ready < 50 ms; idle RSS < 30 MB; RSS flat (±10%) across a 4-hour simulated session.
- All four test layers green; performance gates in CI pass.
- Cache hit ratio, blocked repeat attempts and edit repairs per stage are reported by `tautly eval`.
- Tokens per solved task on the eval starter set is measured, published, and protected by a CI regression gate (9.1).
- Usable as a daily driver on a real repository with each of the four adapters, via `tautly -p`, the REPL (7.11.8) and an ACP client (Zed).
- First run with only a provider key in the environment (or a running Ollama) and no config file reaches a first answer without setup; `tautly doctor` exits 0 on a healthy machine (7.11.2).
- Every turn ends with a receipt whose savings are labelled estimates (7.11.1); `/undo` in a git repository also reverts files changed by `bash` (7.11.11).
- `--resume` reproduces the exact request prefix even when `AGENTS.md` or the repo map changed on disk since (6.5).
- Rewind and branch switching never overwrite a file the user changed without explicit consent, and the abandoned branch stays intact (7.12).

## 2. Architecture

### 2.1 Crate layout (Cargo workspace)
```
tautly-cli        binary: arg parsing, selects REPL, headless or ACP mode; doctor, auth, stats
 ├─ tautly-tui    line-mode daily-driver REPL (7.11.8); ratatui client in sub-project 3
 ├─ tautly-rpc    JSON-RPC 2.0 over stdio (headless)
 ├─ tautly-acp    Agent Client Protocol bridge over stdio (7.11.9)
 └─ tautly-core   the engine — no terminal I/O, no global state
     ├─ tautly-mcp        tools-only MCP client (7.11.10)
     ├─ tautly-provider   Provider trait + anthropic / openai / google / openai-compat
     ├─ tautly-tools      Tool trait + read, write, edit, bash, grep, glob, outline, symbol, refs,
     │                web_fetch, recall, remember, pin, state; output filters
     ├─ tautly-context    context assembly, token budget, pinning, working state, compaction, context GC
     ├─ tautly-memory     code-anchored project memory, episodic memory, recall search
     ├─ tautly-index      tree-sitter symbol/reference index (SQLite), incremental
     ├─ tautly-session    append-only session log, resume, fork, undo
     └─ tautly-sandbox    Sandbox trait; this sub-project ships a passthrough-with-policy stub
```
Dependencies only point downward (`tautly-acp → tautly-core`, `tautly-core → tautly-mcp → {tautly-tools, tautly-sandbox}`, `tautly-tools → tautly-memory → {tautly-index, tautly-session}`, `tautly-index → tautly-sandbox`, `tautly-tools → tautly-sandbox`, `tautly-context → {tautly-provider, tautly-tools, tautly-index, tautly-sandbox}`). Each crate is independently testable.

### 2.2 Rules
- **Single entry point:** `tautly-core` exposes an `Engine` handle. Clients send `Command`s and receive a stream of `Event`s. TUI and RPC are both plain consumers, guaranteeing identical behavior.
- **Async:** Tokio. Each turn runs in its own task scope with a `CancellationToken`; cancellation propagates to provider streams and child processes.
- **Sandbox chokepoint:** tools receive `&dyn Sandbox` and have no direct access to `std::fs` or `std::process`. Enforced by crate visibility and a CI lint (`clippy::disallowed_methods` for `std::fs::*`, `std::process::Command` in `tautly-tools`).
- **Bounded by construction:** fixed-capacity channels between engine and clients; tool output capped with disk spill; session history streamed to disk; only the active context plus an index held in memory.
- **Config:** layered TOML — user (`~/.config/tautly/config.toml`) → project (`.tautly/config.toml`) → CLI flags. The project layer is repository content and therefore untrusted: it may not set `providers` (base URLs, key variables), `telemetry`, `approve_memory`, `catalog_overrides`, `routing` or `evidence`, and in `permissions` it can only tighten (add `ask`/`deny` rules or lower the mode), never loosen. Ignored keys produce a `Warning` event naming the key. `AGENTS.md` in the repository root and ancestors is loaded as context (`CLAUDE.md` where a directory has no `AGENTS.md`, 7.11.6). Keys added by the v1 scope change are listed in 2.4.

### 2.3 Stubs shipped in this sub-project
- **`tautly-tui`:** the line-mode daily-driver REPL (7.11.8), not full-screen. The ratatui client of sub-project 3 replaces it later.
- **`tautly-sandbox` stub:** applies the permission policy (5.5) and enforces workspace-root path confinement (canonicalized paths, symlinks resolved) in-process; spawns processes directly. No OS-level isolation — that is sub-project 2.

### 2.4 Configuration added by the v1 scope change
| Key | Default | Layers |
|---|---|---|
| `providers.auto_detect` | `true` | user, CLI. Provider detection when no layer names a model (7.11.2) |
| `[mcp]` `enabled`, `timeout_s`, `startup_timeout_ms`, `max_tools`, `import_mcp_json` | `true`, 120, 5000, 50, `true` | user, CLI |
| `[mcp.servers.<name>]` `{command, args, env, cwd}` or `{url, headers}` | none | user: trusted. Project layer: `servers` only; each such server is repository-declared and needs approval (7.11.10). A repository `.mcp.json` counts as the project layer |
| `[evidence]` `test_commands`, `replace_defaults` | built-in list, `false` | user, CLI. A repository could otherwise label any command a passing test (7.11.7) |
| `[receipts]` `footer` | `true` | user, CLI |
| `[checkpoints]` `enabled`, `keep_days`, `timeout_ms`, `max_untracked_mb`, `max_untracked_files` | `true`, 14, 3000, 200, 20000 | user, CLI. Turning checkpoints off removes an undo safety net, so a repository cannot (7.11.11) |
| `[telemetry]` | `enabled = false` | Reserved: parsed and ignored. `enabled = true` produces a `Warning` that export is post-v1 (4.6) |

Any of these keys in the project layer, apart from `mcp.servers`, is dropped with a `Warning` (2.2).

## 3. Agent loop and protocol

### 3.1 Turn lifecycle
1. **Assemble** — `tautly-context` builds a `CanonicalRequest` and a per-block token breakdown.
2. **Stream** — the provider streams text, reasoning and tool calls; each is re-emitted as an event immediately.
3. **Decide** — the permission policy returns `allow | ask | deny` per tool call. `ask` emits `ToolApprovalRequested` and awaits `Approve`/`Deny`.
4. **Execute** — approved calls run via the sandbox. Calls whose `Effect` is `ReadOnly` run concurrently; `Write`/`Exec` calls run sequentially in model-emitted order.
5. **Loop** — results are appended; repeat from 1 until the model ends without tool calls, a cap (turns / cost / wall time) is reached, or the user cancels.

Every step is appended to the session log before its effects are surfaced, so any crash is resumable.

### 3.2 Events (engine → client)
Typed, serde-serializable, versioned:
`TurnStarted`, `TextDelta`, `ReasoningDelta`, `ToolCallProposed`, `ToolApprovalRequested{request_id, call_id, tool, args, reason, diff?}`, `ToolStarted`, `ToolOutputDelta`, `ToolFinished{…, blocked, diff?}`, `Usage{role, tokens_in, tokens_out, cache_read, cache_write, cost_usd, quota?}` (`role` ∈ `main | side`), `ContextStatus{used, budget, breakdown, warnings}`, `Compacted{removed_turns, summary, reason}`, `ContextCollected{elided_results, tokens_freed, reason}`, `MemoryProposed{proposal_id, change}` (`change` = an entry to upsert and/or entry ids to delete), `SessionChanged{session_id}`, `TurnReceipt{…}` (7.11.1), `ModelChanged{model, provider}` (7.11.5), `SessionDiff{turn?, files, unified, truncated}` (7.11.4), `Undone{turn, restored, deleted, dry_run, backup_ref?, note?}` (7.11.11), `McpServerApprovalRequested{request_id, server, source, launch}`, `McpServerStatus{server, state, tools, message?}` (7.11.10), `TurnFinished{reason, evidence?}` (7.11.7), `Rewound{at, branch, report}`, `Switched{branch, report}`, `Replayed{offset, t_ms, event}`, `ReplayFinished{session_id, events, cancelled}`, `UserMessage{text}` (7.12), `Error{kind, message, retryable, hint}`, `Warning{message}`.

`TurnFinished.reason` ∈ `completed | cancelled | budget | max_turns | timeout | error`. At the end of a turn `TurnReceipt` comes right before `TurnFinished`, for every reason.

`diff` is a `DiffPreview{unified, lines, truncated}` (7.11.4). New fields are optional or defaulted, so readers built for the first list keep working (3.4).

### 3.3 Commands (client → engine)
`SendMessage`, `Approve{request_id, session_rule?}`, `Deny{request_id, reason?}`, `Cancel`, `SetBudget`, `Compact`, `Uncompact`, `Pin`, `Unpin`, `ApproveMemory{proposal_id}`, `RejectMemory{proposal_id}`, `ConsolidateMemory`, `Fork{at?, label?}`, `SetModel{model}` (7.11.5), `Diff{turn?}` (7.11.4), `Undo{turn?, dry_run}` (7.11.11), `Rewind{at, label?, dry_run, overwrite}`, `Switch{session_id, dry_run, overwrite}`, `Label{label}`, `Replay{session_id?, speed, from_step?}` (7.12), `Shutdown`.

`Approve`/`Deny` answer both tool-call approvals and MCP server approvals; request ids are unique across both. `session_rule` (7.11.8) only applies to tool calls.

### 3.4 Headless protocol (`tautly-rpc`)
- JSON-RPC 2.0, newline-delimited JSON over stdio. Commands are methods; events are notifications.
- `initialize` handshake negotiates protocol version. Event and field additions are minor-version and backwards compatible; any removal or semantic change is a major-version bump.
- One-shot mode: `tautly -p "<prompt>" --output json|text` sends one message, streams events to stdout, exits with a code mapped from `TurnFinished.reason` (0 completed, 1 error, 2 budget/max_turns/timeout, 130 cancelled).
- In headless mode `ask` resolves to `deny` unless `--approve <mode>` is given (`auto-edit` or `yolo`), memory proposals are rejected unless `--approve-memory` is given, and repository-declared MCP servers that the user has not approved are not started unless `--approve-mcp` is given (7.11.10). Headless never blocks on input.
- **ACP:** `tautly acp` speaks the Agent Client Protocol to editors (7.11.9). It is an engine client like this server, not a layer on top of it.
- **Interactive clients (apps, IDE extensions):** `initialize` params carry `client: {interactive: bool, name, version}`. With `interactive: true`, `ask` emits `ToolApprovalRequested{request_id, …}` and waits for `approve`/`deny` with that `request_id`; memory proposals wait for `approve_memory`/`reject_memory`. Pending requests are denied when the client disconnects or sends `shutdown`, so nothing hangs. Without `interactive`, the headless rules above apply unchanged.
- **Several sessions per process:** `session/new {root, resume?, fork?}` returns a `session_id`; `session/list` and `session/close` manage them.
  - `fork` is `{session_id, at?, label?, worktree?, model?}`; with `worktree: true` the fork runs in an isolated git worktree (7.12.4).
  - `session/tree {root}` returns the session tree (7.12.3). Every session-scoped method takes `session_id`, and every `event` notification carries it. Sessions on the same root share one code index, file watcher and memory store (opened once, reference-counted); each session has its own context, read tracker, budget and session log. The single-session form (no `session/new`, `session_id` optional) remains valid for scripts and `tautly -p`.

### 3.5 Stream reliability
- Provider errors are classified retryable (429, 5xx, network) or fatal.
- Retries: exponential backoff with jitter, honoring `retry-after`, max 5 attempts by default.
- A stream that dies mid-response is discarded and the request re-issued from the last complete message; partial tool calls are never executed.

## 4. Providers and cost accounting

### 4.1 Trait
```rust
trait Provider: Send + Sync {
    fn capabilities(&self, model: &ModelId) -> Capabilities;
    fn stream(&self, req: CanonicalRequest, cancel: CancellationToken)
        -> BoxStream<'static, Result<ProviderEvent, ProviderError>>;
}
```
`Capabilities`: context window, max output tokens, parallel tool calls (bool), reasoning style (`none | budget | effort`), cache style (`explicit | automatic | none`), image input (bool), preferred edit format.

### 4.2 Adapters (v1)
| Adapter | Wire API | Provider-specific handling |
|---|---|---|
| `anthropic` | Messages API | `cache_control` breakpoints on system, tools, rolling conversation prefix; extended thinking budget; fine-grained tool streaming |
| `openai` | Responses API | reasoning effort; stateless requests (`store: false`) with reasoning carried across steps as encrypted reasoning items (`include: ["reasoning.encrypted_content"]`); automatic prefix caching |
| `google` | Gemini API | thinking budget; implicit/explicit context caching |
| `openai-compat` | Chat Completions | base URL configurable (OpenRouter, Ollama, vLLM, LM Studio, Groq, …); conservative default capabilities |

Adapters may place the volatile tail (6.1) inside the last tool result's content when the provider would otherwise treat it as a new user turn (earlier reasoning is then dropped); the last cache breakpoint stays on the block before it.

### 4.3 Model catalog
Built-in TOML catalog (context window, prices per token class, capabilities). User overrides in config. `tautly models update` refreshes from a models.dev-format source. Unknown models get conservative defaults and `cost_usd = null`.

### 4.4 Auth
API keys from environment variables or the OS keychain (`keyring` crate). No subscription OAuth. `tautly auth add <provider>` writes the keychain; provider detection when no model is configured is in 7.11.2.

### 4.5 Cache stability
`tautly-context` keeps the request prefix byte-stable across turns: fixed tool ordering, no timestamps or volatile data in the system prompt, pinned block after the stable prefix, working state as a tail block after the last message (6.1). Cache hit ratio is reported in `Usage`.

### 4.6 Cost, quota, budgets
- `Usage` per provider response; cost computed from catalog prices. Aggregated per turn, session and day into a local SQLite ledger (`~/.local/share/tautly/ledger.sqlite`).
- Quota: rate-limit response headers (Anthropic, OpenAI) parsed into `Usage.quota{remaining_requests, remaining_tokens, reset_at}` when present.
- `SetBudget{scope: turn|session, max_usd?, max_tokens?}`. At 80%: warning event. At 100%: the engine finishes the in-flight tool call, then ends with `TurnFinished{reason: budget}`.
- Telemetry: OpenTelemetry export is removed from v1 (user-approved 2026-11-03) and is post-v1. Nothing leaves the machine except provider, `web_fetch` and MCP requests. The `[telemetry]` table stays reserved (2.4).

## 5. Tools and edits

### 5.1 Trait
```rust
trait Tool: Send + Sync {
    fn spec(&self, caps: &Capabilities) -> ToolSpec;
    fn effect(&self, args: &serde_json::Value) -> Effect; // ReadOnly | Write{paths} | Exec{argv}
    async fn run(&self, args: serde_json::Value, sb: &dyn Sandbox, cx: ToolCx) -> ToolResult;
}
```

### 5.2 Tool set (v1)
| Tool | Behavior |
|---|---|
| `read` | Line ranges; emits hashline anchors when the active edit format is `hashline`; images for capable models; re-reads return `unchanged`/diff (7.2) unless `full: true` |
| `edit` | Per-model edit format (5.3) |
| `write` | Create/overwrite a file |
| `bash` | Runs via sandbox in its own process group; default timeout 120 s; output > 2 KB passed through output filters (7.3), else/fallback capped at 30 KB (head + tail kept); full output spilled to a temp file referenced in the result |
| `grep`, `glob` | ripgrep crates (`grep-searcher`, `ignore`); `.gitignore`-aware |
| `outline`, `symbol`, `refs` | Code-index queries (7.1) |
| `find` | One ranked search over paths, symbols and text (7.10) |
| `web_fetch` | HTML → markdown; subject to sandbox network policy; optional `question` arg answered by the side model (7.5) |
| `recall`, `remember` | Memory search and memory proposals (7.4) |
| `pin`, `state` | Context-management tools (section 6) |

### 5.3 Edit formats
- `str_replace` — exact old→new replacement. Default for Anthropic models.
- `patch` — apply-patch / unified-diff style. Default for OpenAI models.
- `hashline` — line references by short content hashes returned from `read`. Default for `openai-compat` and unknown models.
- Google models default to `str_replace` until eval data says otherwise.
- Overridable in config. Defaults are revised from `tautly eval` results.

### 5.4 Edit safety pipeline
1. **Stale-read guard** — reject if the file's mtime or content hash differs from the model's last `read`; the model must re-read.
2. **Matching** — exact match first; whitespace-normalized fallback only when it yields exactly one match. Zero or multiple matches fail with candidate locations.
3. **Atomic write** — temp file in same directory → fsync → rename. Preserve line endings, encoding, trailing newline, file mode.
4. **Syntax check** — tree-sitter parse for supported languages (initially Rust, TypeScript/JavaScript, Python, Go, JSON, TOML, YAML). A newly introduced parse error is reported in the tool result; the edit stays applied.
5. **Undo** — pre-image recorded in the session log; `tautly undo [--turn N]` reverts. In a git work tree, checkpoints taken before `bash` extend undo to command side effects (7.11.11).

### 5.5 Permission policy
Rules are evaluated in `tautly-core`; OS-level enforcement is sub-project 2.
- Modes: `ask` (default), `auto-edit` (workspace file writes allowed; exec still asks), `yolo`.
- TOML rules, e.g. `[permissions] allow = ["bash(cargo test*)"]  deny = ["bash(rm -rf*)"]`. Deny beats allow. MCP tools are matched by name, without parentheses: `mcp__github__create_issue`, `mcp__github__*` (7.11.10).
- Session rules: an approval answered "allow for this session" adds an in-memory allow rule (7.11.8). It is never persisted, not even across resume. Deny rules and protected paths still win over it.
- **Protected paths always ask, in every mode:** `.git/`, `.tautly/`, git hook paths, `.github/workflows/`, `.vscode/`, `.idea/`, `.envrc`, `Makefile`, `package.json`, `Cargo.toml` `build.rs`, shell rc files. Defends against "agent writes a file a trusted host tool later executes."
- **Command parsing:** shell input is parsed (pipes, `&&`, `||`, `;`, subshells, command substitution) into individual commands; every component must pass the rules. Unparseable input resolves to `ask`. No name-based "safe command" shortcuts.

## 6. Context and sessions

### 6.1 Block model
Requests are assembled in fixed order:
```
[stable]   system prompt · tool schemas · AGENTS.md · memory index (7.4) · repo map (7.10)
[pinned]   constraints and facts that survive compaction
[history]  recent turns, verbatim
[state]    structured working state — a tail block after the last message, rendered fresh for each request and never stored in history
```
- Adapters whose wire API requires alternating roles merge consecutive same-role messages (e.g. tool results followed by the state tail).
- AGENTS.md is read through the sandbox resolver (regular files only; in-root symlinks are accepted), capped at 32 KB per file and 64 KB in total, with a visible truncation marker and a status warning. A file above the sandbox root is accepted only when its link target stays inside the directory holding it.
- Cache stability: the working state changes on almost every tool call (the files list follows effects), so it sits after history; the cached prefix (stable · pinned · history up to the last cache breakpoint) survives state changes. Pinned items change rarely and stay before history.
- The sandbox root is the git top level when the start directory is inside a git work tree, else the start directory. AGENTS.md links resolving to protected paths (5.x) are refused with a warning; ancestor files above the root must also be owned by the user and not group- or world-writable.
- `ContextStatus{used, budget, breakdown}` is emitted every turn.
- `tautly prompt show` prints the system prompt; `tautly prompt show --full` prints the exact last request.
- System prompt target: < 2,000 tokens.

### 6.2 Pinned items
Sources: user (`/pin "…"` or `Pin` command), config, the `pin` tool, and AGENTS.md sections headed `Constraints` or `Rules` (auto-pinned). Pinned items are never summarized or dropped. Budget warning if pinned exceeds 10% of the window. Pin text is capped at 2 KB, and at most 20 session pins are held; AGENTS.md pins are capped at 2 KB each and 4 KB in total. Pins render with their source, and only user, config and AGENTS.md pins are authoritative. Every pin added mid-session (by the model or the user's `/pin`) renders in the state tail until the next cache epoch (7.7, including a cold cache), then folds into the pinned block, so it does not break the cached prefix.

### 6.3 Working state
Structured record: task, plan steps with status, files read/edited with content hashes, decisions, open questions, failed attempts (7.8). The files list is maintained by the engine from actual tool effects; plan, decisions and questions are updated by the model through the `state` tool. The rendered files list is capped (~40 entries / 600 tokens): edited, deleted and maybe-stale files first (hashes only for edited files), then the most recent reads, then "+N more files read earlier (re-read to see them)".

### 6.4 Compaction
- Trigger: automatic at a configurable threshold (default 80% of context window; can be disabled), or `/compact`. Always announced via event before running.
- Effect: oldest history turns replaced by a summary generated by the active model; pinned and state blocks preserved verbatim; files list lets the model re-read selectively.
- Compaction forgets read baselines by path for every path read or written in the removed turns (the counterpart of the 7.2 rule for GC), so no `unchanged since turn N` answer refers to content the model can no longer see.
- The summary is capped (≤ 4k tokens) and must be smaller than what it replaces; otherwise the turns are dropped without a summary. The summary attributes requirements only to user turns.
- Reversible: originals remain in the session log; `Uncompact` restores them if they fit the window.
- Context overflow from the provider triggers one compaction and one retry.
- Compaction summaries are generated by the side model (7.5).

### 6.5 Session store
- Append-only JSONL per session: `~/.local/share/tautly/sessions/<project-id>/<session-id>.jsonl`. Large blobs (file contents, tool outputs) stored content-addressed in a sibling `blobs/` directory and referenced by hash.
- `<project-id>` is the blake3 of the canonical git common directory when the root is in a git work tree, else of the canonical root. All worktrees of one repository therefore share session listings and blobs. The code index stays keyed by root, because worktree contents differ.
- Records: commands, canonical requests, provider events, tool calls/results, edit pre-images, compactions, model switches (`ModelSet`), checkpoints (`Checkpoint`, `CheckpointSkipped`) and per-turn savings (7.11).
- Durability: fsync at turn and tool boundaries. On start, a session ending mid-turn is detected and resumable from the last complete step.
- Operations: `tautly --continue`, `tautly --resume <id>`, `/fork` (new log referencing parent at an event offset), `tautly sessions list|show|export|delete` (`delete` also removes the session's checkpoint refs, 7.11.11).
- Format documented and versioned (`schema_version` in the header record).
- **Time-travel hooks** (user-approved 2026-11-03, schema 1, before release). Each one captures data that can never be recovered later:
  - **H1, timestamp:** every record line carries `"t": <unix ms>` in its envelope.
  - **H2, root-relative paths:** paths in records are relative to the header's `root`. On read they are re-confined under the current root: absolute paths, `..` and NUL are refused. Events keep showing paths as before.
  - **H3, header fields:** `project`, `label?`, `origin` (`user | fork | rewind | what_if | eval | import`), `git?: {head?, dirty?}`, `tautly_version`, `model?`, `config_hash?` (blake3 of the effective config), `tags`.
    - `git.head` is read from the `.git` files without spawning git, so start-up stays fast. `dirty` is unknown in the header.
    - The session-start `Checkpoint` record (7.11.11) carries the authoritative head, dirty state and tree.
    - A later `Labeled{label}` record renames the session (the last one wins).
  - **H5, prefix inputs:** `PrefixApplied{agents_md?, repo_map?, system_prompt_hash, tools_hash}` is written before the first request, and again before any request whose stable-prefix inputs changed (for example MCP tools joining, a model switch changing the edit format).
    - `agents_md` and `repo_map` are blobs of the exact text sent.
    - Resume, rewind and replay use the logged text and never regenerate it.
    - If the current binary's system prompt or tool schemas hash differently, resume warns that the cache will be cold.
  - **H6, richer `Request`:** `Request{hash, turn, step, model, blocks: {stable, pinned, history, tail}}`. `turn` is the user turn, `step` the absolute provider step, and `blocks` the blake3 of each block.
  - **H7, edits and undo are logged and reversible:**
    - `Edit` and `PersistedEffect::Edited` carry `post_image_blob`, the written content (deduped).
    - Undo appends `Undone{turn, files: [{path, from_blob?, to?}], backup_ref?}` after storing each file's current content (`from_blob`), so undone states can be restored.
  - **H8, approval requests:** `ApprovalRequested{request_id, kind: tool|mcp_server|memory, call_id?, reason}` is logged before its event is emitted.
  - **H9, point-in-time rebuild:** `rebuild(reader, until)` cuts at any step boundary and synthesizes `cancelled` results for open calls. `Fork{at?, label?}` branches from any step.
  - **H10, project id:** sessions and blobs are keyed by `<project-id>` (above). The header's `root` names the worktree.
  - **Checkpoint record:** `Checkpoint{n, commit, tree, head?, dirty, turn, step}`, plus `CheckpointSkipped{reason, turn, step}`. The single checkpoint mechanism is the git one of 7.11.11.

## 7. Token efficiency and memory

### 7.1 Code index (`tautly-index`)
- tree-sitter extraction of symbols (functions, types, methods, constants: name, kind, parent, signature, line range, content hash) and name-based references, for the syntax-check language set (5.4).
- SQLite at `~/.local/share/tautly/index/<project-hash>.db`: `files(path, hash, mtime, size, lang)`, `symbols(file, name, kind, parent, start_line, end_line, signature, hash)`, `refs(file, line, name)`. WAL mode.
- Incremental: on engine start a background task re-parses files whose size or mtime changed; it never blocks start-up. Each successful edit re-indexes that file synchronously before the tool result is returned.
- Reads files only through `Sandbox`; honors `.gitignore`.
- Until the initial build completes, `outline`/`symbol`/`refs` fall back to tree-sitter on demand (outline) or grep (symbol, refs) and say so in the result.
- Tools: `outline(path)` → symbols with signatures and line ranges, no bodies; `symbol(name, kind?)` → source of the definition with location (several matches → list of locations, no bodies); `refs(name)` → locations with one line of context. `refs` is name-based, and its description says so.
- Targets: initial index of a 10,000-file repository < 10 s; single-file reindex < 50 ms.

### 7.2 Diff-on-re-read and context GC
- The read tracker stores the last content returned per path (blob reference). A re-read of the same range returns `unchanged since turn N` if identical; if fewer than 50% of lines changed it returns a line-numbered unified diff; otherwise the full content. `full: true` forces full content.
- A tool result is *superseded* when: it is a `read` of a path later edited or re-read; it is a `bash` result of a command string later re-run; it is a failed `edit` on a path later edited successfully.
- Superseded results are replaced by `[elided: superseded at turn N]`, keeping the tool-call id and pairing. A read that answered `unchanged`/a diff never supersedes its base, and eliding any read result of a path forgets that path's re-read baselines.
- Granularity: a context `Turn` is one provider step (the assistant message plus its tool results; the user's prompt is its own turn). `max_turns` counts steps; `TurnStarted`/`TurnFinished` stay per user message.
- Collection runs only at step start (before each provider request), scheduled by the cache-aware scheduler (7.7); without catalog prices it applies when elidable tokens ≥ max(2,000, 5% of the context window), so the cache prefix breaks at most once per batch. Emits `ContextCollected`; elisions are logged in the session and restored by `Uncompact`.

### 7.3 Output filters
- Parsers for: `cargo test|build|check|clippy`, `pytest`, `jest`/`vitest`, `go test`, `tsc`, `eslint`. Detected from the parsed command (5.5 parser).
- Applied when raw output exceeds 2 KB. Result: counts (passed/failed/errors/warnings) plus each failure or diagnostic (name, `file:line`, message, ≤ 20 lines each), plus the path of the spilled full output.
- Parse failure falls back to the 30 KB head + tail cap. `raw: true` disables filtering.

### 7.4 Memory (`tautly-memory`)
- **Project memory:** one markdown file per entry in `.tautly/memory/<id>.md` (shareable via git). Frontmatter: `id`, `title`, `kind` (`convention | fact | gotcha | command`), `anchors: [{path, symbol?, hash}]`, `created`, `verified`. Body ≤ 300 tokens.
- **Obsidian-compatible format (user-approved 2026-11-03):** `.tautly/memory/` opens cleanly as an Obsidian vault. Frontmatter is plain YAML that Obsidian shows as properties (`id`, `title`, `kind`, `tags: [..]`, `anchors`, `created`/`verified` as ISO dates). File names are readable (`<title-slug>.md`, `-2`, `-3`… on collision); the stable `id` lives in the frontmatter, so renaming a file never breaks a link. Bodies may link other entries with `[[title]]` or `[[title|label]]`; links are parsed, `recall` adds directly linked entries to its hits within the same 2,000-token cap, and links to unknown entries are kept as written. Tautly never writes Obsidian's own `.obsidian/` folder.
- **Memory index:** one line per entry (`- [kind] title` plus ` [stale]` when applicable), capped at 500 tokens (oldest unverified entries dropped from the index first, never deleted), in the stable block after AGENTS.md.
- **Staleness:** at session start and after each edit, every anchor hash is compared with the code index (symbol hash when `symbol` is set, file hash otherwise). Mismatch marks the entry stale (the memory index in the stable block changes only at a cache epoch (7.7), when the cache is cold, or at the start of a user turn with a cold cache; between epochs the engine holds the pending index text); `recall` returns stale entries with a warning; an entry whose anchors are re-confirmed by the model through `remember` gets `verified` updated.
- **Episodic memory:** `~/.local/share/tautly/memory/<project-hash>/episodes/<session-id>.md` (private, never shared). Written by the side model at session end and at each compaction: task, outcome, what failed, key files. No approval needed.
- **`recall(query)`:** BM25 over titles and bodies of project entries and episodes; returns the top 5, capped at 2,000 tokens.
- **`remember(entry)`:** proposes a new or updated entry; the engine emits `MemoryProposed` and writes the file only after `ApproveMemory`. The system prompt instructs the model to call `remember` when the user corrects it.
- **Consolidation:** `tautly memory consolidate`, and offered automatically when the full index would exceed 500 tokens. The side model proposes merges and deletions, shown as a diff; applied only on approval.

### 7.5 Side-model routing
- Config `[routing] side_model = "<model id>"`. Default: the catalog model with `tier = "small"` from the main model's provider; if none, the main model.
- Used for: compaction summaries, episodes, memory consolidation, and `web_fetch` with a `question` (returns the answer, ≤ 1,000 tokens, plus the source URL).
- Side-model usage is reported with `Usage.role = side` and counted in budgets and the ledger. It is priced with the side model's catalog price and recorded under the side model's id.

### 7.7 Cache-aware scheduling
- The catalog gains `cache_ttl_s: Option<u32>` per model (default 300 for `explicit`/`automatic` cache styles, `None` for `none`).
- `tautly-context` tracks the time of the last provider request and the token length of the cached prefix (up to the last cache breakpoint).
- Every history mutation (context GC 7.2, threshold compaction 6.4) goes through one scheduler that decides at step start (before each provider request):
  - **Cold cache** (time since last request ≥ TTL): apply all pending mutations; nothing cached is lost.
  - **Warm cache:** apply only when `saved > cost`, where `saved = tokens_freed × input_price × 5` (5 = expected further requests in the turn horizon) and `cost = invalidated_prefix_tokens × (cache_write_price − cache_read_price)`. Without catalog prices, fall back to the 7.2 threshold rule.
  - **Forced:** always apply when context use ≥ 90% of the window. Threshold compaction may therefore be deferred from 80% up to 90% while the cache is warm and the cost model says it is not yet worth it — never beyond 90%.
- All mutations decided at one step start are applied together as one "cache epoch", so the prefix breaks at most once. An epoch (including a cold-cache step) also folds session pins into `[pinned]` (6.2) and applies a pending memory index (7.4).
- `ContextCollected` and `Compacted` gain `reason: cold_cache | cost_benefit | forced | manual`.

### 7.8 Attempt ledger and loop breaker
- Deterministic, no model calls. The engine records each failed tool call: tool name, normalized arguments (bash: the command string with whitespace collapsed; edit/write: path + arguments hash), the turn, a failure signature (bash: exit code + blake3 of the filtered failure summary; edit: the match-error kind), and the workspace write-generation counter at that moment (incremented on every successful write effect).
- **Guard:** before executing a call, if an identical call (same tool + normalized args) previously failed and the write-generation has not changed since, the call is not executed. The tool result is an error: `already tried at turn N with identical inputs and no file has changed since; previous result: <first 5 lines>`. If the model repeats the same call immediately after being blocked, it runs (escape hatch for flaky commands).
- `ToolFinished` gains `blocked: bool`.
- Working state (6.3) gains `failed_attempts`: the 10 most recent distinct failures, one line each, capped at 300 tokens; it survives compaction like the rest of working state.

### 7.9 Tolerant edit applier
- Runs only after the exact match (5.4 step 2) fails, for `str_replace` and for `patch` hunk context. `hashline` stale anchors are never repaired.
- Stages, in order; each stage succeeds only with exactly one candidate (otherwise the next stage runs; ambiguity at any stage is an error):
  1. **Normalize the model's text:** CRLF → LF, typographic quotes/dashes → ASCII, NBSP → space, trailing whitespace stripped per line, and accidental `read` artifacts removed (leading `<n>\t` line numbers, `<n>:<hash>|` hashline anchors). Retry exact match. If `new_text` contains such artifacts on some but not all lines, the edit is rejected (never written).
  2. **Whitespace-insensitive match** (the existing 5.4 fallback).
  3. **Indentation-relative match:** compare with common leading indentation removed; re-indent the replacement by the offset found.
  4. **Anchor match:** first and last lines of the old text each match uniquely, the interior has the same number of lines as the old text's interior, and line-similarity ≥ 0.9.
  5. **Structural fallback** (`str_replace` only): if the old text starts with a definition the code index can name (7.1), locate that symbol in the file; replace the symbol's range when the old text's similarity to the symbol source ≥ 0.8. The post-edit syntax check (5.4 step 4) must report no new errors, otherwise the edit is reverted and fails.
- A repaired edit's result says which stage applied (`applied via indentation-relative match`). Repaired (non-exact) edits never record a whole-file re-read baseline. Stages 2–5 may only match inside content the model has seen at the current file hash (whole-file baseline hash equals the current hash, or a range baseline covering the matched lines matches the current lines); otherwise the edit fails as stale.
- When every stage fails, the error shows the single nearest window of real lines (≤ 20, line-numbered) so the model can retry without re-reading the file.
- `tautly eval` reports edit repairs per stage alongside the edit-failure rate.

### 7.10 Workspace engine (fast file access, fewer lookup steps)
- **Live workspace** (`tautly-index`): an in-memory file tree (path, size, mtime, content hash; ~100 bytes per file) loaded from the index database at start-up, reconciled by the background refresh, and kept current by a file-system watcher (`notify`, 50 ms debounce) that re-indexes only changed files. Edits made by Tautly tools update it synchronously. `glob` and directory listings are answered from memory. The tree honors `.gitignore` and the same size/binary skips as the index.
- **Trigram text index:** stored on disk in the index database (`trigrams(tri, file_id)`), not in memory, so the idle-RSS target (1.6) holds. `grep` extracts the required literals from the pattern (`regex-syntax`), intersects the postings of their trigrams to get candidate files, then confirms with the real regex. Patterns without a literal of ≥ 3 bytes, and any index error, fall back to the full scan (5.2). The index is trusted only when fresh: watcher alive, no pending post-bash freshness check, and writes guarded by row generations; otherwise tools use the walker. Target: a selective search on a 100,000-file repository < 50 ms.
- **`find(query, kind?, limit = 20)`:** one call searches file paths (fuzzy), symbol names (index) and text (trigram) concurrently and returns one ranked list — exact symbol definitions, then path matches, then text matches — as `path:line  <kind>  <one-line snippet>`. `kind` ∈ `symbol | path | text`.
- **Repo map:** a generated block of ≤ 400 tokens — top-level directories with file counts, languages, entry points (e.g. `main.rs`, `lib.rs`, workspace members, `package.json` scripts) and the most-referenced symbols — placed in the stable block after the memory index (6.1). Generated once at session start (never mid-session, to keep the cache prefix stable).
- **Batched reads:** `read` also accepts `targets: [{path, start_line?, end_line?} | {symbol}]` (≤ 10 per call); results are concatenated with headers and share the 30 KB cap. Each target goes through the same stale-tracking and diff-on-re-read rules (7.2).

### 7.6 Measurement
- `tautly eval` reports tokens per solved task (total input + output tokens across main and side roles, divided by solved tasks) and cache hit ratio (`cache_read / (input + cache_read)`).

### 7.11 Daily-driver experience and adoption (user-approved 2026-11-03)
This section covers what a user sees in the first ten minutes and in every turn. Rationale: the research notes of 2026-11-03 (opaque cost is the top reason users switch tools; undo and diffs build trust; MCP, ACP and `CLAUDE.md` are expected at launch).

#### 7.11.1 Turn receipts and the savings meter
- At the end of every turn the engine emits `TurnReceipt{turn, model, tokens_in, tokens_out, cache_read, cache_write, side_tokens, cost_usd?, wall_ms, cache_hit_ratio?, steps, tool_calls, saved}`. `turn` is the 1-based user-turn number. Token fields sum the main and side roles; `side_tokens` is the side share (input + output). `cache_hit_ratio = cache_read / (tokens_in + cache_read)`, and it is `None` when both are 0. `wall_ms` runs from `TurnStarted` to `TurnFinished`.
- `saved` is `Savings{index, diff_reads, context_gc, filters, cache_read_tokens, cache_usd?, total_usd?}`. Every number is a counterfactual **estimate** against a plain harness that has the same 30 KB output cap and no Tautly mechanism. Clients must label them "est.". Tokens are bytes / 4.
  - `index`: for each `outline`, `symbol` and symbol `read` target, the bytes of the whole file(s) the answer came from (each capped at 30 KB) minus the bytes returned. `refs`, `find` and the repo map are not estimated (0): they have no reliable counterfactual.
  - `diff_reads`: for each re-read answered `unchanged` or with a diff (7.2), the bytes a full read of that range would have returned (capped at 30 KB) minus the bytes returned.
  - `context_gc`: for each provider request in the turn, the tokens elided by GC (7.2) that are still out of the context at that request, summed over those requests.
  - `filters`: for each filtered `bash` result (7.3), `min(raw bytes, 30 KB)` minus the bytes returned. Floored at 0.
  - `cache_read_tokens` = the turn's `cache_read`. `cache_usd = cache_read × (input_price − cache_read_price) − cache_write × (cache_write_price − input_price)`, which can be negative.
  - `total_usd` = the four token estimates priced at the main model's input price, except `context_gc` at the cache-read price (elided tokens would mostly have been cached), plus `cache_usd`. It is `None` without catalog prices.
- Tools report their part of the estimate in `ToolMeta.saved: Option<Saved{kind, bytes}>`. The engine adds them up and records them in the session log.
- Every receipt is stored in the ledger (4.6) in a `receipts` table: session, project id (6.5), root, user turn, main model, time, plus every field above.
- `tautly stats [--since 7d|30d|all] [--by day|model|project] [--json]` (default `--since 30d --by day`). Tokens and cost come from the ledger's usage rows; savings, wall time and turn counts come from receipt rows. `--by project` joins usage rows to the project through their session.
- `tautly receipt [--last | --session ID] [--markdown]` summarises one session: the most recent session of the current project by default (`--last` says so explicitly), or `ID`. It shows turns, tool calls, wall time, tokens, cache hit ratio, cost, the savings estimates and the last turn's evidence (7.11.7). `--markdown` prints a pasteable table. A receipt never contains paths, prompts, code or the project name.

#### 7.11.2 Zero-config first run, `auth add`, `doctor`
- **Provider detection** runs only when no layer or flag names a model, `providers.auto_detect` is on and no `--mock-script` is given. The first match wins:
  1. Environment variables, in order `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`.
  2. Keychain entries, same order (service `tautly`, accounts `anthropic`, `openai`, `google`).
  3. An OpenRouter key: `OPENROUTER_API_KEY`, then keychain account `compat:openrouter`.
  4. A local Ollama: `GET http://localhost:11434/api/tags` with a 300 ms timeout.

  Built-in compat entries `openrouter` (`https://openrouter.ai/api/v1`) and `ollama` (`http://localhost:11434/v1`) exist unless the user config defines entries with those names.
- **Default model per provider:** among the provider's priced catalog models whose `tier` is not `small` and whose context window is ≥ 128,000, take the lowest input price (ties: lower output price, then id). The built-in catalog carries OpenRouter entries for the three native providers' main-tier models, at OpenRouter prices. Ollama: the first installed model that has a `compat:ollama/…` catalog entry, in catalog order; otherwise the alphabetically first installed model, with conservative capabilities and a `Warning`. Ollama with no installed models counts as not detected.
- The CLI prints one line to stderr: `using <model> (from <source>) · /model to switch · tautly doctor to check`. Nothing detected: exit 1 with a hint naming the variables, `tautly auth add <provider>` and Ollama. A configured model whose provider has no key fails the same way, with the same hint.
- **`tautly auth add <provider>`**, where provider ∈ `anthropic | openai | google | openrouter | compat:<name>`. With a TTY it prompts with hidden input; otherwise it reads one line from stdin. It trims the value, refuses a blank one, writes keychain service `tautly` with account = provider, reads it back to verify, and never prints the key. With no keychain available it exits 1 with a hint to use the environment variable.
- **`tautly doctor [--json] [--offline]`** prints one line per check: `ok | warn | fail | info  <check>: <detail>`.
  - `config`: loads; its warnings are warn.
  - `model`: resolved model and its source; none is fail.
  - `keys`: per provider, env / keychain / none, never the value.
  - `keychain`: available, or the reason it is not (warn).
  - `network`: for each provider with a key, `GET` its model-list endpoint (no tokens spent; key in a header, never in the URL) with a 3 s timeout. 2xx is ok; 401/403 is fail "key rejected"; timeout or other errors are fail for the active model's provider and warn otherwise. Skipped with `--offline`.
  - `data home`: absolute and writable, else fail.
  - `sandbox`: root resolution, plus an info line "no OS isolation in v1".
  - `git`: binary and version; missing is warn (checkpoints off).
  - `index`: present, ready or building, file count.
  - `mcp`: per server, state; a pending repository approval is warn.

  Exit code 1 if any check fails, else 0.

#### 7.11.3 Release and installers
- cargo-dist builds and publishes GitHub releases on `v*` tags for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`. Linux binaries are static (musl). Each release has sha256 checksums, a `curl | sh` installer, a Homebrew formula pushed to the project's tap, and binaries laid out so `cargo binstall tautly` finds them.
- The Linux keychain backend must link statically: `keyring` with the pure-Rust Secret Service backend (zbus). If that cannot link on musl, the Linux targets fall back to `-gnu` and the decision is recorded in the release notes.
- macOS code signing and notarisation are post-v1. Neither curl nor Homebrew sets the quarantine flag.

#### 7.11.4 Diffs in approvals, results and `/diff`
- `Tool` gains a read-only `preview`, implemented by `edit` (every edit format, `patch` included) and `write`. It runs the same pipeline as `run`, including the stale guard and tolerant matching (7.9), without writing, and returns the unified diff it would apply plus the base content hash of each file.
- When the policy says `ask`, the engine calls `preview` first:
  - Success: `ToolApprovalRequested.diff` carries the diff.
  - Error: the call is answered with that error without asking, since nothing would be written.
  - After approval, `run` refuses with `stale: file changed after approval` if a file's hash differs from the preview base. So the approved diff is the applied diff.
- `ToolFinished.diff` carries the diff actually applied (from pre-image and written content) for every successful `edit`, `write` and `patch`, whether or not it was asked.
- `DiffPreview{unified, lines, truncated}`:
  - Format: unified with 3 context lines, `--- a/<path>` / `+++ b/<path>` relative to the root (`/dev/null` for created or deleted files); several files are concatenated.
  - `lines` = lines in the full diff.
  - Cap: 200 lines and 16 KB. Beyond the cap the text ends with `… N more lines (diff truncated)` and `truncated: true`.
  - Cost: one diff gets about 300 ms of line matching; past that a file shows `File <path>: diff too large or slow to show, summary only: +A -R lines` instead of hunks. Changes over 64 KB are diffed off the async workers. A headless `ask` previews without a diff (nobody sees it).
  - Non-UTF-8 content shows `Binary file <path> differs`.
- `Command::Diff{turn?}` answers with `SessionDiff{turn?, files, unified, truncated}`. `None` = the whole session; `n` = user turn `n`. It shows the net change per file that Tautly's tools edited, created or deleted in that scope: the earliest pre-image in scope against the current disk content. `files` = `[{path, status: added|modified|deleted, added, removed}]`. Cap: 2,000 lines and 256 KiB (so a few very long lines cannot make a multi-megabyte event), with the same marker. Changes made by commands are not included (see 7.11.11 for undo).

#### 7.11.5 Mid-session model switch
- `Command::SetModel{model}` takes effect at once when idle; during a turn it is queued for the start of the next user turn. An unknown model, or a provider without a key, gives `Error{kind: provider_fatal, hint}`, and the model is unchanged. On success the engine logs a `ModelSet{model}` session record (resume uses it) and emits `ModelChanged{model, provider}`. `provider` ∈ `anthropic | openai | google | compat:<name>`.
- Recomputed for the new model:
  - capabilities and the context budget;
  - the edit format: the new model's default unless config overrides it (prompt caches are per model, so the schema change costs nothing extra);
  - the side model: the new provider's small tier unless `routing.side_model` is set.
- Budgets carry over. An unpriced model with `max_usd` set warns and enforces token budgets only (Task 25 rule).
- Cache: the first request after a switch is treated as a cold-cache epoch (7.7): pending GC, compaction, pin folds and the memory index apply then, at no extra cost. If context use exceeds 90% of the new window, forced compaction runs at that step start.
- Reasoning:
  - `Reasoning`/`RedactedReasoning` items from a different provider are stripped before the next request.
  - Same-provider switches keep them. If the provider then rejects the request as invalid and the error names thinking, reasoning or signature, the engine strips the reasoning and retries once.

#### 7.11.6 `CLAUDE.md` fallback
At each directory level where `AGENTS.md` is read (root and ancestors, 6.1), a directory whose `AGENTS.md` contributes nothing (absent, empty, dangling or refused) contributes its `CLAUDE.md`, if present, with a warning naming the skipped `AGENTS.md`. It gets the same resolver, confinement, ownership and mode checks, the same caps (32 KB per file, 64 KB total, shared with AGENTS.md) and the same auto-pinning of `Constraints`/`Rules` sections. Where `AGENTS.md` contributes, only it is read. Each file in the block starts with a header line naming it (`[from AGENTS.md]`, `[from CLAUDE.md]`), counted against the caps; pins from `CLAUDE.md` carry the source `CLAUDE.md`. `@path` import lines are kept as text, never followed.

#### 7.11.7 Evidence footer
- A `bash` call is a **test run** when any component of its parsed command (5.5 parser) matches a test pattern and carries none of `--no-run`, `--no-execute`, `--collect-only`. The `counts` of a run whose exit code is non-zero are dropped when the output filter finds no failure in it or a crash it does not show. The patterns use the permission-rule glob syntax. Defaults:
  - `cargo test*`, `cargo nextest run*`
  - `npm test*`, `npm run test*`, `pnpm test*`, `pnpm run test*`, `yarn test*`, `bun test*`
  - `pytest*`, `python -m pytest*`, `python3 -m pytest*`, `uv run pytest*`
  - `go test*`, `jest*`, `npx jest*`, `vitest*`, `npx vitest*`
  - `make test*`, `mvn test*`, `gradle test*`, `./gradlew test*`
  - `rspec*`, `bundle exec rspec*`, `mix test*`, `dotnet test*`, `ctest*`

  `[evidence] test_commands` adds patterns, and `replace_defaults = true` replaces the defaults. Trusted layers only (2.4).
- `TurnFinished.evidence` is a `TestEvidence` for the last test run of the turn, or `None` when the turn ran none. Fields:
  - `command` (whitespace collapsed, ≤ 200 chars);
  - `exit_code`;
  - `passed`/`failed` from the output filter's counts, when parsed;
  - `step`;
  - `write_gen`: the write generation (7.8) right after the run;
  - `stale`, `edits_since`, `execs_since`.

  `stale` is true when a later step of the turn applied an `Edited`/`Deleted` effect or a later `Exec{may_write: true}` that was not itself a test run. The counts say which. The REPL renders `last test: <command> · exit <code> · fresh|stale (N edits since)`. When the turn edited files without a test run, it renders `no test run this turn`. No model claims are parsed (that is Trust).

#### 7.11.8 Daily-driver REPL (`tautly-tui`)
- **Input:** line editing and persistent history (`reedline`; `<data home>/repl_history`, 1,000 entries; approvals and secrets are never stored). Alt+Enter or bracketed paste gives multi-line input. With no TTY or `NO_COLOR` set: plain text, no colour.
- **Output:**
  - Assistant text streams as it arrives. Reasoning shows as one dim `thinking…` line per step.
  - Each tool call is one compact row: `<status> <tool> <target> <duration>[ · <note>]`.
    - `status` ∈ `ok | err | denied | blocked | cancelled`.
    - `target`: path, command or pattern, truncated to 60 columns.
    - `note`: exit code, `+a -r` diff stat, or filter saving.
  - Diffs are coloured (green additions, red removals, dim context).
  - After each turn a footer: `turn N · S steps · 12.4s · ctx <used>/<budget> tok · cache 86% · $0.0712 (session $0.4120) · saved ~41k tok (est.)`, then the evidence line (7.11.7). `[receipts] footer = false` hides it.
- **Approvals:**
  - The prompt shows tool, target and the diff preview, then `allow? [y]es [n]o [a]lways this session (<rule>)`.
  - `y` approves once; `n` denies.
  - `a` approves and sends `session_rule` with the exact-target rule:
    - `edit(<path>)`, `write(<path>)`
    - `bash(<command>)`, whitespace-collapsed, with glob metacharacters escaped
    - `web_fetch(<host>)`
    - `mcp__<server>__<tool>`
  - `a <rule>` sends the user's own pattern instead. The engine accepts a session rule only if it matches the pending call; otherwise it approves once and warns.
  - Session rules last until the process exits. They are never persisted, and deny rules and protected paths still win (5.5).
  - Memory proposals and MCP server approvals use `y`/`n`.
- **Slash commands:**
  - `/model [id]`: no id lists the current model and the models whose provider has a key.
  - `/cost`: turn and session receipt.
  - `/diff [N]`.
  - `/undo [N]`: shows the dry run, asks y/n, then undoes (7.11.11).
  - `/resume [n|id]`: no argument lists the project's last 10 sessions (id, age, first prompt line, cost); the REPL rebuilds the engine with `resume`.
  - `/compact`, `/pin <text>`, `/unpin <text>`, `/fork`, `/memory consolidate`, `/help`.
  - Unknown commands print the help.
- **Keys:**
  - Ctrl+C during a turn cancels it (8). A second Ctrl+C within 2 s exits.
  - At the prompt, Ctrl+C clears the line and Ctrl+D on an empty line exits.

#### 7.11.9 ACP bridge (`tautly acp`)
- Speaks the Agent Client Protocol over stdio: wire `protocolVersion` 1, schema v1.29.1 (agentclientprotocol.com; `agent-client-protocol` crate 1.15.1, checked 2026-11-03). It drives `Engine`s directly, like the RPC server, and shares one `Workspace` per root (3.4). `initialize` advertises `loadSession: true`, embedded-context prompts, and images when the default model takes them. `authMethods` is empty: keys come from env or keychain, and a missing key fails `session/new` with the 7.11.2 hint.
- **Sessions and prompts:**

  | ACP | Engine |
  |---|---|
  | `session/new {cwd, mcpServers}` | Engine at the git top level of `cwd`. Client-supplied MCP servers count as user-trusted: they come from the user's editor config |
  | `session/load` | `resume`, replaying user and agent messages and final tool calls as `session/update` |
  | `session/prompt` | `SendMessage`. Text blocks are joined; resource blocks are inlined as fenced text with their URI; images pass when the model takes them |
  | `session/prompt` starting with `/compact`, `/undo`, `/diff`, `/cost` or `/pin` | The matching command. These are advertised via `available_commands_update` |
  | `session/set_config_option {configId: "model"}` | `SetModel`. The `model` option (category `model`) lists models with keys |

- **Stop reasons:** `completed` → `end_turn`, `max_turns` → `max_turn_requests`, `cancelled` → `cancelled`. `budget` and `timeout` → `end_turn` preceded by a notice. `error` → a JSON-RPC error.
- **Streaming:**
  - `TextDelta` → `agent_message_chunk`. `ReasoningDelta` → `agent_thought_chunk`.
  - `ToolCallProposed` → `tool_call` (`pending`). `kind`:
    - `read`: `read`, `outline`, `symbol`
    - `search`: `grep`, `glob`, `find`, `refs`, `recall`
    - `edit`: `edit`, `write`
    - `execute`: `bash`
    - `fetch`: `web_fetch`
    - `think`: `pin`, `state`, `remember`
    - `other`: MCP tools

    The title is `<tool>: <target>`; `locations` come from path arguments.
  - `ToolStarted` → `tool_call_update` `in_progress`.
  - `ToolFinished` → `completed`/`failed`, with text content (≤ 8 KB) and a `diff` content `{path, oldText, newText}` per edited file when both texts are ≤ 256 KB (else the unified `DiffPreview` as text).
  - `Usage`/`ContextStatus` → `usage_update {used, size, cost{amount, currency: "USD"}}`.
  - `TurnReceipt` and evidence → a `notice` when the client supports `session.notices`.
- **Permissions** go through `session/request_permission`:
  - `ToolApprovalRequested` → options `allow_once`, `allow_always` (= the exact-target `session_rule`, 7.11.8) and `reject_once`; the preview goes in as `diff` content.
  - `McpServerApprovalRequested` and `MemoryProposed` → `allow_once`/`reject_once`.
  - Outcome `cancelled` → `Deny`.
- `session/cancel` → `Cancel`. The bridge answers outstanding permission requests as denied and responds to the prompt with `cancelled`.
- Client `fs/*` and `terminal/*` methods are not used in v1. Tautly reads the disk through its sandbox, so unsaved editor buffers are not seen.

#### 7.11.10 MCP client, tools only (`tautly-mcp`)
- **Implementation:**
  - Built on the official Rust SDK `rmcp` (3.8, checked 2026-11-03). It speaks MCP revision 2026-07-28 (stateless, `server/discover`) and falls back to the `initialize` handshake of 2025-11-25 and 2025-06-18 servers.
  - Transports: stdio, and streamable HTTP. The deprecated HTTP+SSE transport is not supported.
  - stdio servers are spawned through `Sandbox::spawn` in their own process group and killed at shutdown (8).
  - Their environment is the user's, minus every provider key variable (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY` and any configured `api_key_env`), plus the server's `env`.
  - HTTP `headers` values may use `${VAR}`, expanded from the environment, in user config only.
- **Sources:**
  - User config `[mcp.servers.<name>]` (trusted).
  - `tautly mcp import [--from PATH]`: reads a Claude-style JSON (`mcpServers: {name: {command, args, env} | {type: "http", url, headers}}`; default `~/.claude.json`, else `./.mcp.json`) and writes the entries into the user config with comments preserved.
  - Repository-declared servers: the project layer's `[mcp.servers]`, and the repository root `.mcp.json` when `mcp.import_mcp_json` is on.
- **Repository-declared servers are untrusted.** Each needs a one-time user approval, remembered in `<data home>/mcp-approvals.json` under `blake3(canonical root, name, transport, argv or URL, env and header names)`. Any change asks again.
  - Interactive: `McpServerApprovalRequested{request_id, server, source, launch}` is answered by `Approve`/`Deny`. Denials last for the session.
  - Headless: not started unless `--approve-mcp`, which approves for that run only and persists nothing.
  - A user-config server with the same name wins over a repository one, with a `Warning`.
- **Start-up:**
  - Servers start in the background at session start.
  - The first request waits up to `startup_timeout_ms` (5,000) for them. A server ready later joins at the start of the next user turn: one announced cache break.
  - `notifications/tools/list_changed` also applies only at the next user-turn start.
  - `McpServerStatus{server, state, tools, message?}` reports `state` ∈ `starting | ready | failed | skipped | awaiting_approval`.
- **Tools:**
  - Named `mcp__<server>__<tool>`. Characters outside `[A-Za-z0-9_-]` become `_`. Names longer than 64 are cut to 57 characters plus `_` and 6 hex characters of their blake3.
  - Sorted with the other tools by name (Checkpoint 3 rule: wire order is the name sort), so the prefix stays byte-stable.
  - At most `max_tools` (50) per server. Descriptions are truncated to 1 KB; a tool whose input schema exceeds 8 KB is skipped with a `Warning`.
- **Calls:**
  - Effect: `Exec{argv}` (stdio) or `Network{host}` (HTTP). The policy default is `ask`; `auto-edit` still asks; `yolo` allows. Rules match by name (5.5).
  - Timeout `timeout_s` (120).
  - Results: text content concatenated; `structuredContent` as JSON text when there is no text; images for capable models; resource links as text. The 30 KB head+tail cap with spill applies (5.2); command output filters do not.
  - The loop breaker (7.8) applies.
- **Out of v1:** resources, prompts, sampling, elicitation, OAuth and roots (sub-project 4). `tautly mcp list` shows servers, source, approval state and tool count.

#### 7.11.11 Git checkpoints and undo
- **When:** in a git work tree with `[checkpoints] enabled`:
  - at session start;
  - before every `bash` call (which is `Exec{may_write: true}`) and every stdio MCP call;
  - at every `TurnEnd`;
  - before a rewind or branch switch (7.12).

  With a checkpoint before every command and at every turn end, the files of any step boundary can be rebuilt exactly (7.12.1). This is the only checkpoint mechanism: time travel and undo both use these refs. A shadow repository was considered and rejected, because forks in worktrees need the commits in the user's repository.
- **How:** the engine snapshots the work tree with git plumbing, through `Sandbox::spawn`:
  1. `GIT_INDEX_FILE=<temp file>`, then `git read-tree HEAD` (empty tree when unborn).
  2. `git add -A` (includes untracked files and respects ignore rules).
  3. `git write-tree`, then `git commit-tree -p <previous checkpoint or HEAD>`.
  4. `git update-ref refs/tautly/checkpoints/<session>/<n> <commit>`.

  Every git call runs with `-c core.fsmonitor=false -c core.hooksPath=/dev/null`. The user's index, HEAD, branches, stash and ignored files are never read or written. Only objects and `refs/tautly/` change.
- **Dedup:** if no file changed since the previous checkpoint (write generation unchanged and the watcher saw no change), the previous checkpoint is reused.
- **Bounds:** the checkpoint is skipped with a `Warning` (`checkpoint skipped: <reason>`; the session records `CheckpointSkipped`) when:
  - git is missing, or the tree is bare;
  - the snapshot takes longer than `timeout_ms` (3,000; git is killed);
  - untracked files exceed `max_untracked_mb` (200) or `max_untracked_files` (20,000), checked with `git ls-files --others --exclude-standard` first.

  The session log records `Checkpoint{n, commit, tree, head?, dirty, turn, step}` (6.5).
- **Forks:** a fork (7.12) creates its own refs `refs/tautly/checkpoints/<child>/<n>` for every parent checkpoint it copies. Deleting or pruning one session therefore never breaks another.
- **Visibility:** the refs appear in `git log --all`. To hide them, use `git log --exclude='refs/tautly/*' --all` (`--exclude` must come before `--all`); this is documented.
- **Undo:**
  - `Command::Undo{turn?, dry_run}` reverts user turn `turn` (default: the last). It walks that turn's records in reverse:
    - an `Edit` restores its pre-image (5.4);
    - a `Checkpoint` restores the work tree to that snapshot: changed and deleted files come back from the checkpoint via the temp index; files added since, untracked and not ignored, are deleted.
  - Before writing, it takes a backup snapshot `refs/tautly/checkpoints/<session>/undo-<n>`, so the undo itself can be reversed with git.
  - It answers with `Undone{turn, restored, deleted, dry_run, backup_ref?, note?}`. `dry_run` changes nothing.
  - Ignored files (e.g. `node_modules/`, `target/`) and submodule contents are never touched; `note` says so when the turn ran commands.
  - Outside git, only tool pre-images are restored, and `note` says command side effects were not undone.
  - `tautly undo [--turn N]` uses the same path, with the same user-turn numbering.
- **Pruning:**
  - At engine start, checkpoint refs older than `keep_days` (14) are deleted with `git update-ref -d`. `git gc` is never run.
  - `tautly sessions delete <id>` removes the session log and its checkpoint refs, plus its worktree when it is a worktree fork (7.12.4). Blobs are shared across the project's sessions and are kept; blob garbage collection is post-v1.

### 7.12 Time travel (v1 core; user-approved 2026-11-03)
Every operation here creates a new branch or a new view. None truncates a log, and none overwrites a file the user changed without explicit consent. Rationale: the 2026-11-03 research notes. Untrustworthy file rewind is the most-voiced complaint about other tools, and forks elsewhere share one mutable working tree.

#### 7.12.1 Step addresses and file states
- **Step addresses.** Steps are numbered by the absolute 1-based provider step (the context turn counter, 7.2). "At step N" means steps 1..N are kept and the next step is N+1. `at = 0` means before the first step. CLIs and the REPL show step numbers next to user turns (`tautly sessions show`, `/tree`).
- **`state_at(session, N)`, the files at a step boundary:**
  1. Take the earliest `Checkpoint` record after step N.
  2. Restore its tree.
  3. Revert, in reverse order, every `Edit` (to its pre-image) and every `Undone` (to its `from_blob`) between step N and that checkpoint.

  This is exact, because a checkpoint precedes every command (7.11.11).
- **Partial states.** These cases give a *partial* state, reported with a note, and only tool edits are reverted:
  - no later checkpoint exists (a crash mid-turn, or outside git);
  - a command between the boundary and that checkpoint had its checkpoint skipped.
- **`expected_tip(session)`** is the files as the agent left them: the latest checkpoint plus later tool-edit post-images. A file whose disk hash differs from `expected_tip` was changed by someone else: a **human edit**.
- **Working-tree lease.** Engines hold a shared `flock` on `<data home>/locks/<root-hash>.lock`. Rewind and switch need it exclusive. They are refused while another live session, in this process or another, uses the same working tree.

#### 7.12.2 Rewind as fork
- `Command::Rewind{at, label?, dry_run, overwrite}` (REPL `/rewind <step>`):
  1. Take a checkpoint of the current tree and append it to the current session, so its tip stays restorable.
  2. Create a branch: a new session with `origin: rewind`, `parent: (current, offset of step at)`, the records up to that step copied verbatim, and child checkpoint refs.
  3. Materialize `state_at(at)` in place.
  4. Continue on the branch (`SessionChanged`). The old branch's log is never modified apart from that tip checkpoint.
- **Materializing** changes only the paths where the target state differs from `expected_tip`.
  - A path that would change and has a human edit, including a human-added file that would be deleted, is a **conflict**.
  - When conflicts remain that are not listed in `overwrite`, nothing is written and no branch is created (`applied: false`).
  - Human edits to paths the rewind does not touch are left alone.
  - Before any write, a backup snapshot is taken (`undo-<n>` ref, 7.11.11).
- **The answer is `Rewound{at, branch, report}`**, where `report` is `{restored, deleted, conflicts, dry_run, applied, backup_ref?, note?}`. The REPL first sends a dry run and shows what would be restored, deleted and in conflict. It asks y/n, and on conflicts asks separately before sending `overwrite` with the paths.
- **Conversation:** the branch's context is `rebuild(reader, at)` (H9). Requests after a rewind start from that exact prefix.
- **Outside git:** only tool edits are restored, and `note` says command effects were not.

#### 7.12.3 Session tree and switching
- **Branches are first-class.** Each session's header gives its parent `(session, step)`, `label` and `origin`. `Fork{at?, label?}` (REPL `/fork [label]`) creates a conversation-only branch at step `at` (default: the tip) without touching files. `Label{label}` (`/label <name>`) renames the current branch.
- **`session/tree {root}`** (RPC), `tautly sessions tree` and `/tree` list the project's sessions (H10) as a tree. Each node is `{session_id, parent?, label?, origin, created, steps, last_prompt, cost_usd?, live, current}`. The REPL prints one indented line per branch: `* <label or id8>  step N · <origin> · $cost · <last prompt, 40 chars>`, with `*` marking the current branch.
- **`Command::Switch{session_id, dry_run, overwrite}`** (`/switch <label|id|n>`) materializes the target branch's tip (`expected_tip(target)`) in place, with the same conflict rules and backup as rewind. It then continues that session (`Switched{branch, report}`, `SessionChanged`). Switching back and forth restores each tip exactly.

#### 7.12.4 Fork at a step into an isolated worktree
- **Command:** `tautly fork <session> --at N [--model M] [--prompt TEXT] [--label L] [--dir PATH] [--approve auto-edit|yolo] [--budget-usd N] [--output json|text]`.
- **Worktree:** it creates `git worktree add --detach <dir> <commit>` at `state_at(N)`'s checkpoint and applies the tool-edit reverts there. Git runs with hooks disabled. `<dir>` defaults to `<data home>/worktrees/<project-id>/<session-id>`.
- **Session:** a new session (`origin: what_if`, parent `(session, step N)`, `root` = the worktree) holding the records up to step N. Root-relative paths make them valid there. It shares blobs with its parent (H10). `--model` applies `SetModel` before the first message.
- **Running:**
  - Without `--prompt` it prints the worktree path, the session id and the resume command.
  - With `--prompt` it runs one-shot in the worktree under the headless rules (3.4).
  - The original working tree and its index are never touched.
- **Logged approvals are never consent:** every call in a fork is decided by the current policy afresh.
- **Requirements and cleanup:** outside git the command is refused. `session/new{fork:{…, worktree: true}}` does the same over RPC. `tautly sessions delete` removes a clean fork worktree and refuses a dirty one without `--force`.

#### 7.12.5 Playback
- `tautly replay <session> [--speed X] [--from-step N] [--output text|json]` and `Command::Replay{session_id?, speed, from_step?}` (idle only) re-emit the logged stream. Each item is `Replayed{offset, t_ms, event}`, and the stream ends with `ReplayFinished{session_id, events, cancelled}`.
- **Record-to-event mapping:**

  | Record | Event |
  |---|---|
  | `SendMessage` | `UserMessage{text}` + `TurnStarted` |
  | provider text and reasoning | `TextDelta` / `ReasoningDelta` (redacted reasoning skipped) |
  | `ToolCall` | `ToolCallProposed` |
  | `ApprovalRequested` | `ToolApprovalRequested` (no `diff`) |
  | `ToolResult` | `ToolFinished` (output from its blob) |
  | provider usage | `Usage` |
  | `Compaction` | `Compacted` |
  | `GcApplied` | `ContextCollected` |
  | `MemoryProposal` | `MemoryProposed` |
  | `ModelSet` | `ModelChanged` |
  | `TurnEnd` | `TurnFinished` |

- **Pacing:** each gap is `Δt / speed`, capped at 2 s; `speed = 0` is instant.
- **Read-only:** no provider request, no tool execution, no file access beyond the log and its blobs. It works without an API key. Replayed approval request ids cannot be answered. `Cancel` stops a replay.

#### 7.12.6 Compare
- `tautly compare <a> <b> [<c>…] [--json]` takes 2–5 sessions. When the sessions share a fork point, figures count from it; otherwise they cover whole sessions.
- **One column per session:**
  - label;
  - models used;
  - user turns and steps;
  - tokens (input, output, cache read) and cost (ledger by session, which includes discarded attempts, falling back to logged usage);
  - wall time: the sum of `SendMessage → TurnEnd` timestamps;
  - tool calls, blocked calls and edit failures;
  - last test evidence (7.11.7);
  - files touched;
  - diff stat (`+a -r`, the net change since the fork point).
- A files table shows `only <a> | only <b> | both`.

#### 7.12.7 Bisect
- **Command:** `tautly bisect <session> --test "<cmd>" [--good N] [--bad M] [--timeout S] [--yes] [--json]`. Defaults: good = 0, bad = the tip, timeout 600 s per run.
- **Candidates:** the step boundaries whose state is exact (7.12.1). Partial ones are skipped.
- **Pre-pass:** logged runs of the identical normalized command, with their exit codes and write generations, narrow the range before anything runs.
- **Runs:**
  - Each run materializes `state_at(k)` in one temporary worktree under `<data home>/worktrees/<project-id>/bisect-<id>`. Ignored files such as build caches are kept between runs.
  - The command runs through the sandbox with the `bash` tool's process-group and timeout handling. Exit 0 is good, 125 skip, any other code bad, and a timeout skip.
  - The endpoints are verified first (good must pass and bad must fail), else exit 2.
- **Safety:** before running, it prints the command and the maximum number of runs and asks, unless `--yes` is given. A deny rule matching the command refuses. The worktree is removed at the end, also on error, and the user's working tree is never touched.
- **Result:** the first failing step, its user turn, its tool calls (`tool: target`) and ≤ 300 characters of the step's assistant text.

#### 7.12.8 Post-v1: the Time travel sub-project (after Trust)
- **Race / best-of-N:** `tautly race -n K [--models a,b,…] --verify "<cmd>"`.
  - Runs K forks in worktrees, mixing providers, with one hard budget for the whole fan-out.
  - Selection is by the verify command; ties go to the smaller diff, then fewer tokens.
  - Losers are cancelled early once a candidate passes.
  - Results are ranked by cost per pass.
- **Cassettes:** deterministic replay of a session.
  - A `ReplayProvider` serves each step's logged response and checks the regenerated request hash (H6). A divergence is located with the block hashes.
  - Tool results are served by call id; `web_fetch` is always served from the recording.
  - In live-tools mode, logged approvals are not consent: the policy is re-checked, and runs happen only in a worktree.
  - Real sessions become `eval --replay` regression cases.
- **Cherry-pick and path-level merge:** `tautly merge <fork> [--paths …] [--steps N..M]` applies a fork's changes three-way, logged with provenance. Conflicts surface as normal approvals.
- **Redacted export/import:**
  - **Bundle:** one JSONL file: the session log plus `{"type":"blob",…}` lines. Old readers skip unknown line types, so a bundle is still a valid log.
  - **HTML:** a self-contained HTML export.
  - **PR trailer:** an optional `Tautly-Session: <id>` trailer, compatible with Agent Trace attribution.
  - **Safety rules:**
    - Redact secrets in tool output (environment dumps, `.env` reads, key patterns).
    - Replace home, user and host names.
    - Offer a hashes-only mode for proprietary code.
    - Strip `RedactedReasoning` and reasoning signatures.
    - Mark request hashes unverifiable after redaction.
  - **Imports:** read-only, `origin: import`, paths re-confined. Undo or rewind is never applied against the local tree.

## 8. Error handling

| Class | Behavior |
|---|---|
| Provider transient | Backoff + jitter retry (3.5); after exhaustion `Error{retryable: true}` and `TurnFinished{error}` |
| Provider fatal (auth, invalid request) | `Error` with actionable `hint`; turn ends |
| Context overflow | One compaction + one retry, then fatal |
| Tool error | Returned to the model as a tool result; turn continues |
| Sandbox / policy denial | Returned to model and surfaced to user with the blocking rule |
| Engine bug (panic in a turn task) | Caught at turn boundary, logged with session id, session left resumable |

- Errors are typed per crate (`thiserror`); `anyhow` only in `tautly-cli`.
- Cancellation: first Ctrl-C cancels the turn and kills child process groups within 200 ms; second Ctrl-C exits.
- Shutdown: all spawned process groups are terminated; verified by test.

## 9. Testing

1. **Unit** — edit matching, atomic write, command parser, permission rules, context assembly, cost math. `proptest` for edit application and budget accounting.
2. **Adapter** — recorded real provider streams replayed through each adapter. Nightly live smoke suite with real keys (not a merge gate).
3. **Engine** — `MockProvider` with scripted responses: tool calls, errors, stream drops, oversized outputs. Covers loop, approvals, cancellation, budgets, compaction, crash-resume.
4. **Protocol** — `tautly -p` golden-file tests over the JSON event stream.

### 9.1 CI performance gates (merge-blocking)
- Cold start < 50 ms (`hyperfine`, release build, `tautly --version` and `tautly -p` against `MockProvider` to first event).
- Idle RSS < 30 MB.
- 4-hour simulated session (mock provider, accelerated clock): RSS within ±10% of the 10-minute mark.
- 100 MB tool output: RSS bounded, output spilled to disk.
- Release binary size tracked and reported (not gated).
- Index build of a generated 10,000-file repository < 10 s; single-file reindex < 50 ms.
- Selective `grep` via the trigram index on a generated 100,000-file repository < 50 ms; `glob` from the live tree < 5 ms.
- Token regression: tokens per solved task on the starter set (scripted mock replay, deterministic) must not exceed the committed baseline by more than 10%.

### 9.2 Eval runner (interface for sub-project 5)
- `tautly eval --tasks <dir> --model <id> [--edit-format <fmt>]` runs tasks headlessly in temporary git worktrees and reports pass rate, cost, tokens, tokens per solved task, cache hit ratio, and edit-failure rate as JSON.
- Ships with a starter set of ~50 small repository tasks (each: setup script, prompt, verification command).
- The full benchmark program (leaderboard, third-party submissions) is sub-project 5. v1 ships the kit in 9.3.

### 9.3 Public benchmark kit (launch deliverable)
- `benchmarks/terminal-bench/`: an adapter that runs Tautly as an installed agent in the Terminal-Bench 2.0 harness (harness version pinned in the kit's README), plus a script for N trials per task (default 5) with a fixed model and `--approve yolo` inside the harness container.
- `benchmarks/tokens/`: runs `tautly eval` on the starter set with fixed models and trials and writes raw per-task JSON plus a generated markdown table: pass rate, tokens per solved task, cache hit ratio and cost, per model.
- `docs/benchmark.md`: methodology, exact commands to reproduce, known failures, and how to compare against other harnesses on the same model.
- CI only lints the scripts and runs the report generator on a committed fixture. Live runs are manual, and their raw logs are published with the results.

## 10. Out of scope (this sub-project)
Real OS sandbox enforcement (first after v1, 1.5); full TUI; OpenTelemetry export; semantic (type-aware) references; embedding-based memory search; learned skills; speculative prefetch; pre-run cost estimates; MCP resources, prompts, sampling and OAuth; race/best-of-N, cassettes, cherry-pick/merge and session export/import (post-v1 Time travel, 7.12.8); hooks; skills; multi-agent/subagents; LSP diagnostics; deterministic replay; Windows; team policy; plugin marketplace.

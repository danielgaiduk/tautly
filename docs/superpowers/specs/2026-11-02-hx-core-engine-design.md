# hx — Core Engine Design (Sub-project 1)

- **Date:** 2026-11-02
- **Status:** Draft for review
- **Working name:** `hx` (codename; rename is a find-and-replace)

## 1. Intent

### 1.1 Goal
An open-source, provider-neutral agentic coding harness that becomes the go-to tool for agentic coders, winning on **performance and reliability**, with extensibility as the underlying architecture.

### 1.2 Decisions made
| Topic | Decision |
|---|---|
| Model | Open-source product, permissive license (Apache-2.0) |
| Wedge | Performance + reliability |
| v1 surfaces | Interactive TUI + headless/JSON mode on one engine |
| Providers | Native Anthropic, OpenAI, Google adapters + generic OpenAI-compatible |
| Team | Solo developer + coding agents |
| Language / shape | Rust, library-first engine, thin clients in-process |

### 1.3 Assumptions
- Platforms: macOS and Linux in v1; Windows (WSL first) after.
- Distribution: single static binary.
- No vendor subscription OAuth reuse — API keys only (avoids ToS/blocking risk seen in 2026).

### 1.4 Why this can win (from market research, Nov 2026)
- Codex CLI is fast (Rust) but OpenAI-centric; OpenCode is provider-neutral but has documented memory runaways (10–15 GB+), orphaned processes and version-to-version breakage; pi is minimal and transparent but has no sandbox; Claude Code is closed and single-vendor.
- Gaps this design targets: lowest tokens per solved task (code index, diff re-reads, context GC, output filters, code-anchored memory, cheap side-model routing), bounded memory and instant start, flicker-free TUI, sandbox covering *all* tools, transparent context with constraints that survive compaction, model-adaptive and measured edit tools, live cost/quota with hard budgets, open session format, standards interop (AGENTS.md, SKILL.md, MCP, ACP).

### 1.5 Program decomposition
| # | Sub-project | Covered here? |
|---|---|---|
| 1 | Core engine (loop, providers, tools, context, sessions, protocol) | **Yes** |
| 2 | Sandbox + permission enforcement (Seatbelt, Landlock/bubblewrap) | Interface + stub only |
| 3 | TUI client (ratatui, synchronized output) | Stub only |
| 4 | Extensions (MCP, hooks, skills, ACP bridge) | No (protocol designed to support) |
| 5 | Eval & benchmark program | `hx eval` runner + starter set only |

Post-v1, out of scope for all of the above: multi-agent orchestration, deterministic replay, team policy-as-code, plugin marketplaces.

### 1.6 Success criteria for this sub-project
- Cold start to ready < 50 ms; idle RSS < 30 MB; RSS flat (±10%) across a 4-hour simulated session.
- All four test layers green; performance gates in CI pass.
- Cache hit ratio, blocked repeat attempts and edit repairs per stage are reported by `hx eval`.
- Tokens per solved task on the eval starter set is measured, published, and protected by a CI regression gate (9.1).
- Usable as a daily driver on a real repository with each of the four adapters, via `hx -p` and the interactive stub (2.3).

## 2. Architecture

### 2.1 Crate layout (Cargo workspace)
```
hx-cli        binary: arg parsing, selects TUI or headless mode
 ├─ hx-tui    ratatui client (stub in this sub-project)
 ├─ hx-rpc    JSON-RPC 2.0 over stdio (headless; later ACP bridge)
 └─ hx-core   the engine — no terminal I/O, no global state
     ├─ hx-provider   Provider trait + anthropic / openai / google / openai-compat
     ├─ hx-tools      Tool trait + read, write, edit, bash, grep, glob, outline, symbol, refs,
     │                web_fetch, recall, remember, pin, state; output filters
     ├─ hx-context    context assembly, token budget, pinning, working state, compaction, context GC
     ├─ hx-memory     code-anchored project memory, episodic memory, recall search
     ├─ hx-index      tree-sitter symbol/reference index (SQLite), incremental
     ├─ hx-session    append-only session log, resume, fork, undo
     └─ hx-sandbox    Sandbox trait; this sub-project ships a passthrough-with-policy stub
```
Dependencies only point downward (`hx-tools → hx-memory → {hx-index, hx-session}`, `hx-index → hx-sandbox`, `hx-tools → hx-sandbox`, `hx-context → {hx-provider, hx-tools}`). Each crate is independently testable.

### 2.2 Rules
- **Single entry point:** `hx-core` exposes an `Engine` handle. Clients send `Command`s and receive a stream of `Event`s. TUI and RPC are both plain consumers, guaranteeing identical behavior.
- **Async:** Tokio. Each turn runs in its own task scope with a `CancellationToken`; cancellation propagates to provider streams and child processes.
- **Sandbox chokepoint:** tools receive `&dyn Sandbox` and have no direct access to `std::fs` or `std::process`. Enforced by crate visibility and a CI lint (`clippy::disallowed_methods` for `std::fs::*`, `std::process::Command` in `hx-tools`).
- **Bounded by construction:** fixed-capacity channels between engine and clients; tool output capped with disk spill; session history streamed to disk; only the active context plus an index held in memory.
- **Config:** layered TOML — user (`~/.config/hx/config.toml`) → project (`.hx/config.toml`) → CLI flags. `AGENTS.md` in the repository root and ancestors is loaded as context.

### 2.3 Stubs shipped in this sub-project
- **`hx-tui` stub:** a line-mode interactive REPL (no full-screen UI). Prints streamed text and tool activity, prompts y/n for approvals, shows `ContextStatus` and session cost after each turn, supports `/pin`, `/compact`, `/fork`, `/undo`. Replaced by the ratatui client in sub-project 3.
- **`hx-sandbox` stub:** applies the permission policy (5.5) and enforces workspace-root path confinement (canonicalized paths, symlinks resolved) in-process; spawns processes directly. No OS-level isolation — that is sub-project 2.

## 3. Agent loop and protocol

### 3.1 Turn lifecycle
1. **Assemble** — `hx-context` builds a `CanonicalRequest` and a per-block token breakdown.
2. **Stream** — the provider streams text, reasoning and tool calls; each is re-emitted as an event immediately.
3. **Decide** — the permission policy returns `allow | ask | deny` per tool call. `ask` emits `ToolApprovalRequested` and awaits `Approve`/`Deny`.
4. **Execute** — approved calls run via the sandbox. Calls whose `Effect` is `ReadOnly` run concurrently; `Write`/`Exec` calls run sequentially in model-emitted order.
5. **Loop** — results are appended; repeat from 1 until the model ends without tool calls, a cap (turns / cost / wall time) is reached, or the user cancels.

Every step is appended to the session log before its effects are surfaced, so any crash is resumable.

### 3.2 Events (engine → client)
Typed, serde-serializable, versioned:
`TurnStarted`, `TextDelta`, `ReasoningDelta`, `ToolCallProposed`, `ToolApprovalRequested`, `ToolStarted`, `ToolOutputDelta`, `ToolFinished{…, blocked}`, `Usage{role, tokens_in, tokens_out, cache_read, cache_write, cost_usd, quota?}` (`role` ∈ `main | side`), `ContextStatus{used, budget, breakdown}`, `Compacted{removed_turns, summary, reason}`, `ContextCollected{elided_results, tokens_freed, reason}`, `MemoryProposed{proposal_id, change}` (`change` = an entry to upsert and/or entry ids to delete), `SessionChanged{session_id}`, `TurnFinished{reason}`, `Error{kind, message, retryable, hint}`.

`TurnFinished.reason` ∈ `completed | cancelled | budget | max_turns | timeout | error`.

### 3.3 Commands (client → engine)
`SendMessage`, `Approve`, `Deny`, `Cancel`, `SetBudget`, `Compact`, `Uncompact`, `Pin`, `Unpin`, `ApproveMemory{proposal_id}`, `RejectMemory{proposal_id}`, `ConsolidateMemory`, `Fork`, `Shutdown`.

### 3.4 Headless protocol (`hx-rpc`)
- JSON-RPC 2.0, newline-delimited JSON over stdio. Commands are methods; events are notifications.
- `initialize` handshake negotiates protocol version. Event and field additions are minor-version and backwards compatible; any removal or semantic change is a major-version bump.
- One-shot mode: `hx -p "<prompt>" --output json|text` sends one message, streams events to stdout, exits with a code mapped from `TurnFinished.reason` (0 completed, 1 error, 2 budget/max_turns/timeout, 130 cancelled).
- In headless mode `ask` resolves to `deny` unless `--approve <mode>` is given (`auto-edit` or `yolo`), and memory proposals are rejected unless `--approve-memory` is given. Headless never blocks on input.

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

### 4.3 Model catalog
Built-in TOML catalog (context window, prices per token class, capabilities). User overrides in config. `hx models update` refreshes from a models.dev-format source. Unknown models get conservative defaults and `cost_usd = null`.

### 4.4 Auth
API keys from environment variables or the OS keychain (`keyring` crate). No subscription OAuth.

### 4.5 Cache stability
`hx-context` keeps the request prefix byte-stable across turns: fixed tool ordering, no timestamps or volatile data in the system prompt, pinned block after the stable prefix, working state as a tail block after the last message (6.1). Cache hit ratio is reported in `Usage`.

### 4.6 Cost, quota, budgets
- `Usage` per provider response; cost computed from catalog prices. Aggregated per turn, session and day into a local SQLite ledger (`~/.local/share/hx/ledger.db`).
- Quota: rate-limit response headers (Anthropic, OpenAI) parsed into `Usage.quota{remaining_requests, remaining_tokens, reset_at}` when present.
- `SetBudget{scope: turn|session, max_usd?, max_tokens?}`. At 80%: warning event. At 100%: the engine finishes the in-flight tool call, then ends with `TurnFinished{reason: budget}`.
- Telemetry: optional OpenTelemetry export using GenAI semantic conventions. Off by default; nothing leaves the machine unless enabled.

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
- Overridable in config. Defaults are revised from `hx eval` results.

### 5.4 Edit safety pipeline
1. **Stale-read guard** — reject if the file's mtime or content hash differs from the model's last `read`; the model must re-read.
2. **Matching** — exact match first; whitespace-normalized fallback only when it yields exactly one match. Zero or multiple matches fail with candidate locations.
3. **Atomic write** — temp file in same directory → fsync → rename. Preserve line endings, encoding, trailing newline, file mode.
4. **Syntax check** — tree-sitter parse for supported languages (initially Rust, TypeScript/JavaScript, Python, Go, JSON, TOML, YAML). A newly introduced parse error is reported in the tool result; the edit stays applied.
5. **Undo** — pre-image recorded in the session log; `hx undo [--turn N]` reverts.

### 5.5 Permission policy
Rules are evaluated in `hx-core`; OS-level enforcement is sub-project 2.
- Modes: `ask` (default), `auto-edit` (workspace file writes allowed; exec still asks), `yolo`.
- TOML rules, e.g. `[permissions] allow = ["bash(cargo test*)"]  deny = ["bash(rm -rf*)"]`. Deny beats allow.
- **Protected paths always ask, in every mode:** `.git/`, `.hx/`, git hook paths, `.github/workflows/`, `.vscode/`, `.idea/`, `.envrc`, `Makefile`, `package.json`, `Cargo.toml` `build.rs`, shell rc files. Defends against "agent writes a file a trusted host tool later executes."
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
- Cache stability: the working state changes on almost every tool call (the files list follows effects), so it sits after history; the cached prefix (stable · pinned · history up to the last cache breakpoint) survives state changes. Pinned items change rarely and stay before history.
- `ContextStatus{used, budget, breakdown}` is emitted every turn.
- `hx prompt show` prints the system prompt; `hx prompt show --full` prints the exact last request.
- System prompt target: < 2,000 tokens.

### 6.2 Pinned items
Sources: user (`/pin "…"` or `Pin` command), config, the `pin` tool, and AGENTS.md sections headed `Constraints` or `Rules` (auto-pinned). Pinned items are never summarized or dropped. Budget warning if pinned exceeds 10% of the window.

### 6.3 Working state
Structured record: task, plan steps with status, files read/edited with content hashes, decisions, open questions, failed attempts (7.8). The files list is maintained by the engine from actual tool effects; plan, decisions and questions are updated by the model through the `state` tool.

### 6.4 Compaction
- Trigger: automatic at a configurable threshold (default 80% of context window; can be disabled), or `/compact`. Always announced via event before running.
- Effect: oldest history turns replaced by a summary generated by the active model; pinned and state blocks preserved verbatim; files list lets the model re-read selectively.
- Reversible: originals remain in the session log; `Uncompact` restores them if they fit the window.
- Context overflow from the provider triggers one compaction and one retry.
- Compaction summaries are generated by the side model (7.5).

### 6.5 Session store
- Append-only JSONL per session: `~/.local/share/hx/sessions/<project-hash>/<session-id>.jsonl`. Large blobs (file contents, tool outputs) stored content-addressed in a sibling `blobs/` directory and referenced by hash.
- Records: commands, canonical requests, provider events, tool calls/results, edit pre-images, compactions.
- Durability: fsync at turn and tool boundaries. On start, a session ending mid-turn is detected and resumable from the last complete step.
- Operations: `hx --continue`, `hx --resume <id>`, `/fork` (new log referencing parent at an event offset), `hx sessions list|show|export`.
- Format documented and versioned (`schema_version` in the header record).

## 7. Token efficiency and memory

### 7.1 Code index (`hx-index`)
- tree-sitter extraction of symbols (functions, types, methods, constants: name, kind, parent, signature, line range, content hash) and name-based references, for the syntax-check language set (5.4).
- SQLite at `~/.local/share/hx/index/<project-hash>.db`: `files(path, hash, mtime, size, lang)`, `symbols(file, name, kind, parent, start_line, end_line, signature, hash)`, `refs(file, line, name)`. WAL mode.
- Incremental: on engine start a background task re-parses files whose size or mtime changed; it never blocks start-up. Each successful edit re-indexes that file synchronously before the tool result is returned.
- Reads files only through `Sandbox`; honors `.gitignore`.
- Until the initial build completes, `outline`/`symbol`/`refs` fall back to tree-sitter on demand (outline) or grep (symbol, refs) and say so in the result.
- Tools: `outline(path)` → symbols with signatures and line ranges, no bodies; `symbol(name, kind?)` → source of the definition with location (several matches → list of locations, no bodies); `refs(name)` → locations with one line of context. `refs` is name-based, and its description says so.
- Targets: initial index of a 10,000-file repository < 10 s; single-file reindex < 50 ms.

### 7.2 Diff-on-re-read and context GC
- The read tracker stores the last content returned per path (blob reference). A re-read of the same range returns `unchanged since turn N` if identical; if fewer than 50% of lines changed it returns a line-numbered unified diff; otherwise the full content. `full: true` forces full content.
- A tool result is *superseded* when: it is a `read` of a path later edited or re-read; it is a `bash` result of a command string later re-run; it is a failed `edit` on a path later edited successfully.
- Superseded results are replaced by `[elided: superseded at turn N]`, keeping the tool-call id and pairing. A read that answered `unchanged`/a diff never supersedes its base, and eliding any read result of a path forgets that path's re-read baselines.
- Collection runs only at turn start, scheduled by the cache-aware scheduler (7.7); without catalog prices it applies when elidable tokens ≥ max(2,000, 5% of the context window), so the cache prefix breaks at most once per batch. Emits `ContextCollected`; elisions are logged in the session and restored by `Uncompact`.

### 7.3 Output filters
- Parsers for: `cargo test|build|check|clippy`, `pytest`, `jest`/`vitest`, `go test`, `tsc`, `eslint`. Detected from the parsed command (5.5 parser).
- Applied when raw output exceeds 2 KB. Result: counts (passed/failed/errors/warnings) plus each failure or diagnostic (name, `file:line`, message, ≤ 20 lines each), plus the path of the spilled full output.
- Parse failure falls back to the 30 KB head + tail cap. `raw: true` disables filtering.

### 7.4 Memory (`hx-memory`)
- **Project memory:** one markdown file per entry in `.hx/memory/<id>.md` (shareable via git). Frontmatter: `id`, `title`, `kind` (`convention | fact | gotcha | command`), `anchors: [{path, symbol?, hash}]`, `created`, `verified`. Body ≤ 300 tokens.
- **Memory index:** one line per entry (`- [kind] title` plus ` [stale]` when applicable), capped at 500 tokens (oldest unverified entries dropped from the index first, never deleted), in the stable block after AGENTS.md.
- **Staleness:** at session start and after each edit, every anchor hash is compared with the code index (symbol hash when `symbol` is set, file hash otherwise). Mismatch marks the entry stale; `recall` returns stale entries with a warning; an entry whose anchors are re-confirmed by the model through `remember` gets `verified` updated.
- **Episodic memory:** `~/.local/share/hx/memory/<project-hash>/episodes/<session-id>.md` (private, never shared). Written by the side model at session end and at each compaction: task, outcome, what failed, key files. No approval needed.
- **`recall(query)`:** BM25 over titles and bodies of project entries and episodes; returns the top 5, capped at 2,000 tokens.
- **`remember(entry)`:** proposes a new or updated entry; the engine emits `MemoryProposed` and writes the file only after `ApproveMemory`. The system prompt instructs the model to call `remember` when the user corrects it.
- **Consolidation:** `hx memory consolidate`, and offered automatically when the full index would exceed 500 tokens. The side model proposes merges and deletions, shown as a diff; applied only on approval.

### 7.5 Side-model routing
- Config `[routing] side_model = "<model id>"`. Default: the catalog model with `tier = "small"` from the main model's provider; if none, the main model.
- Used for: compaction summaries, episodes, memory consolidation, and `web_fetch` with a `question` (returns the answer, ≤ 1,000 tokens, plus the source URL).
- Side-model usage is reported with `Usage.role = side` and counted in budgets and the ledger.

### 7.7 Cache-aware scheduling
- The catalog gains `cache_ttl_s: Option<u32>` per model (default 300 for `explicit`/`automatic` cache styles, `None` for `none`).
- `hx-context` tracks the time of the last provider request and the token length of the cached prefix (up to the last cache breakpoint).
- Every history mutation (context GC 7.2, threshold compaction 6.4) goes through one scheduler that decides at turn start:
  - **Cold cache** (time since last request ≥ TTL): apply all pending mutations; nothing cached is lost.
  - **Warm cache:** apply only when `saved > cost`, where `saved = tokens_freed × input_price × 5` (5 = expected further requests in the turn horizon) and `cost = invalidated_prefix_tokens × (cache_write_price − cache_read_price)`. Without catalog prices, fall back to the 7.2 threshold rule.
  - **Forced:** always apply when context use ≥ 90% of the window. Threshold compaction may therefore be deferred from 80% up to 90% while the cache is warm and the cost model says it is not yet worth it — never beyond 90%.
- All mutations decided at one turn start are applied together as one "cache epoch", so the prefix breaks at most once.
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
- `hx eval` reports edit repairs per stage alongside the edit-failure rate.

### 7.10 Workspace engine (fast file access, fewer lookup steps)
- **Live workspace** (`hx-index`): an in-memory file tree (path, size, mtime, content hash; ~100 bytes per file) loaded from the index database at start-up, reconciled by the background refresh, and kept current by a file-system watcher (`notify`, 50 ms debounce) that re-indexes only changed files. Edits made by hx tools update it synchronously. `glob` and directory listings are answered from memory. The tree honors `.gitignore` and the same size/binary skips as the index.
- **Trigram text index:** stored on disk in the index database (`trigrams(tri, file_id)`), not in memory, so the idle-RSS target (1.6) holds. `grep` extracts the required literals from the pattern (`regex-syntax`), intersects the postings of their trigrams to get candidate files, then confirms with the real regex. Patterns without a literal of ≥ 3 bytes, and any index error, fall back to the full scan (5.2). The index is trusted only when fresh: watcher alive, no pending post-bash freshness check, and writes guarded by row generations; otherwise tools use the walker. Target: a selective search on a 100,000-file repository < 50 ms.
- **`find(query, kind?, limit = 20)`:** one call searches file paths (fuzzy), symbol names (index) and text (trigram) concurrently and returns one ranked list — exact symbol definitions, then path matches, then text matches — as `path:line  <kind>  <one-line snippet>`. `kind` ∈ `symbol | path | text`.
- **Repo map:** a generated block of ≤ 400 tokens — top-level directories with file counts, languages, entry points (e.g. `main.rs`, `lib.rs`, workspace members, `package.json` scripts) and the most-referenced symbols — placed in the stable block after the memory index (6.1). Generated once at session start (never mid-session, to keep the cache prefix stable).
- **Batched reads:** `read` also accepts `targets: [{path, start_line?, end_line?} | {symbol}]` (≤ 10 per call); results are concatenated with headers and share the 30 KB cap. Each target goes through the same stale-tracking and diff-on-re-read rules (7.2).

### 7.6 Measurement
- `hx eval` reports tokens per solved task (total input + output tokens across main and side roles, divided by solved tasks) and cache hit ratio (`cache_read / (input + cache_read)`).

## 8. Error handling

| Class | Behavior |
|---|---|
| Provider transient | Backoff + jitter retry (3.5); after exhaustion `Error{retryable: true}` and `TurnFinished{error}` |
| Provider fatal (auth, invalid request) | `Error` with actionable `hint`; turn ends |
| Context overflow | One compaction + one retry, then fatal |
| Tool error | Returned to the model as a tool result; turn continues |
| Sandbox / policy denial | Returned to model and surfaced to user with the blocking rule |
| Engine bug (panic in a turn task) | Caught at turn boundary, logged with session id, session left resumable |

- Errors are typed per crate (`thiserror`); `anyhow` only in `hx-cli`.
- Cancellation: first Ctrl-C cancels the turn and kills child process groups within 200 ms; second Ctrl-C exits.
- Shutdown: all spawned process groups are terminated; verified by test.

## 9. Testing

1. **Unit** — edit matching, atomic write, command parser, permission rules, context assembly, cost math. `proptest` for edit application and budget accounting.
2. **Adapter** — recorded real provider streams replayed through each adapter. Nightly live smoke suite with real keys (not a merge gate).
3. **Engine** — `MockProvider` with scripted responses: tool calls, errors, stream drops, oversized outputs. Covers loop, approvals, cancellation, budgets, compaction, crash-resume.
4. **Protocol** — `hx -p` golden-file tests over the JSON event stream.

### 9.1 CI performance gates (merge-blocking)
- Cold start < 50 ms (`hyperfine`, release build, `hx --version` and `hx -p` against `MockProvider` to first event).
- Idle RSS < 30 MB.
- 4-hour simulated session (mock provider, accelerated clock): RSS within ±10% of the 10-minute mark.
- 100 MB tool output: RSS bounded, output spilled to disk.
- Release binary size tracked and reported (not gated).
- Index build of a generated 10,000-file repository < 10 s; single-file reindex < 50 ms.
- Selective `grep` via the trigram index on a generated 100,000-file repository < 50 ms; `glob` from the live tree < 5 ms.
- Token regression: tokens per solved task on the starter set (scripted mock replay, deterministic) must not exceed the committed baseline by more than 10%.

### 9.2 Eval runner (interface for sub-project 5)
- `hx eval --tasks <dir> --model <id> [--edit-format <fmt>]` runs tasks headlessly in temporary git worktrees and reports pass rate, cost, tokens, tokens per solved task, cache hit ratio, and edit-failure rate as JSON.
- Ships with a starter set of ~50 small repository tasks (each: setup script, prompt, verification command).
- Terminal-Bench adapter and the published benchmark program are sub-project 5.

## 10. Out of scope (this sub-project)
Real OS sandbox enforcement; full TUI; semantic (type-aware) references; embedding-based memory search; learned skills; speculative prefetch; pre-run cost estimates; MCP, hooks, skills, ACP; multi-agent/subagents; LSP diagnostics; deterministic replay; Windows; team policy; plugin marketplace.

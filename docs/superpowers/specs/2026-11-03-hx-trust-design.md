# hx — Trust: "done means proven" (Sub-project after v1)

- **Date:** 2026-11-03
- **Status:** DRAFT — not approved. Priority approved; design open for review. Decisions needing the user are collected in §6.
- **Builds on:** `2026-11-02-hx-core-engine-design.md` (cited as v1 §N). All protocol and session-format changes are additive (minor version, v1 §3.4).
- **Sources merged:** verify-loop half of `design-verify-loop-and-prefetch.md`; claim auditor, receipts, ripple check and done contracts from `design-unique-differentiators.md`; the success labeller from `design-learned-skills-and-autopilot.md` (built here, consumed later by Economics).

## 1. Intent

### 1.1 Goal
When `hx` says a turn is done, it shows the proof. Today "done" means "the model stopped calling tools". After this sub-project, every turn that changed code ends with a status (`proven | unproven | failed | not_applicable`) backed by evidence that hx itself recorded: what ran, what changed, what was observed. The status costs **zero model tokens when the work is already correct**.

### 1.2 Design principle: one ledger, many readers
The five requested features are five readers and writers of one data structure: the **evidence ledger** (3.1), an append-only, hash-chained record of the session's writes, executions and observations. Everything else is a projection of it:

| Feature | Role relative to the ledger |
|---|---|
| Verify loop (3.3) | *Produces* test evidence; picks tests from the ledger's change set |
| Done contracts (3.4) | User-confirmed *obligations*; discharged by ledger evidence |
| Ripple check (3.5) | Derives *obligations* (unupdated call sites) from ledger symbol changes |
| Claim auditor (3.6) | Checks prose claims *against* the ledger |
| Receipts (3.6) | *Export* a slice of the ledger with its hash chain |
| Success labeller (3.7) | *Summarises* the ledger per task segment for Economics |

These meet in one place, the **done gate** (3.2): obligations in, evidence out, one status, one shared fix-round budget.

### 1.3 Who benefits
- Interactive users: no more "all tests pass" that was never true; failures caught before they read the summary.
- Headless/CI users: a machine-checkable status, an opt-in exit code and a portable receipt for review.
- The program itself: `hx eval` gains an honest false-done metric, and Economics (learned skills, budget autopilot) gets a success label it can trust.

### 1.4 Success criteria (all measured by `hx eval`, A/B with `--trust on|off`)
| Metric | Definition | Target |
|---|---|---|
| False-done rate | runs ending `completed` that fail the task's hidden verification command (v1 §9.2) | ≥ 40% relative reduction vs off, same model and edit format |
| Proven precision | share of runs with status `proven` that pass hidden verification | ≥ 95% |
| Catch rate | share of false-done runs labelled `unproven` or `failed` | ≥ 80% |
| Selection recall | failures caught by selected tests ÷ failures caught by the full suite (shadow run, 3.3) | ≥ 90%; CI gate on fixtures |
| Token overhead | added tokens on turns whose gate passes in round 1 | exactly 0 beyond a fixed ≤ 150-token cached prefix addition; CI gate |
| Token overhead (overall) | tokens per solved task vs off | ≤ +10% |
| Latency overhead | added wall time per gated turn (p50) / selection compute (p95, 10k files) / ledger append per tool call | ≤ 20 s / < 100 ms / < 1 ms |
| Claim false alarms | `contradicted` verdicts on runs that pass hidden verification | ≤ 5% |
| Labeller precision | `Success` labels whose run passes hidden verification | ≥ 90% |
| Receipt integrity | eval runs whose receipt passes `hx receipt verify`; single-byte mutations detected | 100% / 100% (proptest) |

v1 performance gates (v1 §9.1: cold start, idle RSS, 4-hour RSS) must still pass with Trust on.

## 2. Architecture

### 2.1 Crate layout
One new crate, no new top-level concepts in `hx-core` beyond a gate call:
```
hx-core ── turn loop calls trust::Gate at the would-finish point; feeds Ledger from the executor
 └─ hx-trust   NEW
     ├─ ledger     Evidence, Ledger, hash chain, freshness, change sets          (3.1)
     ├─ gate       obligations, discharge, status, fix-round feedback            (3.2)
     ├─ verify     LangRules, test selection, runner, flake retry                (3.3)
     ├─ contract   Contract, Check, proposals                                    (3.4)
     ├─ ripple     signature-change detection, call-site obligations             (3.5)
     ├─ audit      claim extraction, verdicts; receipt export/verify             (3.6)
     └─ outcome    success labeller                                              (3.7)
```
Dependencies (downward only, extending v1 §2.1): `hx-core → hx-trust → {hx-tools, hx-index, hx-session, hx-sandbox}`. `hx-trust` never depends on `hx-core`, `hx-rpc` or a provider. The labeller's output type `SegmentOutcome` is defined in **`hx-session`** (data only), so Economics reads it from the session log without depending on `hx-trust`.

### 2.2 Plug-in points in v1
| v1 component | Change (all additive) |
|---|---|
| Turn loop (v1 §3.1) | New step 5a: when the model ends without tool calls, run the gate; on actionable failure inject one engine message and continue (counts as a turn) |
| Executor (v1 §3.1 step 4) | After every tool call: `Ledger::record`. Around `Write` effects: snapshot index symbols of the file before, diff after. After every `Exec`: workspace stat walk (3.1) |
| `hx-session` (v1 §6.5) | New record types `evidence`, `gate`, `contract`, `segment_outcome`; `schema_version` minor bump; old readers skip unknown types |
| `hx-index` (v1 §7.1) | `IndexQuery` trait exposed read-only: `symbols_in(path)`, `refs(name)`, `package_of(path)` |
| `hx-tools` filters (v1 §7.3) | Expose the structured parse that already exists internally: `filters::parse(cmd, output) -> Option<Parsed{class, counts, failures}>`. Text rendering unchanged |
| `hx-tools` | Two tools, registered only when enabled: `verify` (Exec) and `propose_contract` (ReadOnly) |
| `hx-rpc` / events (v1 §3.2) | New events/commands in 2.3; `TurnFinished` gains optional `trust` field; `reason` values unchanged |

### 2.3 Protocol additions (minor version)
- **Events:** `GateStarted{round, checks:[{kind, label, reason}]}` · `GateFinished{round, status, obligations:[{kind, label, discharge, evidence?}], duration_ms}` · `ClaimsAudited{turn, claims:[{text, kind, verdict, evidence?}]}` · `ContractProposed{proposal_id, contract, rationale}` · `SegmentLabelled{segment, label, weight, final}`.
- Gate command output streams through existing `ToolStarted/ToolOutputDelta/ToolFinished` with synthetic tool name `verify` or `contract`, so clients need no new rendering path.
- `TurnFinished` gains `trust: Option<{status, rounds, receipt_root}>`. `reason` stays `completed`: adding a reason value would break strict clients.
- **Commands:** `Verify` (run the gate now; results shown, nothing injected into the conversation) · `SetTrust{verify?, audit?}` · `SetContract{checks, scope}` · `ClearContract` · `ApproveContract{proposal_id}` · `RejectContract{proposal_id}` · `ExportReceipt{turns, format}`.
- **Headless:** JSON output includes `trust` and `claims`. Exit codes unchanged by default; `hx -p --require-proven` maps `completed` with status `unproven|failed` to **exit 3** (open question Q3).

## 3. Components

### 3.1 Evidence ledger (`hx-trust::ledger`)
Single source of truth for "what actually happened". Built only from engine-observed effects, never from model text.
```rust
pub struct Gen(pub u64);          // workspace write generation (v1 §7.8 counter, now also bumped by Exec side effects)
pub struct EvidenceId(pub u32);   // dense per session
pub struct Evidence { pub id: EvidenceId, pub turn: u32, pub gen: Gen, pub source: Source,
                      pub kind: EvidenceKind, pub chain: Blake3 }
pub enum Source { Model { tool_call_id: String }, Gate { round: u8 }, User, Undo }
pub enum EvidenceKind {
    Write { path: PathBuf, pre: Option<Blake3>, post: Option<Blake3> /* None = deleted */,
            via: WriteVia /* Tool | Exec | Undo */, symbols: Vec<SymbolChange> },
    Exec  { argv: String, class: CmdClass, scope: Scope, outcome: ExecOutcome,
            exit: Option<i32>, counts: Option<Counts>, failures: Vec<FailureSig>, output: BlobRef },
    Observe { tool: ObserveTool /* Refs | Grep | Symbol */, query: String, hits: u32 },
}
pub enum CmdClass { Test, Build, Typecheck, Lint, Other }        // from the v1 §7.3 filter detection
pub enum ExecOutcome { Ok, Failed, TimedOut, Denied, Cancelled, RunnerMissing }
pub struct SymbolChange { pub key: SymbolKey, pub old_sig: Option<String>, pub new_sig: Option<String>, pub body_changed: bool }
pub struct Counts { pub passed: u32, pub failed: u32, pub skipped: u32 }

impl Ledger {
    pub fn record(&mut self, turn: u32, src: Source, kind: EvidenceKind, log: &mut SessionLog) -> Result<EvidenceId, LedgerError>;
    pub fn gen(&self) -> Gen;
    pub fn changes_since(&self, g: Gen) -> ChangeSet;            // files + changed symbols
    pub fn is_fresh(&self, id: EvidenceId) -> bool;               // no relevant write after it (rules below)
    pub fn find(&self, pred: impl Fn(&Evidence) -> bool) -> impl Iterator<Item = &Evidence>;
    pub fn replay(records: impl Iterator<Item = SessionRecord>) -> Result<Self, LedgerError>;
}
```
Rules:
- **Write-ahead:** the `evidence` record is appended to the session log (v1 §6.5) before the tool result is surfaced; the in-memory ledger is a compact index (no output bytes; outputs are blob references) and is rebuilt on resume by `replay`. Memory: ~100 bytes per entry, bounded by session length.
- **Exec side effects are detected, not assumed.** After every `Exec`, a stat walk (mtime + size, `.gitignore`-aware, the same walk as the v1 §7.1 incremental scan) finds changed files; each becomes a `Write{via: Exec}` with symbol diffs (≤ 20 files reindexed synchronously; beyond that, one `bulk` write and package-level selection). If the walk exceeds 200 ms, the workspace is treated as changed (conservative) and the event says so.
- **Freshness:** `Exec` evidence is fresh iff no `Write` with a later `Gen` touches a path inside its `Scope` (package roots covered by the command; workspace root when unknown). Writes to `*.md` never stale evidence. Stale evidence is kept and reported as stale, never deleted.
- **Zero tests is not a pass.** An `Exec` of class `Test` with `counts.passed + counts.failed == 0` (filter matched nothing, all skipped) has outcome `Failed` for proof purposes, whatever its exit code.
- **Hash chain:** `chain_0 = blake3("hx.ledger/1" ‖ session_id ‖ git_head)`, `chain_i = blake3(chain_{i-1} ‖ jcs(entry_i without chain))` using RFC 8785 canonical JSON. Tamper-evident, not tamper-proof (no signing; Q4).

### 3.2 Done gate (`hx-trust::gate`)
Runs at the would-finish point (v1 §3.1 step 5a) and on the `Verify` command. Never runs on turns with no writes, no active contracts and no checkable claims (status `not_applicable`).
```rust
pub enum Obligation { Tests(Selection), Contract(ContractId), Ripple(RippleSite), Claim(Claim) }
pub enum Discharge { Proven(EvidenceId), Failed(EvidenceId), Stale(EvidenceId), Open(OpenReason) }
pub enum OpenReason { NoTests, Denied, TimedOut, SkippedBudget, RunnerMissing, Unverified, Skipped }
pub enum TrustStatus { Proven, Unproven, Failed, NotApplicable }
pub struct GateReport { pub round: u8, pub status: TrustStatus,
                        pub items: Vec<(Obligation, Discharge)>, pub feedback: Option<String> }
pub struct GateCx<'a> { pub sb: &'a dyn Sandbox, pub policy: &'a Policy, pub index: &'a dyn IndexQuery,
                        pub cancel: CancellationToken, pub tx: &'a EventTx, pub budget: BudgetView, pub round: u8 }
impl Gate { pub async fn run(&self, final_text: &str, ledger: &mut Ledger, cx: GateCx<'_>) -> GateReport; }
```
Algorithm:
1. **Collect** obligations: `Tests(select(changes_since(last_proven_gen)))` if anything changed; every active contract; open ripple sites; claims extracted from `final_text`.
2. **Reuse** fresh evidence first: a fresh passing `Exec` whose scope covers an obligation discharges it without re-running (the model already ran `cargo test -p foo` after its last edit → nothing runs).
3. **Run** the rest sequentially in the sandbox: grouped test commands, then contract checks. If a test command fails to compile, contract checks are marked `Open(Skipped)` for this round. Every run is recorded as `Source::Gate{round}` evidence.
4. **Re-evaluate** ripple sites and claims against the updated ledger.
5. **Status:** `Failed` if any obligation is `Failed` or a claim is contradicted; else `Unproven` if any is `Open` or `Stale`; else `Proven`.
6. **Feedback:** if the status is actionable (failing tests or contracts, contradicted or stale claims, unresolved ripple sites) and `round < max_rounds` and budget allows, one engine message is injected (`[hx gate] round 1: 2 tests failed …`) built from filtered failures (v1 §7.3), capped at `max_feedback_tokens`; the loop continues. Non-actionable opens (denied, runner missing, no tests) are reported, never fed back more than once per session.
7. A **passing gate injects nothing** into the conversation: the model's final message stands and the status goes to the client only. This is what makes token overhead zero on correct work.

Early stop: if round N fails with the same failure signatures as round N−1, stop (no progress). Rounds count toward `max_turns` and cost budgets; gate wall time counts toward the turn's wall-time cap (v1 §4.6). If the remaining budget cannot cover the estimated run (last run duration of the same commands, else `round_timeout`), the obligation is `Open(SkippedBudget)`. The gate never pushes a turn past a budget.

### 3.3 Verify loop and impact-scoped selection (`hx-trust::verify`)
Taken from the verify-loop draft (A.2–A.7) with the ledger replacing its ad hoc ChangeSet. Trigger is the gate (turn end, not per edit batch, so multi-file changes are not tested half-done), plus the model-callable `verify` tool for TDD-style mid-turn runs.
```rust
pub struct ChangeSet { pub files: Vec<PathBuf>, pub symbols: Vec<SymbolChange>, pub bulk: bool }
pub enum TestTarget { File(PathBuf), Named { file: PathBuf, name: String }, Package(PathBuf) }
pub enum SelectReason { Direct, CoLocated, Ref { symbol: String, via: Option<String> }, Escalated, Contract }
pub struct Selection { pub targets: Vec<(TestTarget, SelectReason)>, pub escalated: bool, pub degraded: Option<String> }
pub trait LangRules: Send + Sync {                    // Rust, Python, TS/JS, Go in the first cut
    fn lang(&self) -> Lang;
    fn is_test_file(&self, p: &Path) -> bool;
    fn colocated(&self, p: &Path, idx: &dyn IndexQuery) -> Vec<TestTarget>;
    fn commands(&self, targets: &[TestTarget], cfg: &LangCfg) -> Vec<VerifyCommand>;
}
pub struct VerifyCommand { pub argv: String, pub cwd: PathBuf, pub timeout: Duration, pub scope: Scope }
impl Verifier {
    pub fn select(&self, cs: &ChangeSet, idx: &dyn IndexQuery) -> Selection;
    pub async fn run(&self, cmds: &[VerifyCommand], ledger: &mut Ledger, cx: &GateCx<'_>) -> Vec<EvidenceId>;
}
```
- **Selection:** (1) changed test files; (2) co-located tests per language table (draft A.3); (3) reference-reachable: `refs(name)` hits in test files for each changed symbol, one hop through non-test callers, ≤ 200 lookups (name-based over-selection is the safe direction); (4) escalate to package targets when > `max_targets` or a manifest/config file changed or `bulk`; (5) nothing found → `Open(NoTests)`. Targets are grouped into one command per package per runner.
- **Never guesses a command:** no matching `LangRules` and no configured template → `NoTests`.
- **Flaky tests:** failed tests rerun alone `retry_failed` times; pass-on-rerun = flaky, listed, not fed back, does not fail the gate (counts as `Proven` with a `flaky` note on the evidence).
- **Pre-existing failures** (ledger payoff, no extra runs): a failing test that already failed in ledger evidence *before the session's first write* is labelled `preexisting`, reported but not fed back and not counted as `Failed`. Without such evidence, a failure whose file and referenced symbols do not overlap the change set is labelled `possibly_preexisting` in feedback. Worktree baselines are deferred (§5).
- **Index not ready:** steps 3–4 degrade to steps 1–2 plus package targets; `Selection.degraded` says why.
- **Permissions:** every command is an `Exec` checked by the normal policy and command parser (v1 §5.5); no implicit allow. Templates from project config are repo-controlled and get no extra trust. Headless `ask` → deny → `Open(Denied)` = unproven, not failed. `hx trust allow` writes user-config allow rules for the detected runner commands on request.
- **Shadow mode (eval only):** `hx eval --trust-shadow` also runs the full suite after each gate round to compute selection recall and precision.

### 3.4 Done contracts (`hx-trust::contract`)
Explicit acceptance checks the turn must pass to be `proven`, in addition to impacted tests.
```rust
pub struct Contract { pub id: ContractId, pub check: Check, pub origin: Origin, pub scope: ContractScope }
pub enum Check { Command { argv: String, expect_exit: i32 }, Test { target: TestTarget } }
pub enum Origin { User, ProjectConfig, Model { proposal_id: ProposalId } }   // Model only after ApproveContract
pub enum ContractScope { NextTurn, Session }
```
- **Sources:** `hx -p --done "<cmd>"` / `/done <cmd>` / `SetContract` (user, default `NextTurn`); `[trust] done = [...]` in config (a project's standing definition of done, `Session` scope; project-config commands need an allow rule like any repo-controlled template); `propose_contract` tool, which the model may call at the start of a task to turn acceptance criteria in the prompt into checks → `ContractProposed` → binding only after `ApproveContract`. Headless rejects proposals unless `--approve-contracts` (mirrors v1 §3.4). Q5 decides which sources ship.
- **`Test` checks** require the named test to *exist and run* (≥ 1 matching test executed) and pass; this is how "add a test for X" becomes enforceable.
- **Anti-gaming:** the model cannot modify or clear contracts (only user commands can). Any `Write` to a test file that proving evidence depends on is flagged on the obligation and in the receipt (`proof depends on test files edited this session: tests/cfg.rs@turn 6`). Contract commands are recorded verbatim in the receipt so a reviewer sees exactly what "done" meant.

### 3.5 Ripple check (`hx-trust::ripple`)
Index-based, no LSP. Catches "changed a signature, forgot a call site".
```rust
pub struct RippleSite { pub symbol: SymbolKey, pub old_sig: String, pub new_sig: Option<String> /* None = removed/renamed */,
                        pub site: Location, pub since: Gen }
pub fn ripple(changes: &[SymbolChange], idx: &dyn IndexQuery, ledger: &Ledger, cap: usize) -> Vec<RippleSite>;
```
- **Detect:** a `SymbolChange` whose signature changed (whitespace-normalised) or which disappeared. Sites = `refs(name)` outside the edited ranges.
- **At edit time:** the executor appends a short note to the edit's tool result (≤ 10 sites, ≤ 200 tokens): `ripple: signature of parse_cfg changed; 4 call sites not updated yet: src/a.rs:12, …`. This guides the next edits instead of waiting for a failure.
- **Discharge** (at the gate): the site's file was written after `since` touching the site's line range; or the line no longer references the name; or, for compiled/typechecked languages (Rust, Go, TS with `tsc`), a fresh passing `Build`/`Typecheck`/`Test` evidence covers the site's package, because the compiler is the proof. Remaining sites are `Open(Unverified)`: fed back once, then reported; ripple alone never makes a turn `Failed` (refs are name-based and over-match).

### 3.6 Claim auditor and receipts (`hx-trust::audit`)
**Claims.** Deterministic, English-only pattern library over the final message, with a negation/hedge window ("not yet", "should", "I couldn't") and optional scope (crate/package/path). Unknown phrasing yields no claim; no side-model extraction in this cut.
```rust
pub enum ClaimKind { TestsPass { scope: Option<String> }, BuildOk, LintClean, TypecheckOk,
                     NoOtherRefs { symbol: String }, Edited { path: PathBuf }, CommandRan { program: String } }
pub struct Claim { pub kind: ClaimKind, pub span: Range<usize>, pub text: String }
pub enum Verdict { Verified(EvidenceId), Stale { ev: EvidenceId, writes_since: u32 }, Contradicted(EvidenceId), Unverified }
pub trait ClaimExtractor: Send + Sync { fn extract(&self, text: &str) -> Vec<Claim>; }
pub fn audit(claims: &[Claim], ledger: &Ledger) -> Vec<(Claim, Verdict)>;
```
Matching: `TestsPass{scope}` needs a fresh `Test` evidence covering scope with `failed == 0` and > 0 tests; `BuildOk`/`LintClean`/`TypecheckOk` the same for their class; `NoOtherRefs{s}` needs an `Observe` (`refs`/`grep`) of `s` after the last write touching a definition of `s`; `Edited{p}` needs a `Write` of `p` this segment; `CommandRan{prog}` an `Exec` of that program. Because the gate runs checks *before* auditing (3.2 step 4), a claim like "tests pass" is usually verified by the gate's own run. Modes: `feedback` (contradicted/stale claims enter the gate's fix round), `annotate` (reported only), `off`. Rendering: `[contradicted] "build succeeds" — cargo build at turn 11 exited 101`.

**Receipts.** `hx receipt [--session id] [--turns a..b] [--format json|md] [--git-note]` exports a ledger slice. JSON is canonical (RFC 8785); no file contents or command outputs, only hashes:
```json
{ "schema": "hx.receipt/1", "hx_version": "0.4.0", "session_id": "01JC…", "turns": [3, 7],
  "base": { "git_head": "9f2c1e0", "dirty": false }, "prev_chain": "b3:41aa…",
  "models": [{ "role": "main", "id": "anthropic/…" }, { "role": "side", "id": "…" }],
  "entries": [
    { "seq": 41, "turn": 5, "gen": 12, "kind": "write", "path": "src/cfg.rs", "pre": "b3:07d1…", "post": "b3:c9e4…",
      "via": "tool", "symbols": [{ "name": "parse_cfg", "sig_changed": true }], "chain": "b3:5b30…" },
    { "seq": 44, "turn": 6, "gen": 13, "kind": "exec", "source": "gate:1", "argv": "cargo test -p cfg",
      "class": "test", "outcome": "ok", "exit": 0, "counts": { "passed": 31, "failed": 0, "skipped": 0 },
      "output": "b3:aa02…", "chain": "b3:e81f…" } ],
  "obligations": [{ "kind": "tests", "label": "cfg (3 targets)", "discharge": "proven", "evidence": [44] },
                  { "kind": "contract", "label": "cargo clippy -- -D warnings", "origin": "user", "discharge": "proven", "evidence": [45] }],
  "claims": [{ "text": "all tests pass", "kind": "tests_pass", "verdict": "verified", "evidence": 44 }],
  "flags": ["proof depends on test files edited this session: none"],
  "status": "proven", "usage": { "tokens_in": 48210, "tokens_out": 3104, "cost_usd": 0.21 },
  "root": "b3:e81f…", "signature": null }
```
`hx receipt verify <file> [--at <rev>] [--rerun]`: recomputes the chain from `prev_chain` (first bad `seq` reported), compares the last `post` hash per path with the tree (or `<rev>`), and with `--rerun` re-executes `source: gate`/contract `exec` entries through the normal policy and compares outcomes. `--git-note` writes the JSON to `refs/notes/hx` on HEAD. Exit codes: 0 valid, 4 chain broken, 5 tree mismatch, 6 rerun mismatch. `argv` is included verbatim; receipts stay local unless the user exports them (secret redaction belongs to the later secret-firewall work).

### 3.7 Success labeller (`hx-trust::outcome`, type in `hx-session`)
Produces the per-task-segment label Economics consumes (learned skills §1.4, autopilot §2.2 of the drafts). A segment runs from one user message to the next.
```rust
// hx-session (data only)
pub struct SegmentOutcome { pub segment: SegmentId, pub label: OutcomeLabel, pub weight: f32,
                            pub signals: OutcomeSignals, pub final_: bool }
pub enum OutcomeLabel { Success, Failure, Unknown }
pub struct OutcomeSignals { pub finish: FinishReason, pub trust: Option<TrustStatus>, pub undone: bool,
                            pub corrected: bool, pub edits_survived: bool, pub cost_usd: Option<f64>, pub tokens: u64 }
// hx-trust
pub fn label(seg: &SegmentView, ledger: &Ledger, final_: bool) -> SegmentOutcome;
```
- **Failure:** finish ≠ `completed`; or trust `Failed`; or the segment's turns were undone (v1 §5.4); or corrected (the next segment's first 2 turns contain a `remember` proposal, a `Deny`, or writes restoring a segment file's `pre` hash); or segment edits did not survive to session end.
- **Success:** completed, not corrected, edits survived, and trust `Proven` (weight 1.0) or `Unproven`/`NotApplicable` (weight 0.5). `Unknown` for cancelled segments with no writes.
- **Two-phase:** a provisional record at segment end, a final record once the correction window closes or the session ends. Both append-only; consumers take the latest per segment. Eval worktree sessions are tagged and excluded from Economics by default.

### 3.8 Config
```toml
[trust]
verify = "on"            # "on" | "tool-only" | "off"          (Q1)
audit = "feedback"       # "feedback" | "annotate" | "off"     (Q6)
max_rounds = 2           # shared by tests, contracts, claims, ripple   (Q2)
max_feedback_tokens = 3000
round_timeout = "300s"   # per gate round; per-command default 120 s (as bash)
max_targets = 50
retry_failed = 1
ripple_max_sites = 10
done = []                # project definition of done, e.g. ["cargo clippy -- -D warnings"]
propose_contracts = true # registers propose_contract tool    (Q5)
[trust.lang.rust]   command = "cargo test -p {package} {filter} -- --quiet"   # overrides built-ins
[trust.lang.python] command = "pytest -q {files}"
```
When `verify != "off"`, one static line (≤ 60 tokens) joins the system prompt: hx runs impacted tests when you finish, so do not run the full suite yourself (Q9). It is in the stable prefix, so it is cached (v1 §4.5).

## 4. Error handling and testing

### 4.1 Failure modes
| Case | Behavior |
|---|---|
| No tests for the change | `Open(NoTests)` → `unproven`; told once per session; never invents a command |
| Runner missing (exit 127) | `Open(RunnerMissing)` → `unproven`; hint names the config key |
| Policy denial / headless `ask` | `Open(Denied)` → `unproven`, not `failed`; the blocking rule is shown |
| Timeout | process group killed (v1 §8); `Open(TimedOut)`; fed back once ("narrow or skip") |
| Flaky | rerun alone; pass-on-rerun = flaky note, not fed back |
| Compile failure | `Failed`; diagnostics via build filters; contracts skipped this round |
| Budget would be exceeded | `Open(SkippedBudget)`; never exceeds budgets |
| Cancellation during the gate | test processes killed within 200 ms; evidence so far is kept; `TurnFinished{cancelled}` with `trust: None` |
| Index not built | selection degrades (3.3); ripple skipped with a note |
| Gate bug (panic) | caught at the gate boundary; `Error` event; status `unproven`; the turn still ends normally |
| Session log write failure | as v1 §6.5 durability: the turn errors, the session stays resumable |
| Resume mid-gate | ledger replayed; an unfinished gate round is re-run from step 1 |
| Corrupt or edited receipt | `receipt verify` exits 4 with the first bad `seq` |

### 4.2 Testing
1. **Unit:** pattern library golden tests (≥ 150 phrases incl. negations and hedges), freshness and zero-tests rules, chain computation (proptest: any single-byte mutation of any entry is detected), selection per language, ripple discharge, labeller truth table, status precedence.
2. **Fixture repos:** one per language (Rust workspace, Python package, TS with vitest, Go module) with a known call graph and golden selections (changed file → expected targets with reasons), plus a monorepo fixture for escalation.
3. **Engine (`MockProvider` scripts):** edit → gate fails → fix → proven; fresh evidence reused (no command spawned, checked with a recording sandbox); max_rounds exhaustion and no-progress stop; flaky runner (fails once); preexisting failure; headless denial → unproven; budget skip; cancel mid-gate (no orphan processes); signature change → ripple note → site fixed; contradicted claim → feedback; contract proposed/approved/rejected; model edits a contract test file → flagged; Exec that modifies files (`sed -i`) stales prior evidence; crash and resume mid-gate.
4. **Protocol:** golden JSON for new events, `TurnFinished.trust`, exit code 3 under `--require-proven`, receipt schema.
5. **`hx eval` additions:** `--trust on|off` A/B, `--trust-shadow` (full-suite recall), and per-run reports of status, rounds, gate wall time and tokens, selection recall/precision, claim verdict mix, labeller agreement with hidden verification, receipt verify result. The starter set gains ~10 tasks built to tempt false "done" (a second failing test elsewhere, a signature change with distant callers, a requested test that must exist).
6. **CI gates (merge-blocking):** selection recall ≥ 90% on fixture replay; zero added tokens on round-1-pass turns (deterministic mock replay); receipt mutation detection; ledger append < 1 ms and post-Exec stat walk p95 < 50 ms on the generated 10k-file repo; all v1 §9.1 gates with Trust on.

## 5. Out of scope / deferred
Coverage-instrumented selection; type-aware references or LSP diagnostics; cross-session flake database; worktree baseline runs for pre-existing failures; parallel test sharding; per-edit-batch verification; forcing the model to write tests; side-model or non-English claim extraction; manual (non-executable) contract items; receipt signing (sigstore/SSH) and CI-status lookup; automatic git-note publishing; secret redaction of receipts; Windows-specific runners; anything from Economics beyond producing `SegmentOutcome`.

## 6. Open questions for the user
Each has a recommended default; the draft is written as if the defaults are accepted.
1. **Verify on by default?** *Recommend:* `on` in interactive **and** headless. It degrades safely: without an allow rule, headless gets `unproven`, not a failure or a hang.
2. **Max fix rounds?** *Recommend:* 2, shared across tests, contracts, claims and ripple, plus the no-progress early stop.
3. **Exit code for "completed but not proven"?** *Recommend:* opt-in `--require-proven` → exit 3 for `unproven|failed`; default exit codes keep their v1 meaning.
4. **Receipt signing?** *Recommend:* none in this cut (blake3 chain, tamper-evident only, stated plainly); reserve the `signature` field; SSH-key signing (`ssh-keygen -Y sign`) as the first follow-up.
5. **Who writes done contracts?** *Recommend:* both. User-written (flag, `/done`, config) always bind; model-proposed bind only after explicit approval and are rejected in headless unless `--approve-contracts`. Alternative: user-only in the first cut, model proposals after eval shows they help.
6. **Claim audit default:** *Recommend:* `feedback` (contradicted/stale claims use the shared fix round). Alternative: `annotate` until the false-alarm rate is measured below 5%.
7. **Exec side-effect detection:** *Recommend:* stat walk after every `Exec` (correct, ~tens of ms). Alternative: treat every non-test `Exec` as a write (zero cost, but many needless stale verdicts).
8. **Should passing turns run tests the model already ran?** *Recommend:* no, reuse fresh evidence (3.2 step 2). Saves time but trusts the model's chosen command scope as long as it covers the selection.
9. **System-prompt line telling the model not to run the full suite?** *Recommend:* yes; it should cut redundant test turns. Measure it in the A/B.
10. **Permission for hx-generated test commands:** *Recommend:* no implicit allow, even in `auto-edit` (test code is arbitrary code); provide `hx trust allow` to add explicit rules. Alternative: `auto-edit` implies allow for built-in templates only.
11. **No tests found:** *Recommend:* label `unproven` with no nudge to write tests (that is AGENTS.md policy). Alternative: nudge once when non-test code changed.
12. **Labeller weight for `unproven` success:** *Recommend:* 0.5, as in the learned-skills draft. Alternative: label `Unknown` so Economics learns only from proven work (cleaner, less data).
13. **Receipts in git:** *Recommend:* only on request (`--git-note`, `refs/notes/hx`), never automatic. Alternative: commit trailers referencing `root`.

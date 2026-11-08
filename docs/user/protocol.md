# Driving Tautly from other programs: `-p`, JSON-RPC and ACP

All three are plain clients of the same engine: they send it commands and read its
events, so they behave exactly like the REPL.

| Surface | Start | Use it for |
| --- | --- | --- |
| One-shot | `tautly -p "<prompt>" [--output text\|json]` | scripts, CI, benchmarks |
| JSON-RPC | `tautly --rpc` | your own front end, several sessions in one process |
| ACP | `tautly acp` | editors that speak the Agent Client Protocol (Zed) |

Diagnostics go to stderr; stdout carries only output (`-p`) or protocol frames (`--rpc`,
`acp`).

## One-shot (`-p`)

`tautly -p "fix the failing test"` sends one message, prints the model's text, shuts the
session down and exits. `--output json` prints every event instead, one JSON object per
line (the event types are listed [below](#events)). The session is headless: anything the
policy would ask about is denied unless `--approve auto-edit|yolo` allows it, and memory
proposals are rejected unless `--approve-memory`.

| Exit code | Meaning |
| --- | --- |
| 0 | the turn completed |
| 1 | an error |
| 2 | a budget, `max_turns` or `turn_timeout_s` stopped the turn |
| 130 | cancelled with Ctrl-C |
| 143 / 129 | SIGTERM / SIGHUP (after the session shut down) |

`--continue`, `--resume ID`, `--model`, `--reasoning`, `--budget-usd` and
`--no-checkpoints` apply as in the REPL.

## JSON-RPC (`--rpc`)

JSON-RPC 2.0 over stdin and stdout, one JSON message per line (lines over 8 MiB are
answered with an error and skipped). The server runs until the client sends `shutdown`
or closes its input (exit 0), or SIGTERM / SIGHUP arrives (143 / 129).

### Starting

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol":"1.0","client":{"name":"my-ui","version":"0.1","interactive":true}}}
{"jsonrpc":"2.0","id":1,"result":{"protocol":"1.0","session_id":"..."}}
```

- `initialize` must come first. A protocol whose major version is not 1 is refused.
- It opens the **default session** on the server's working tree (or resumes the session
  given with `--continue` / `--resume`); `"session": false` opens none.
- With `client.interactive: true`, the client answers approvals: a call the policy asks
  about emits `tool_approval_requested` and waits for `approve` / `deny`, and memory
  proposals wait for `approve_memory` / `reject_memory`. Otherwise the headless rules
  apply and nothing ever waits.

### Methods

Engine commands, with the command's fields as params. Each takes an optional
`session_id` (default: the default session, or the only open one). The result is `null`
once the session has the command; what it does arrives as events.

| Method | Params | Answered by |
| --- | --- | --- |
| `send_message` | `text`, `id?` | the turn's events, ending with `turn_finished` |
| `approve` | `request_id`, `session_rule?` | the call runs (a session rule must match the pending call) |
| `deny` | `request_id`, `reason?` | |
| `cancel` | | the turn ends `cancelled` |
| `set_budget` | `scope` (`turn` or `session`), `max_usd?`, `max_tokens?` | |
| `compact` | `id?` | `compact_finished` |
| `uncompact` | | undoes the latest compaction (a `warning` when it cannot) |
| `pin` / `unpin` | `text` | |
| `approve_memory` / `reject_memory` | `proposal_id` | |
| `consolidate_memory` | `id?` | `memory_proposed` events, then `consolidate_finished` |
| `set_model` | `model`, `id?` | `model_changed` or `error` |
| `set_reasoning` | `level` (`auto`, `off`, `low`, `medium`, `high`) | (no answer) |
| `diff` | `turn?`, `id?` | `session_diff` |
| `undo` | `turn?`, `dry_run?`, `id?` | `undone` or `error` |
| `fork` | `at?`, `label?`, `id?` | `session_changed` (the branch) |
| `rewind` | `at`, `label?`, `dry_run?`, `overwrite?`, `id?` | `rewound` (+ `session_changed`) |
| `switch` | `target` (the session to switch to), `dry_run?`, `overwrite?`, `id?` | `switched` (+ `session_changed`) |
| `label` | `label` | |
| `replay` | `target?` (the session to play back), `speed?`, `from_step?`, `id?` | `replayed` events, then `replay_finished` |

`id` is your correlation id, echoed on the command's answer (default: the JSON-RPC
request id). `switch` and `replay` name their target session `target`, because
`session_id` addresses the session the command is sent to.

Session management:

| Method | Params | Result |
| --- | --- | --- |
| `session/new` | `root` (absolute), `resume?`, `fork?: {session_id, at?, label?, worktree?, model?, dir?}` | `{session_id}`; with `fork.worktree: true`, `{session_id, worktree, note}` |
| `session/tree` | `root`, `session_id?` | `{nodes}`: the project's sessions as a tree |
| `session/list` | | `{sessions: [{session_id, root}]}` |
| `session/close` | `session_id` | `null`, once its last event is out |
| `shutdown` | | `null`, once every session has shut down; the server exits |

Sessions on the same work tree share its code index, file watcher and memory store.

### Events

Events arrive as notifications `{"method":"event","params":{"session_id":...,"event":{"type":...}}}`.
A session that stops by itself is announced with `session/closed {session_id}`. A session
that forks, rewinds or switches announces the new branch with `session_changed` under its
old id; the old id keeps working as an alias.

| Event | When |
| --- | --- |
| `turn_started` | a message started a turn |
| `text_delta`, `reasoning_delta` | streamed model output |
| `tool_call_proposed` | the model asked for a tool call |
| `tool_approval_requested` | the policy asks; carries `request_id`, `reason` and a `diff` preview for edits |
| `tool_started`, `tool_output_delta`, `tool_finished` | a call runs; `tool_finished` carries the output, effects, exit code and applied diff |
| `usage` | one provider request's tokens and cost (`role`: `main` or `side`) |
| `context_status` | how full the context is, by part |
| `compacted`, `context_collected`, `compact_finished` | context compaction and cleanup |
| `memory_proposed`, `consolidate_finished` | memory changes waiting for approval |
| `mcp_server_approval_requested`, `mcp_server_status` | MCP servers |
| `approval_withdrawn` | a pending approval request no longer waits (cancelled turn, shutdown) |
| `turn_receipt` | what the turn spent and saved, right before `turn_finished` |
| `turn_finished` | `reason`: `completed`, `cancelled`, `budget`, `max_turns`, `timeout` or `error`; `evidence`: the turn's last test run |
| `session_diff`, `undone`, `rewound`, `switched`, `model_changed`, `session_changed` | answers to commands |
| `replayed`, `replay_finished` | playback |
| `warning`, `error` | something to show the user; `error` has `kind`, `message`, `retryable`, `hint` |

Readers must ignore fields they do not know and treat unknown event types and enum values
as unknown, not as errors: additions are minor-version changes of the protocol (`1.x`).
The authoritative definitions are `Event` and `Command` in
`crates/tautly-core/src/protocol.rs` (`cargo doc -p tautly-core --open`).

### Error codes

| Code | Meaning |
| --- | --- |
| -32700 | invalid JSON |
| -32600 | valid JSON that is not a request |
| -32601 | no such method |
| -32602 | params that do not fit the method |
| -32000 | a session could not be created, or a command could not be handed over |
| -32001 | no open session has that `session_id` |
| -32002 | a method other than `initialize` before `initialize` |
| -32003 | `initialize` asked for an unsupported protocol major version |

## ACP (`tautly acp`)

Tautly speaks the Agent Client Protocol (wire `protocolVersion` 1) on stdin and stdout.
Zed, for example:

```json
{
  "agent_servers": {
    "Tautly": { "command": "tautly", "args": ["acp"] }
  }
}
```

Flags go before `acp` (`tautly --model openai/gpt-5 acp`); `--continue` and `--resume`
do not apply (the editor loads sessions).

- **Keys** come from the environment or the keychain; ACP authentication is not used. A
  missing key fails `session/new` with a setup hint (run `tautly auth add <provider>` or
  `tautly doctor`).
- **`session/new {cwd, mcpServers}`** opens a session at the git top level of `cwd`. MCP
  servers the editor passes are trusted like your own (stdio and HTTP; SSE servers are
  skipped with a notice). **`session/load`** resumes a session and replays its messages and
  tool calls first.
- **`session/prompt`** is a message. Text and embedded text resources are supported;
  images, audio and binary resources are refused. A prompt that is `/compact`, `/undo
  [N]`, `/diff [N]`, `/cost` or `/pin <text>` runs that command (they are advertised to
  the editor).
- **Approvals** arrive as `session/request_permission` with `allow_once`, `allow_always`
  (the exact-target session rule, see the [policy reference](policy.md#session-rules-allow-for-this-session))
  and `reject_once`; edits carry a diff. MCP server and memory approvals offer
  `allow_once` / `reject_once`.
- **`session/cancel`** cancels the turn; outstanding approvals are answered as denied.
- **`session/set_config_option`**: `model` switches the model (the editor lists the models
  whose provider has a key), `reasoning` sets the reasoning level.
- Usage is reported as `usage_update`; the turn receipt and test evidence as a `notice`
  when the editor supports `session.notices`.
- Tautly reads files from disk through its sandbox, so unsaved editor buffers are not
  seen. The editor's `fs/*` and `terminal/*` methods are not used.

Stop reasons: a completed turn, or one stopped by a budget or time limit (announced first),
is `end_turn`; `max_turns` is `max_turn_requests`; a cancelled turn is `cancelled`; an
engine error is a JSON-RPC error.

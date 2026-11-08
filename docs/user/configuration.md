# Configuration reference

Tautly reads TOML from up to three places and merges them in this order, later ones
winning:

1. **Built-in defaults.**
2. **The user file**: `config.toml` in the config home (below). Trusted.
3. **The project file**: `<work tree root>/.tautly/config.toml`. Repository content, so
   **untrusted**: it may only set some keys, and most of those only towards safer values
   (see [What a project file may set](#what-a-project-file-may-set)).
4. **Command-line flags** (`--model`, `--reasoning`, `--approve`, `--approve-memory`,
   `--budget-usd`, `--no-checkpoints`, `--no-footer`). They are yours and override every file.

Tables merge key by key; arrays and scalars replace. A missing file is skipped.

## Where the files are

| What | Path |
| --- | --- |
| Config home | `$TAUTLY_HOME` if set, else `$XDG_CONFIG_HOME/tautly`, else `~/.config/tautly` |
| User config | `<config home>/config.toml` (may be a symlink, e.g. from a dotfile manager) |
| Model catalog updates | `<config home>/models.toml` (written by `tautly models update`) |
| Project config | `<root>/.tautly/config.toml` (must be a regular file in a real `.tautly` directory, not a symlink) |
| Data home | `$TAUTLY_HOME` if set, else `$XDG_DATA_HOME/tautly`, else `~/.local/share/tautly` |

Both homes must resolve to absolute paths; a relative `TAUTLY_HOME` or `XDG_*` value is
refused (the user file is then skipped with a warning), since a relative path would make
repository content the trusted layer. Note that `TAUTLY_HOME` sets **both** homes to the
same directory.

The user file is strict: an unknown key or a bad value is an error naming the file. The
project file is lenient: what it may not set, unknown keys included, is dropped with a
warning, so a teammate's newer config does not break an older binary. A syntax error in
either file is always an error. Files over 256 KiB are refused.

## Top-level keys

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `model` | string | detected, else `anthropic/claude-sonnet-5-5` | The main model, as `<provider>/<model>`: `anthropic/…`, `openai/…`, `google/…` or `compat:<name>/…` |
| `reasoning` | `"auto"`, `"off"`, `"low"`, `"medium"`, `"high"` | `"auto"` | How hard the main model thinks. `auto` is `medium` for a model whose catalog entry can reason, else off. Budget-style models get 4k / 12k / 32k thinking tokens for low / medium / high; effort-style models get the effort level. Where thinking cannot be turned off (`effort_always`), `off` sends the lowest effort. Side-model requests never reason |
| `compaction_threshold` | number in (0, 1], or `0` | `0.8` | Share of the context window at which automatic compaction starts; `0` turns automatic compaction off |
| `max_turns` | integer ≥ 1 | `100` | Provider steps per user message |
| `turn_timeout_s` | integer ≥ 1 | none | Wall-clock limit per user message, in seconds |
| `approve_memory` | bool | `false` | Save memory the agent proposes without asking (`--approve-memory`) |
| `pins` | array of strings | `[]` | Text every request carries, never summarized (each at most 2,048 bytes) |

When no layer or flag sets `model` (and `providers.auto_detect` is on), Tautly detects a
provider; see [Providers](../../README.md#providers-and-keys).

## `[permissions]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `mode` | `"ask"`, `"auto-edit"` (or `"auto_edit"`), `"yolo"` | `"ask"` | What runs without asking |
| `allow` | array of rules | `[]` | Calls that run without asking |
| `deny` | array of rules | `[]` | Calls that never run |

Rules are `tool(glob)` or a bare `tool`. See the [policy reference](policy.md).

## `[budget.turn]` and `[budget.session]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `max_usd` | positive number | none | Stop once this many US dollars are spent |
| `max_tokens` | integer ≥ 1 | none | Stop once this many tokens are used |

`--budget-usd N` sets the session's `max_usd` for one run. A model without a catalog price
cannot be held to a USD budget (its cost is unknown, never counted as zero).

## `[providers]` (user file only)

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `auto_detect` | bool | `true` | Detect a provider when no layer or flag names a model |
| `anthropic`, `openai`, `google` | table | none | `base_url` (http or https URL) and `api_key_env` (the variable holding the key instead of `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or `GEMINI_API_KEY`) |
| `[[providers.compat]]` | array of tables | built-ins only | OpenAI-compatible Chat Completions endpoints: `name` (letters, digits, `-`, `_`, `.`; unique), `base_url`, optional `api_key_env` |

Two compat entries are built in unless you define an entry of the same name:
`openrouter` (`https://openrouter.ai/api/v1`, key in `OPENROUTER_API_KEY`) and `ollama`
(`http://localhost:11434/v1`, no key). A compat model id is `compat:<name>/<model>`, for
example `compat:ollama/qwen3-coder:30b`.

```toml
[providers.anthropic]
api_key_env = "WORK_ANTHROPIC_KEY"

[[providers.compat]]
name = "vllm"
base_url = "http://gpu-box:8000/v1"
api_key_env = "VLLM_KEY"
```

Keys never come from configuration: each provider's key is read from its environment
variable first, then from the OS keychain (service `tautly`, account = the provider name:
`anthropic`, `openai`, `google`, `compat:<name>`). Store one with `tautly auth add`.

## `[routing]` (user file only)

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `side_model` | model id | the catalog's `tier = "small"` model of the main provider, else the main model | Model for compaction summaries, memory episodes and consolidation, and `web_fetch` questions |

## `[catalog_overrides]` (user file only)

`[catalog_overrides.models."<provider>/<model>"]` entries replace the built-in catalog
entry of the same id, or add a model. An entry needs every capability field:

```toml
[catalog_overrides.models."compat:vllm/qwen3-coder"]
context_window = 131072
max_output = 16384
parallel_tools = true
reasoning = "none"          # none | budget | effort | effort_always
cache = "none"              # explicit | automatic | none
images = false
edit_format = "hashline"    # str_replace | patch | hashline
# optional:
# tier = "small"            # small | large
# cache_ttl_s = 300
# thinking_budget_max = 24576
[catalog_overrides.models."compat:vllm/qwen3-coder".price]   # USD per million tokens
input = 0.0
output = 0.0
cache_read = 0.0
cache_write = 0.0
```

`tautly models update [--from URL]` refreshes windows, output limits, image support and
prices from a models.dev-format file into `<config home>/models.toml`; entries there may
be partial for built-in models (the rest comes from the running version's built-in
entry), and your own `[catalog_overrides]` win over them. `tautly models list` prints the
effective catalog.

## `[mcp]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Start MCP servers at all |
| `timeout_s` | integer | `120` | Per tool call |
| `startup_timeout_ms` | integer | `5000` | How long the first request waits for servers still starting |
| `max_tools` | integer | `50` | Tools used per server |
| `import_mcp_json` | bool | `true` | Also read the repository root's `.mcp.json` (its servers need approval) |
| `[mcp.servers.<name>]` | table | none | One server: either `command` with optional `args`, `env` and `cwd` (relative to the work tree root), or `url` (http or https) with optional `headers` |

```toml
[mcp.servers.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "..." }

[mcp.servers.docs]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer ${DOCS_TOKEN}" }   # ${VAR}: user file only
```

Only `servers` may appear in a project file, and each such server needs your approval
before it starts. See [MCP](../../README.md#mcp-servers).

## `[evidence]` (user file only)

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `test_commands` | array of globs | `[]` | Commands (permission-rule glob syntax, matched per command component) that count as test runs for the evidence line, added to the built-in list (`cargo test*`, `pytest*`, `go test*`, `npm test*`, `jest*`, `vitest*`, `make test*`, …) |
| `replace_defaults` | bool | `false` | Use only `test_commands` |

## `[checkpoints]` (user file only)

Git snapshots of the work tree before every command and at every turn end, which undo and
time travel restore. See [time travel](time-travel.md).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Take checkpoints (`--no-checkpoints` turns them off for one run) |
| `keep_days` | integer ≥ 1 | `14` | Checkpoint refs older than this are deleted at engine start |
| `timeout_ms` | integer ≥ 1 | `3000` | Longest one snapshot may take; git is killed after it and the checkpoint is skipped |
| `max_untracked_mb` | integer | `200` | Skip the snapshot above this many MiB of untracked, non-ignored files |
| `max_untracked_files` | integer | `20000` | Skip the snapshot above this many untracked, non-ignored files |

## `[receipts]` (user file only)

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `footer` | bool | `true` | Print the receipt line after each REPL turn (`--no-footer` turns it off for one run) |

## `[telemetry]` (user file only)

Reserved. `enabled` (default `false`) and `endpoint` are parsed and ignored; nothing
leaves the machine. `enabled = true` only produces a warning.

## What a project file may set

A repository must not be able to loosen what you set up. In `.tautly/config.toml`:

| Key | Allowed |
| --- | --- |
| `model` | Only a model the trusted catalog knows *and* prices (otherwise a repository could make USD budgets unenforceable) |
| `reasoning` | Only lower than yours (`auto` counts as `medium`) |
| `permissions` | `mode` only stricter than yours (`ask` is the strictest); `deny` rules are added to yours; `allow` is dropped |
| `max_turns`, `turn_timeout_s`, `budget.*.max_usd`, `budget.*.max_tokens` | Only lower |
| `compaction_threshold` | Only lower, and only within 0.5–1 (it cannot disable compaction) |
| `pins` | Added after yours; each at most 2,048 bytes, and all together capped |
| `mcp.servers` | Kept apart from yours; each server needs approval before it starts |
| `providers`, `telemetry`, `approve_memory`, `catalog_overrides`, `routing`, `evidence`, `checkpoints`, `receipts`, other `mcp` keys | Never (dropped with a warning) |

A project value that would be rejected (a negative limit, a non-string model) is dropped
with a warning rather than stopping startup. The exception is `permissions`: a malformed
deny rule fails the load, because dropping it would loosen the policy.

## Other files Tautly reads

- `AGENTS.md` in the work tree root and its ancestors, outermost first, is put in the
  context; where a directory has no `AGENTS.md`, its `CLAUDE.md` is used instead. Items
  under a `Rules` or `Constraints` heading become pins.
- `.mcp.json` at the root (Claude-style `mcpServers`), when `mcp.import_mcp_json` is on.
- `.tautly/memory/*.md`: project memory (see [Memory](../../README.md#memory)).

## Example user file

```toml
model = "anthropic/claude-sonnet-5-5"
reasoning = "auto"

[permissions]
mode = "auto-edit"
allow = ["bash(cargo test*)", "bash(cargo clippy*)", "web_fetch(docs.rs)"]
deny = ["bash(git push*)", "bash(rm -*r*)"]

[budget.session]
max_usd = 5.0

[routing]
side_model = "anthropic/claude-haiku-5-5"
```

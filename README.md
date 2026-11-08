# Tautly

A fast, token-frugal AI coding agent for the terminal. Open source (Apache-2.0), written
in Rust, provider-neutral.

- **Any model:** Anthropic, OpenAI and Google natively, plus any OpenAI-compatible endpoint
  (OpenRouter, a local Ollama, vLLM, …). API keys only, from your environment or the OS
  keychain.
- **Fewer tokens per solved task:** a tree-sitter code index (`outline`, `symbol`,
  `refs`), re-reads answered with a diff, output filters for test and build tools, context
  cleanup and compaction that keep pinned constraints, code-anchored project memory, and a
  cheap side model for summaries.
- **Undo anything:** git checkpoints before every command, `/undo`, rewind to any step,
  fork into a separate worktree, replay, compare and bisect sessions.
- **Safe by default:** every write, command and network call asks unless your rules allow
  it; protected paths always ask; repository config can only tighten your settings.
- **Transparent cost:** a receipt after every turn, hard USD and token budgets, a local
  cost ledger.
- **One engine, three surfaces:** a line REPL, headless `-p` and JSON-RPC, and the Agent
  Client Protocol for editors.

Platforms: macOS and Linux. Tautly has no OS-level sandbox yet (see
[Permissions](#permissions)).

## Contents

- [Install](#install) · [Quick start](#quick-start) · [Providers and keys](#providers-and-keys)
- [The REPL](#the-repl) · [Headless use](#headless-use) · [Configuration](#configuration)
- [Permissions](#permissions) · [Time travel](#time-travel) · [Sessions](#sessions)
- [MCP servers](#mcp-servers) · [Memory](#memory) · [Editors (ACP) and JSON-RPC](#editors-acp-and-json-rpc)
- [Cost](#cost-receipts-and-budgets) · [Eval and benchmarks](#eval-and-benchmarks)
- [Troubleshooting](#troubleshooting) · [Command reference](#command-reference)

User guides: [configuration](docs/user/configuration.md) ·
[permission policy](docs/user/policy.md) · [time travel](docs/user/time-travel.md) ·
[`-p`, JSON-RPC and ACP](docs/user/protocol.md). Contributors: [CONTRIBUTING.md](CONTRIBUTING.md).

## Install

### From source

Needs Rust 1.90 or newer and `git`.

```sh
git clone <repo-url> tautly && cd tautly
cargo install --locked --path crates/tautly-cli    # the package is named `tautly`
tautly --version
```

### Prebuilt binaries (coming)

Nothing is published yet. From the first `v*` release on there will be binaries for macOS
and Linux on arm64 and x86_64 (the Linux ones static, musl), each with a `.sha256`
checksum, installable through a Homebrew tap, a shell installer and `cargo binstall`. The
exact commands will be added here once the repository and tap are public; see
[docs/release.md](docs/release.md) for how releases are built.

## Quick start

```sh
export ANTHROPIC_API_KEY=...       # or OPENAI_API_KEY, GEMINI_API_KEY, OPENROUTER_API_KEY
cd your-project                    # ideally a git repository
tautly doctor                      # checks keys, model, git, index, MCP
tautly                             # start the REPL
```

Type a request; the agent reads, searches, edits and runs commands, asking before each
write or command. Answer `y` (yes), `n` (no) or `a` (allow this exact call for the rest of
the session). After each turn a receipt shows steps, time, context use, cache hits and cost.

```sh
tautly -p "make the failing test pass" --approve auto-edit   # one headless turn
tautly -c                                                    # continue the last session here
```

## Providers and keys

| Provider | Model ids | Key variable | Keychain account |
| --- | --- | --- | --- |
| Anthropic | `anthropic/<model>` | `ANTHROPIC_API_KEY` | `anthropic` |
| OpenAI | `openai/<model>` | `OPENAI_API_KEY` | `openai` |
| Google | `google/<model>` | `GEMINI_API_KEY` | `google` |
| OpenRouter (built in) | `compat:openrouter/<model>` | `OPENROUTER_API_KEY` | `compat:openrouter` |
| Ollama (built in, local) | `compat:ollama/<model>` | none | |
| Any OpenAI-compatible endpoint | `compat:<name>/<model>` | `api_key_env` of its `[[providers.compat]]` entry | `compat:<name>` |

- A key is read from its environment variable first, then from the OS keychain (macOS
  Keychain, Linux Secret Service; service `tautly`). Store one with
  `tautly auth add <provider>` (hidden prompt; `--stdin` to pipe it). Keys never go in
  config files.
- **Choosing a model:** `--model <id>`, or `model = "<id>"` in a config file. When neither
  names one, Tautly picks the first provider it finds a key for, in this order: the
  `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` and `GEMINI_API_KEY` variables, the keychain
  entries for those providers, OpenRouter (variable, then keychain), and finally a local
  Ollama with a model pulled. It says what it chose on stderr. Turn this off with
  `[providers] auto_detect = false`.
- `tautly models list` shows the catalog: context window, output limit and price per
  million tokens of each model. A model missing from the catalog runs with conservative
  defaults and no price.
- **Reasoning:** `--reasoning auto|off|low|medium|high`, `reasoning = "…"` in config, or
  `/reasoning` in the REPL. `auto` (the default) is `medium` for models that can reason.
- Switch models mid-session with `/model <id>` (or `/model` to list the ones you have keys
  for).

Custom endpoints, key variable names, base URLs, the side model and catalog entries are
set in the user config: see [Configuration](docs/user/configuration.md#providers-user-file-only).

## The REPL

`tautly` on a terminal starts an interactive session in the work tree of the current
directory (its git top level). History is kept in `<data home>/repl_history`.

| Command | Does |
| --- | --- |
| `/model [id]` | switch model; without an id, list the current one and those with a key |
| `/reasoning <level>` | `auto`, `off`, `low`, `medium` or `high`, from the next request on |
| `/cost` | the last turn's and the session's receipt |
| `/diff [N]` | what the session (or user turn N) changed |
| `/undo [N]` | undo the last turn (or turn N), after showing what it would do |
| `/resume [n\|id]` | list this work tree's last sessions, or resume one |
| `/compact` | compact the conversation now |
| `/pin <text>` / `/unpin <text>` | keep text in every request / remove that pin |
| `/fork` | branch the conversation into a new session |
| `/rewind N` | rewind files and conversation to step N as a new branch |
| `/tree` | this project's sessions as a tree (`*` marks the current one) |
| `/switch <label\|id\|n>` | continue another branch, files included (`n`: its line in `/tree`) |
| `/label <name>` | rename the current branch |
| `/replay [id]` | play this session (or another) back from its log |
| `/memory consolidate` | propose merging and pruning project memory |
| `/help` | the list |

Keys: Ctrl+C cancels a turn (twice within 2 s exits) and clears the line at the prompt;
Ctrl+D on an empty line exits; Alt+Enter starts a new line.

Approvals show the tool, the whole call and, for edits, the diff, then
`allow? [y]es [n]o [a]lways this session`. `a` adds the narrowest rule that names this
call (`bash(cargo test -p core)`, `edit(src/lib.rs)`, …) for the rest of the session;
`a <rule>` gives your own pattern. Memory proposals and MCP server approvals take `y`/`n`.

## Headless use

```sh
tautly -p "explain src/main.rs"                          # prints the model's text
tautly -p "fix the lint errors" --approve auto-edit --budget-usd 0.50
tautly -p "run the tests and fix failures" --approve yolo --output json > events.jsonl
```

Headless, anything the policy would ask about is **denied** unless `--approve auto-edit`
(edits in the work tree) or `--approve yolo` (everything no deny rule or protected path
stops) allows it. Exit codes: 0 completed, 1 error, 2 budget / `max_turns` / timeout,
130 Ctrl-C, 143 SIGTERM, 129 SIGHUP. `--output json` prints every engine event as one
JSON object per line. Details: [protocol guide](docs/user/protocol.md).

## Configuration

Layered TOML; later layers win:

1. built-in defaults;
2. the user file `~/.config/tautly/config.toml` (`$XDG_CONFIG_HOME/tautly/`, or
   `$TAUTLY_HOME/`);
3. the project file `.tautly/config.toml` at the work tree root (untrusted: it can only
   tighten);
4. command-line flags.

| Key | Default | |
| --- | --- | --- |
| `model` | detected, else `anthropic/claude-sonnet-5-5` | `<provider>/<model>` |
| `reasoning` | `auto` | `auto`, `off`, `low`, `medium`, `high` |
| `compaction_threshold` | `0.8` | share of the window that triggers compaction; `0` = off |
| `max_turns` | `100` | provider steps per message |
| `turn_timeout_s` | none | wall-clock limit per message |
| `approve_memory` | `false` | save memory proposals without asking |
| `pins` | `[]` | text every request carries |
| `[permissions]` `mode`, `allow`, `deny` | `ask`, `[]`, `[]` | see [Permissions](#permissions) |
| `[budget.turn]`, `[budget.session]` `max_usd`, `max_tokens` | none | hard limits |
| `[providers]` `auto_detect`, `anthropic`, `openai`, `google`, `[[providers.compat]]` | detection on | endpoints and key variables (user file only) |
| `[routing]` `side_model` | the provider's small model | for summaries, memory and `web_fetch` questions (user file only) |
| `[catalog_overrides.models."<id>"]` | none | add or replace catalog entries (user file only) |
| `[mcp]` `enabled`, `timeout_s`, `startup_timeout_ms`, `max_tools`, `import_mcp_json`, `[mcp.servers.<name>]` | `true`, 120, 5000, 50, `true` | see [MCP servers](#mcp-servers) |
| `[evidence]` `test_commands`, `replace_defaults` | built-in list, `false` | what counts as a test run (user file only) |
| `[checkpoints]` `enabled`, `keep_days`, `timeout_ms`, `max_untracked_mb`, `max_untracked_files` | `true`, 14, 3000, 200, 20000 | git snapshots for undo (user file only) |
| `[receipts]` `footer` | `true` | the REPL's receipt line (user file only) |
| `[telemetry]` `enabled`, `endpoint` | `false` | reserved, ignored; nothing leaves your machine |

Every key, its rules and what a project file may set: [configuration reference](docs/user/configuration.md).

Tautly also reads `AGENTS.md` from the work tree root and the directories above it
(`CLAUDE.md` where a directory has none) into the context; items under a `Rules` or
`Constraints` heading become pins that survive compaction.

## Permissions

| Mode | Without asking | Asks |
| --- | --- | --- |
| `ask` (default) | reads and searches | writes, commands, network, MCP calls |
| `auto-edit` | + writes inside the work tree | commands, network, MCP calls |
| `yolo` | everything not denied | protected paths, unparseable commands |

```toml
[permissions]
mode = "auto-edit"
allow = ["bash(cargo test*)", "bash(git diff*)", "web_fetch(docs.rs)", "mcp__github__get_*"]
deny = ["bash(git push*)", "bash(rm -*r*)"]
```

Rules are `tool(glob)` or a bare `tool`; deny beats allow. Shell input is parsed so every
command in a pipeline or `&&` chain must pass, and write redirects are matched
structurally. Writing a protected path (`.git/`, `.tautly/`, `.github/workflows/`,
`.vscode/`, `.idea/`, `Makefile`, `package.json`, `Cargo.toml`, `build.rs`, `.envrc`,
shell rc files, …) always asks, in every mode. Nothing logged in a session ever counts as
approval. The full rules: [policy reference](docs/user/policy.md).

Tautly confines file access to the work tree and applies these rules itself; there is no
OS-level isolation yet, so commands you allow run with your user's rights. Keep `yolo`
for containers, VMs and throwaway clones.

## Time travel

In a git work tree Tautly snapshots the files (as commits under `refs/tautly/`, never
touching your index, branches or stash) before every command and at every turn end.
With the session log this lets you go back safely; nothing is ever truncated, and a file
you changed is never overwritten without asking.

| Want to | REPL | CLI |
| --- | --- | --- |
| Undo a turn | `/undo [N]` | `tautly undo [--turn N] [--dry-run] [--session ID]` |
| See what changed | `/diff [N]` | |
| Go back to step N as a new branch | `/rewind N` | |
| Branch the conversation | `/fork` | |
| Try another approach in a separate worktree | | `tautly fork <session> --at N [--model ID] [--prompt TEXT]` |
| List and switch branches | `/tree`, `/switch <label\|id\|n>`, `/label <name>` | `tautly sessions tree` |
| Watch a session again (no model calls) | `/replay [id]` | `tautly replay <session> [--speed X] [--from-step N]` |
| Compare 2–5 sessions | | `tautly compare <a> <b> [...] [--json]` |
| Find the step that broke a check | | `tautly bisect <session> --test "<cmd>" [--good N] [--bad M]` |

Outside git (or with `--no-checkpoints`) undo and rewind restore the agent's own edits but
not what commands changed. Details and safety rules: [time travel guide](docs/user/time-travel.md).

## Sessions

Every session is an append-only JSONL log under
`<data home>/sessions/<project-id>/` (data home: `~/.local/share/tautly`,
`$XDG_DATA_HOME/tautly` or `$TAUTLY_HOME`), shared by all worktrees of a repository.

```sh
tautly sessions list                 # this work tree's sessions, newest first
tautly sessions show <id>            # header and user turns, with step numbers
tautly sessions export <id> --format md|jsonl
tautly sessions delete <id>          # log, checkpoint refs, and a fork's worktree
tautly -c                            # continue the latest session here
tautly --resume <id>                 # resume a specific one (an id prefix works)
tautly prompt show [--full]          # the system prompt, or the last request as JSON
```

`tautly sessions restore-backup <id>` puts back files a cut-off rewind or switch kept in
the log.

## MCP servers

Tautly is an MCP client for tools (stdio and streamable HTTP). Servers come from:

- your user config (trusted):

  ```toml
  [mcp.servers.github]
  command = "npx"
  args = ["-y", "@modelcontextprotocol/server-github"]
  env = { GITHUB_TOKEN = "..." }

  [mcp.servers.docs]
  url = "https://mcp.example.com/mcp"
  headers = { Authorization = "Bearer ${DOCS_TOKEN}" }
  ```

- `tautly mcp import [--from PATH]`, which copies a Claude-style `mcpServers` file
  (default `~/.claude.json`, else the work tree's `.mcp.json`) into your user config;
- the repository: `[mcp.servers]` in `.tautly/config.toml` and the root's `.mcp.json`.
  These are **untrusted**: each needs your approval once (remembered in
  `<data home>/mcp-approvals.json`, keyed by the command line, environment and the files
  it names, so any change asks again). Headless they do not start unless `--approve-mcp`
  (this run only). Editors can pass servers over ACP too.

Tools appear as `mcp__<server>__<tool>`; calls ask by default (also in `auto-edit`), and
rules match by name (`mcp__github__*`). Servers never see your provider keys.
`tautly mcp list` shows each server's source, approval, state and tool count.

## Memory

- **Project memory** lives in `.tautly/memory/`, one Markdown file per entry (a
  convention, fact, gotcha or command, anchored to the code it is about), and opens as an
  Obsidian vault. The model proposes entries with its `remember` tool; you approve each
  (`y`/`n`; headless they are rejected unless `--approve-memory` or `approve_memory =
  true`). An index of entries goes into every request, and entries whose anchored code
  changed are marked stale. Commit the directory to share it with your team.
- **Episodes**: at each compaction and at session end, the side model writes a short
  private summary (task, outcome, what failed, key files) under the data home.
- The model searches both with its `recall` tool.

```sh
tautly memory list          # entries; stale ones are marked
tautly memory show <id>
tautly memory consolidate   # the side model proposes merges and deletions; you approve each
```

## Editors (ACP) and JSON-RPC

**Editors:** `tautly acp` speaks the [Agent Client Protocol](https://agentclientprotocol.com)
on stdio. In Zed's settings:

```json
{ "agent_servers": { "Tautly": { "command": "tautly", "args": ["acp"] } } }
```

The editor gets streaming text, tool calls with diffs, permission requests (allow once,
allow for the session, reject), `/compact`, `/undo`, `/diff`, `/cost` and `/pin`, model and
reasoning options, and usage. Flags such as `--model` go before `acp`.

**Your own client:** `tautly --rpc` serves JSON-RPC 2.0, one message per line on
stdin/stdout: engine commands are methods (`send_message`, `approve`, `undo`, `rewind`,
`fork`, …), engine events arrive as `event` notifications, and one process can run several
sessions. Methods, events and error codes: [protocol guide](docs/user/protocol.md).

## Cost, receipts and budgets

- After every REPL turn: `turn N · S steps · time · ctx used/window · cache % · $turn
  (session $total) · saved ~N tok (est.)`, plus the last test run and whether later edits
  made it stale. `/cost` shows it again; `[receipts] footer = false` or `--no-footer` hides
  it.
- `--budget-usd N` or `[budget.session] max_usd` / `[budget.turn]` stop a session or a turn
  at a hard limit (exit code 2 headless). Models without a catalog price report no cost and
  cannot be held to a USD budget.
- Every request is recorded in a local ledger (`<data home>/ledger.sqlite`):

  ```sh
  tautly stats [--since 7d|30d|all] [--by day|model|project] [--json]
  tautly receipt [--session ID] [--markdown]
  ```

The "saved" figures are estimates against a plain harness with the same 30 KB output cap.

## Eval and benchmarks

```sh
tautly eval --tasks evals/starter --model anthropic/claude-sonnet-5-5 --jobs 4 --trials 3 --out results.json
tautly eval report results.json other.json --markdown
```

`tautly eval` runs task directories (setup script, prompt, verify script) headless, each in
a private temporary work tree, and reports pass rate, cost, tokens, tokens per solved task,
cache hit ratio and edit failures as JSON. `evals/starter/` holds 50 small tasks in Python,
TypeScript, Rust and Go. **Task directories are trusted code** and the starter tasks run
with `approve = "yolo"`: run them in a container or VM. See [evals/README.md](evals/README.md),
and [docs/benchmark.md](docs/benchmark.md) for the methodology, the token benchmark
(`benchmarks/tokens/`) and the Terminal-Bench 2.0 adapter (`benchmarks/terminal-bench/`).

## Troubleshooting

Start with `tautly doctor`. It checks the configuration, the model and its key, network
access to each provider with a key (one model-list request, no tokens; `--offline` skips
it), the data directory, the sandbox, git, the code index and MCP servers, one line each
(`ok`, `warn`, `fail`, `info`), and exits 1 if anything fails. `--json` prints one object.

| Symptom | Fix |
| --- | --- |
| `no model provider found` | Set a key variable, `tautly auth add <provider>`, or run Ollama with a model pulled |
| `keychain unavailable` on Linux | No Secret Service is running; use the environment variable instead |
| `<name> not found on PATH outside the work tree` | Tautly never runs programs from inside the repository by bare name (an activated `.venv/bin` or direnv path is the usual cause); put the tool on `PATH` outside the work tree or use an absolute path |
| `checkpoint skipped: …` | Git is missing, the snapshot was too slow, or there are too many untracked files (see `[checkpoints]`); undo then covers only the agent's own edits |
| Headless run did nothing | Headless, every ask is denied: add `--approve auto-edit` or `--approve yolo`, or allow rules |
| A project setting was ignored | The warning names it: a project file may only tighten your settings ([why](docs/user/configuration.md#what-a-project-file-may-set)) |
| A repository MCP server did not start | Approve it in an interactive session, or pass `--approve-mcp` for one run |
| Code index looks wrong | `tautly index status`, then `tautly index rebuild` |
| Cost shows `?` | The model has no catalog price; add one with `[catalog_overrides]` |
| Checkpoint refs clutter `git log --all` | `git log --exclude='refs/tautly/*' --all` |

## Command reference

| Command | Purpose |
| --- | --- |
| `tautly` | the REPL |
| `tautly -p PROMPT [--output text\|json]` | one headless turn |
| `tautly --rpc` | JSON-RPC server on stdio |
| `tautly acp` | Agent Client Protocol server on stdio |
| `tautly auth add <provider> [--stdin]` | store a key in the OS keychain |
| `tautly doctor [--json] [--offline]` | check the setup |
| `tautly models list` / `models update [--from URL]` | the model catalog |
| `tautly sessions list\|show\|tree\|export\|restore-backup\|delete` | sessions |
| `tautly undo [--turn N] [--dry-run] [--session ID]` | undo a turn |
| `tautly fork <session> --at N [...]` | fork into a new worktree |
| `tautly replay <session> [...]` | play a session back |
| `tautly compare <a> <b> [...]` | compare sessions |
| `tautly bisect <session> --test CMD [...]` | find the failing step |
| `tautly memory list\|show\|consolidate` | project memory |
| `tautly index status\|rebuild` | the code index |
| `tautly mcp list\|import` | MCP servers |
| `tautly stats [...]` / `tautly receipt [...]` | spend and savings |
| `tautly prompt show [--full]` | the system prompt or the last request |
| `tautly eval [...]` / `tautly eval report FILE...` | run and compare eval tasks |

Flags shared by the REPL, `-p`, `--rpc` and `acp`: `--model ID`, `--reasoning LEVEL`,
`--approve auto-edit|yolo`, `--approve-memory`, `--approve-mcp`, `--budget-usd USD`,
`--continue` / `-c`, `--resume ID` (not with `acp`), `--no-checkpoints`, `--no-footer`
(REPL). Run `tautly <command> --help` for every option.

## License

Apache-2.0.

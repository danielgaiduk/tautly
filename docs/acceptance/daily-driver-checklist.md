# Daily-driver acceptance checklist (v1)

Copy this file to `docs/acceptance/<YYYY-MM-DD>-daily-driver.md`, work through it top to
bottom, and fill in each **Result** block. Each failure becomes an issue. Put its link in
the block and in the summary at the end.

Record first:

| Field | Value |
|---|---|
| Date | |
| Tester | |
| `tautly --version` | |
| OS and version | |
| Install method (release installer, Homebrew, `cargo install`, source) | |
| Models used (anthropic / openai / google / ollama) | |

Conventions:
- `$` lines are shell commands.
- PASS means every "Success looks like" bullet held.
- For a failure, keep the exact command, the exit code and the last ~30 lines of output,
  and run `tautly doctor` right after.

---

## 1. Zero-config first run, then `tautly doctor`

Goal: a clean account with no config file and only one env key gets a first answer, and
doctor is happy.

```sh
$ export TAUTLY_HOME="$(mktemp -d)"          # stands in for a clean account
$ ls "$TAUTLY_HOME"                          # empty: no config.toml
$ env | grep -E '_API_KEY=' | cut -d= -f1    # exactly one, e.g. ANTHROPIC_API_KEY
$ mkdir -p /tmp/tautly-first && cd /tmp/tautly-first && git init -q
$ tautly -p "Say hello in five words."; echo "exit=$?"
$ tautly doctor; echo "exit=$?"
```

Success looks like:
- stderr announces the provider and model it detected from the one key in the environment;
- an answer is printed and the first `exit=0`;
- doctor reports no failing check, and the second `exit=0`;
- no config file was needed.

**Result:** PASS / FAIL — notes, issue link:

---

## 2. One task driven from Zed through `tautly acp`

Add to Zed's `settings.json` (Zed → Settings → Open Settings):

```json
"agent_servers": {
  "Tautly": { "command": "tautly", "args": ["acp"] }
}
```

Then:
1. Open a real git repository in Zed.
2. Open the Agent panel, start a new thread and pick **Tautly**.
3. Ask for a small real change, for example "add a `--verbose` flag to the CLI and a test
   for it".
4. Approve the edit when Zed asks.

Success looks like:
- the thread streams text and tool calls;
- Zed shows the approval and the diff, and the file changes on disk after approval;
- cancelling a turn (stop button) stops it within a second or two;
- closing the thread or Zed leaves no `tautly acp` process behind:
  `$ pgrep -fl "tautly acp"` prints nothing.

**Result:** PASS / FAIL — notes, issue link:

---

## 3. `/undo` reverts a `bash` change in a git repo

```sh
$ cd "$(mktemp -d)" && git init -q && echo one > notes.txt && git add -A && git -c user.name=t -c user.email=t@t commit -qm init
$ tautly
> Use the bash tool (not edit) to run: echo two >> notes.txt && touch made-by-bash.txt
  (approve the bash call with y)
> /undo
  (Ctrl-D to exit)
$ cat notes.txt; ls; git status --short
```

Success looks like:
- before `/undo`, `notes.txt` has two lines and `made-by-bash.txt` exists;
- after `/undo`, `notes.txt` is `one`, `made-by-bash.txt` is gone and `git status --short`
  is empty;
- the user's git history is untouched: `git log --oneline` shows only `init`.

**Result:** PASS / FAIL — notes, issue link:

---

## 4. One real stdio MCP server used through a tool call

Use any real stdio server. The example uses the reference filesystem server (needs Node).

```sh
$ mkdir -p /tmp/mcp-files && echo "the secret is 7341" > /tmp/mcp-files/secret.txt
$ cat >> "${TAUTLY_HOME:-$HOME/.config/tautly}/config.toml" <<'EOF'
[mcp.servers.fs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp/mcp-files"]
EOF
$ tautly mcp list
$ tautly -p "Using the fs MCP server's tools, read secret.txt and tell me the secret." --output json > mcp-run.jsonl; echo "exit=$?"
$ grep -E '"tool_call_proposed"|"tool_finished"' mcp-run.jsonl
```

Success looks like:
- `tautly mcp list` shows `fs` as ready with a tool count above 0;
- the event stream has a `tool_call_proposed` whose `tool` starts with `mcp__fs__`, and a
  matching `tool_finished` with `"is_error":false`;
- the answer contains `7341`, and `exit=0`.

If the call is denied headless, approve it in the REPL instead (`tautly`, then ask the same
question).

**Result:** PASS / FAIL — server used, notes, issue link:

---

## 5. One real task per adapter in the REPL

On a real repository you work in, do one genuine task per adapter. Use a real task, not a
toy: a small bug fix, a test or a refactor. Start each one with:

```sh
$ tautly --model anthropic/claude-sonnet-5-5
$ tautly --model openai/gpt-5
$ tautly --model google/gemini-3-pro
$ tautly --model compat:ollama/qwen2.5-coder:7b     # local Ollama, model pulled
```

Swap in current model ids if these are outdated. In each session use at least: a message
that needs reads and an edit, an approval, `/diff`, `/cost`, and the receipt footer after
each turn.

Success looks like:
- the task is done, with the diff reviewed and accepted by you;
- no crash, hang, garbled output or unexplained error;
- the receipt shows tokens and cost (Ollama shows no cost, which is expected);
- `/cost` agrees roughly with the provider's dashboard for the cloud adapters.

| Adapter | Model | Repository and task | Result | Issue |
|---|---|---|---|---|
| anthropic | | | | |
| openai | | | | |
| google | | | | |
| compat:ollama | | | | |

---

## 6. Live smoke suite

From a checkout of this repository, with whichever keys you have:

```sh
$ export ANTHROPIC_API_KEY=... OPENAI_API_KEY=... GEMINI_API_KEY=...
$ cargo test -p tautly --features mock --test live_smoke -- --ignored --nocapture 2>&1 | tee live-smoke.log
$ grep -E '^(SKIPPED|PASSED|test result)' live-smoke.log
$ grep -A8 '^CONTINUITY' live-smoke.log
```

Success looks like:
- `test result: ok`, and every provider you have a key for shows `PASSED
  fix_failing_test` rather than `SKIPPED`;
- each `CONTINUITY` table has 3 or more steps.

How to read the continuity tables: see `docs/benchmark.md`, "Reasoning continuity". **If a
`*_reasoning_continuity` test fails** with "continuity lost", re-run it once. A second
failure means applying the documented `volatile_tail` placement for that adapter: the
working state goes inside the last tool result, with the last cache breakpoint on the block
before it. Open an issue for that adapter. Do not change adapters on a single failure.

| Test | anthropic | openai | google | ollama |
|---|---|---|---|---|
| fixes_failing_test | | | | |
| reasoning_continuity | | | | n/a |

Paste the `CONTINUITY` tables here:

```
```

---

## Summary

| # | Item | Result | Issue |
|---|---|---|---|
| 1 | Zero-config first run + doctor | | |
| 2 | Zed via `tautly acp` | | |
| 3 | `/undo` reverts bash change | | |
| 4 | Real stdio MCP server | | |
| 5 | One real task per adapter | | |
| 6 | Live smoke | | |

Accepted for daily use: yes / no

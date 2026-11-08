#!/usr/bin/env python3
"""Write the seed corpora (fuzz/corpus/<target>/) from the workspace's own test inputs.

Run from anywhere: `python3 fuzz/seed.py`. Re-running is harmless (files are named by
content hash). Keep the result small: libFuzzer grows its own corpus from these.
"""
import hashlib
import json
import pathlib
import re

FUZZ = pathlib.Path(__file__).resolve().parent
CRATES = FUZZ.parent / "crates"


def put(target, data):
    if isinstance(data, str):
        data = data.encode()
    d = FUZZ / "corpus" / target
    d.mkdir(parents=True, exist_ok=True)
    (d / hashlib.sha1(data).hexdigest()).write_bytes(data)


def unescape(lit):
    """A Rust string literal's body as text; None when its escapes are not JSON's."""
    try:
        return json.loads('"' + lit + '"', strict=False)
    except ValueError:
        return None


def rust_strings(path, call):
    """String literals passed straight to `call(` in a Rust file (plain and raw)."""
    text = path.read_text()
    out = []
    for m in re.finditer(re.escape(call) + r'\(\s*"((?:[^"\\]|\\.)*)"', text):
        if (t := unescape(m.group(1))) is not None:
            out.append(t)
    for m in re.finditer(re.escape(call) + r'\(\s*r#"(.*?)"#', text, re.S):
        out.append(m.group(1))
    return out


# 1. Shell parser and policy.
shell = set()
for f in ["tests/shell.rs", "tests/policy.rs", "src/shell.rs", "src/policy.rs"]:
    p = CRATES / "tautly-sandbox" / f
    shell.update(rust_strings(p, "parse_commands"))
    shell.update(rust_strings(p, "decide_cmd"))
    shell.update(rust_strings(p, "cmd"))
text = (CRATES / "tautly-sandbox/tests/shell.rs").read_text()
for m in re.finditer(r'"((?:[^"\\]|\\.)*ZZMARK(?:[^"\\]|\\.)*)"', text):
    if (t := unescape(m.group(1))) is not None:
        shell.add(t.replace("ZZMARK", "@"))
for s in sorted(shell):
    if s and len(s) < 300:
        put("shell_policy", s)
for s, pat in [
    ("cargo test > out.txt", "cargo test > *"),
    ("git status", "git *"),
    ("cargo test --release", "cargo test*"),
    ("rm -rf src > x", "* --version"),
]:
    put("shell_policy", s + "\0" + pat)
# Regressions the fuzzer found (see the tests in crates/tautly-sandbox/tests/shell.rs).
for s in ["A=1;FOX=2 time", "command -p", " < @ \nc", "p0>.", "a | ex=$ @", "[a = @ ]"]:
    put("shell_policy", s)

# 2. Session logs.
HEADER = ('{"type":"header","schema_version":1,"session_id":"s","created":'
          '{"secs_since_epoch":0,"nanos_since_epoch":0},"root":"/r","parent":null}')
B = "0" * 64


def rec(i, data):
    return json.dumps({"type": "record", "offset": i, "t": 0, "data": data})


bodies = [
    [{"type": "turn_start", "data": {"turn": 1, "text": "hi"}},
     {"type": "edit", "data": {"path": "a.txt", "pre_image": None, "post_image_blob": B, "step": 1}},
     {"type": "turn_end", "data": {"reason": "completed"}}],
    [{"type": "edit", "data": {"path": "../etc/passwd", "pre_image": None, "post_image_blob": None, "step": 1}},
     {"type": "edit", "data": {"path": "/abs", "pre_image": None, "post_image_blob": None, "step": 1}},
     {"type": "hologram", "data": {}},
     {"type": "edit", "data": {"bogus": 1}}],
    [{"type": "undone", "data": {"turn": 1, "files": [{"path": "a\u0000b", "from_blob": None, "to": None}], "backup_ref": None}},
     {"type": "writes_lost", "data": {"paths": ["x/./y", "x//y"], "late": True}},
     {"type": "restore_begun", "data": {"backup_ref": None, "paths": [".."]}}],
]
for body in bodies:
    lines = [HEADER] + [rec(i, d) for i, d in enumerate(body)]
    put("session_reader", "\n".join(lines) + "\n")
    put("session_reader", "\n".join(lines))  # torn tail
put("session_reader", HEADER + "\n")

# 3. Time travel: record op sequences (see fuzz_targets/timetravel.rs).
for seq in [
    [0, 1, 2, 4, 9],
    [0, 1, 3, 4, 129, 5, 6, 1, 2, 4, 7, 9],
    [0, 1, 2, 7, 128, 129, 129, 9, 0, 1, 2, 4, 10, 9],
    [0, 1, 255, 2, 4, 9, 0, 1, 2, 4, 9],
    [11, 0, 1, 2, 4, 12, 13, 9],
    # Regressions: a closer then a response event; a lone ToolCall record.
    [0, 0, 1, 8, 4, 0x31, 13, 2],
    [11, 0, 1, 2, 0x3F, 12, 2, 0, 1, 2, 5, 13, 8],
]:
    put("timetravel", bytes(seq))

# 4. Config: the TOML the config tests write.
cfg = (CRATES / "tautly-core/tests/config.rs").read_text()
tomls = [m.group(1) for m in re.finditer(r'r#"(.*?)"#', cfg, re.S) if "=" in m.group(1)]
user = 'model = "anthropic/claude-sonnet-4-5"\n[permissions]\nmode = "auto-edit"\ndeny = ["bash(rm -rf*)"]\n'
for t in tomls:
    put("config_project", "\0" + t)
    put("config_project", user + "\0" + t)
put("config_project", user + "\0" + '[permissions]\nmode = "yolo"\nallow = ["bash"]\ndeny = []\nmax_turns = 1000\n')

# 5. JSON-RPC and ACP.
rpc = [
    {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocol_version": 1}},
    {"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {}},
    {"jsonrpc": "2.0", "id": 3, "method": "send_message", "params": {"text": "hi"}},
    {"jsonrpc": "2.0", "id": 4, "method": "set_budget", "params": {"scope": "week", "max_usd": 1}},
    {"jsonrpc": "2.0", "id": 5, "method": "replay", "params": {"target": "x", "speed": 1e309}},
    {"jsonrpc": "2.0", "method": "cancel"},
    {"jsonrpc": "2.0", "id": 6, "method": "shutdown"},
]
acp = [
    {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}},
    {"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": "/tmp", "mcpServers": []}},
    {"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": "x", "prompt": [{"type": "text", "text": "hi"}]}},
    {"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "x"}},
    {"jsonrpc": "2.0", "id": 4, "method": "session/set_config_option", "params": {"sessionId": "x", "configId": "model", "value": "m"}},
]
stream = lambda msgs: "\n".join(json.dumps(m) for m in msgs) + "\n"
put("rpc_acp", b"\x00" + stream(rpc).encode())
put("rpc_acp", b"\x00" + b'{"jsonrpc":"2.0","method":"frobnicate"}\n[1,2]\n{"id":{}}\n')
put("rpc_acp", b"\x01" + stream(rpc).encode())
put("rpc_acp", b"\x02" + stream(acp).encode())
put("rpc_acp", b"\x03" + json.dumps([{"type": "text", "text": "/compact"}, {"type": "resource", "resource": {"uri": "file:///a", "text": "x"}}]).encode()
    + b"\x00--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n")
put("rpc_acp", b"\x03" + json.dumps([{"name": "s", "command": "node", "args": ["x.js"], "env": [{"name": "A", "value": "1"}]}]).encode()
    + b"\x00--- /dev/null\n+++ b/../evil\n@@ -0,0 +1 @@\n+x\n")

# 6. MCP launch files.
for words in [
    ["node", "scripts/mcp.js"],
    ["python", "-m", "pkg.sub.tool"],
    ["python3", "-um", "pkg.sub"],
    ["sh", "-c", "cd scripts && node mcp.js"],
    ["npm", "run", "mcp"],
    ["env", "-u", "PYTHONPYCACHEPREFIX", "python", "-m", "pkg"],
    ["node", "--require=./scripts/mcp.js", "-r./x.js"],
]:
    put("mcp_launch", "\0".join(words))
put("mcp_launch", "node\0x.js\x01NODE_OPTIONS=--require ./scripts/mcp.js\0PYTHONPATH=.")
# Regression: a module deeper than any package (its candidates grew quadratically).
put("mcp_launch", "python\0-m\0" + "a." * 2000 + "a")

# 7. Escaping.
for s in ["plain", "a\\b", "\x1b[2J\r\nx", "‮evil‬", "tag\U000e0041", "\t​﻿"]:
    put("escaping", s)

# 8. Edit appliers: text, old, new, patch.
put("edit_apply", "fn a() {\n    one();\n}\n\0    one();\n\0    two();\n\0")
put("edit_apply", "x\ny\nz\n\0Y\0Q\0*** Begin Patch\n*** Update File: a.txt\n@@\n x\n-y\n+Y\n z\n*** End Patch\n")
put("edit_apply", "a\nb\nc\n\0" + "1:0000 2:0000" + "\0new\0--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n")

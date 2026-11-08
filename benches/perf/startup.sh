#!/usr/bin/env bash
# Start-up gates (spec 9.1), run from the repository root after
# `cargo build --release -p tautly --features mock`:
#   - cold start, median of 40 runs: `tautly --version` (hyperfine -N) and `tautly -p` on
#     the scripted provider to its first event (first_event.py);
#   - relative gate: with TAUTLY_BASELINE_BIN (the merge-base's binary, built the same
#     way), both medians must stay within 1.10x + 2 ms of the baseline's, measured in the
#     same run so they share the runner's conditions;
#   - the absolute 50 ms bound is reported, and enforced only on Linux with
#     TAUTLY_ENFORCE_ABSOLUTE=1 (off until CI shows the runners have the headroom);
#   - idle RSS of `tautly --rpc` 2 s after `initialize` under 30 MB;
#   - the release binary's size, reported only.
# Results go to stdout and, on GitHub Actions, to the job summary.
set -euo pipefail

abs() { echo "$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"; }
BIN=$(abs "${TAUTLY_BIN:-target/release/tautly}")
BASE=""
if [ -n "${TAUTLY_BASELINE_BIN:-}" ]; then
  if [ -x "$TAUTLY_BASELINE_BIN" ]; then
    BASE=$(abs "$TAUTLY_BASELINE_BIN")
  else
    echo "no baseline binary at $TAUTLY_BASELINE_BIN: the relative gate is skipped" >&2
  fi
fi
HERE=$(cd "$(dirname "$0")" && pwd)
HELLO=$HERE/hello.json
OUT=${PERF_OUT:-$PWD/target/perf}
RUNS=40
mkdir -p "$OUT"
fail=0

report() {
  echo "$1"
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then echo "- $1" >>"$GITHUB_STEP_SUMMARY"; fi
}
ms() { python3 -c "print(f'{float(\"$1\") * 1000:.1f}')"; }

# A scratch project and data home: nothing of the user's is read or written.
work=$(mktemp -d)
home=$(mktemp -d)
trap 'rm -rf "$work" "$home"' EXIT
export TAUTLY_HOME="$home" HOME="$home"
unset XDG_CONFIG_HOME XDG_DATA_HOME
cd "$work"

# `--version`: median of 40 (no shell in between).
bins=("$BIN")
[ -n "$BASE" ] && bins+=("$BASE")
if command -v hyperfine >/dev/null; then
  cmds=()
  # Quoted: -N splits the command as a shell would, and the paths may hold spaces.
  for b in "${bins[@]}"; do cmds+=("'$b' --version"); done
  hyperfine -N --warmup 5 --runs $RUNS --export-json "$OUT/version.json" "${cmds[@]}"
  version=$(python3 -c 'import json,sys; print(" ".join(str(r["median"]) for r in json.load(open(sys.argv[1]))["results"]))' "$OUT/version.json")
else
  # shortcut: without hyperfine (local runs), python times the same 40 runs; CI installs hyperfine.
  echo "hyperfine not found: timing --version with python3" >&2
  version=$(python3 - "${bins[@]}" <<'PY'
import statistics, subprocess, sys, time
out = []
for b in sys.argv[1:]:
    for _ in range(5):
        subprocess.run([b, "--version"], check=True, capture_output=True)
    s = []
    for _ in range(40):
        t = time.perf_counter()
        subprocess.run([b, "--version"], check=True, capture_output=True)
        s.append(time.perf_counter() - t)
    out.append(str(statistics.median(s)))
print(" ".join(out))
PY
)
fi
# `-p` to its first event, alternating the binaries.
oneshot=$(python3 "$HERE/first_event.py" "$HELLO" $RUNS "${bins[@]}")

gate() { # name candidate [baseline]
  local name=$1 cand=$2 base=${3:-}
  local line="startup: $name median $(ms "$cand") ms"
  if [ -n "$base" ]; then
    local limit
    limit=$(python3 -c "print(float('$base') * 1.10 + 0.002)")
    line="$line (baseline $(ms "$base") ms, limit $(ms "$limit") ms)"
    if python3 -c "import sys; sys.exit(0 if float('$cand') <= float('$limit') else 1)"; then
      line="$line ok"
    else
      line="$line FAILS the relative gate"
      fail=1
    fi
  fi
  if python3 -c "import sys; sys.exit(0 if float('$cand') <= 0.050 else 1)"; then
    line="$line; within 50 ms"
  elif [ "${TAUTLY_ENFORCE_ABSOLUTE:-0}" = 1 ] && [ "$(uname)" = Linux ]; then
    line="$line; FAILS the 50 ms bound"
    fail=1
  else
    line="$line; over 50 ms (reported, not enforced)"
  fi
  report "$line"
}
read -r v_cand v_base <<<"$version"
read -r p_cand p_base <<<"$oneshot"
gate "tautly --version" "$v_cand" "${v_base:-}"
gate "tautly -p (mock) to first event" "$p_cand" "${p_base:-}"

# Idle RSS: the server with one session, 2 s after `initialize`.
mkfifo "$work/in"
"$BIN" --rpc --mock-script "$HELLO" <"$work/in" >"$work/rpc.out" 2>"$work/rpc.err" &
pid=$!
exec 3>"$work/in"
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' >&3
sleep 2
rss_kb=$(ps -o rss= -p "$pid" | tr -d ' ')
exec 3>&-
wait "$pid" || true
grep -q '"id":1' "$work/rpc.out" || { echo "no initialize response:"; cat "$work/rpc.out" "$work/rpc.err"; exit 1; }
rss_mb=$(python3 -c "print(f'{$rss_kb / 1024:.1f}')")
if [ "$rss_kb" -le $((30 * 1024)) ]; then
  report "idle RSS: tautly --rpc after initialize $rss_mb MB (gate 30 MB)"
else
  report "idle RSS: tautly --rpc after initialize $rss_mb MB FAILS the 30 MB gate"
  fail=1
fi

report "binary size: $(ls -l "$BIN" | awk '{print $5}') bytes"
exit $fail

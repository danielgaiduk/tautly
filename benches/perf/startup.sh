#!/usr/bin/env bash
# Start-up gates (spec 1.6, 9.1), run from the repository root after
# `cargo build --release -p tautly --features mock`:
#   - cold start: mean of `tautly --version` and of a one-shot `-p` on the scripted provider
#     (`--mock-script`, which skips provider detection) under 50 ms each (hyperfine);
#   - idle RSS of `tautly --rpc` 2 s after `initialize` under 30 MB;
#   - the release binary's size, reported only.
# Results go to stdout and, on GitHub Actions, to the job summary.
set -euo pipefail

BIN=$(cd "$(dirname "${TAUTLY_BIN:-target/release/tautly}")" && pwd)/$(basename "${TAUTLY_BIN:-target/release/tautly}")
HELLO=$(cd "$(dirname "$0")" && pwd)/hello.json
OUT=${PERF_OUT:-$PWD/target/perf}
mkdir -p "$OUT"
fail=0

report() {
  echo "$1"
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then echo "- $1" >>"$GITHUB_STEP_SUMMARY"; fi
}

# A scratch project and data home: nothing of the user's is read or written.
work=$(mktemp -d)
home=$(mktemp -d)
trap 'rm -rf "$work" "$home"' EXIT
export TAUTLY_HOME="$home" HOME="$home"
unset XDG_CONFIG_HOME XDG_DATA_HOME
cd "$work"

# Run through a shell by hyperfine: quoted, as the paths may hold spaces.
version="'$BIN' --version"
oneshot="'$BIN' -p hi --mock-script '$HELLO' --output json"
if command -v hyperfine >/dev/null; then
  hyperfine --warmup 3 --export-json "$OUT/startup.json" "$version" "$oneshot"
  means=$(python3 -c 'import json,sys; print(" ".join(str(r["mean"]) for r in json.load(open(sys.argv[1]))["results"]))' "$OUT/startup.json")
else
  # shortcut: without hyperfine (local runs), the mean of 20 timed runs after 3 warm-ups; CI installs hyperfine.
  echo "hyperfine not found: timing 20 runs per command with python3" >&2
  means=$(python3 - "$version" "$oneshot" <<'PY'
import subprocess, sys, time
out = []
for cmd in sys.argv[1:]:
    for _ in range(3):
        subprocess.run(cmd, shell=True, check=True, capture_output=True)
    t = time.perf_counter()
    for _ in range(20):
        subprocess.run(cmd, shell=True, check=True, capture_output=True)
    out.append(str((time.perf_counter() - t) / 20))
print(" ".join(out))
PY
)
fi
read -r mean_version mean_oneshot <<<"$means"
for pair in "tautly --version:$mean_version" "tautly -p (mock):$mean_oneshot"; do
  name=${pair%%:*}
  mean=${pair##*:}
  ms=$(python3 -c "print(f'{float(\"$mean\") * 1000:.1f}')")
  if python3 -c "import sys; sys.exit(0 if float('$mean') <= 0.050 else 1)"; then
    report "startup: $name mean $ms ms (gate 50 ms)"
  else
    report "startup: $name mean $ms ms FAILS the 50 ms gate"
    fail=1
  fi
done

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

report "binary size: $(ls -l "$BIN" | awk '{print $5}') bytes ($BIN)"
exit $fail

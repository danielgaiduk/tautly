#!/usr/bin/env bash
# Terminal-Bench 2.0 with Tautly: TRIALS attempts per task (default 5) on one fixed model.
# Makes real provider calls; for the user to run, never CI. See README.md.
#
#   TAUTLY_BINARY=/path/to/linux/tautly ANTHROPIC_API_KEY=... benchmarks/terminal-bench/run.sh
#
# Variables: TAUTLY_BINARY (required), TAUTLY_TB_MODEL (default
# anthropic/claude-sonnet-5-5), TRIALS (5), CONCURRENCY (4), OUT (results/<date>).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
model="${TAUTLY_TB_MODEL:-anthropic/claude-sonnet-5-5}"
trials="${TRIALS:-5}"
concurrency="${CONCURRENCY:-4}"
out="${OUT:-$here/results/$(date +%Y-%m-%d)}"
: "${TAUTLY_BINARY:?set TAUTLY_BINARY to a Linux tautly release binary}"
export TAUTLY_BINARY

if ! command -v harbor >/dev/null; then
  echo "harbor not found: pip install 'harbor==0.25.0' (the version README.md pins)" >&2
  exit 1
fi

mkdir -p "$out"
PYTHONPATH="$here${PYTHONPATH:+:$PYTHONPATH}" harbor run \
  --dataset terminal-bench@2.0 \
  --agent tautly_agent:TautlyAgent \
  --model "$model" \
  --n-attempts "$trials" \
  --n-concurrent "$concurrency" \
  --jobs-dir "$out" \
  --job-name "tautly-${model//\//_}"
echo "results: $out" >&2

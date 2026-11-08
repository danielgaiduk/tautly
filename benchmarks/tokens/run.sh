#!/usr/bin/env bash
# The token benchmark (spec 9.3): `tautly eval` on the starter set with fixed models and
# trials. Raw results go to benchmarks/tokens/results/<date>/<model>.json, and the table
# goes to results.md beside them.
#
# This makes real, billed model calls: it needs the provider keys (environment or
# keychain) and costs money. CI never runs it; it only runs `tautly eval report` on
# fixtures/sample.json.
#
# Usage: benchmarks/tokens/run.sh   (TAUTLY=path/to/tautly to use another binary)
set -euo pipefail

MODELS=(
  anthropic/claude-sonnet-5-5
  openai/gpt-5
  google/gemini-3-pro
)
TRIALS=3
JOBS=4

root=$(cd "$(dirname "$0")/../.." && pwd)
tautly=${TAUTLY:-tautly}
out="$root/benchmarks/tokens/results/$(date -u +%Y-%m-%d)"
mkdir -p "$out"

for model in "${MODELS[@]}"; do
  file="$out/$(printf '%s' "$model" | tr '/:' '__').json"
  echo "== $model -> $file"
  "$tautly" eval --tasks "$root/evals/starter" --model "$model" \
    --trials "$TRIALS" --jobs "$JOBS" --out "$file"
done

"$tautly" eval report "$out"/*.json --markdown > "$out/results.md"
cat "$out/results.md"

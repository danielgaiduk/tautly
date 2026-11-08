"""Token and cost totals from a `tautly --output json` event stream.

Kept apart from tautly_agent.py so it runs without Harbor installed. Self-check:
`python3 -m doctest benchmarks/terminal-bench/tautly_usage.py`.

Mapping to Harbor's AgentContext: Tautly's `tokens_in` excludes cache reads, Harbor's
`n_input_tokens` includes them, so n_input = sum(tokens_in + cache_read),
n_cache = sum(cache_read), n_output = sum(tokens_out). Main and side roles both count.
`cost_usd` is None when no event carried a price.

>>> lines = [
...     '{"type":"session_changed","session_id":"s"}',
...     '{"type":"usage","role":"main","tokens_in":100,"tokens_out":20,"cache_read":900,"cache_write":0,"cost_usd":0.01}',
...     'not json',
...     '{"type":"usage","role":"side","tokens_in":50,"tokens_out":5,"cache_read":0,"cache_write":0,"cost_usd":null}',
...     '{"type":"turn_finished","reason":"completed"}',
... ]
>>> summarize(lines)
{'n_input_tokens': 1050, 'n_cache_tokens': 900, 'n_output_tokens': 25, 'cost_usd': 0.01, 'finish_reason': 'completed'}
>>> summarize([])['cost_usd'] is None
True
"""

import json


def summarize(lines):
    tin = cache = tout = 0
    cost = None
    reason = None
    for line in lines:
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        if not isinstance(ev, dict):
            continue
        if ev.get("type") == "usage":
            tin += ev.get("tokens_in") or 0
            cache += ev.get("cache_read") or 0
            tout += ev.get("tokens_out") or 0
            if ev.get("cost_usd") is not None:
                cost = (cost or 0.0) + ev["cost_usd"]
        elif ev.get("type") == "turn_finished" and reason is None:
            reason = ev.get("reason")
    return {
        "n_input_tokens": tin + cache,
        "n_cache_tokens": cache,
        "n_output_tokens": tout,
        "cost_usd": cost,
        "finish_reason": reason,
    }

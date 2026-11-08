"""Tautly as a Harbor installed agent for Terminal-Bench 2.0 (Harbor 0.25.0, see README.md).

Use: harbor run -d terminal-bench@2.0 -a tautly_agent:TautlyAgent -m <provider/model>
with this directory on PYTHONPATH and TAUTLY_BINARY naming a Linux release binary.

install() only copies that binary into the container. run() executes
`tautly -p <instruction> --approve yolo --output json --model <model>` and keeps the event
stream in the agent logs, from which populate_context_post_run() fills the token counts.
"""

import os
import shlex

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from tautly_usage import summarize

# Forwarded from the host environment when set; never passed on a command line.
KEY_VARS = ("ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GEMINI_API_KEY", "OPENROUTER_API_KEY")
EVENTS = "tautly.jsonl"
STDERR = "tautly.stderr"
EXIT = "tautly.exit"
UPLOAD = "/tmp/tautly-upload"
BIN = "/usr/local/bin/tautly"


class TautlyAgent(BaseInstalledAgent):
    @staticmethod
    def name() -> str:
        return "tautly"

    async def install(self, environment: BaseEnvironment) -> None:
        binary = os.environ.get("TAUTLY_BINARY")
        if not binary or not os.path.isfile(binary):
            raise ValueError("set TAUTLY_BINARY to a Linux tautly release binary")
        await environment.upload_file(binary, UPLOAD)
        # `install` is a copy that also sets the mode; nothing else runs at install time.
        await self.exec_as_root(environment, command=f"install -m 0755 {UPLOAD} {BIN}")

    @with_prompt_template
    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        if not self.model_name:
            raise ValueError("pass a model with -m <provider/model>")
        logs = self.environment_logs_dir
        env = {k: os.environ[k] for k in KEY_VARS if os.environ.get(k)}
        cmd = (
            f"{BIN} -p {shlex.quote(instruction)} --approve yolo --output json"
            f" --model {shlex.quote(self.model_name)}"
            f" > {shlex.quote(str(logs / EVENTS))} 2> {shlex.quote(str(logs / STDERR))};"
            # A budget or turn-limit exit (2) is a failed task, not a broken harness run.
            f" echo $? > {shlex.quote(str(logs / EXIT))}"
        )
        await self.exec_as_agent(environment, command=cmd, env=env)

    def populate_context_post_run(self, context: AgentContext) -> None:
        events = self.logs_dir / EVENTS
        if not events.exists():
            return
        totals = summarize(events.read_text(errors="replace").splitlines())
        context.n_input_tokens = totals["n_input_tokens"]
        context.n_cache_tokens = totals["n_cache_tokens"]
        context.n_output_tokens = totals["n_output_tokens"]
        context.cost_usd = totals["cost_usd"]
        exit_file = self.logs_dir / EXIT
        context.metadata = {
            **(context.metadata or {}),
            "tautly_finish_reason": totals["finish_reason"],
            "tautly_exit": exit_file.read_text().strip() if exit_file.exists() else None,
        }

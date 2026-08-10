"""Claw-SWE-Bench adapter for sirbone.

Install into a checkout of https://github.com/opensquilla/claw-swe-bench as
``claw_swebench/claws/sirbone.py`` (``install.sh`` does this and registers it).

Contract with the benchmark core:
  * the runtime is a **static musl** sirbone bind-mounted read-only, exactly
    like the upstream ZeroClaw adapter — SWE-bench task images span many glibc
    versions and a dynamically linked binary dies with ``GLIBC_x.y not found``;
  * sirbone edits ``/testbed`` in place. It never emits a patch: the runner
    collects one with ``git diff`` after the process exits;
  * every model call happens inside the container against the provider env
    forwarded here. No provider env forwarded == every task fails, so a missing
    key raises instead of quietly producing 0-token runs.

Benchmark rules this adapter enforces on the sirbone side:
  * **no network answers** — sirbone's ``web_search``/``web_fetch`` tools are
    ablated off, otherwise the agent can read the upstream issue and its fix;
  * **no state inside the workspace** — ``HOME`` is pinned outside ``/testbed``
    so sessions, snapshots and HISTORIA cannot leak into ``git.patch``.
"""

import json
import os
import subprocess
import time
from pathlib import Path

from claw_swebench.claws.base import BaseClawAdapter, decode_output
from claw_swebench.types import AgentResult

SIRBONE_BIN = os.environ.get("SIRBONE_BIN", "")

REPO_DIR = "/testbed"
CONTAINER_BIN = "/usr/local/bin/sirbone"
# sirbone state (~/.sirbone: sessions, snapshots, project store) must land
# outside the work tree the runner diffs.
CONTAINER_HOME = "/opt/sirbone-home"

# Provider/runtime env forwarded from the host when set.
FORWARDED_ENV = (
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "SIRBONE_CONTEXT_WINDOW",
    "SIRBONE_THINKING_BUDGET",
)
PROVIDER_KEYS = (
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
)

# Kill grace after the runner's per-instance budget, so a hung agent cannot
# still be writing to /testbed while the patch is collected.
SUBPROCESS_TIMEOUT_BUFFER = 120


def _resolve_bin() -> str:
    """Host path of the static sirbone binary, or raise with the fix."""
    if not SIRBONE_BIN:
        raise RuntimeError(
            "SIRBONE_BIN is unset — point it at a static musl sirbone "
            "(bench/claw/build_arms.sh builds and verifies one)"
        )
    if not os.access(SIRBONE_BIN, os.X_OK):
        raise RuntimeError(f"SIRBONE_BIN is not an executable file: {SIRBONE_BIN}")
    return SIRBONE_BIN


class SirBoneAdapter(BaseClawAdapter):
    name = "sirbone"

    def container_run_args(self, instance_id: str) -> list[str]:
        return ["-v", f"{_resolve_bin()}:{CONTAINER_BIN}:ro"]

    def _agent_env(self) -> dict[str, str]:
        env = {
            "HOME": CONTAINER_HOME,
            "SIRBONE_USAGE": "1",
            # Benchmark rule: the agent must solve the task, not look up its fix.
            "SIRBONE_DISABLE": "tool:web_search,tool:web_fetch",
        }
        if self.model:
            env["SIRBONE_MODEL"] = self.model
        if self.max_turns:
            env["SIRBONE_MAX_STEPS"] = str(self.max_turns)
        env.update(
            {k: os.environ[k] for k in FORWARDED_ENV if os.environ.get(k, "").strip()}
        )
        if not any(env.get(k) for k in PROVIDER_KEYS):
            raise RuntimeError(
                "no provider credential in the environment "
                f"(one of {', '.join(PROVIDER_KEYS)}) — sirbone cannot reach a model"
            )
        return env

    def send_task(
        self,
        prompt: str,
        agent_id: str,
        container_name: str,
        artifact_dir: Path | None = None,
        instance_id: str | None = None,
    ) -> AgentResult:
        stdout_path = stderr_path = None
        if artifact_dir:
            artifact_dir.mkdir(parents=True, exist_ok=True)
            stdout_path = artifact_dir / "agent_stdout.log"
            stderr_path = artifact_dir / "agent_stderr.log"

        env_flags = [f for k, v in self._agent_env().items() for f in ("-e", f"{k}={v}")]
        # No `bash -c`: the prompt travels as one argv element, so nothing in a
        # SWE-bench issue body can be interpreted by a shell.
        cmd = [
            "docker",
            "exec",
            "-w",
            REPO_DIR,
            *env_flags,
            container_name,
            CONTAINER_BIN,
            "-p",
            "--output-format",
            "json",
            prompt,
        ]

        started = time.time()
        timed_out = False
        try:
            done = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=self.timeout + SUBPROCESS_TIMEOUT_BUFFER,
            )
            exit_code, stdout, stderr = done.returncode, done.stdout, done.stderr
        except subprocess.TimeoutExpired as exc:
            timed_out = True
            exit_code = -1
            stdout, stderr = decode_output(exc.stdout), decode_output(exc.stderr)
            # `docker exec` died on the host; the agent inside did not.
            subprocess.run(
                ["docker", "exec", container_name, "pkill", "-f", CONTAINER_BIN],
                capture_output=True,
                check=False,
                timeout=60,
            )

        duration = time.time() - started
        if stdout_path:
            stdout_path.write_text(stdout)
        if stderr_path:
            stderr_path.write_text(stderr)

        if timed_out:
            finish_reason = "timeout"
        elif exit_code != 0:
            finish_reason = "error"
        elif not stdout.strip():
            finish_reason = "empty"
        else:
            finish_reason = "stop"

        payload = _result_payload(stdout)
        return AgentResult(
            success=finish_reason == "stop",
            timeout=timed_out,
            exit_code=exit_code,
            finish_reason=finish_reason,
            stdout_path=stdout_path,
            stderr_path=stderr_path,
            session_id=payload.get("session"),
            duration_seconds=round(duration, 1),
            usage=payload.get("usage") or {},
        )

    def collect_usage(self, workspace, artifact_dir: Path) -> dict:
        """Real per-run accounting from sirbone's own headless JSON.

        No static pricing table: tokens are measured, cost is applied once at
        analysis time from a pricing constant versioned with the campaign
        (upstream's ZeroClaw table has no GLM-5.2 row).
        """
        stdout_file = artifact_dir / "agent_stdout.log"
        if not stdout_file.exists():
            return {}
        payload = _result_payload(stdout_file.read_text())
        usage = dict(payload.get("usage") or {})
        session = payload.get("session")
        if session:
            usage["session"] = session
            workspace.copy_from_container(session, str(artifact_dir / "session.jsonl"))
        return usage


def _result_payload(stdout: str) -> dict:
    """The final ``{"result":…,"usage":…}`` object from a headless run.

    Scanned from the end: sirbone prints the result object last, but an opt-in
    grounding report can follow it, and stream-json emits event lines before it.
    """
    for line in reversed(stdout.splitlines()):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(obj, dict) and "usage" in obj:
            return obj
    return {}

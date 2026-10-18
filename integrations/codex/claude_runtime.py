"""Finite Claude Code headless assessment transport; no Codex executable or API.

Subscription auth is left to Claude Code in the ordinary user home. Safe mode is
not an OS sandbox: managed policy may still run commands. No prompt/session,
reasoning, stderr or credential is copied into a Mneme receipt. Source contracts:
https://code.claude.com/docs/en/cli-reference
https://code.claude.com/docs/en/env-vars
https://code.claude.com/docs/en/agent-sdk/cost-tracking
https://github.com/anthropics/claude-agent-sdk-python/blob/main/src/claude_agent_sdk/_internal/message_parser.py
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import threading
import time
import uuid

from assessment import AssessmentMixin, AssessmentResponse, TransportError
import host_files
import host_process
from host_platform import is_executable
from librarian_policy import resolve
from rollout_primitives import decode_json

MAX_FRAME_BYTES = 512 * 1024
MAX_EVENTS_BYTES = 1_000_000
MAX_FRAMES = 1024
MAX_PROBE_BYTES = 64 * 1024
PROBE_SECONDS = 5
# Compatibility policy: documented Claude Code v2 CLI, at least the documented
# --permission-prompts floor (2.1.259). Unknown majors are not assumed compatible.
MINIMUM_CLI_VERSION = (2, 1, 259)
# Help is positive compatibility metadata, NOT proof that a parser honors flags:
# Claude's --help/--version paths even ignore unknown switches. Official v2 docs
# explicitly say help is nonexhaustive; --max-turns is documented yet absent in
# the official 2.1.302 artifact's help. Exempt ONLY that known hidden flag here;
# retain --max-turns=1 below. Never retry with weaker flags or provider work.
# https://code.claude.com/docs/en/cli-reference (CLI flags + --max-turns)
# https://code.claude.com/docs/en/cli-reference (--permission-prompts version)
REQUIRED_FLAGS = (
    "--safe-mode", "--setting-sources", "--settings", "--tools", "--disallowedTools",
    "--strict-mcp-config", "--mcp-config", "--disable-slash-commands", "--no-chrome",
    "--no-session-persistence", "--permission-mode", "--permission-prompts",
    "--output-format", "--verbose", "--input-format", "--model",
    "--effort", "--session-id", "--system-prompt",
)


def subscription_environment(budget):
    """Do not let an inherited API key silently select paid API billing."""
    env = os.environ.copy()
    auth_env = {key: env[key] for key in ("CLAUDE_CONFIG_DIR", "CLAUDE_CODE_OAUTH_TOKEN") if key in env}
    for key in tuple(env):
        if (key.startswith(("ANTHROPIC_", "CLAUDE_", "CODEX_", "OPENAI_"))
                or key in ("CLAUDECODE", "MNEME_DB", "DEBUG", "NODE_OPTIONS", "BASH_ENV", "ENV")
                or key.startswith(("ENABLE_", "FALLBACK_", "DISABLE_"))):
            env.pop(key, None)
    env.update(auth_env)
    env.update({"CLAUDE_CODE_SAFE_MODE": "1", "CLAUDE_CODE_MAX_RETRIES": "0",
                "CLAUDE_CODE_RETRY_WATCHDOG": "0", "CLAUDE_ENABLE_STREAM_WATCHDOG": "0",
                "CLAUDE_ENABLE_BYTE_WATCHDOG": "0", "CLAUDE_CODE_MAX_OUTPUT_TOKENS": str(budget.output_tokens),
                "CLAUDE_CODE_SKIP_PROMPT_HISTORY": "1", "ENABLE_CLAUDEAI_MCP_SERVERS": "false",
                "CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS": "1",
                "DISABLE_AUTOUPDATER": "1", "DISABLE_TELEMETRY": "1", "DISABLE_ERROR_REPORTING": "1",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "DISABLE_COMPACT": "1"})
    return env


def result_usage(result):
    """Whole-call camelCase modelUsage, not placeholder assistant output usage.

    Input includes uncached + cache reads + cache creation. Cache creation is
    separately exposed; reasoning is unknown, never represented as free tokens.
    Crash results can carry zeroed totals: those are unknown, not known zero.
    """
    if result.get("subtype") == "error_during_execution":
        raise TransportError("usage_unknown")
    models = result.get("modelUsage")
    if type(models) is not dict or not models or len(models) > 64:
        raise TransportError("usage_unknown")
    totals = {key: 0 for key in ("inputTokens", "outputTokens", "cacheReadInputTokens", "cacheCreationInputTokens")}
    for model, value in models.items():
        if type(model) is not str or not model or type(value) is not dict:
            raise TransportError("usage_unknown")
        for key in totals:
            amount = value.get(key)
            if type(amount) is not int or not 0 <= amount <= 1_000_000:
                raise TransportError("usage_unknown")
            totals[key] += amount
    inp = totals["inputTokens"] + totals["cacheReadInputTokens"] + totals["cacheCreationInputTokens"]
    out = totals["outputTokens"]
    if inp > 1_000_000 or out > 1_000_000:
        raise TransportError("usage_unknown")
    return {"input_tokens": inp, "output_tokens": out,
            "cached_input_tokens": totals["cacheReadInputTokens"],
            "cache_write_input_tokens": totals["cacheCreationInputTokens"],
            "uncached_input_tokens": inp - totals["cacheReadInputTokens"],
            "reasoning_output_tokens": None, "total_tokens": inp + out}


class ClaudeRuntime(AssessmentMixin):
    def __init__(self, config, scratch_dir: Path):
        self.config = dict(config) if isinstance(config, dict) else config
        try:
            self.budget = resolve(self.config)
            if self.budget.assessor != "claude_code":
                self.budget = None
        except ValueError:
            self.budget = None
        self.scratch_dir = Path(scratch_dir)
        self.process = self._pipe = self._own_cwd = None
        self._lock = threading.Lock()
        self._closed = self._halted = self._assessment_attempt = False
        self._current_usage = None
        self._verified = None

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def _config(self, key, default=None):
        return self.config.get(key, default) if isinstance(self.config, dict) else getattr(self.config, key, default)

    def _check_binary(self):
        path, digest = self._config("reader_claude"), self._config("reader_claude_sha256")
        if (self.budget is None or not isinstance(path, (str, Path)) or not Path(path).is_absolute()
                or type(digest) is not str or re.fullmatch(r"[0-9a-f]{64}", digest) is None):
            raise TransportError("invalid_config")
        path = Path(path)
        if not is_executable(path):
            raise TransportError("unavailable")
        hasher = hashlib.sha256()
        with path.open("rb") as source:
            for block in iter(lambda: source.read(1024 * 1024), b""):
                hasher.update(block)
        if hasher.hexdigest() != digest:
            raise TransportError("binary_changed")
        return path, digest

    def _probe(self, executable, switch, deadline, env):
        """Offline capability metadata only; the same finite tree/pipe ownership."""
        process, pipe = None, None
        try:
            process = host_process.spawn([str(executable), switch], tree=True, cwd=self._own_cwd,
                env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, bufsize=0)
            pipe = host_process.BoundedPipe(process)
            process.stdin.close()
            output = bytearray()
            while True:
                data = pipe.read(min(8192, MAX_PROBE_BYTES + 1 - len(output)), deadline)
                if not data:
                    break
                output.extend(data)
                if len(output) > MAX_PROBE_BYTES:
                    raise TransportError("unsupported_runtime")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TransportError("timeout")
            if process.wait(timeout=remaining) != 0:
                raise TransportError("unsupported_runtime")
            return output.decode("utf-8", errors="strict")
        finally:
            if process is not None:
                host_process.terminate_tree(process, timeout=0.1)
            if pipe is not None:
                pipe.close()

    def _preflight(self, executable, digest, request, env):
        if self._verified == (str(executable), digest):
            return
        deadline = min(request.deadline, time.monotonic() + PROBE_SECONDS)
        version = self._probe(executable, "--version", deadline, env)
        help_text = self._probe(executable, "--help", deadline, env)
        match = re.fullmatch(r"(\d{1,4})\.(\d{1,4})\.(\d{1,6}) \(Claude Code\)\s*", version)
        cli_version = tuple(map(int, match.groups())) if match else None
        advertised = lambda flag: re.search(r"(?<![\w-])" + re.escape(flag) + r"(?=[\s,=<]|$)", help_text) is not None
        if (cli_version is None or cli_version[0] != 2 or cli_version < MINIMUM_CLI_VERSION
                or not all(advertised(flag) for flag in REQUIRED_FLAGS)
                or not (advertised("--print") or advertised("-p"))):
            raise TransportError("unsupported_runtime")
        self._verified = (str(executable), digest)

    def _begin_assessment(self, _frame_bytes):
        self._assessment_attempt = False
        self._current_usage = None
        # Fresh private neutral cwd, never beneath the project or the user home.
        self._own_cwd = Path(tempfile.gettempdir()) / ("mneme-claude-cwd-" + uuid.uuid4().hex)
        host_files.ensure_private_directory(self._own_cwd, parents=False, exist_ok=False)

    def _assessment_attempted(self):
        return self._assessment_attempt

    def _run_assessment(self, request, *, service_name):
        del service_name  # Host role is already expressed by its prepared instructions.
        env = subscription_environment(self.budget)
        executable, digest = self._check_binary()
        # Schema is plain prompt data: --json-schema can run hidden repair turns.
        # Host strict validation is terminal; no output repair/retry lives here.
        prompt = request.prompt_with_schema().encode("utf-8")
        try:
            self._preflight(executable, digest, request, env)
        except (TimeoutError, subprocess.TimeoutExpired):
            raise TransportError("timeout") from None
        self._check_binary()  # Refuse changed executable before any provider attempt.
        session = str(uuid.uuid4())
        command = [str(executable), "-p", "--safe-mode", "--setting-sources=",
            '--settings={"disableAllHooks":true,"autoMemoryEnabled":false,"disableClaudeAiConnectors":true}',
            "--tools=", "--disallowedTools=mcp__*", "--strict-mcp-config", '--mcp-config={"mcpServers":{}}',
            "--disable-slash-commands", "--no-chrome", "--no-session-persistence", "--permission-mode=dontAsk",
            "--permission-prompts=none", "--output-format=stream-json", "--verbose", "--input-format=text",
            "--max-turns=1", "--model=" + self.budget.model, "--effort=" + self.budget.effort,
            "--session-id=" + session, "--system-prompt=" + request.instructions]
        if time.monotonic() >= request.deadline:
            raise TransportError("timeout")
        self.process = host_process.spawn(command, tree=True, cwd=self._own_cwd, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, bufsize=0)
        # A spawn may reach inference even if the subsequent write fails.
        self._assessment_attempt = True
        self._pipe = host_process.BoundedPipe(self.process)
        try:
            self._pipe.write(prompt, request.deadline)
            self.process.stdin.close()
            return self._read_response(request, session)
        except (TimeoutError, subprocess.TimeoutExpired):
            raise TransportError("timeout") from None
        except OSError:
            raise TransportError("transport_error") from None

    def _read_response(self, request, session):
        buffer = b""
        frames = total = 0
        result = None
        initialized = False
        mismatch = False
        provider_error = False
        while True:
            pos = buffer.find(b"\n")
            if pos < 0:
                if len(buffer) >= MAX_FRAME_BYTES:
                    raise TransportError("frame_cap")
                block = self._pipe.read(min(8192, MAX_FRAME_BYTES - len(buffer)), request.deadline)
                if not block:
                    if buffer:
                        raise TransportError("invalid_events")
                    break
                total += len(block)
                if total > MAX_EVENTS_BYTES:
                    raise TransportError("frame_cap")
                buffer += block
                continue
            raw, buffer = buffer[:pos], buffer[pos + 1:]
            frames += 1
            if frames > MAX_FRAMES:
                raise TransportError("frame_cap")
            try:
                event = decode_json(raw)
            except (ValueError, TypeError, UnicodeError, RecursionError):
                raise TransportError("invalid_events") from None
            if type(event) is not dict:
                raise TransportError("invalid_events")
            kind = event.get("type")
            if result is not None:
                # Multiple terminal totals are ambiguous, never additive or last-wins.
                self._current_usage = None
                raise TransportError("invalid_events")
            if event.get("session_id") != session:
                # Still consume the sole terminal accounting, but never accept it.
                provider_error = True
            if kind == "result":
                result = event
                self._current_usage = result_usage(result)
                mismatch |= set(result["modelUsage"]) != {self.budget.model}
                continue
            if kind == "system" and event.get("subtype") == "init" and not initialized:
                initialized = True
                mismatch |= event.get("model") != self.budget.model
                if event.get("tools") != [] or event.get("mcp_servers") != []:
                    raise TransportError("tool_activity")
                # These are loaded customizations, not mere tool execution.
                # Built-ins can normally populate agents even with --tools=;
                # we additionally disable their registration via the documented
                # SDK environment switch, so no name allowlist is guessed here.
                # https://code.claude.com/docs/en/sub-agents
                for field in ("plugins", "skills", "plugin_errors", "agents"):
                    if field in event and event[field] != []:
                        raise TransportError("tool_activity")
                continue
            if not initialized:
                raise TransportError("invalid_events")
            if kind == "assistant":
                message = event.get("message")
                if type(message) is not dict or type(message.get("content")) is not list:
                    raise TransportError("invalid_events")
                mismatch |= message.get("model") != self.budget.model
                provider_error |= bool(event.get("error"))
                if event.get("parent_tool_use_id") is not None:
                    raise TransportError("tool_activity")
                for block in message["content"]:
                    if type(block) is not dict or block.get("type") not in ("text", "thinking", "redacted_thinking"):
                        raise TransportError("tool_activity")
                continue  # Raw reasoning/text is neither stored nor returned.
            if kind == "rate_limit_event":
                continue
            # Hook lifecycle, tool progress, subagents, controls and unknown events
            # are not normal output from this tool-free one-shot contract.
            raise TransportError("tool_activity" if kind in ("system", "tool_progress", "tool_use", "user", "control_request")
                                 else "invalid_events")
        remaining = request.deadline - time.monotonic()
        if remaining <= 0:
            raise TransportError("timeout")
        exit_code = self.process.wait(timeout=remaining)
        if result is None:
            raise TransportError("usage_unknown")
        outcome = "completed"
        # Success-shaped results can still be cancellation or an injected turn.
        # Absence/null is the documented legacy/unstamped own-prompt form;
        # unknown explicit values are not silently treated as completion.
        # https://github.com/anthropics/claude-agent-sdk-python/blob/main/src/claude_agent_sdk/types.py
        origin = result.get("origin")
        own_origin = origin is None or (type(origin) is dict and origin.get("kind") == "human")
        completed_terminal = result.get("terminal_reason") in (None, "completed")
        if (not own_origin or not completed_terminal or exit_code != 0 or not initialized or provider_error or result.get("subtype") != "success"
                or result.get("is_error") is not False or type(result.get("num_turns")) is not int
                or result["num_turns"] != 1 or result.get("stop_reason") != "end_turn"
                or result.get("errors") not in (None, []) or result.get("deferred_tool_use") is not None):
            outcome = "provider_error"
        if result.get("permission_denials") != []:
            outcome = "tool_activity"
        if mismatch:
            outcome = "model_mismatch"
        return AssessmentResponse(result.get("result"), self._current_usage,
                                  self.budget.model if not mismatch else None, outcome)

    def _finish_assessment(self, _parked, _clean_success):
        self._stop_process()

    def _stop_process(self):
        process, self.process = self.process, None
        pipe, self._pipe = self._pipe, None
        try:
            if process is not None:
                host_process.terminate_tree(process, timeout=0.1)
        finally:
            if pipe is not None:
                pipe.close()
            if self._own_cwd is not None:
                shutil.rmtree(self._own_cwd)
                self._own_cwd = None

    def close(self):
        with self._lock:
            self._closed = True
            self._stop_process()

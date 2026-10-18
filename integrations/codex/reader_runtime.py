"""Bounded, ephemeral Codex app-server transport for selection and assessment.

No Mneme protocol, persistent transcript, tool, or provider retry lives here.
"""

from __future__ import annotations

import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import threading
import time
import uuid

import host_files
import host_process
from host_platform import is_executable

WINDOWS = os.name == "nt"
MAX_AUTH_BYTES = 1024 * 1024

from reader_contract import BASE_INSTRUCTIONS, OUTPUT_SCHEMA, prepare, validate_answer
import recording_contract
from rollout_primitives import decode_json
from librarian_policy import MODEL, resolve

TURN_SECONDS = 45
MAX_FRAMES = 1024
MAX_FRAME_BYTES = 64 * 1024
MAX_RECORDING_FRAME_BYTES = 512 * 1024
MAX_EVENTS_BYTES = 1_000_000
MAX_ANSWER_BYTES = 4096
ROTATE_TURNS = 8
ROTATE_INPUT_TOKENS = 32_000
USAGE_FIELDS = {"inputTokens": "input_tokens", "cachedInputTokens": "cached_input_tokens",
                "cacheWriteInputTokens": "cache_write_input_tokens", "outputTokens": "output_tokens",
                "reasoningOutputTokens": "reasoning_output_tokens", "totalTokens": "total_tokens"}
from assessment import (AssessmentMixin, AssessmentResponse, TransportError, assessment_diagnostic,
    RUNTIME_DIAGNOSTIC_REASONS, VALIDATION_DIAGNOSTIC_REASONS, _VALIDATOR_DIAGNOSTIC_REASONS)
_NO_ANSWER = object()


def _usage(value):
    if not isinstance(value, dict):
        raise TransportError("usage_unknown")
    result = {}
    for source, target in USAGE_FIELDS.items():
        amount = value.get(source)
        if source == "cacheWriteInputTokens" and amount is None:
            result[target] = None  # Optional breakdown is unknown, not free usage.
            continue
        if isinstance(amount, bool) or not isinstance(amount, int) or amount < 0:
            raise TransportError("usage_unknown")
        result[target] = amount
    if (result["cached_input_tokens"] > result["input_tokens"]
            or result["reasoning_output_tokens"] > result["output_tokens"]):
        raise TransportError("usage_unknown")
    result["uncached_input_tokens"] = result["input_tokens"] - result["cached_input_tokens"]
    return result


class ReaderRuntime(AssessmentMixin):
    def __init__(self, config, scratch_dir: Path):
        self.config = dict(config) if isinstance(config, dict) else config
        try:
            self.budget = resolve(self.config)
        except ValueError:
            self.budget = None  # Diagnose before starting a process; never default.
        self.scratch_dir = Path(scratch_dir)
        self.process = None
        self.home = None
        self.cwd = None
        self._own_dir = None
        self._own_cwd = None
        self._pipe = None
        self._buffer = b""
        self._request_id = 0
        self._thread_id = None
        self._pending = []
        self._turns = 0
        self._last_input = 0
        self._current_usage = None
        self._closed = False
        self._halted = False
        self._lock = threading.Lock()
        self._frames = 0
        self._bytes = 0
        self._max_frame_bytes = MAX_FRAME_BYTES

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def _config(self, key, default=None):
        if isinstance(self.config, dict):
            return self.config.get(key, default)
        return getattr(self.config, key, default)

    def _check_config(self):
        codex = self._config("reader_codex")
        digest = self._config("reader_codex_sha256")
        model = self._config("reader_model")
        if (not isinstance(codex, (str, Path)) or not Path(codex).is_absolute()
                or not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest)
                or self.budget is None or self.budget.assessor != "codex" or model != self.budget.model):
            raise TransportError("invalid_config")
        codex = Path(codex)
        if not is_executable(codex):
            raise TransportError("unavailable")
        hasher = hashlib.sha256()
        with codex.open("rb") as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                hasher.update(block)
        if hasher.hexdigest() != digest:
            raise TransportError("binary_changed")
        auth = self._config("reader_auth")
        if auth is None:
            auth = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")) / "auth.json"
        auth = Path(auth)
        if not auth.is_absolute() or not auth.is_file():
            raise TransportError("unavailable")
        return codex, auth

    def _send(self, message, deadline):
        if self.process is None or self.process.poll() is not None or self._pipe is None:
            raise TransportError("transport_error")
        try:
            raw = json.dumps(message, separators=(",", ":")).encode() + b"\n"
            if len(raw) > self._max_frame_bytes:
                raise TransportError("frame_cap")
            self._pipe.write(raw, deadline)
        except TimeoutError as exc:
            raise TransportError("timeout") from exc
        except OSError as exc:
            raise TransportError("transport_error") from exc

    def _next(self, deadline):
        # Backpressure stays in the bounded OS pipe. There are no blocking I/O
        # threads to outlive a deadline, role switch or retired process.
        while True:
            if time.monotonic() >= deadline:
                raise TransportError("timeout")
            pos = self._buffer.find(b"\n")
            if pos >= 0:
                raw, self._buffer = self._buffer[:pos + 1], self._buffer[pos + 1:]
                break
            if len(self._buffer) >= self._max_frame_bytes:
                raise TransportError("frame_cap")
            try:
                block = self._pipe.read(min(8192, self._max_frame_bytes - len(self._buffer)), deadline)
            except TimeoutError as exc:
                raise TransportError("timeout") from exc
            except OSError as exc:
                raise TransportError("transport_error") from exc
            if not block:
                raise TransportError("transport_error")
            self._buffer += block
        if len(raw) > self._max_frame_bytes:
            raise TransportError("frame_cap")
        self._frames += 1
        self._bytes += len(raw)
        if self._frames > MAX_FRAMES or self._bytes > MAX_EVENTS_BYTES:
            raise TransportError("frame_cap")
        try:
            message = json.loads(raw)
        except (ValueError, UnicodeError, RecursionError) as exc:
            raise TransportError("invalid_events") from exc
        if not isinstance(message, dict):
            raise TransportError("invalid_events")
        if "method" in message and "id" in message:
            raise TransportError("tool_activity")
        return message

    def _handoff_auth(self, auth):
        target = self.home / "auth.json"
        if not WINDOWS:
            target.symlink_to(auth)
            return
        # Windows symlinks need an optional privilege. Copy one bounded, private
        # regular-file edition instead; never follow a reparse/hard-link source.
        # Read and metadata verification retain the same opened source handle.
        with os.fdopen(host_files.open_regular(auth), "rb") as source:
            host_files.check_private_fd(source.fileno())
            before = os.fstat(source.fileno())
            if before.st_size > MAX_AUTH_BYTES:
                raise TransportError("unavailable")
            raw = source.read(MAX_AUTH_BYTES + 1)
            after = os.fstat(source.fileno())
            identity = lambda value: (value.st_dev, value.st_ino, value.st_size,
                                      value.st_mtime_ns, value.st_ctime_ns)
            if len(raw) > MAX_AUTH_BYTES or identity(before) != identity(after):
                raise TransportError("unavailable")
            # A pathname replacement during the copy is not the reviewed source.
            with os.fdopen(host_files.open_regular(auth), "rb") as current:
                host_files.check_private_fd(current.fileno())
                if identity(os.fstat(current.fileno())) != identity(after):
                    raise TransportError("unavailable")
            host_files.atomic_write(target, raw)
        host_files.check_private(target)

    @staticmethod
    def _activity(message):
        method = message.get("method")
        if not isinstance(method, str):
            return False
        params = message.get("params")
        item = params.get("item") if isinstance(params, dict) else None
        item_type = item.get("type") if isinstance(item, dict) else None
        if method.startswith("item/") and item_type is not None and item_type not in ("userMessage", "agentMessage", "reasoning"):
            return True
        lower = method.lower()
        return any(word in lower for word in ("approval", "toolcall", "command", "filechange", "mcp/", "websearch", "imageview", "imagegeneration", "collab"))

    def _request(self, method, params, deadline, *, buffer_notifications=False):
        if time.monotonic() >= deadline:
            raise TransportError("timeout")
        self._request_id += 1
        request_id = self._request_id
        self._send({"method": method, "id": request_id, "params": params}, deadline)
        while True:
            message = self._next(deadline)
            if self._activity(message):
                raise TransportError("tool_activity")
            if message.get("id") != request_id:
                if buffer_notifications:
                    if len(self._pending) >= MAX_FRAMES:
                        raise TransportError("frame_cap")
                    self._pending.append(message)
                continue
            if "error" in message or not isinstance(message.get("result"), dict):
                raise TransportError("provider_error")
            return message["result"]

    def _start(self, deadline, *, base_instructions=BASE_INSTRUCTIONS, service_name="mneme_reader"):
        codex, auth = self._check_config()
        if WINDOWS:
            host_files.ensure_private_directory(self.scratch_dir)
        else:
            # Existing Unix scratch roots were created with the caller's umask;
            # privacy belongs to each exclusive 0700 child, as before.
            self.scratch_dir.mkdir(parents=True, exist_ok=True)
        own_dir = self.scratch_dir / ("reader-" + uuid.uuid4().hex)
        host_files.ensure_private_directory(own_dir, parents=False, exist_ok=False)
        self._own_dir = own_dir
        self.home = self._own_dir / "home"
        # The caller's scratch may live under the project. The model's cwd must
        # not: project config/rules can be inherited from ancestors.
        own_cwd = Path(tempfile.gettempdir()) / ("mneme-reader-cwd-" + uuid.uuid4().hex)
        host_files.ensure_private_directory(own_cwd, parents=False, exist_ok=False)
        self._own_cwd = own_cwd
        self.cwd = self._own_cwd
        host_files.ensure_private_directory(self.home, parents=False, exist_ok=False)
        self._handoff_auth(auth)
        env = os.environ.copy()
        for key in tuple(env):
            if key == "MNEME_DB" or key.startswith(("CODEX_", "OPENAI_", "ANTHROPIC_", "XDG_")):
                env.pop(key, None)
        env.update({"HOME": str(self.home), "CODEX_HOME": str(self.home), "GIT_CEILING_DIRECTORIES": str(self.cwd.parent),
                    "HF_HUB_OFFLINE": "1", "TRANSFORMERS_OFFLINE": "1"})
        if WINDOWS:
            # Libraries using native Windows home conventions must see the same
            # isolated root, not the user's profile/configuration directories.
            env.update({"USERPROFILE": str(self.home), "APPDATA": str(self.home / "AppData/Roaming"),
                        "LOCALAPPDATA": str(self.home / "AppData/Local"),
                        "HOMEDRIVE": self.home.drive, "HOMEPATH": str(self.home)[len(self.home.drive):]})
        command = [str(codex), "app-server", "--disable", "hooks", "--disable", "memories",
                   "--disable", "multi_agent", "--disable", "shell_tool",
                   "--disable", "plugins", "--disable", "apps",
                   "--disable", "browser_use", "--disable", "computer_use",
                   "--disable", "skill_search", "-c", "project_doc_max_bytes=0"]
        self.process = host_process.spawn(command, cwd=self.cwd, env=env, tree=True,
                                          stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                          stderr=subprocess.DEVNULL, bufsize=0)
        self._pipe = host_process.BoundedPipe(self.process)
        initialized = self._request("initialize", {"clientInfo": {"name": "mneme_reader",
                                              "title": "Mneme memory reader", "version": "1.0.0"}}, deadline)
        self._send({"method": "initialized", "params": {}}, deadline)
        if not isinstance(initialized, dict):
            raise TransportError("provider_error")
        self._start_thread(deadline, base_instructions=base_instructions, service_name=service_name)

    def _start_thread(self, deadline, *, base_instructions=BASE_INSTRUCTIONS,
                      service_name="mneme_reader"):
        started = self._request("thread/start", {
            "model": self.budget.model, "cwd": str(self.cwd), "approvalPolicy": "never",
            "sandbox": "read-only", "baseInstructions": base_instructions,
            "ephemeral": True, "serviceName": service_name,
        }, deadline)
        thread = started.get("thread")
        if (not isinstance(thread, dict) or not isinstance(thread.get("id"), str)
                or thread.get("ephemeral") is not True or started.get("model") != self.budget.model
                or started.get("approvalPolicy") != "never"
                or started.get("instructionSources", []) != []):
            raise TransportError("thread_contract")
        self._thread_id = thread["id"]
        self._turns = 0
        self._last_input = 0
        self._pending.clear()

    def _turn(self, prompt, context, deadline, *, output_schema=OUTPUT_SCHEMA,
              validator=validate_answer, answer_bytes=MAX_ANSWER_BYTES, strict_json=False, raw_answer=False):
        thread_id = self._thread_id
        started = self._request("turn/start", {
            "threadId": thread_id, "input": [{"type": "text", "text": prompt}],
            "model": self.budget.model, "effort": self.budget.effort, "cwd": str(self.cwd),
            "approvalPolicy": "never", "sandboxPolicy": {"type": "readOnly"},
            "outputSchema": output_schema,
        }, deadline, buffer_notifications=True)
        turn = started.get("turn")
        if not isinstance(turn, dict) or not isinstance(turn.get("id"), str):
            raise TransportError("provider_error")
        turn_id = turn["id"]
        usage = None
        answer = _NO_ANSWER
        while True:
            message = self._pending.pop(0) if self._pending else self._next(deadline)
            if self._activity(message):
                raise TransportError("tool_activity")
            method = message.get("method")
            params = message.get("params")
            if method in ("item/completed", "thread/tokenUsage/updated", "turn/completed"):
                # App-server v2 ItemCompleted/ThreadTokenUsageUpdated carry
                # threadId+turnId; TurnCompleted carries threadId+turn.id.
                # A reused process can still deliver notifications from an old
                # thread. They count toward work bounds, never toward this turn.
                if not isinstance(params, dict):
                    raise TransportError("invalid_events")
                event_thread = params.get("threadId")
                if method == "turn/completed":
                    event_turn = params.get("turn")
                    event_turn = event_turn.get("id") if isinstance(event_turn, dict) else None
                else:
                    event_turn = params.get("turnId")
                if not all(isinstance(value, str) and value for value in (event_thread, event_turn)):
                    raise TransportError("invalid_events")
                if (event_thread, event_turn) != (thread_id, turn_id):
                    continue
            if not isinstance(params, dict):
                continue
            if method == "thread/tokenUsage/updated" and params.get("turnId") == turn_id:
                usage = params.get("tokenUsage")
            elif method == "item/completed":
                item = params.get("item")
                if isinstance(item, dict) and item.get("type") == "agentMessage":
                    answer = item.get("text", _NO_ANSWER)
            elif method == "turn/completed":
                finished = params.get("turn")
                if isinstance(finished, dict) and finished.get("id") == turn_id:
                    if finished.get("status") != "completed":
                        raise TransportError("provider_error")
                    break
        if not isinstance(usage, dict):
            raise TransportError("usage_unknown")
        last = _usage(usage.get("last"))
        total = _usage(usage.get("total"))
        if any(total[key] < last[key] for key in USAGE_FIELDS.values()
               if total[key] is not None and last[key] is not None):
            raise TransportError("usage_unknown")
        self._turns += 1
        self._last_input = last["input_tokens"]
        self._current_usage = last
        if answer is _NO_ANSWER:
            raise TransportError("invalid_output", validation_reason="answer_missing")
        if not isinstance(answer, str):
            raise TransportError("invalid_output", validation_reason="answer_type")
        try:
            encoded_answer = answer.encode()
        except UnicodeError:
            raise TransportError("invalid_output") from None
        if len(encoded_answer) > answer_bytes:
            raise TransportError("invalid_output", validation_reason="answer_bytes_limit")
        if raw_answer:
            return answer, last
        try:
            parsed = decode_json(encoded_answer) if strict_json else json.loads(answer)
        except (ValueError, TypeError, UnicodeError, RecursionError):
            raise TransportError("invalid_output", validation_reason="invalid_json") from None
        try:
            selected = validator(parsed, context)
        except (ValueError, TypeError, UnicodeError, RecursionError) as exc:
            reason = (exc.args[0] if type(exc) is ValueError and len(exc.args) == 1
                      and type(exc.args[0]) is str else None)
            reason = reason if reason in _VALIDATOR_DIAGNOSTIC_REASONS else "unknown"
            raise TransportError("invalid_output", validation_reason=reason) from None
        return selected, last

    def select(self, dialogue: list[dict], cards: list[dict], *, concern_rows=None) -> dict:
        started = time.monotonic()
        receipt = {"selected_ids": [], "concerns": [], "reason": "provider_error", "usage": None,
                   "elapsed_ms": 0.0, "provider_attempt": False}
        with self._lock:
            try:
                if self._closed or self._halted:
                    raise TransportError("closed" if self._closed else "usage_unknown")
                if self.budget is None:
                    raise TransportError("invalid_config")
                prompt, ids_or_reason = prepare(dialogue, cards, concern_rows=concern_rows, budget=self.budget)
                if prompt is None:
                    raise TransportError(ids_or_reason)
                deadline = started + TURN_SECONDS
                self._frames = self._bytes = 0
                self._current_usage = None
                self._max_frame_bytes = MAX_FRAME_BYTES
                if self.process is None:
                    self._start(deadline)
                    receipt["provider_attempt"] = True
                elif (self._thread_id is None or self._turns >= ROTATE_TURNS
                      or self._last_input >= ROTATE_INPUT_TOKENS):
                    self._start_thread(deadline)
                receipt["provider_attempt"] = True
                selected, usage = self._turn(prompt, ids_or_reason, deadline, answer_bytes=self.budget.selector_answer_bytes)
                receipt.update(selected, reason="selected" if selected["selected_ids"] else "abstained", usage=usage)
            except TransportError as exc:
                receipt["reason"] = exc.reason
                preflight = exc.reason in ("invalid_input", "empty_or_seen", "nonsubstantive", "prompt_cap", "closed")
                if self.process is not None and not preflight:
                    receipt["provider_attempt"] = True
                if not preflight:
                    receipt["usage"] = self._current_usage
                    if receipt["provider_attempt"] and receipt["usage"] is None:
                        self._halted = True
                if not preflight:
                    self._stop_process()
            except (OSError, ValueError, TypeError, OverflowError):
                receipt["reason"] = "provider_error"
                if self.process is not None:
                    receipt["provider_attempt"] = True
                receipt["usage"] = self._current_usage
                if receipt["provider_attempt"] and receipt["usage"] is None:
                    self._halted = True
                self._stop_process()
            finally:
                receipt["elapsed_ms"] = (time.monotonic() - started) * 1000
        return receipt

    def _assessment_seconds(self):
        return TURN_SECONDS

    def _begin_assessment(self, frame_bytes):
        parked = (self.process, self._thread_id, self._turns, self._last_input)
        self._frames = self._bytes = 0
        self._current_usage = None
        self._assessment_attempt = False
        self._max_frame_bytes = frame_bytes
        return parked

    def _assessment_attempted(self):
        return self._assessment_attempt or self.process is not None

    def _run_assessment(self, request, *, service_name):
        options = {"base_instructions": request.instructions, "service_name": service_name}
        if self.process is None:
            self._start(request.deadline, **options)
        else:
            self._start_thread(request.deadline, **options)
        self._assessment_attempt = True
        answer, usage = self._turn(request.prompt, None, request.deadline,
            output_schema=request.output_schema, answer_bytes=request.answer_byte_limit,
            strict_json=True, raw_answer=True)
        return AssessmentResponse(answer, usage, self.budget.model)

    def _finish_assessment(self, parked, clean_success):
        if (clean_success and parked is not None and parked[0] is self.process
                and self.process is not None and self.process.poll() is None
                and not self._halted and not self._closed):
            self._thread_id, self._turns, self._last_input = parked[1:]
        else:
            self._thread_id = None
            self._turns = self._last_input = 0
        self._pending.clear()
        self._max_frame_bytes = MAX_FRAME_BYTES

    def _stop_process(self):
        process = self.process
        self.process = None
        self._thread_id = None
        self._pending.clear()
        pipe, self._pipe = self._pipe, None
        if process is not None:
            try:
                host_process.terminate_tree(process, timeout=3)
            finally:
                if pipe is not None:
                    pipe.close()
                else:
                    for stream in (process.stdin, process.stdout):
                        if stream is not None:
                            stream.close()
        if self._own_dir is not None:
            # A Windows home contains a credential copy, not just a symlink.
            # Cleanup failure must stay visible and retain the path for retry.
            shutil.rmtree(self._own_dir, ignore_errors=not WINDOWS)
            self._own_dir = None
        if self._own_cwd is not None:
            shutil.rmtree(self._own_cwd, ignore_errors=not WINDOWS)
            self._own_cwd = None
        self.home = self.cwd = None
        self._buffer = b""

    def close(self):
        with self._lock:
            self._closed = True
            self._stop_process()

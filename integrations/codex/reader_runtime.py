"""Bounded, ephemeral Codex app-server transport for selection and assessment.

No Mneme protocol, persistent transcript, tool, or provider retry lives here.
"""

from __future__ import annotations

import hashlib
import json
import math
import os
from pathlib import Path
import queue
import re
import shutil
import signal
import subprocess
import tempfile
import threading
import time

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
RUNTIME_DIAGNOSTIC_REASONS = frozenset({
    "unknown", "proposed", "abstained", "validation_failed", "invalid_output", "usage_unknown", "closed",
    "invalid_timeout", "invalid_config", "unavailable", "binary_changed", "transport_error",
    "frame_cap", "timeout", "invalid_events", "tool_activity", "provider_error", "thread_contract",
    "invalid_input", "invalid_observation", "source_turn_incomplete", "unsupported_public_coverage",
    "unsupported_public_selection",
    "unsupported_public_omissions", "unsupported_json_depth", "unsupported_json_value",
    "invalid_source_reference", "source_item_limit", "source_bytes_limit", "missing_tool_result",
    "invalid_completion_boundary", "invalid_evidence_text", "evidence_item_limit",
    "unsupported_evidence_kind", "source_prompt_missing", "source_order_invalid", "invalid_message_shape",
    "unsupported_assistant_phase", "invalid_message_content", "invalid_content_slot", "invalid_tool_call",
    "duplicate_tool_call", "invalid_tool_result", "unmatched_tool_result", "tool_type_mismatch",
    "invalid_overlap_cards", "invalid_overlap_card", "static_prefix_limit", "authored_input_limit",
    "invalid_project_focus", "invalid_project_focus_scope",
})
_VALIDATOR_DIAGNOSTIC_REASONS = frozenset({
    "invalid_validation_context", "unsupported_json_depth", "unsupported_json_value", "answer_bytes_limit",
    "invalid_answer_shape", "invalid_proposal_shape", "invalid_proposal_text", "invalid_citation_count",
    "invalid_citation_shape", "invalid_citation_reference", "invalid_exact_quote", "duplicate_citation",
    "invalid_association_shape", "invalid_association_reference", "invalid_association_kind",
    "invalid_answer", "delivery_source_evidence_missing",
})
VALIDATION_DIAGNOSTIC_REASONS = _VALIDATOR_DIAGNOSTIC_REASONS | frozenset({
    "unknown", "answer_missing", "answer_type", "invalid_json",
})
_NO_ANSWER = object()


def assessment_diagnostic(result):
    """Project only static host codes; never serialize a model value or exception.

    This optional diagnostic does not establish provenance or decide admission,
    retry, accounting, or a write. Its two ASCII codes fit within 256 JSON bytes.
    """
    unknown = {"runtime_reason": "unknown", "validation_reason": "unknown"}
    if type(result) is not dict:
        return unknown
    try:
        reason = result.get("reason")
        reason = (reason if type(reason) is str and len(reason) <= 64
                  and reason in RUNTIME_DIAGNOSTIC_REASONS else "unknown")
        validation = result.get("validation_reason")
        validation = (validation if reason == "invalid_output" and type(validation) is str and len(validation) <= 64
                      and validation in VALIDATION_DIAGNOSTIC_REASONS else "unknown")
        return {"runtime_reason": reason, "validation_reason": validation}
    except Exception:
        return unknown


class TransportError(Exception):
    def __init__(self, reason, *, validation_reason=None):
        super().__init__(reason)
        self.reason = reason
        self.validation_reason = (validation_reason if reason == "invalid_output"
                                  and type(validation_reason) is str
                                  and validation_reason in VALIDATION_DIAGNOSTIC_REASONS else "unknown")


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


class ReaderRuntime:
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
        self._stdout = queue.Queue(maxsize=256)
        self._overflow = threading.Event()
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
                or self.budget is None or model != self.budget.model):
            raise TransportError("invalid_config")
        codex = Path(codex)
        if not codex.is_file() or not os.access(codex, os.X_OK):
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

    def _drain_stdout(self, process, output, overflow):
        try:
            while True:
                # The process survives changes of role. Read with the largest
                # envelope here; _next enforces the active role's smaller cap.
                raw = process.stdout.readline(MAX_RECORDING_FRAME_BYTES + 1)
                if not raw:
                    break
                if len(raw) > MAX_RECORDING_FRAME_BYTES or not raw.endswith(b"\n"):
                    overflow.set()
                    break
                try:
                    output.put_nowait(raw)
                except queue.Full:
                    overflow.set()
                    break
        finally:
            try:
                output.put_nowait(None)
            except queue.Full:
                overflow.set()

    def _drain_stderr(self, process):
        # Authentication and provider details must not be persisted or surfaced.
        for _ in iter(lambda: process.stderr.read(4096), b""):
            pass

    def _send(self, message):
        if self.process is None or self.process.poll() is not None:
            raise TransportError("transport_error")
        try:
            raw = json.dumps(message, separators=(",", ":")).encode() + b"\n"
            if len(raw) > self._max_frame_bytes:
                raise TransportError("frame_cap")
            self.process.stdin.write(raw)
            self.process.stdin.flush()
        except OSError as exc:
            raise TransportError("transport_error") from exc

    def _next(self, deadline):
        if self._overflow.is_set():
            raise TransportError("frame_cap")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TransportError("timeout")
        try:
            raw = self._stdout.get(timeout=remaining)
        except queue.Empty as exc:
            raise TransportError("frame_cap" if self._overflow.is_set() else "timeout") from exc
        if self._overflow.is_set():
            raise TransportError("frame_cap")
        if raw is None:
            raise TransportError("transport_error")
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
        self._send({"method": method, "id": request_id, "params": params})
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
        self.scratch_dir.mkdir(parents=True, exist_ok=True)
        self._own_dir = Path(tempfile.mkdtemp(prefix="reader-", dir=self.scratch_dir))
        self.home = self._own_dir / "home"
        # The caller's scratch may live under the project. The model's cwd must
        # not: project config/rules can be inherited from ancestors.
        self._own_cwd = Path(tempfile.mkdtemp(prefix="mneme-reader-cwd-"))
        self.cwd = self._own_cwd
        self.home.mkdir(mode=0o700)
        (self.home / "auth.json").symlink_to(auth)
        env = os.environ.copy()
        for key in tuple(env):
            if key == "MNEME_DB" or key.startswith(("CODEX_", "OPENAI_", "ANTHROPIC_", "XDG_")):
                env.pop(key, None)
        env.update({"HOME": str(self.home), "CODEX_HOME": str(self.home), "GIT_CEILING_DIRECTORIES": str(self.cwd.parent),
                    "HF_HUB_OFFLINE": "1", "TRANSFORMERS_OFFLINE": "1"})
        command = [str(codex), "app-server", "--disable", "hooks", "--disable", "memories",
                   "--disable", "multi_agent", "--disable", "shell_tool",
                   "--disable", "plugins", "--disable", "apps",
                   "--disable", "browser_use", "--disable", "computer_use",
                   "--disable", "skill_search", "-c", "project_doc_max_bytes=0"]
        self.process = subprocess.Popen(command, cwd=self.cwd, env=env,
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE, start_new_session=True)
        threading.Thread(target=self._drain_stdout,
                         args=(self.process, self._stdout, self._overflow), daemon=True).start()
        threading.Thread(target=self._drain_stderr, args=(self.process,), daemon=True).start()
        initialized = self._request("initialize", {"clientInfo": {"name": "mneme_reader",
                                              "title": "Mneme memory reader", "version": "1.0.0"}}, deadline)
        self._send({"method": "initialized", "params": {}})
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
              validator=validate_answer, answer_bytes=MAX_ANSWER_BYTES, strict_json=False):
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
            raw_answer = answer.encode()
        except UnicodeError:
            raise TransportError("invalid_output") from None
        if len(raw_answer) > answer_bytes:
            raise TransportError("invalid_output", validation_reason="answer_bytes_limit")
        try:
            parsed = decode_json(raw_answer) if strict_json else json.loads(answer)
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

    def assess(self, observation, overlap_cards=None, *, timeout=TURN_SECONDS,
               recording_scope="project", expected_db_id=None, global_preferences_enabled=False,
               project_focus=None, tag_context=None) -> dict:
        """Assess one complete public projection, never perform a native write.

        A fresh recording-contract thread is used for every admitted assessment,
        even after another assessment. The process can survive success, but its
        thread cannot become a later reader/assessor conversation. A preflight
        refusal leaves an existing reader thread alone. The caller must reserve
        this one possible attempt durably before invoking us; no retry occurs.
        """
        receipt = self._fresh_assessment(
            lambda: recording_contract.prepare(observation, overlap_cards, budget=self.budget,
                recording_scope=recording_scope, expected_db_id=expected_db_id,
                global_preferences_enabled=global_preferences_enabled, project_focus=project_focus,
                **({"tag_context": tag_context} if tag_context is not None else {})),
            instructions=recording_contract.instructions_for, schema=recording_contract.schema_for,
            validator=recording_contract.validate_answer, field="assessment", service_name="mneme_recorder",
            success=lambda value: ("proposed" if value["proposal"] or value["maintenance"] else
                                   "validation_failed" if (value.get("omissions", {}).get("proposal")
                                       or value.get("omissions", {}).get("maintenance")) else "abstained"), timeout=timeout,
            frame_bytes=MAX_RECORDING_FRAME_BYTES, answer_bytes=recording_contract.MAX_OUTPUT_BYTES)
        value = receipt.pop("assessment")
        if value is not None:
            value = dict(value)
            omissions = value.pop("omissions", None)
            if omissions is not None:
                receipt["intent_omissions"] = omissions
        receipt.update(value if value is not None else {"proposal": None, "maintenance": []})
        return receipt

    def steward(self, targets, tag_context, *, timeout=TURN_SECONDS) -> dict:
        """Classify one finite summary batch, on a fresh tool-free thread."""
        import stewardship_contract
        receipt = self._fresh_assessment(
            lambda: stewardship_contract.prepare(targets, tag_context, budget=self.budget),
            instructions=stewardship_contract.instructions, schema=stewardship_contract.schema,
            validator=stewardship_contract.validate_answer, field="stewardship",
            service_name="mneme_tag_steward", success=lambda _: "classified", timeout=timeout,
            frame_bytes=MAX_FRAME_BYTES,
            answer_bytes=(self.budget.stewardship_answer_bytes if self.budget else MAX_ANSWER_BYTES))
        value = receipt.pop("stewardship")
        receipt["decisions"] = value["decisions"] if value is not None else None
        return receipt

    def route(self, current, witnesses, *, expected_db_id, timeout=TURN_SECONDS) -> dict:
        """One fresh bounded matching call; never a write or selector continuation."""
        import routing_contract
        return self._fresh_assessment(
            lambda: routing_contract.prepare(current, witnesses, expected_db_id=expected_db_id, budget=self.budget),
            instructions=lambda _: routing_contract.INSTRUCTIONS, schema=routing_contract.schema,
            validator=routing_contract.validate_answer, field="routing", service_name="mneme_router",
            success=lambda value: "matched" if value.hints else "neutral", timeout=timeout,
            frame_bytes=MAX_FRAME_BYTES, answer_bytes=(self.budget.routing_answer_bytes if self.budget is not None
                          else routing_contract.MAX_OUTPUT_BYTES))

    def _fresh_assessment(self, preparation, *, instructions, schema, validator, field,
                          service_name, success, timeout, frame_bytes, answer_bytes):
        """Shared one-shot transport; role-specific context never leaks into select.

        The caller owns durable reservation and accounting for this exact attempt.
        Preflight refusal leaves the existing selector thread alone. Once work
        starts, its selector handle and rotation counters are parked. Clean success
        restores that selector on the same live process, never role history.
        Any failure retires both handles; unknown usage still halts the runtime.
        """
        started = time.monotonic()
        receipt = {field: None, "reason": "provider_error", "usage": None,
                   "elapsed_ms": 0.0, "provider_attempt": False}
        with self._lock:
            preflight = True
            parked = None
            clean_success = False
            try:
                if self._closed or self._halted:
                    raise TransportError("closed" if self._closed else "usage_unknown")
                if self.budget is None:
                    raise TransportError("invalid_config")
                if (type(timeout) not in (int, float) or not 0 < timeout <= TURN_SECONDS
                        or not math.isfinite(timeout)):
                    raise TransportError("invalid_timeout")
                prompt, context_or_reason = preparation()
                if prompt is None:
                    raise TransportError(context_or_reason)
                deadline = started + timeout
                if time.monotonic() >= deadline:
                    raise TransportError("timeout")
                parked = (self.process, self._thread_id, self._turns, self._last_input)
                preflight = False
                self._frames = self._bytes = 0
                self._current_usage = None
                self._max_frame_bytes = frame_bytes
                options = {"base_instructions": instructions(context_or_reason),
                           "service_name": service_name}
                if self.process is None:
                    self._start(deadline, **options)
                else:
                    self._start_thread(deadline, **options)
                receipt["provider_attempt"] = True
                value, usage = self._turn(
                    prompt, context_or_reason, deadline,
                    output_schema=schema(context_or_reason), validator=validator,
                    answer_bytes=answer_bytes, strict_json=True)
                receipt.update({field: value, "reason": success(value), "usage": usage})
                clean_success = True
            except TransportError as exc:
                receipt["reason"] = exc.reason
                if exc.reason == "invalid_output":
                    receipt["validation_reason"] = exc.validation_reason
                if not preflight:
                    if self.process is not None:
                        receipt["provider_attempt"] = True
                    receipt["usage"] = self._current_usage
                    if receipt["provider_attempt"] and receipt["usage"] is None:
                        self._halted = True
                    self._stop_process()
            except (OSError, ValueError, TypeError, OverflowError):
                receipt["reason"] = "provider_error"
                if not preflight:
                    if self.process is not None:
                        receipt["provider_attempt"] = True
                    receipt["usage"] = self._current_usage
                    if receipt["provider_attempt"] and receipt["usage"] is None:
                        self._halted = True
                    self._stop_process()
            finally:
                if not preflight:
                    if (clean_success and parked is not None and parked[0] is self.process
                            and self.process is not None and self.process.poll() is None
                            and not self._halted and not self._closed):
                        self._thread_id, self._turns, self._last_input = parked[1:]
                    else:
                        self._thread_id = None
                        self._turns = self._last_input = 0
                    self._pending.clear()
                    self._max_frame_bytes = MAX_FRAME_BYTES
                receipt["elapsed_ms"] = (time.monotonic() - started) * 1000
        return receipt

    def _stop_process(self):
        process = self.process
        self.process = None
        self._thread_id = None
        self._pending.clear()
        if process is not None:
            if process.poll() is None:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                pass
            for stream in (process.stdin, process.stdout, process.stderr):
                if stream is not None:
                    stream.close()
        if self._own_dir is not None:
            shutil.rmtree(self._own_dir, ignore_errors=True)
            self._own_dir = None
        if self._own_cwd is not None:
            shutil.rmtree(self._own_cwd, ignore_errors=True)
            self._own_cwd = None
        self.home = self.cwd = None
        self._stdout = queue.Queue(maxsize=256)
        self._overflow.clear()

    def close(self):
        with self._lock:
            self._closed = True
            self._stop_process()

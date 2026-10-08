#!/usr/bin/env python3
"""Small streaming Codex adapter for the frozen async-task evaluation.

The caller owns the temporary project, auth symlink, installed hooks and call
budget. This module makes exactly one actor launch, never a retry or preflight.
It does not write transcripts or use the final-output file: Stop hooks can make
that file a checkpoint instead of the first submitted task plan.

Times are host receipt times, not model-generation or model-visibility times.
The CLI's item IDs are not presumed to be hook tool_use_id values. Detached
reader/service cleanup remains the caller's responsibility.
The sole raw-text receipt exception is the first task submission (at most 4096
bytes); tool output, progress, stderr and provider messages remain hashes only.
"""
from __future__ import annotations

import hashlib
import json
import math
import os
from pathlib import Path
import re
import selectors
import signal
import subprocess
import tempfile
import time

import async_task_cases

MAX_STDOUT_BYTES = 1024 * 1024
MAX_FRAME_BYTES = 64 * 1024
MAX_ORDINARY_FRAME_BYTES = 1024 * 1024
MAX_STDERR_BYTES = 64 * 1024
MAX_PROMPT_BYTES = 64 * 1024
MAX_EVENTS = 1024
MAX_ERRORS = 32
PROCESS_GROUP_ABSENCE_GRACE_SECONDS = 2.0
MODEL = "gpt-5.6-sol"
EFFORT = "low"
LIVE_HOME = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")).resolve()
EVENT_TYPES = {"thread.started", "turn.started", "turn.completed", "turn.failed",
               "item.started", "item.updated", "item.completed", "error"}
TOOL_TYPES = {"command_execution", "mcp_tool_call", "web_search", "file_change",
              "tool_call", "collab_tool_call", "todo_list"}
ITEM_TYPES = TOOL_TYPES | {"agent_message", "reasoning", "error"}
STATUSES = {"in_progress", "completed", "failed", "cancelled", "interrupted"}
USAGE_FIELDS = ("input_tokens", "cached_input_tokens", "output_tokens")


def _sha(raw):
    return hashlib.sha256(raw).hexdigest()


def _encoded(value):
    return json.dumps(value, ensure_ascii=True, separators=(",", ":")).encode()


def _identity(value):
    # IDs are useful for joins, but arbitrary event strings are not a transcript
    # escape hatch. Reject paths, whitespace, long data and credential prefixes.
    if (isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9_.:-]{1,128}", value)
            and not value.lower().startswith(("sk-", "bearer", "eyj"))):
        return value
    return None


def _error_details(raw):
    """Classification with a fixed vocabulary; no raw provider/log text escapes."""
    lower = raw.lower()
    patterns = (
        ("authentication", (b"invalid_api_key", b"authentication", b"unauthorized", b"401")),
        ("permission", (b"permission denied", b"forbidden", b"403")),
        ("rate_limit", (b"rate_limit", b"rate limit", b"quota", b"429")),
        ("context_limit", (b"context_length", b"context window", b"too many tokens")),
        ("model_unavailable", (b"model_not_found", b"model not found", b"unsupported model")),
        ("configuration", (b"config", b"unknown argument", b"unrecognized", b"invalid value")),
        ("connection", (b"connection", b"network", b"dns", b"stream disconnected", b"websocket")),
        ("timeout", (b"timeout", b"timed out", b"deadline")),
        ("provider_unavailable", (b"server_error", b"service unavailable", b"500", b"502", b"503")),
    )
    categories = [name for name, needles in patterns if any(n in lower for n in needles)]
    return {"categories": categories or ["unclassified"], "bytes": len(raw), "sha256": _sha(raw)}


def _actor_policy(model, effort):
    if (type(model) is not str or not re.fullmatch(r"[A-Za-z0-9_.:/-]{1,128}", model)
            or type(effort) is not str or effort not in ("low", "medium", "high", "xhigh", "max", "ultra")):
        raise ValueError("actor requires an explicit model identifier and supported reasoning effort")


def _command(codex, project, home, case=None, *, ordinary=False, model=MODEL, effort=EFFORT):
    _actor_policy(model, effort)
    if type(ordinary) is not bool or (ordinary and case is not None) or (not ordinary and case is None):
        raise ValueError("ordinary actor mode and case scoring are mutually exclusive")
    temporary_roots = (Path(tempfile.gettempdir()).resolve(), Path("/tmp").resolve())
    if (not home.is_dir() or home.is_symlink() or home.resolve().is_relative_to(LIVE_HOME)
            or not any(home.resolve().is_relative_to(root) for root in temporary_roots)):
        raise ValueError("actor home must be a temporary isolated directory")
    if (not project.is_dir() or project.is_symlink()
            or not any(project.resolve().is_relative_to(root) for root in temporary_roots)):
        raise ValueError("actor project must be a temporary isolated directory")
    command = [str(codex), "exec", *([] if ordinary else ["--ephemeral"]), "--skip-git-repo-check", "--ignore-rules",
               "--sandbox", "workspace-write", "--disable", "memories", "--disable", "multi_agent",
               "--disable", "unbounded_connection_retries", "-c", "project_doc_max_bytes=0",
               "-c", f'model_reasoning_effort="{effort}"', "-c", 'approval_policy="never"',
               "-c", 'web_search="disabled"']
    if ordinary:
        for feature in ("plugins", "apps", "browser_use", "computer_use", "skill_search"):
            command += ["--disable", feature]
    # Trust bypass is confined to this disposable harness invocation. Never
    # alter the user's config/trust state, and never follow a live hook symlink.
    hook_paths = (home / "hooks.json", project / ".codex/hooks.json")
    for path in hook_paths:
        owner = home if path == hook_paths[0] else project
        if path.is_symlink() or not path.resolve().is_relative_to(owner.resolve()):
            raise ValueError("temporary hook configuration must not be a symlink")
    if any(path.is_file() for path in hook_paths):
        command.append("--dangerously-bypass-hook-trust")
    if not ordinary and case["scene"]["tool_policy"] == "none":
        for feature in ("shell_tool", "unified_exec", "view_image", "image_generation", "apps",
                        "plugins", "browser_use", "computer_use", "goals", "sleep_tool",
                        "code_mode", "code_mode_host", "tool_suggest", "skill_search"):
            command += ["--disable", feature]
        command += ["-c", "tools.update_plan.enabled=false",
                    "-c", "tools.experimental_request_user_input.enabled=false"]
    # No output schema: the actor must retain its ordinary tool/hook path.
    return command + ["-m", model, "--json", "-C", str(project), "-"]


def _stop_group(process):
    """Finite cleanup even if a descendant inherited stdout/stderr."""
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            break
        except OSError:
            return False
        if sig == signal.SIGTERM:
            try:
                process.wait(timeout=0.2)
            except subprocess.TimeoutExpired:
                pass
    try:
        process.wait(timeout=1)
    except subprocess.TimeoutExpired:
        return False
    # killpg(..., 0) verifies group absence independently of reaping the leader.
    # A lingering zombie counts as unverified cleanup, not a claimed success.
    # Orphan reaping can lag SIGKILL/leader exit on macOS. This grace changes
    # cleanup latency only, never the recorded first-submission receipt time.
    deadline = time.monotonic() + PROCESS_GROUP_ABSENCE_GRACE_SECONDS
    while True:
        try:
            os.killpg(process.pid, 0)
        except ProcessLookupError:
            return True
        except OSError:
            return False
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.01)


def run_actor(*, codex: Path, project: Path, home: Path, env: dict,
              prompt: str, case: dict | None = None, timeout: float = 120, ordinary: bool = False,
              model: str = MODEL, effort: str = EFFORT) -> dict:
    """Run one actor, streaming and retaining only bounded, sanitized evidence.

    A completed CLI process without a submission remains a completed actor with
    first_submission=None, not an invented incorrect plan. A valid first plan
    survives later checkpoint/error events. Usage covers all reported turns.
    The caller owns the finite positive timeout; there is no additional host cap.
    """
    if type(ordinary) is not bool or (ordinary and case is not None) or (not ordinary and case is None):
        raise ValueError("ordinary actor mode and case scoring are mutually exclusive")
    _actor_policy(model, effort)
    started = time.monotonic_ns()
    result = {"status": "not_started", "actor_exit": None, "model": model, "effort": effort,
              "started_monotonic_ns": started, "first_submission": None,
              "session_id": None, "events": [], "errors": [], "error_count": 0,
              "usage": {"status": "missing", "provider_reported": False, "turns": [], "totals": None},
              "timing": "host_monotonic_receipt", "memory_visibility": "unknown",
              "tool_boundary_before_submission": None, "cleanup_complete": True,
              "cleanup_scope": "owned_process_group_absent_and_parent_reaped_not_detached_readers",
              "cleanup_group_absence_grace_ms": round(PROCESS_GROUP_ABSENCE_GRACE_SECONDS * 1000),
              "retry_policy": {"harness_retries": 0, "unbounded_connection_retries": False,
                               "internal_provider_retries": "unknown"},
              "tool_policy": "ordinary" if ordinary else case["scene"]["tool_policy"],
              "rollout_policy": "persisted" if ordinary else "ephemeral",
              "grading": "external_artifact_only" if ordinary else "case_first_submission",
              "provider_tool_list_observed": False,
              "input": {"prompt_bytes": None, "prompt_sha256": None, "stdin_bytes_sent": 0,
                        "timeout_seconds": None}}
    stderr = bytearray()
    stderr_hash = hashlib.sha256()
    stdout_hash = hashlib.sha256()
    stdout_bytes = stderr_bytes = 0
    event_count = 0
    accounting_unknown = False
    known_totals = {key: 0 for key in (*USAGE_FIELDS, "uncached_input_tokens")}
    valid_usage_turns = 0
    reasoning_total = 0
    reasoning_complete = True
    pending = bytearray()
    discard_hash = None
    discard_bytes = 0
    oversized_hash = hashlib.sha256()
    observation = {"diagnostics_complete": True, "parsed_frames": 0, "omitted_events": 0,
                   "omitted_usage_turns": 0, "stdout_retention_limit_reached": False,
                   "stderr_retention_limit_reached": False, "unparsed_frames": 0,
                   "stderr_tail_complete": True,
                   "unparsed_bytes": 0, "unparsed_receipts": [], "unparsed_receipts_omitted": 0,
                   "frame_limit_bytes": MAX_ORDINARY_FRAME_BYTES}
    if ordinary:
        result["observation"] = observation
    raw_events = []  # Scorer-only; never included in a receipt or written to disk.
    completed_turns = 0
    saw_failed_turn = False
    process = None
    selector = selectors.DefaultSelector()
    failed = False

    def error(category, *, source="host", raw=None, event_index=None, details=None):
        result["error_count"] += 1
        if len(result["errors"]) < MAX_ERRORS:
            row = {"category": category, "source": source}
            if raw is not None:
                row.update(_error_details(raw))
            if event_index is not None:
                row["event_index"] = event_index
            if details is not None:
                row["details"] = details  # Only fixed host strings or numeric errno.
            result["errors"].append(row)

    def fail(category, *, raw=None, source="host", event_index=None):
        nonlocal failed
        error(category, raw=raw, source=source, event_index=event_index)
        result["status"] = "timeout" if category == "actor_deadline" else "protocol_error"
        failed = True

    def stderr_chunk(chunk):
        nonlocal stderr_bytes
        previous = stderr_bytes
        stderr_bytes += len(chunk)
        stderr_hash.update(chunk)
        stderr.extend(chunk[:max(0, MAX_STDERR_BYTES - len(stderr))])
        if previous <= MAX_STDERR_BYTES < stderr_bytes:
            if ordinary:
                observation.update(diagnostics_complete=False, stderr_retention_limit_reached=True)
            else:
                fail("stderr_byte_limit")

    def event_frame(raw, received):
        nonlocal completed_turns, saw_failed_turn, failed, event_count
        nonlocal valid_usage_turns, reasoning_total, reasoning_complete
        index = event_count
        if not ordinary and index >= MAX_EVENTS:
            fail("event_count_limit")
            return
        try:
            event = json.loads(raw)
            if not isinstance(event, dict):
                raise ValueError("event is not an object")
        except (ValueError, UnicodeError, RecursionError):
            fail("invalid_json_event", raw=raw, source="stdout", event_index=index)
            return
        event_count += 1
        if ordinary:
            observation["parsed_frames"] += 1
        else:
            raw_events.append(event)
        kind = event.get("type") if isinstance(event.get("type"), str) else None
        row = {"event_index": index, "received_monotonic_ns": received,
               "event_type": kind if kind in EVENT_TYPES else "other",
               "frame_bytes": len(raw), "frame_sha256": _sha(raw)}
        for key in ("session_id", "thread_id", "turn_id", "request_id"):
            value = _identity(event.get(key))
            if value is not None:
                row[key] = value
        if kind == "thread.started" and row.get("thread_id"):
            result["session_id"] = row["thread_id"]
        elif row.get("session_id"):
            result["session_id"] = row["session_id"]
        item = event.get("item")
        if isinstance(item, dict):
            item_kind = item.get("type") if isinstance(item.get("type"), str) else None
            row["item_type"] = item_kind if item_kind in ITEM_TYPES else "other"
            for key in ("id", "tool_use_id", "turn_id", "request_id"):
                value = _identity(item.get(key))
                if value is not None:
                    row["item_id" if key == "id" else key] = value
            if isinstance(item.get("status"), str) and item["status"] in STATUSES:
                row["status"] = item["status"]
            if type(item.get("exit_code")) is int:
                row["exit_code"] = item["exit_code"]
            if item_kind in TOOL_TYPES:
                row["tool_category"] = item_kind
            # Log only lengths/hashes, not tool command/arguments/results or
            # progress messages, even if they contain plausible credential text.
            for key in ("command", "arguments", "aggregated_output", "output", "result", "text"):
                if key in item:
                    value = item[key]
                    data = value.encode(errors="replace") if isinstance(value, str) else _encoded(value)
                    row[key + "_bytes"] = len(data)
                    row[key + "_sha256"] = _sha(data)
        if not ordinary or (len(result["events"]) < MAX_EVENTS and stdout_bytes <= MAX_STDOUT_BYTES):
            result["events"].append(row)
        else:
            observation["diagnostics_complete"] = False
            observation["omitted_events"] += 1
        # Tool/item failures can be recoverable within the same actor turn.
        # Preserve classified evidence without treating them as turn.failed.
        if row.get("item_type") == "error":
            error("actor_item_error", source="stdout", raw=raw, event_index=index)
        elif row.get("tool_category") in TOOL_TYPES and (
                row.get("status") == "failed" or
                (type(row.get("exit_code")) is int and row["exit_code"] != 0)):
            error("tool_item_failure", source="stdout", raw=raw, event_index=index)
        if not ordinary and case["scene"]["tool_policy"] == "none" and row.get("tool_category") in TOOL_TYPES:
            fail("no_tool_policy_violation", source="stdout", event_index=index)
            return
        if (not ordinary and result["first_submission"] is None and kind == "item.completed"
                and row.get("item_type") == "agent_message"):
            try:
                submission = async_task_cases.first_submission(case, raw_events)
            except (ValueError, TypeError, RecursionError, UnicodeError):
                fail("scorer_rejected_event", raw=raw, source="stdout", event_index=index)
                return
            if submission is not None:
                observed = result["events"][submission["event_index"]]
                submission.update({"received_monotonic_ns": observed["received_monotonic_ns"],
                                   "timing": "host_monotonic_receipt",
                                   "session_id": result["session_id"],
                                   "item_id": observed.get("item_id"), "turn_id": observed.get("turn_id")})
                text = raw_events[submission["event_index"]]["item"]["text"]
                submission["text"] = text if len(text.encode()) <= 4096 else None
                result["first_submission"] = submission
                result["tool_boundary_before_submission"] = any(
                    r["event_type"] == "item.completed" and r.get("tool_category") in TOOL_TYPES
                    for r in result["events"][:submission["event_index"]])
        if kind == "turn.completed":
            completed_turns += 1
            supplied = event.get("usage")
            if (isinstance(supplied, dict) and all(type(supplied.get(k)) is int
                    and supplied[k] >= 0 for k in USAGE_FIELDS)
                    and supplied["cached_input_tokens"] <= supplied["input_tokens"]):
                usage = {k: supplied[k] for k in USAGE_FIELDS}
                usage["uncached_input_tokens"] = usage["input_tokens"] - usage["cached_input_tokens"]
                reasoning = supplied.get("reasoning_output_tokens")
                usage["reasoning_output_tokens"] = (reasoning if type(reasoning) is int
                    and 0 <= reasoning <= supplied["output_tokens"] else None)
                if reasoning is not None and usage["reasoning_output_tokens"] is None:
                    error("reasoning_usage_invalid", source="stdout", event_index=index)
                usage["event_index"] = index
                valid_usage_turns += 1
                for key in known_totals:
                    known_totals[key] += usage[key]
                if usage["reasoning_output_tokens"] is None:
                    reasoning_complete = False
                else:
                    reasoning_total += usage["reasoning_output_tokens"]
                if not ordinary or len(result["usage"]["turns"]) < MAX_EVENTS:
                    result["usage"]["turns"].append(usage)
                else:
                    observation["omitted_usage_turns"] += 1
                    observation["diagnostics_complete"] = False
            else:
                error("usage_missing_or_invalid", source="stdout", event_index=index)
        if kind in ("error", "turn.failed"):
            saw_failed_turn = True
            error("provider_event_error", source="stdout", raw=raw, event_index=index)
            result["status"] = "error"
            # No harness retries or acceptance of a later recovery as a clean run.
            failed = True

    def ordinary_stdout(chunk, received, *, eof=False):
        """Bound decoding, not the actor: discard opaque oversized lines through LF."""
        nonlocal discard_hash, discard_bytes, event_count, accounting_unknown
        offset = 0
        while offset < len(chunk) or eof:
            end = chunk.find(b"\n", offset)
            segment = chunk[offset:end if end >= 0 else len(chunk)]
            if discard_hash is not None:
                discard_hash.update(segment)
                oversized_hash.update(segment)
                discard_bytes += len(segment)
            elif len(pending) + len(segment) > MAX_ORDINARY_FRAME_BYTES:
                accounting_unknown = True
                observation["diagnostics_complete"] = False
                discard_hash = hashlib.sha256()
                discard_hash.update(pending)
                discard_hash.update(segment)
                oversized_hash.update(pending)
                oversized_hash.update(segment)
                discard_bytes = len(pending) + len(segment)
                pending.clear()
            else:
                pending.extend(segment)
            if end >= 0 or eof:
                if discard_hash is not None:
                    accounting_unknown = True
                    observation["diagnostics_complete"] = False
                    observation["unparsed_frames"] += 1
                    observation["unparsed_bytes"] += discard_bytes
                    receipt = {"event_index": event_count, "bytes": discard_bytes,
                               "sha256": discard_hash.hexdigest(), "terminated_by_newline": end >= 0}
                    if len(observation["unparsed_receipts"]) < MAX_ERRORS:
                        observation["unparsed_receipts"].append(receipt)
                    else:
                        observation["unparsed_receipts_omitted"] += 1
                    event_count += 1
                    discard_hash, discard_bytes = None, 0
                elif pending.strip():
                    event_frame(bytes(pending), received)
                pending.clear()
            if end < 0 or failed:
                break
            offset = end + 1

    try:
        if not isinstance(prompt, str) or len(prompt.encode()) > MAX_PROMPT_BYTES:
            raise ValueError("actor prompt must be text within 64 KiB")
        if type(timeout) not in (int, float) or not math.isfinite(timeout) or timeout <= 0:
            raise ValueError("actor timeout must be a finite positive number")
        result["input"].update(prompt_bytes=len(prompt.encode()), prompt_sha256=_sha(prompt.encode()),
                                timeout_seconds=timeout)
        codex, project, home = Path(codex), Path(project), Path(home)
        argv = _command(codex, project, home, case, ordinary=ordinary, model=model, effort=effort)
        actor_env = dict(env)
        if ordinary:
            for key in tuple(actor_env):
                if key.startswith(("CODEX_", "OPENAI_", "ANTHROPIC_", "XDG_")):
                    actor_env.pop(key, None)
            actor_env["HOME"] = str(home)
        actor_env["CODEX_HOME"] = str(home)
        actor_env["GIT_CEILING_DIRECTORIES"] = str(project.parent)
        process = subprocess.Popen(argv, cwd=project, env=actor_env, stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=True)
        result["status"] = "running"
        assert process.stdin is not None and process.stdout is not None and process.stderr is not None
        for stream, label, mask in ((process.stdout, "stdout", selectors.EVENT_READ),
                                    (process.stderr, "stderr", selectors.EVENT_READ),
                                    (process.stdin, "stdin", selectors.EVENT_WRITE)):
            os.set_blocking(stream.fileno(), False)
            selector.register(stream, mask, label)
        prompt_bytes = prompt.encode()
        prompt_offset = 0
        deadline = time.monotonic() + timeout
        while selector.get_map() and not failed:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                fail("actor_deadline")
                break
            for key, _mask in selector.select(min(remaining, 0.1)):
                stream, label = key.fileobj, key.data
                if label == "stdin":
                    try:
                        sent = os.write(stream.fileno(), prompt_bytes[prompt_offset:prompt_offset + 8192])
                        prompt_offset += sent
                        result["input"]["stdin_bytes_sent"] = prompt_offset
                    except BlockingIOError:
                        # Readiness may change between select and write. Resume
                        # the same stdin write, not a new actor/provider attempt.
                        continue
                    except BrokenPipeError:
                        fail("stdin_closed_before_prompt_sent")
                    if failed or prompt_offset == len(prompt_bytes):
                        selector.unregister(stream)
                        stream.close()
                    continue
                try:
                    chunk = os.read(stream.fileno(), 8192)
                except BlockingIOError:
                    continue
                received = time.monotonic_ns()
                if not chunk:
                    selector.unregister(stream)
                    stream.close()
                    if label == "stdout":
                        if ordinary:
                            ordinary_stdout(b"", received, eof=True)
                        elif pending:
                            event_frame(bytes(pending), received)
                            pending.clear()
                    continue
                if label == "stderr":
                    stderr_chunk(chunk)
                    continue
                stdout_bytes += len(chunk)
                stdout_hash.update(chunk)
                if stdout_bytes > MAX_STDOUT_BYTES:
                    if ordinary:
                        observation.update(diagnostics_complete=False, stdout_retention_limit_reached=True)
                    else:
                        fail("stdout_byte_limit")
                        break
                if ordinary:
                    ordinary_stdout(chunk, received)
                    if failed:
                        break
                    continue
                pending.extend(chunk)
                while b"\n" in pending and not failed:
                    end = pending.index(b"\n")
                    if end > MAX_FRAME_BYTES:
                        fail("frame_byte_limit")
                        break
                    raw = bytes(pending[:end])
                    del pending[:end + 1]
                    if raw.strip():
                        event_frame(raw, received)
                if len(pending) > MAX_FRAME_BYTES and not failed:
                    fail("frame_byte_limit")
                if failed:
                    break
        if ordinary and discard_hash is not None:
            # Deadline/I/O/terminal failure may end observation mid-opaque-line.
            ordinary_stdout(b"", time.monotonic_ns(), eof=True)
        if failed:
            result["cleanup_complete"] = _stop_group(process)
        else:
            try:
                process.wait(timeout=max(0.001, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                fail("actor_deadline")
                result["cleanup_complete"] = _stop_group(process)
            if not failed:
                result["status"] = "completed" if process.returncode == 0 else "error"
        result["actor_exit"] = process.returncode
        if process.returncode not in (None, 0) and not failed:
            error("actor_nonzero_exit", details={"returncode": process.returncode})
    except (OSError, ValueError, TypeError) as exc:
        result["status"] = "spawn_error" if process is None else "host_error"
        details = {"exception": type(exc).__name__}
        if isinstance(exc, OSError):
            details["errno"] = exc.errno
        error("actor_launch_or_io_error", raw=str(exc).encode(errors="replace"), details=details)
        if process is not None:
            result["cleanup_complete"] = _stop_group(process)
            result["actor_exit"] = process.returncode
    finally:
        if ordinary and discard_hash is not None:
            ordinary_stdout(b"", time.monotonic_ns(), eof=True)
        # Reap/terminate the owned group even after a successful parent exit:
        # a child may have redirected its pipes and outlived the actor.
        if process is not None:
            result["cleanup_complete"] = _stop_group(process)
            result["actor_exit"] = process.returncode
        # A stdout failure may have won the selector race against diagnostics
        # already buffered on stderr. Drain that finite tail after cleanup too.
        if process is not None and process.stderr is not None and not process.stderr.closed:
            tail_remaining = MAX_STDERR_BYTES
            while (ordinary and tail_remaining > 0) or (not ordinary and stderr_bytes <= MAX_STDERR_BYTES):
                try:
                    chunk = os.read(process.stderr.fileno(), 8192)
                except (BlockingIOError, OSError):
                    break
                if not chunk:
                    break
                stderr_chunk(chunk)
                tail_remaining -= len(chunk)
            if ordinary and tail_remaining <= 0:
                observation.update(diagnostics_complete=False, stderr_tail_complete=False)
        for key in list(selector.get_map().values()):
            selector.unregister(key.fileobj)
            key.fileobj.close()
        selector.close()
        result["finished_monotonic_ns"] = time.monotonic_ns()
        result["elapsed_ms"] = round((result["finished_monotonic_ns"] - started) / 1_000_000, 3)
    if not result["cleanup_complete"]:
        error("process_group_cleanup_unverified")
        if result["status"] == "completed":
            result["status"] = "host_error"
    if stderr:
        result["stderr"] = {**_error_details(bytes(stderr)), "bytes": stderr_bytes,
                            "sha256": stderr_hash.hexdigest(), "retained_bytes": len(stderr)}
        if result["status"] != "completed":
            error("actor_stderr", source="stderr", raw=bytes(stderr))
    else:
        result["stderr"] = {"bytes": 0, "sha256": stderr_hash.hexdigest(), "categories": []}
    result["stdout"] = {"bytes": stdout_bytes, "sha256": stdout_hash.hexdigest()}
    if ordinary:
        observation["unparsed_sha256"] = oversized_hash.hexdigest()
        result["usage"]["accounting_unknown"] = accounting_unknown
        result["usage"]["totals_scope"] = "known_reported_turns_may_be_incomplete" if accounting_unknown else "reported_turns"
        result["usage"]["reported_turns"] = valid_usage_turns
        result["usage"]["observed_completed_turns"] = completed_turns
    turns = result["usage"]["turns"]
    if turns:
        result["usage"]["provider_reported"] = True
        result["usage"]["status"] = ("complete" if valid_usage_turns == completed_turns and not accounting_unknown
            and not saw_failed_turn and result["status"] == "completed" else "partial")
        result["usage"]["totals"] = known_totals
        result["usage"]["totals"]["reasoning_output_tokens"] = (
            reasoning_total
            if result["usage"]["status"] == "complete"
            and reasoning_complete else None)
    elif accounting_unknown:
        result["usage"]["status"] = "partial"
    return result

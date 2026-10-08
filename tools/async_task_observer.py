#!/usr/bin/env python3
"""Disposable, metadata-only observation of the installed async task path.

No product files are changed. The sole intentional behavioral restriction is a
conservative one-select-per-arm reservation. Observation adds timing overhead;
transaction bounds and flushed hook output do not establish model visibility.
"""
from __future__ import annotations

import argparse
import contextlib
import fcntl
import hashlib
import importlib
import io
import json
import os
from pathlib import Path
import shlex
import sys
import time

MAX_TRACE = 1024 * 1024
MAX_EVENT = 16 * 1024
SCRIPT = Path(__file__).resolve()


def _sha(raw):
    return hashlib.sha256(raw).hexdigest()


def _string(value, limit=160):
    return value if isinstance(value, str) and len(value.encode()) <= limit else None


def _cards(value):
    return [{key: _string(card.get(key)) for key in ("id", "fingerprint")}
            for card in value[:6] if isinstance(card, dict)] if isinstance(value, list) else []


def _identity(value):
    if not isinstance(value, dict):
        return {}
    return {key: _string(value.get(key)) for key in ("session_id", "turn_id", "tool_use_id")
            if _string(value.get(key)) is not None}


def _job(value):
    if not isinstance(value, dict):
        return None
    return {key: value[key] for key in ("turn", "request", "epoch", "closed", "fence")
            if key in value and (type(value[key]) in (int, bool) or _string(value[key]) is not None)}


def _snapshot(data):
    result = {key: data.get(key) for key in
              ("generation", "epoch", "context_generation", "fence", "attempts", "unknown_usage")}
    for key in ("active", "pending", "inflight", "reservation", "ready"):
        result[key] = _job(data.get(key))
    if isinstance(data.get("ready"), dict):
        result["ready"]["cards"] = _cards(data["ready"].get("cards"))
    result["counts"] = {key: value for key, value in data.get("counts", {}).items()
                        if key in ("selected", "ready", "emitted", "dropped", "failed", "abstained", "capacity")
                        and type(value) is int}
    return result


def _paths(value):
    """Copy only typed native graph observations, never summaries or sources."""
    if not isinstance(value, dict):
        return None
    rows = value.get("cards")
    if not isinstance(rows, list):
        return None
    projected = []
    for row in rows[:6]:
        if not isinstance(row, dict):
            continue
        item = {key: _string(row.get(key)) for key in ("node_id", "card_sha256", "lane")}
        path = row.get("graph_path")
        if isinstance(path, list):
            item["graph_path"] = [{key: _string(hop.get(key)) for key in
                ("previous", "target", "from", "to", "kind", "anchor")}
                for hop in path[:4] if isinstance(hop, dict)]
        projected.append(item)
    return {"schema": value.get("schema"), "learning": _string(value.get("learning")), "cards": projected}


class Observer:
    def __init__(self, bundle_lib: Path, trace: Path, session_id=None):
        self.bundle_lib = bundle_lib.resolve()
        self.trace = trace.resolve()
        self.identity = {"session_id": session_id} if session_id else {}
        self.hook_name = None
        self.consumed = []

    def record(self, event, **values):
        """One bounded O_APPEND write; no logging error escapes into product code."""
        fd = None
        try:
            row = {"schema": "mneme.async-task-observer.v1", "event": event,
                   "monotonic_ns": time.monotonic_ns(), "pid": os.getpid(),
                   **self.identity, **values}
            raw = (json.dumps(row, ensure_ascii=False, separators=(",", ":"), allow_nan=False) + "\n").encode()
            if len(raw) > MAX_EVENT:
                return
            fd = os.open(self.trace, os.O_WRONLY | os.O_CREAT | os.O_APPEND | getattr(os, "O_NOFOLLOW", 0), 0o600)
            # Separate observer lock, never the reader's state lock.
            fcntl.flock(fd, fcntl.LOCK_EX)
            if os.fstat(fd).st_size + len(raw) <= MAX_TRACE:
                os.write(fd, raw)
        except Exception:
            pass
        finally:
            if fd is not None:
                try:
                    os.close(fd)
                except OSError:
                    pass

    def reserve(self):
        try:
            fd = os.open(self.trace.with_suffix(".reader-reserved"),
                         os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            os.close(fd)
            return True
        except OSError:
            return False

    def instrument_worker(self, rw):
        """Observe each freshly loaded worker module without changing its loader."""
        if getattr(rw, "_async_task_observer", None) is self:
            return
        original_state = rw._state
        def state(config, session_id, mutate, **kwargs):
            started = time.monotonic_ns()
            captured = {}
            def observe_mutation(data):
                try:
                    captured["before"] = _snapshot(data)
                except Exception:
                    pass
                result = mutate(data)
                try:
                    captured["after"] = _snapshot(data)
                    captured["changed"] = bool(result[1])
                except Exception:
                    pass
                return result
            result = original_state(config, session_id, observe_mutation, **kwargs)
            completed = time.monotonic_ns()
            if result[0] and captured.get("after"):
                active = captured["after"].get("active")
                self.identity.setdefault("session_id", session_id)
                if active and self.hook_name is None:
                    self.identity.update(turn_id=active.get("turn"), request_sha256=active.get("request"))
                elif active and active.get("turn") == self.identity.get("turn_id"):
                    self.identity["request_sha256"] = active.get("request")
            if not result[0] or captured.get("changed"):
                self.record("state_transaction", operation=_string(getattr(mutate, "__name__", "unknown")),
                            start_monotonic_ns=started, end_monotonic_ns=completed,
                            success=result[0], before=captured.get("before"), after=captured.get("after"))
            return result
        rw._state = state

        original_consume = rw.consume
        def consume(*args, **kwargs):
            result = original_consume(*args, **kwargs)
            try:
                if isinstance(result, dict) and result.get("outcome") == "emitted":
                    self.consumed = _cards(result.get("cards"))
            except Exception:
                pass
            return result
        rw.consume = consume
        rw._async_task_observer = self

    def install(self, hooks, rw, recall, runtime):
        original_handle = hooks.handle_event
        def handle(event, config, **kwargs):
            try:
                self.identity = _identity(event)
                self.hook_name = _string(event.get("hook_event_name")) if isinstance(event, dict) else None
                self.consumed = []
                self.record("hook_enter", hook_event_name=self.hook_name,
                            reader_background=kwargs.get("reader_background", False))
            except Exception:
                pass
            return original_handle(event, config, **kwargs)
        hooks.handle_event = handle

        self.instrument_worker(rw)
        original_loader = getattr(hooks, "_reader_worker", None)
        if original_loader is not None:
            def load_worker():
                module = original_loader()
                self.instrument_worker(module)
                return module
            hooks._reader_worker = load_worker

        original_collect = recall.collect_reader
        def collect(*args, **kwargs):
            started = time.monotonic_ns()
            result = original_collect(*args, **kwargs)
            try:
                if isinstance(result, dict):
                    self.record("native_pool", start_monotonic_ns=started,
                                outcome=_string(result.get("outcome")), elapsed_ms=result.get("elapsed_ms"),
                                cards=_cards(result.get("cards")), observation=_paths(result.get("observation")))
            except Exception:
                pass
            return result
        recall.collect_reader = collect

        original_select = runtime.ReaderRuntime.select
        def select(instance, dialogue, cards):
            started = time.monotonic_ns()
            try:
                self.record("reader_select_enter", cards=_cards(cards),
                            graph_paths=_paths({"cards": [{"node_id": card.get("id"),
                                "graph_path": card.get("native", {}).get("graph_path")}
                                for card in cards[:6] if isinstance(card, dict)]}) if isinstance(cards, list) else None)
            except Exception:
                pass
            if not self.reserve():
                self.record("reader_cap_refused", provider_attempt=False)
                return {"selected_ids": [], "reason": "experiment_reader_cap", "usage": None,
                        "elapsed_ms": 0.0, "provider_attempt": False}
            try:
                result = original_select(instance, dialogue, cards)
            except BaseException:
                self.record("reader_select_error", start_monotonic_ns=started,
                            provider_attempt="unknown")
                raise
            try:
                if isinstance(result, dict):
                    usage = result.get("usage")
                    usage = {key: value for key, value in usage.items() if key in
                             ("input_tokens", "cached_input_tokens", "cache_write_input_tokens", "uncached_input_tokens",
                              "output_tokens", "reasoning_output_tokens", "total_tokens")
                             and type(value) is int} if isinstance(usage, dict) else None
                    self.record("reader_select_complete", start_monotonic_ns=started,
                                selected_ids=[value for value in result.get("selected_ids", [])[:2]
                                              if _string(value) is not None],
                                reason=_string(result.get("reason")), usage=usage,
                                elapsed_ms=result.get("elapsed_ms"), provider_attempt=result.get("provider_attempt") is True)
            except Exception:
                pass
            return result
        runtime.ReaderRuntime.select = select

        original_popen = rw.subprocess.Popen
        expected = str((self.bundle_lib / "reader_worker.py").resolve())
        def popen(command, *args, **kwargs):
            if (isinstance(command, list) and len(command) >= 3
                    and command[:3] == [sys.executable, expected, "--serve"]):
                command = [command[0], str(SCRIPT), "--bundle-lib", str(self.bundle_lib),
                           "--trace", str(self.trace), "--", *command[2:]]
                # Popen occurs under the product lock: no observer IO here.
            return original_popen(command, *args, **kwargs)
        rw.subprocess.Popen = popen

    def run_hook(self, hooks, args):
        stream = io.StringIO()
        original_stdout = sys.stdout
        with contextlib.redirect_stdout(stream):
            result = hooks.main(args)
        output = stream.getvalue()
        original_stdout.write(output)
        original_stdout.flush()
        completed = time.monotonic_ns()
        try:
            parsed = json.loads(output)
            specific = parsed.get("hookSpecificOutput", {})
            context = specific.get("additionalContext")
            context = context if isinstance(context, str) else ""
            self.record("hook_stdout_flushed", hook_event_name=self.hook_name,
                        completed_monotonic_ns=completed, output_sha256=_sha(output.encode()),
                        output_bytes=len(output.encode()), context_sha256=_sha(context.encode()),
                        context_bytes=len(context.encode()),
                        cards=self.consumed if context and self.hook_name == "PostToolUse" else [])
        except Exception:
            pass
        return result


def install_observer(home_hooks: Path, prefix: Path, trace: Path) -> dict:
    """Replace only installed command routes in an explicitly disposable config."""
    home_hooks, prefix, trace = map(lambda path: Path(path).resolve(), (home_hooks, prefix, trace))
    if trace.exists() or trace.with_suffix(".reader-reserved").exists():
        raise ValueError("observer trace/reservation must be fresh")
    original = home_hooks.read_bytes()
    config = json.loads(original)
    count = 0
    for groups in config.get("hooks", {}).values():
        for group in groups:
            for hook in group.get("hooks", []):
                if hook.get("type") != "command":
                    continue
                command = shlex.split(hook.get("command", ""))
                if len(command) < 2 or Path(command[1]).resolve() != prefix / "lib/hooks.py":
                    continue
                hook["command"] = shlex.join([command[0], str(SCRIPT), "--bundle-lib", str(prefix / "lib"),
                    "--trace", str(trace), "--", *command[2:]])
                count += 1
    if not count:
        raise ValueError("no installed hook commands found")
    raw = (json.dumps(config, ensure_ascii=False, indent=2) + "\n").encode()
    home_hooks.write_bytes(raw)
    return {"observer_sha256": _sha(SCRIPT.read_bytes()), "hook_commands_wrapped": count,
            "hooks_before_sha256": _sha(original), "hooks_after_sha256": _sha(raw),
            "max_trace_bytes": MAX_TRACE, "max_event_bytes": MAX_EVENT,
            "reader_select_cap": 1, "reservation": "exclusive_before_select_conservative",
            "timing": "host_monotonic_observation_with_instrumentation_overhead",
            "ready_timing": "successful_transaction_start_end_bounds",
            "visibility": "unknown"}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle-lib", type=Path, required=True)
    parser.add_argument("--trace", type=Path, required=True)
    parser.add_argument("args", nargs=argparse.REMAINDER)
    options = parser.parse_args(argv)
    args = options.args[1:] if options.args[:1] == ["--"] else options.args
    if not args:
        parser.error("an installed hook or worker command is required")
    bundle = options.bundle_lib.resolve()
    sys.path.insert(0, str(bundle))
    modules = [importlib.import_module(name) for name in ("hooks", "reader_worker", "hook_recall", "reader_runtime")]
    if any(Path(module.__file__).resolve().parent != bundle for module in modules):
        raise RuntimeError("observer requires modules from the copied installed bundle")
    session = args[args.index("--session-id") + 1] if "--session-id" in args else None
    observer = Observer(bundle, options.trace, session)
    observer.install(*modules)
    if args[0] == "--serve":
        observer.record("worker_enter")
        return modules[1].main(args)
    return observer.run_hook(modules[0], args)


if __name__ == "__main__":
    raise SystemExit(main())

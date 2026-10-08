#!/usr/bin/env python3
"""Bounded, optional async reader state and one reusable worker per root session.

The foreground functions perform local, nonblocking state transactions only. They
never call Mneme, Codex, or wait for a worker. A failed optional transaction skips.
"""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import sys
import tempfile
import time
from typing import Any
from reader_contract import plan as reader_plan, prepare as reader_prepare
from librarian_policy import resolve, LibrarianBudget
from touchstone_contract import delivery_cache_fingerprint
from target_policy import (target_kwargs, global_preferences_policy,
                           GLOBAL_PREFERENCE_CUE)

SCHEMA = "mneme.codex-reader.state.v2"
ID = re.compile(r"^[A-Za-z0-9_:.-]{1,160}$")
MAX_STATE = 18_000
MAX_SELECTION_BYTES = 4096
MAX_PROMPT = 4096
MAX_WORKSPACE_BINDING = 16 * 1024
MAX_FRAGMENT = 360
# Medium aliases for offline callers; operational admission resolves captured policy.
MAX_ATTEMPTS = LibrarianBudget().attempts
MAX_INPUT = LibrarianBudget().input_tokens
MAX_OUTPUT = LibrarianBudget().output_tokens
IDLE_SECONDS = 300
MAX_WORKERS = 2


def _short(value: str, limit: int) -> str:
    return value.encode("utf-8")[:limit].decode("utf-8", "ignore")


def _fragment(value: str) -> str:
    return _short(" ".join("".join(c if ord(c) >= 32 and ord(c) != 127 else " " for c in value).split()), MAX_FRAGMENT)


def _identity(event: dict[str, Any], prompt: bool = False) -> tuple[str, str] | None:
    session, turn = event.get("session_id"), event.get("turn_id")
    if not isinstance(session, str) or not isinstance(turn, str) or not ID.fullmatch(session) or not ID.fullmatch(turn):
        return None
    if prompt and not isinstance(event.get("prompt"), str):
        return None
    return session, turn


def _directory(config: dict[str, Any]) -> Path:
    return Path(config["state_dir"]) / "reader"


def _paths(config: dict[str, Any], session_id: str) -> tuple[Path, Path]:
    base = _directory(config) / hashlib.sha256(session_id.encode()).hexdigest()
    return base.with_suffix(".json"), base.with_suffix(".lock")


def _new() -> dict[str, Any]:
    return {"schema": SCHEMA, "generation": secrets.token_hex(16), "epoch": 0,
            "context_generation": 0, "active": None, "pending": None,
            "inflight": None, "reservation": None, "ready": None, "fence": "",
            "prior": "", "emitted": [], "attempts": 0, "input_tokens": 0,
            "output_tokens": 0, "cached_input_tokens": 0, "reasoning_output_tokens": 0,
            "unknown_usage": False, "worker_pid": None,
            "counts": {name: 0 for name in ("selected", "ready", "emitted", "dropped", "failed", "abstained", "capacity")},
            "touched": time.time()}


def _valid(data: Any) -> bool:
    return (isinstance(data, dict) and data.get("schema") == SCHEMA
            and isinstance(data.get("generation"), str) and len(data["generation"]) == 32
            and isinstance(data.get("epoch"), int) and data["epoch"] >= 0
            and isinstance(data.get("context_generation"), int) and data["context_generation"] >= 0
            and isinstance(data.get("fence"), str) and len(data["fence"]) in (0, 32)
            and isinstance(data.get("attempts"), int) and data["attempts"] >= 0
            and isinstance(data.get("input_tokens"), int) and data["input_tokens"] >= 0
            and isinstance(data.get("output_tokens"), int) and data["output_tokens"] >= 0
            and all(data.get(name) is None or (type(data[name]) is int and data[name] >= 0)
                    for name in ("cached_input_tokens", "reasoning_output_tokens"))
            and isinstance(data.get("unknown_usage"), bool)
            and isinstance(data.get("prior"), str) and len(data["prior"].encode()) <= MAX_FRAGMENT
            and isinstance(data.get("emitted"), list) and len(data["emitted"]) <= 32
            and isinstance(data.get("counts"), dict)
            and set(data["counts"]) == {"selected", "ready", "emitted", "dropped", "failed", "abstained", "capacity"}
            and all(isinstance(v, int) and not isinstance(v, bool) and 0 <= v <= 1_000_000
                    for v in data["counts"].values()))


def _count(data: dict[str, Any], name: str) -> None:
    counters = data.get("counts")
    if isinstance(counters, dict) and isinstance(counters.get(name), int):
        counters[name] = min(counters[name] + 1, 1_000_000)


def _write(path: Path, data: dict[str, Any]) -> None:
    def encode():
        return json.dumps(data, ensure_ascii=False, separators=(",", ":")).encode()
    raw = encode()
    if len(raw) > MAX_STATE and "last_selection" in data:
        # Diagnostics must never displace useful recall or settled accounting.
        from hook_recall import _unknown_discovery
        diagnostic = data["last_selection"]
        if isinstance(diagnostic, dict) and "discovery" in diagnostic:
            diagnostic["discovery"] = _unknown_discovery("state_byte_cap")
            raw = encode()
        if len(raw) > MAX_STATE:
            data.pop("last_selection")
        raw = encode()
    ready = data.get("ready")
    if len(raw) > MAX_STATE and isinstance(ready, dict):
        # Output is optional; the settled provider ledger is not. Strip private
        # learning metadata before useful recall, then drop an oversized packet
        # rather than making its already-spent usage impossible to persist.
        for case in ready.get("concerns", []):
            case["expected_row"] = None
        raw = encode()
        ready["cards"] = [{k: v for k, v in card.items() if k not in {"routing_binding", "conditional_binding"}}
                          if isinstance(card, dict) else card
                          for card in ready.get("cards", [])]
        raw = encode()
        if len(raw) > MAX_STATE:
            ready["cards"] = [{k: v for k, v in card.items() if k != "entry_kind"}
                              if isinstance(card, dict) else card for card in ready.get("cards", [])]
            raw = encode()
        if len(raw) > MAX_STATE:
            ready["concerns"] = []  # Cases lose the private budget contest before ordinary cards.
            raw = encode()
        if len(raw) > MAX_STATE:
            ready.pop("db_id", None)
            raw = encode()
        if len(raw) > MAX_STATE:
            data["ready"] = None
            _count(data, "dropped")
            raw = encode()
    if len(raw) > MAX_STATE:
        raise ValueError("reader state too large")
    fd, name = tempfile.mkstemp(prefix=".reader-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        try:
            os.unlink(name)
        except FileNotFoundError:
            pass


def _fence(config: dict[str, Any], session_id: str) -> str | None:
    try:
        path, _ = _paths(config, session_id)
        marker = path.with_suffix(".fence")
        if not marker.exists():
            return ""
        raw = marker.read_bytes()
        token = raw.decode("ascii")
        return token if len(token) == 32 and all(c in "0123456789abcdef" for c in token) else None
    except (OSError, UnicodeError):
        return None


def _context_token(config: dict[str, Any], session_id: str) -> str | None:
    try:
        path, _ = _paths(config, session_id)
        marker = path.with_suffix(".context")
        if not marker.exists():
            return ""
        token = marker.read_bytes().decode("ascii")
        return token if len(token) == 32 and all(c in "0123456789abcdef" for c in token) else None
    except (OSError, UnicodeError):
        return None


def _invalidate(config: dict[str, Any], session_id: str) -> bool:
    try:
        directory = _directory(config)
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        path, _ = _paths(config, session_id)
        marker = path.with_suffix(".fence")
        fd, name = tempfile.mkstemp(prefix=".fence-", dir=directory)
        try:
            with os.fdopen(fd, "wb") as stream:
                stream.write(secrets.token_hex(16).encode("ascii"))
            os.replace(name, marker)
        finally:
            try:
                os.unlink(name)
            except FileNotFoundError:
                pass
        return True
    except OSError:
        return False


def _rotate_context(config: dict[str, Any], session_id: str) -> bool:
    try:
        directory = _directory(config)
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        path, _ = _paths(config, session_id)
        marker = path.with_suffix(".context")
        fd, name = tempfile.mkstemp(prefix=".context-", dir=directory)
        try:
            with os.fdopen(fd, "wb") as stream:
                stream.write(secrets.token_hex(16).encode("ascii"))
            os.replace(name, marker)
        finally:
            try:
                os.unlink(name)
            except FileNotFoundError:
                pass
        return True
    except OSError:
        return False


def _looks_replayed(config: dict[str, Any], session_id: str, turn: str, digest: str) -> bool:
    """A bounded unlocked hint; the locked notice still decides identity."""
    try:
        path, _ = _paths(config, session_id)
        if not path.is_file() or path.stat().st_size > MAX_STATE:
            return False
        data = json.loads(path.read_bytes())
        active = (data.get("active") if _valid(data) and
                  data.get("config_pin") == _config_current(config) else None)
        return isinstance(active, dict) and active.get("turn") == turn and active.get("request") == digest
    except (OSError, ValueError, UnicodeError, TypeError):
        return False


def _config_current(config):
    """Production hooks capture this at load; direct offline callers capture once."""
    from recording_jobs import _config_digest
    try:
        resolve(config)
        live = _config_digest(config)
        captured = config.setdefault("_reader_config_pin", live)
        return captured if captured == live else None
    except (OSError, ValueError, TypeError, KeyError):
        return None


def _state(config: dict[str, Any], session_id: str, mutate, *, create: bool = True, allow_stale: bool = False):
    """Return (success, result); never wait for another hook or worker."""
    try:
        pin = None if allow_stale else _config_current(config)
        if not allow_stale and pin is None:
            return False, None
        directory = _directory(config)
        if create:
            directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        path, lock = _paths(config, session_id)
        fd = os.open(lock, os.O_CREAT | os.O_RDWR | getattr(os, "O_NOFOLLOW", 0), 0o600) if create else os.open(lock, os.O_RDWR | getattr(os, "O_NOFOLLOW", 0))
        with os.fdopen(fd, "rb+") as stream:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            if path.exists():
                if path.is_symlink() or path.stat().st_size > MAX_STATE:
                    return False, None
                data = json.loads(path.read_bytes())
                if not _valid(data):
                    return False, None
            elif create:
                data = _new()
            else:
                return False, None
            from recording_jobs import _bind_workspace
            bound = _bind_workspace(config, data)
            rebound = not allow_stale and data.get("config_pin") != pin
            if rebound:
                # Invalidate payload/history, NOT outstanding paid-call identities or totals.
                data.update(config_pin=pin, generation=secrets.token_hex(16),
                            context_generation=data["context_generation"] + 1,
                            epoch=data["epoch"] + 1, active=None, pending=None, ready=None,
                            prior="", emitted=[])
                if data.get("reservation") is None:
                    data["inflight"] = None  # Unpaid obsolete native work needs no settlement.
                data.pop("last_selection", None)
            result, changed = mutate(data)
            if changed or rebound or bound:
                data["touched"] = time.time()
                _write(path, data)
            return True, result
    except (OSError, ValueError, TypeError, KeyError, UnicodeError):
        return False, None


def notice(config: dict[str, Any], event: dict[str, Any], substantive: bool) -> dict[str, Any]:
    """Invalidate old ready work on *every* prompt, even a trivial one."""
    if _config_current(config) is None:
        return {"outcome": "busy"}
    budget = resolve(config)
    identity = _identity(event, prompt=True)
    if identity is None:
        return {"outcome": "skipped"}
    session, turn = identity
    prompt = event["prompt"]
    digest = hashlib.sha256(prompt.encode()).hexdigest()
    if not _looks_replayed(config, session, turn, digest) and not _invalidate(config, session):
        return {"outcome": "busy"}
    def mark(data):
        active = data["active"]
        if isinstance(active, dict) and active.get("turn") == turn and active.get("request") == digest:
            return "replay", False
        if isinstance(active, dict) and active.get("turn") == turn:
            return "invalid_identity", False
        if data["ready"] is not None:
            _count(data, "dropped")
        data["ready"] = None
        data["pending"] = None
        data["epoch"] += 1
        data["fence"] = _fence(config, session)
        if data["fence"] is None:
            return "busy", False
        cue = ("Recent task: " + data["prior"] + "\n" if data["prior"] else "") + "Current task: " + _short(prompt, MAX_PROMPT)
        data["active"] = {"turn": turn, "request": digest, "epoch": data["epoch"],
                          "cue": cue, "closed": False}
        if not substantive or not prompt.strip():
            return "skipped", True
        data["prior"] = _fragment(prompt)
        if data["unknown_usage"] or data["attempts"] >= resolve(config).attempts or data["input_tokens"] >= resolve(config).input_tokens or data["output_tokens"] >= resolve(config).output_tokens:
            return "budget", True
        data["pending"] = {"turn": turn, "request": digest, "epoch": data["epoch"]}
        return "queued", True
    ok, result = _state(config, session, mark)
    return {"outcome": result if ok else "busy"}


def _cache_key(card):
    return (card.get("db_id"), card["id"], delivery_cache_fingerprint(card))


def _was_emitted(card, emitted):
    key = _cache_key(card)
    # Old single-store packets had no per-card owner. They can suppress only
    # project cards, never independently owned global preference identities.
    return (key in emitted or card.get("scope") != "global_preference" and
            (card["id"], key[2]) in emitted)


def _emitted_keys(values):
    return {tuple(v) for v in values if isinstance(v, list) and len(v) in (2, 3)
            and all(isinstance(x, str) or x is None for x in v)}


def consume(config: dict[str, Any], event: dict[str, Any]) -> dict[str, Any]:
    identity = _identity(event)
    if identity is None:
        return {"outcome": "skipped", "cards": []}
    session, turn = identity
    def claim(data):
        active, ready = data["active"], data["ready"]
        marker = _fence(config, session)
        if marker is None or data["fence"] != marker:
            if data["ready"] is not None:
                _count(data, "dropped")
            data["ready"] = None
            return ("stale", [], None), True
        if not isinstance(active, dict) or active.get("turn") != turn or active.get("closed") or not isinstance(ready, dict):
            return ("empty", [], None), False
        key = ("turn", "request", "epoch")
        if any(active.get(k) != ready.get(k) for k in key):
            _count(data, "dropped")
            data["ready"] = None
            return ("stale", [], None), True
        if ready.get("fence") != marker:
            _count(data, "dropped")
            data["ready"] = None
            return ("stale", [], None), True
        from hooks import _pack_async_delivery
        cards = []
        seen = _emitted_keys(data["emitted"])
        case_endpoints = {i for c in ready.get("concerns", []) for i in c["displayed_endpoint_ids"]}
        for card in ready.get("cards", []):
            if isinstance(card, dict) and (not _was_emitted(card, seen) or
                    card.get("scope") != "global_preference" and card.get("id") in case_endpoints):
                cards.append(card)
                seen.add(_cache_key(card))
        packed = _pack_async_delivery(cards, session, turn, ready.get("db_id"),
                                      concerns=ready.get("concerns"))
        cards = packed["cards"]
        for card in cards:
            key = list(_cache_key(card)) if "db_id" in card else [card["id"], delivery_cache_fingerprint(card)]
            if key not in data["emitted"]:
                data["emitted"].append(key)
        data["emitted"] = data["emitted"][-32:]
        db_id = ready.get("db_id") if cards else None
        data["ready"] = None  # Emitted is not confirmed visible to the model.
        if cards:
            _count(data, "emitted")
        data["last_delivery"] = {"turn": turn, "offered_count": len(cards) + packed["budget_omitted_count"],
                                 "emitted_count": len(cards),
                                 "budget_omitted_count": packed["budget_omitted_count"],
                                 "concern_omitted_count": packed["concern_omitted_count"],
                                 "concern_count": len(packed["concerns"]),
                                 "registered_concern_count": sum(c["expected_row"] is not None for c in packed["concerns"])}
        outcome = "emitted" if cards else "omitted" if packed["budget_omitted_count"] else "duplicate"
        return (outcome, cards, db_id, packed), True
    ok, result = _state(config, session, claim, create=False)
    outcome, cards, db_id = result[:3] if ok else ("busy", [], None)
    response = {"outcome": outcome, "cards": cards}
    if ok and len(result) == 4:
        response.update({key: result[3][key] for key in ("context", "displayed", "budget_omitted_count", "concerns", "concern_omitted_count")})
    if isinstance(db_id, str) and re.fullmatch(r"[0-7][0-9A-HJKMNP-TV-Z]{25}", db_id):
        response["db_id"] = db_id
    return response


def reset(config: dict[str, Any], session_id: str) -> None:
    if _config_current(config) is None:
        return
    if not isinstance(session_id, str) or not ID.fullmatch(session_id):
        return
    if not _invalidate(config, session_id):
        return
    if not _rotate_context(config, session_id):
        return
    def clear(data):
        was_ended = data.get("ended") is True
        if data["ready"] is not None:
            _count(data, "dropped")
        data.update(epoch=data["epoch"] + 1, context_generation=data["context_generation"] + 1,
                    active=None, pending=None, ready=None,
                    prior="", emitted=[], ended=False)
        if was_ended:
            data["generation"] = secrets.token_hex(16)
            data["worker_pid"] = None
        data["fence"] = _fence(config, session_id)
        return None, True
    _state(config, session_id, clear)


def close_turn(config: dict[str, Any], session_id: str, turn_id: str) -> None:
    if not all(isinstance(x, str) and ID.fullmatch(x) for x in (session_id, turn_id)):
        return
    def close(data):
        active = data["active"]
        if not isinstance(active, dict) or active.get("turn") != turn_id:
            return None, False
        active["closed"] = True
        data["pending"] = None
        if data["ready"] is not None:
            _count(data, "dropped")
        data["ready"] = None
        return None, True
    ok, _ = _state(config, session_id, close, create=False)
    if not ok:
        # We cannot establish which turn owns a busy/malformed optional state.
        # Drop a possibly newer optional packet rather than emit after Stop.
        _invalidate(config, session_id)


def end_session(config: dict[str, Any], session_id: str) -> None:
    if not isinstance(session_id, str) or not ID.fullmatch(session_id):
        return
    if config.get("recording_mode") == "automatic":
        import recording_jobs
        recording_jobs.close_turn(config, session_id, ended=True)
    if not _invalidate(config, session_id):
        return
    def clear(data):
        if data["ready"] is not None:
            _count(data, "dropped")
        data.update(active=None, pending=None, ready=None, prior="", ended=True,
                    fence=_fence(config, session_id))
        return None, True
    _state(config, session_id, clear, create=False)
    # Keep the small ledger until a new session generation is established, so an
    # in-flight old worker cannot resurrect cards after SessionEnd.


def _alive(pid: Any) -> bool:
    if not isinstance(pid, int) or pid <= 0:
        return False
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def background(config: dict[str, Any], event: dict[str, Any]):
    """Async hook: wait briefly for sync notice, then launch at most one worker."""
    recording = config.get("recording_mode") == "automatic"
    closing = recording and event.get("hook_event_name") in ("Stop", "SessionEnd")
    identity = ((event["session_id"], event.get("turn_id"))
                if closing and isinstance(event.get("session_id"), str) and ID.fullmatch(event["session_id"])
                else _identity(event, prompt=True))
    if identity is None:
        return {"outcome": "skipped"}
    session, turn = identity
    digest = hashlib.sha256(event.get("prompt", "").encode()).hexdigest()
    deadline = time.monotonic() + 0.5
    while time.monotonic() < deadline:
        recording_work = False
        if recording:
            import recording_jobs
            recording_work = recording_jobs.has_work(config, session)
        def launch(data):
            active = data["active"]
            pending = data["pending"]
            reader_work = (isinstance(pending, dict) and isinstance(active, dict)
                           and (closing or (active.get("turn") == turn and active.get("request") == digest)))
            if not recording_work and not reader_work:
                return "not_ready", False
            if _alive(data.get("worker_pid")):
                return "running", False
            config_path = config.get("_config_path")
            if not isinstance(config_path, Path) or not config_path.is_absolute():
                return "no_config", False
            try:
                command = [sys.executable, str(Path(__file__).resolve()), "--serve", "--config", str(config_path), "--session-id", session]
                if config.get("memory_scope") == "misc":
                    binding = json.dumps(config["workspace_binding"], ensure_ascii=False, separators=(",", ":"))
                    if len(binding.encode()) > MAX_WORKSPACE_BINDING:
                        return "no_config", False
                    command.extend(["--workspace-binding", binding])
                child = subprocess.Popen(command,
                                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                         stderr=subprocess.DEVNULL, close_fds=True, start_new_session=True)
            except (OSError, ValueError, TypeError, KeyError):
                return "unavailable", False
            data["worker_pid"] = child.pid
            return "launched", True
        ok, result = _state(config, session, launch, create=False)
        if ok and result != "not_ready":
            return {"outcome": result}
        time.sleep(0.02)
    return {"outcome": "busy_or_not_ready"}


def _slot(config: dict[str, Any]):
    """Cross-session cap; flock releases on crash without a stale PID lease."""
    directory = _directory(config)
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    for number in range(MAX_WORKERS):
        path = directory / f"slot-{number}.lock"
        fd = os.open(path, os.O_CREAT | os.O_RDWR | getattr(os, "O_NOFOLLOW", 0), 0o600)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return fd
        except BlockingIOError:
            os.close(fd)
    return None


def _baseline_routing(session, pending):
    """Stable baseline opportunity, not guaranteed exposure or useful recovery."""
    raw = json.dumps(["mneme.routing-baseline.v1", session, pending["turn"], pending["request"]],
                     separators=(",", ":")).encode()
    return int.from_bytes(hashlib.sha256(raw).digest()[:4], "big") % 4 == 0


def _match_routing(config, cue, runtime, reserve, account):
    """One optional off-thread interpretation; caller owns its durable budget."""
    baseline = {"hints": [], "mode": "no_history", "stop": False}
    try:
        from hook_recall import collect_routing_witnesses
        import routing_contract
        budget = resolve(config)
        current = [{"source": "bounded_task_cue", "text": cue}]
        plan, reason = routing_contract.discovery_plan(current, budget=budget)
        if plan is None or plan["max_nodes"] == 0:
            return {**baseline, "mode": "input_refused"}
        if config.get("memory_scope") == "misc" and _config_current(config) is None:
            return {**baseline, "mode": "config_changed"}
        discovery = collect_routing_witnesses(config["service_config"], cue, config["project_root"],
            current=current, budget=budget, discovery_plan=plan, timeout=budget.native_seconds,
            **target_kwargs(config))
        if "routing_discovery" in discovery:
            from hook_recall import _bounded_routing_discovery
            baseline["routing_discovery"] = _bounded_routing_discovery(discovery["routing_discovery"])
        if discovery.get("outcome") not in ("ok", "empty"):
            return {**baseline, "mode": "discovery_unavailable"}
        witnesses = discovery.get("witnesses", [])
        prompt, context = routing_contract.prepare(current, witnesses, expected_db_id=discovery.get("db_id"), budget=budget)
        if prompt is None:
            return {**baseline, "mode": "no_history" if not witnesses else "input_refused"}
    except (ImportError, OSError, ValueError, TypeError, KeyError, UnicodeError):
        return {**baseline, "mode": "discovery_unavailable"}
    if not reserve():
        return {**baseline, "mode": "budget"}
    try:
        if config.get("memory_scope") == "misc" and _config_current(config) is None:
            result = {"provider_attempt": False, "reason": "config_changed"}
        else:
            result = runtime.route(current, witnesses, expected_db_id=discovery["db_id"])
        if not isinstance(result, dict):
            raise ValueError("invalid router receipt")
    except Exception:
        result = {"provider_attempt": True, "usage": None, "reason": "provider_error"}
    # Settlement is independent of whether the source turn is still active. A
    # cancelled result still cost tokens. Never settle this usage again at publish.
    try:
        settled = account(result)
    except Exception:
        settled = False
    if not settled or (result.get("provider_attempt") and _usage(result) is None):
        return {**baseline, "mode": "usage_unresolved", "stop": True}
    routed = result.get("routing")
    if result.get("reason") not in ("matched", "neutral") or not isinstance(routed, routing_contract.Result):
        return {**baseline, "mode": "matching_refused"}
    return {**baseline, "hints": list(routed.hints), "mode": "matched" if routed.hints else "neutral", "stop": False,
            "omitted_groups": json.loads(context.snapshot_json).get("omitted_history", {}).get("route_groups", 0)}


def _execute(config: dict[str, Any], cue: str, runtime, reserve,
             emitted: set[tuple[str, str]], *, routing=None) -> dict[str, Any]:
    from hook_recall import collect_reader, _bounded_discovery, _unknown_discovery
    # Private, last-completed diagnostic only. These are POSTPACK candidates,
    # not the native retrieval universe; no summaries, cue or graph paths.
    budget = resolve(config)
    read_plan, preflight_reason = reader_plan([{"role": "user", "text": cue}], budget=budget)
    selection = {"pool_stage": "postpack_unseen", "pool": None,
                 "stage": "routing", "selected_ids": [], "discovery": _unknown_discovery()}
    def done(value):
        value["selection"] = selection
        return value
    if read_plan is None or read_plan["max_nodes"] == 0:
        selection["stage"] = "native"
        return done({"outcome": "skipped" if read_plan is None else "empty", "cards": [], "provider_attempt": False})
    routing_result = routing() if routing is not None else None
    routing_meta = ({"routing_mode": routing_result["mode"]} if routing_result is not None else {})
    if routing_result is not None and "routing_discovery" in routing_result:
        selection["routing_discovery"] = routing_result["routing_discovery"]
    if (routing_result is not None and type(routing_result.get("omitted_groups")) is int
            and 0 <= routing_result["omitted_groups"] <= 256):
        routing_meta["routing_omitted_groups"] = routing_result["omitted_groups"]
    if routing_result is not None and routing_result.get("stop"):
        return done({"outcome": "unavailable", "cards": [], "provider_attempt": False, **routing_meta})
    selection["stage"] = "native"
    preference_policy = None
    try:
        preference_policy = global_preferences_policy(config)
    except (OSError, ValueError, TypeError, KeyError):
        # An unavailable optional owner must not erase project results.
        selection["global_preference_outcome"] = "unavailable"
    shared = ({"deadline": time.monotonic() + budget.native_seconds, "decoded_bytes": 0}
              if preference_policy is not None else None)
    if config.get("memory_scope") == "misc" and _config_current(config) is None:
        return done({"outcome": "unavailable", "cards": [], "provider_attempt": False, **routing_meta})
    native = collect_reader(config["service_config"], cue, config["project_root"],
                            timeout=budget.native_seconds, budget=budget, read_plan=read_plan,
                            **target_kwargs(config),
                            **({"shared_work": shared} if shared is not None else {}),
                            **({"routing_hints": routing_result["hints"]} if routing_result is not None else {}))
    if preference_policy is not None and isinstance(native, dict):
        from hook_recall import _bytes, MAX_READER_BYTES
        project_cards = native.get("cards", []) if native.get("outcome") == "ok" else []
        project_cards = [{**c, **({"db_id": native["db_id"]} if native.get("db_id") else {})}
                         for c in project_cards]
        remaining = shared["deadline"] - time.monotonic()
        global_result = {"outcome": "budget", "cards": []}
        if remaining > 0 and shared["decoded_bytes"] < budget.native_read_bytes:
            global_result = collect_reader(Path(preference_policy.service_config), GLOBAL_PREFERENCE_CUE,
                config["project_root"], timeout=min(remaining, budget.native_seconds),
                budget=budget, read_plan=read_plan, store_target=preference_policy, shared_work=shared)
        selection["global_preference_outcome"] = global_result.get("outcome", "unavailable")
        selection["global_preference_discovery"] = _bounded_discovery(global_result.get("discovery"))
        combined = project_cards
        for card in global_result.get("cards", []) if global_result.get("outcome") == "ok" else []:
            # One aggregate retained-content fuse, not one allocation per store.
            trial = {**native, "cards": combined + [card]}
            content = {k: v for k, v in trial.items() if k not in
                       {"discovery", "concern_endpoints", "concern_rows", "concern_lookup", "native_work"}}
            if _bytes(content) <= MAX_READER_BYTES - 16:
                combined = combined + [card]
            else:
                selection["global_preference_budget_omitted"] = selection.get("global_preference_budget_omitted", 0) + 1
        native = {**native, "cards": combined,
                  "outcome": "ok" if combined else native.get("outcome", "unavailable"),
                  "native_work": {"decoded_bytes": shared["decoded_bytes"],
                      "read_allowance_bytes": budget.native_read_bytes,
                      "read_allowance_exhausted": shared["decoded_bytes"] >= budget.native_read_bytes}}
    if not isinstance(native, dict):
        return done({"outcome": "unavailable", "cards": [], "provider_attempt": False, **routing_meta})
    selection["discovery"] = _bounded_discovery(native.get("discovery"))
    work = native.get("native_work")
    if (isinstance(work, dict) and set(work) == {"decoded_bytes", "read_allowance_bytes", "read_allowance_exhausted"}
            and all(type(work[k]) is int and 0 <= work[k] <= 1_000_000 for k in ("decoded_bytes", "read_allowance_bytes"))
            and type(work["read_allowance_exhausted"]) is bool):
        selection["native_work"] = dict(work)
    selection["requested_effort"] = budget.effort
    if native.get("concern_lookup") in ("unknown", "bounded_pages_complete"):
        selection["concern_lookup"] = native["concern_lookup"]
    cards = native.get("cards", [])
    if isinstance(native.get("outcome"), str):
        selection["native_outcome"] = _short(native["outcome"], 64)
    if isinstance(cards, list):
        selection["postpack_count"] = len(cards)
        selection["postpack_truncated"] = False
        if not cards and native.get("outcome") in ("ok", "empty"):
            selection["pool"] = []
    if native.get("outcome") != "ok" or not isinstance(cards, list) or not cards:
        return done({"outcome": native.get("outcome", "unavailable"), "cards": [], "provider_attempt": False, **routing_meta})
    from hooks import _valid_card
    selection["stage"] = "filter"
    valid = [card for card in cards if _valid_card(card)]
    selection["invalid_count"] = len(cards) - len(valid)
    case_endpoints = set()
    from turn_observer import validate_concern_row
    for raw in native.get("concern_rows", []):
        try:
            row = validate_concern_row(raw)
            case_endpoints.update(e["id"] for e in row["notice"]["binding"]["endpoints"])
        except (ValueError, TypeError, KeyError):
            pass
    cards = [card for card in valid if not _was_emitted(card, emitted)
             or card.get("scope") != "global_preference" and card["id"] in case_endpoints]
    selection["seen_count"] = len(valid) - len(cards)
    selection["pool"] = [{"id": card["id"], "fingerprint": card["fingerprint"],
                           **({"db_id": card["db_id"]} if "db_id" in card else {})} for card in cards]
    if not cards:
        return done({"outcome": "empty", "cards": [], "provider_attempt": False, **routing_meta})
    observation = native.get("observation")
    rows = observation.get("cards") if (isinstance(observation, dict)
            and observation.get("schema") == 1 and observation.get("learning") == "disabled") else None
    paths = {}
    routing_bindings = {}
    conditional_bindings = {}
    conditional_ids = set()
    if isinstance(rows, list):
        for row in rows:
            if isinstance(row, dict) and isinstance(row.get("node_id"), str):
                if row.get("entry_kind") == "conditional":
                    conditional_ids.add(row["node_id"])
                    if row.get("graph_path") in (None, []) and row.get("routing_binding") is None:
                        try:
                            from routing_memory import validate_conditional_binding
                            conditional_bindings[row["node_id"]] = validate_conditional_binding(
                                row.get("conditional_binding"), row["node_id"], expected_db_id=native.get("db_id"))
                        except (ValueError, TypeError, KeyError, UnicodeError, ImportError):
                            pass
                    continue  # A conditional winner cannot borrow a losing graph path.
                if "entry_kind" in row or "conditional_binding" in row:
                    continue  # Unknown optional origin cannot borrow ordinary/graph attribution.
                path = row.get("graph_path")
                if isinstance(path, list) and len(path) <= 4 and len(json.dumps(path, ensure_ascii=False).encode()) <= 1200:
                    paths[row["node_id"]] = path
                if path and row.get("routing_binding") is not None:
                    try:
                        from routing_memory import validate_observed_binding
                        binding = validate_observed_binding(row["routing_binding"], path,
                                                            expected_db_id=native.get("db_id"))
                        if binding["route"]["target"] == row["node_id"] and native.get("db_id") == binding["db_id"]:
                            routing_bindings[row["node_id"]] = binding
                    except (ValueError, TypeError, KeyError, UnicodeError, ImportError):
                        pass
    duplicate_ids = {c["id"] for c in cards if sum(other["id"] == c["id"] for other in cards) > 1}
    selector_id = lambda c: "global:" + c["id"] if c.get("scope") == "global_preference" and c["id"] in duplicate_ids else c["id"]
    lookup_native = {selector_id(c): c for c in cards}
    reader_cards = [{**card, "id": selector_id(card), **({} if card.get("scope") == "global_preference" else
                               {"native": {"entry_kind": "conditional"}}
                               if card["id"] in conditional_ids else
                               {"native": {"graph_path": paths[card["id"]]}}
                               if card["id"] in paths else {})} for card in cards]
    project_ids = {c["id"] for c in cards if c.get("scope") != "global_preference"}
    concern_rows = []
    for raw in native.get("concern_rows", []):
        try:
            row = validate_concern_row(raw)
            if {e["id"] for e in row["notice"]["binding"]["endpoints"]} <= project_ids:
                concern_rows.append(row)
        except (ValueError, TypeError, KeyError):
            pass
    prompt, offered = reader_prepare(read_plan["dialogue"], reader_cards,
                                     concern_rows=concern_rows, budget=budget)
    if prompt is None:
        selection["selector_reason"] = offered
        return done({"outcome": "empty", "cards": [], "provider_attempt": False, **routing_meta})
    selection["prompt_omitted_count"] = offered["prompt_omitted_count"]
    selection["path_omitted_count"] = len(offered["path_omitted_ids"])
    if conditional_ids:
        selection["entry_omitted_count"] = len(offered["entry_omitted_ids"])
    offered_ids = set(offered["ids"])
    cards = [c for c in cards if selector_id(c) in offered_ids]
    reader_cards = [c for c in reader_cards if c["id"] in offered_ids]
    for card in reader_cards:
        if card["id"] in offered["path_omitted_ids"]:
            card.pop("native", None)
            routing_bindings.pop(card["id"], None)
        if card["id"] in offered["entry_omitted_ids"]:
            card.pop("native", None)
            conditional_bindings.pop(card["id"], None)
    selection["pool"] = [{"id": c["id"], "fingerprint": c["fingerprint"],
                           **({"db_id": c["db_id"]} if "db_id" in c else {})} for c in cards]
    if not reserve():
        return done({"outcome": "budget", "cards": [], "provider_attempt": False, **routing_meta})
    selection["stage"] = "selector"
    try:
        if config.get("memory_scope") == "misc" and _config_current(config) is None:
            return done({"outcome": "unavailable", "cards": [], "provider_attempt": False, **routing_meta})
        result = runtime.select(read_plan["dialogue"], reader_cards,
                                **({"concern_rows": concern_rows} if concern_rows else {}))
    except Exception:
        return done({"outcome": "unavailable", "cards": [], "provider_attempt": True, "usage": None, **routing_meta})
    if not isinstance(result, dict):
        return done({"outcome": "unavailable", "cards": [], "provider_attempt": True, "usage": None, **routing_meta})
    if isinstance(result.get("reason"), str):
        selection["selector_reason"] = _short(result["reason"], 64)
    ids = result.get("selected_ids", [])
    selected = []
    if result.get("reason") == "selected" and isinstance(ids, list) and len(ids) <= len(cards) and all(isinstance(x, str) for x in ids) and len(set(ids)) == len(ids):
        lookup = {selector_id(c): c for c in cards}
        if all(x in lookup for x in ids):
            selected = [{**lookup[x], **({"routing_binding": routing_bindings[x]}
                                         if x in routing_bindings and lookup[x].get("scope") != "global_preference" else {}),
                         **({"entry_kind": "conditional"} if x in conditional_ids and lookup[x].get("scope") != "global_preference" else {}),
                         **({"conditional_binding": conditional_bindings[x]}
                            if x in conditional_bindings and lookup[x].get("scope") != "global_preference" else {})} for x in lookup if x in ids]
    nominations = []
    if selected:
        from reader_contract import validate_answer
        try:
            checked = validate_answer({"selected_ids": ids, "concerns": result.get("concerns", [])},
                                      {"ids": [selector_id(c) for c in cards], "answer_bytes": budget.selector_answer_bytes})
            nominations = [c for c in checked["concerns"] if all(
                lookup_native[i].get("scope") != "global_preference" for i in (c["left_id"], c["right_id"]))]
        except ValueError:
            selected = []
    selection["selected_ids"] = [card["id"] for card in selected]
    if any("db_id" in c for c in selected):
        selection["selected_keys"] = [[c.get("db_id"), c["id"]] for c in selected]
    return done({"outcome": "selected" if selected else "abstained" if result.get("reason") == "abstained" else "unavailable",
            "cards": selected, "usage": result.get("usage"),
            "concern_nominations": nominations, "concern_endpoints": native.get("concern_endpoints", {}),
            "db_id": native.get("db_id"),
            "provider_attempt": result.get("provider_attempt") is True,
            "elapsed_ms": result.get("elapsed_ms"),
            **routing_meta})


def _selection_diagnostic(result, origin, publish_current):
    """A bounded snapshot, never an archive or proof of host delivery."""
    record = {"schema": 1, "origin": origin, "publish_current": publish_current,
              "outcome": "unavailable"}
    details = result.get("selection")
    if details is None:
        details = dict(pool_stage="postpack_unseen", pool=None, selected_ids=[], stage="unknown")
    try:
        if isinstance(result.get("outcome"), str):
            record["outcome"] = _short(result["outcome"], 64)
        required = {"pool_stage", "pool", "stage", "selected_ids"}
        counts = {"postpack_count", "invalid_count", "seen_count", "prompt_omitted_count", "path_omitted_count", "entry_omitted_count", "global_preference_budget_omitted"}
        labels = {"native_outcome", "selector_reason", "concern_lookup", "requested_effort", "global_preference_outcome"}
        if (not isinstance(details, dict) or not required <= details.keys()
                or details.keys() - required - counts - labels - {"postpack_truncated", "discovery", "native_work", "routing_discovery", "global_preference_discovery", "selected_keys"}
                or details["pool_stage"] != "postpack_unseen"
                or details["stage"] not in ("unknown", "routing", "native", "filter", "selector")):
            raise ValueError("invalid diagnostic shape")
        pool, selected = details["pool"], details["selected_ids"]
        if pool is not None and (not isinstance(pool, list) or any(
                not isinstance(card, dict) or not {"id", "fingerprint"} <= set(card)
                or set(card) - {"id", "fingerprint", "db_id"}
                or "db_id" in card and (not isinstance(card["db_id"], str) or not re.fullmatch(r"[0-7][0-9A-HJKMNP-TV-Z]{25}", card["db_id"]))
                or not isinstance(card["id"], str) or not re.fullmatch(r"[0-7][0-9A-HJKMNP-TV-Z]{25}", card["id"])
                or not isinstance(card["fingerprint"], str) or not 0 < len(card["fingerprint"]) <= 160 for card in pool)):
            raise ValueError("invalid diagnostic pool")
        if (not isinstance(selected, list) or len(selected) > len(pool or [])
                or any(not isinstance(node, str) for node in selected)
                or (len(set(selected)) != len(selected) and "selected_keys" not in details)
                or not set(selected) <= {card["id"] for card in pool or []}
                or any(type(details[key]) is not int or not 0 <= details[key] <= 32768 for key in counts & details.keys())
                or any(not isinstance(details[key], str) or len(details[key].encode()) > 64 for key in labels & details.keys())
                or ("postpack_truncated" in details and type(details["postpack_truncated"]) is not bool)):
            raise ValueError("invalid diagnostic values")
        from hook_recall import _bounded_discovery, _unknown_discovery
        complete = {**record, **details, "discovery": _bounded_discovery(details.get("discovery"))}
        if "global_preference_discovery" in complete:
            complete["global_preference_discovery"] = _bounded_discovery(complete["global_preference_discovery"])
        if "selected_keys" in complete:
            keys = complete["selected_keys"]
            if (not isinstance(keys, list) or len(keys) != len(selected)
                    or any(not isinstance(k, list) or len(k) != 2 or
                           not isinstance(k[1], str) or k[0] is not None and
                           (not isinstance(k[0], str) or not re.fullmatch(r"[0-7][0-9A-HJKMNP-TV-Z]{25}", k[0]))
                           for k in keys)
                    or [k[1] for k in keys] != selected
                    or len({tuple(k) for k in keys}) != len(keys)
                    or not {tuple(k) for k in keys} <= {(c.get("db_id"), c["id"]) for c in pool or []}):
                raise ValueError("invalid scoped selection")
        if "routing_discovery" in complete:
            from hook_recall import _bounded_routing_discovery
            complete["routing_discovery"] = _bounded_routing_discovery(complete["routing_discovery"])
        if len(json.dumps(complete, ensure_ascii=False, separators=(",", ":")).encode()) > MAX_SELECTION_BYTES:
            complete["discovery"] = _unknown_discovery("diagnostic_byte_cap")
        if len(json.dumps(complete, ensure_ascii=False, separators=(",", ":")).encode()) > MAX_SELECTION_BYTES:
            complete.pop("routing_discovery", None)
        if len(json.dumps(complete, ensure_ascii=False, separators=(",", ":")).encode()) > MAX_SELECTION_BYTES:
            # Keep bounded aggregate facts even when per-ID evidence is too large.
            return {**record, **{k: details[k] for k in counts & details.keys()},
                    "omitted": "byte_cap"}
        return complete
    except (ValueError, TypeError, KeyError, UnicodeError):
        return {**record, "omitted": "invalid_diagnostic"}


def _usage(result: dict[str, Any]) -> tuple[int, int] | None:
    value = result.get("usage")
    if not isinstance(value, dict):
        return None
    in_tokens, out_tokens = value.get("input_tokens"), value.get("output_tokens")
    if not all(isinstance(v, int) and not isinstance(v, bool) and 0 <= v <= 1_000_000 for v in (in_tokens, out_tokens)):
        return None
    return in_tokens, out_tokens


def _recording_reserve(config, session_id, key):
    def reserve(data):
        if (data["unknown_usage"] or data.get("reservation") is not None
                or data["attempts"] >= resolve(config).attempts or data["input_tokens"] >= resolve(config).input_tokens
                or data["output_tokens"] >= resolve(config).output_tokens):
            return False, False
        data["attempts"] += 1
        data["reservation"] = {"recording": key}
        return True, True
    ok, value = _state(config, session_id, reserve, create=False)
    return bool(ok and value)


def _settle_usage(data, result, usage):
    """Only call for a matching durable reservation; missing breakdown stays unknown."""
    data["input_tokens"] += usage[0]
    data["output_tokens"] += usage[1]
    value = result["usage"]
    for name, maximum in (("cached_input_tokens", usage[0]), ("reasoning_output_tokens", usage[1])):
        previous, amount = data.get(name), value.get(name)
        data[name] = (previous + amount
                      if type(previous) is int and type(amount) is int and 0 <= amount <= maximum
                      else None)


def _unknown_usage(data):
    data.update(unknown_usage=True, cached_input_tokens=None, reasoning_output_tokens=None)


def _recording_account(config, session_id, key, result):
    def account(data):
        usage = _usage(result) if result.get("provider_attempt") else None
        matched = data.get("reservation") == {"recording": key}
        if matched:
            data["reservation"] = None
        if result.get("provider_attempt"):
            if usage is None:
                _unknown_usage(data)
            elif matched:
                _settle_usage(data, result, usage)
        return matched or not result.get("provider_attempt"), True
    ok, settled = _state(config, session_id, account, create=False, allow_stale=True)
    return bool(ok and settled)


def serve(config: dict[str, Any], session_id: str, *, runtime_factory=None, idle_seconds: float = IDLE_SECONDS) -> None:
    """Worker loop. Tests inject a fake runtime; production imports it lazily."""
    if not isinstance(session_id, str) or not ID.fullmatch(session_id):
        return
    path, _ = _paths(config, session_id)
    lease = path.with_suffix(".worker.lock")
    try:
        fd = os.open(lease, os.O_CREAT | os.O_RDWR | getattr(os, "O_NOFOLLOW", 0), 0o600)
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except (OSError, BlockingIOError):
        return
    slot = None
    runtime = None
    handoff = False
    try:
        slot = _slot(config)
        if slot is None:
            def capacity(data):
                data["pending"] = None
                _count(data, "capacity")
                return None, True
            _state(config, session_id, capacity, create=False)
            return
        if runtime_factory is None:
            from reader_runtime import ReaderRuntime
            runtime_factory = ReaderRuntime
        last_activity = time.monotonic()
        generation = None
        runtime_context = None
        while time.monotonic() - last_activity < idle_seconds:
            def take(data):
                nonlocal generation
                if generation is None:
                    generation = data["generation"]
                if data["generation"] != generation:
                    return "rotated", False
                if data.get("ended"):
                    return "end", False
                marker = _fence(config, session_id)
                if marker is None or marker != data["fence"]:
                    if data["ready"] is not None:
                        _count(data, "dropped")
                    data.update(active=None, pending=None, ready=None, prior="", fence=marker or "")
                    return None, True
                if data.get("reservation") is not None or data.get("inflight") is not None:
                    # A prior process died after reserving a provider call. We
                    # cannot reconstruct usage from a PID or retry safely.
                    _unknown_usage(data)
                    if (isinstance(data.get("reservation"), dict)
                            and "routing" in data["reservation"]
                            and isinstance(data.get("routing_usage"), dict)):
                        _unknown_usage(data["routing_usage"])
                    data["reservation"] = None
                    data["inflight"] = None
                    data["pending"] = None
                    data["ready"] = None
                    _count(data, "failed")
                    return None, True
                pending = data["pending"]
                if pending is None:
                    return None, False
                active = data["active"]
                data["pending"] = None
                if not isinstance(active, dict) or active.get("closed") or any(active.get(k) != pending.get(k) for k in ("turn", "request", "epoch")):
                    return None, True
                if data["unknown_usage"] or data["attempts"] >= resolve(config).attempts or data["input_tokens"] >= resolve(config).input_tokens or data["output_tokens"] >= resolve(config).output_tokens:
                    return None, True
                context_token = _context_token(config, session_id)
                if context_token is None:
                    return None, True
                data["inflight"] = pending
                emitted = _emitted_keys(data["emitted"])
                return (pending, active["cue"],
                        (data["context_generation"], context_token),
                        emitted, marker), True
            ok, job = _state(config, session_id, take, create=False)
            if not ok:
                break
            if job == "rotated":
                handoff = config.get("recording_mode") == "automatic"
                break
            if job is None or job == "end":
                if config.get("recording_mode") == "automatic":
                    import recording_jobs
                    if recording_jobs.has_work(config, session_id):
                        if runtime is None:
                            scratch = _directory(config) / ("scratch-" + hashlib.sha256(session_id.encode()).hexdigest()[:16])
                            runtime = runtime_factory(config, scratch)
                        worked = recording_jobs.step(config, session_id, runtime,
                            lambda key: _recording_reserve(config, session_id, key),
                            lambda key, result: _recording_account(config, session_id, key, result))
                        if worked:
                            last_activity = time.monotonic()
                        if job == "end" and recording_jobs.has_work(config, session_id):
                            time.sleep(.05)
                            continue
                if job == "end":
                    break
            if job is None:
                time.sleep(0.05)
                continue
            last_activity = time.monotonic()
            pending, cue, context_generation, emitted, marker = job
            if runtime is not None and runtime_context != context_generation:
                try:
                    runtime.close()
                except Exception:
                    pass
                runtime = None
            if runtime is None:
                scratch = _directory(config) / ("scratch-" + hashlib.sha256(session_id.encode()).hexdigest()[:16])
                runtime = runtime_factory(config, scratch)
                runtime_context = context_generation
            def reserve(*, stage="selector"):
                def admit(data):
                    active = data.get("active")
                    # Matching is optional. Prefer the ordinary selector when
                    # near a quota, without claiming a provider-wrapper bound.
                    matching = stage == "routing"
                    required_calls = 2 if matching else 1
                    if (data.get("generation") != generation or data.get("inflight") != pending
                            or not isinstance(active, dict) or active.get("closed")
                            or any(active.get(k) != pending.get(k) for k in ("turn", "request", "epoch"))
                            or data.get("pending") is not None or data["unknown_usage"]
                            or data.get("reservation") is not None
                            or data.get("fence") != marker or _fence(config, session_id) != marker
                            or data["attempts"] + required_calls > resolve(config).attempts
                            or data["input_tokens"] >= resolve(config).input_tokens - (resolve(config).selector_room_input_tokens if matching else 0)
                            or data["output_tokens"] >= resolve(config).output_tokens - (resolve(config).selector_room_output_tokens if matching else 0)):
                        return False, False
                    # Conservative pre-call reservation: process death cannot buy an
                    # uncounted retry, and rotation does not reset the parent ledger.
                    data["attempts"] += 1
                    data["reservation"] = {"routing": pending, "generation": generation} if matching else pending
                    if matching:
                        usage = data.setdefault("routing_usage", {"attempts": 0, "input_tokens": 0,
                            "output_tokens": 0, "cached_input_tokens": 0, "reasoning_output_tokens": 0,
                            "unknown_usage": False})
                        usage["attempts"] += 1
                    return True, True
                ok, admitted = _state(config, session_id, admit, create=False)
                return bool(ok and admitted)
            def account_routing(result):
                def settle(data):
                    matched = data.get("reservation") == {"routing": pending, "generation": generation}
                    if not matched:
                        return False, False
                    data["reservation"] = None
                    if data.get("generation") != generation and data.get("inflight") == pending:
                        data["inflight"] = None  # Retire only this stale call, never B's pending turn.
                    if result.get("provider_attempt"):
                        usage = _usage(result)
                        if usage is None:
                            _unknown_usage(data)
                            _unknown_usage(data["routing_usage"])
                        else:
                            _settle_usage(data, result, usage)
                            _settle_usage(data["routing_usage"], result, usage)
                    return True, True
                ok, settled = _state(config, session_id, settle, create=False, allow_stale=True)
                return bool(ok and settled)
            routing = None
            if config.get("recording_mode") == "automatic":
                if _baseline_routing(session_id, pending):
                    routing = lambda: {"hints": [], "mode": "baseline_exploration", "stop": False}
                else:
                    routing = lambda: _match_routing(config, cue, runtime,
                                                     lambda: reserve(stage="routing"), account_routing)
            try:
                result = _execute(config, cue, runtime, reserve, emitted, **({"routing": routing} if routing else {}))
            except Exception:
                result = {"outcome": "unavailable", "cards": [], "provider_attempt": False}
            usage = _usage(result) if result.get("provider_attempt") else None
            def settle_selector(data):
                matched = data.get("reservation") == pending
                if matched:
                    data["reservation"] = None
                    if data.get("generation") != generation and data.get("inflight") == pending:
                        data["inflight"] = None
                if result.get("provider_attempt"):
                    if usage is None:
                        _unknown_usage(data)
                    elif matched:
                        _settle_usage(data, result, usage)
                return matched and usage is not None, matched or result.get("provider_attempt") is True
            settled_ok, settled = _state(config, session_id, settle_selector, create=False, allow_stale=True)
            def concern_current():
                def check(data):
                    active = data.get("active")
                    return (data["generation"] == generation and data.get("inflight") == pending
                            and isinstance(active, dict) and not active.get("closed")
                            and all(active.get(k) == pending.get(k) for k in ("turn", "request", "epoch"))
                            and data.get("pending") is None and data.get("fence") == marker
                            and _fence(config, session_id) == marker), False
                ok, current = _state(config, session_id, check, create=False)
                return bool(ok and current)
            nominations = result.get("concern_nominations", [])
            if nominations:
                from hook_recall import register_reader_concerns
                # Usage is durable before optional native work. A fence loss keeps no ready packet.
                result["concerns"] = register_reader_concerns(
                    config["service_config"], config["project_root"], result.get("db_id"),
                    nominations, result.get("concern_endpoints", {}),
                    **target_kwargs(config),
                    allowed=lambda: (config.get("recording_mode") == "automatic"
                                     and settled_ok and settled and concern_current()))
            def publish(data):
                changed = False
                if result.get("outcome") == "selected":
                    _count(data, "selected")
                    changed = True
                elif result.get("outcome") == "abstained":
                    _count(data, "abstained")
                    changed = True
                elif result.get("outcome") not in ("empty", "skipped", "budget"):
                    _count(data, "failed")
                    changed = True
                if isinstance(result.get("routing_mode"), str):
                    data["routing_mode"] = result["routing_mode"]
                    data.pop("routing_omitted_groups", None)
                    if "routing_omitted_groups" in result:
                        data["routing_omitted_groups"] = result["routing_omitted_groups"]
                    changed = True
                if data.get("inflight") == pending:
                    data["inflight"] = None
                    changed = True
                active = data.get("active")
                publish_current = (data["generation"] == generation
                        and isinstance(active, dict) and not active.get("closed")
                        and all(active.get(k) == pending.get(k) for k in ("turn", "request", "epoch"))
                        and data.get("pending") is None and data.get("fence") == marker
                        and _fence(config, session_id) == marker)
                previous = data.get("last_selection")
                previous = previous.get("origin") if isinstance(previous, dict) else None
                previous = previous if isinstance(previous, dict) else {}
                previous_epoch = previous.get("epoch", -1)
                if (data["generation"] == generation and
                        (previous.get("generation") != generation or type(previous_epoch) is not int
                         or previous_epoch <= pending["epoch"])):
                    origin = {"session": session_id, "generation": generation,
                              "context_generation": context_generation[0], "context_token": context_generation[1],
                              **pending, "fence": marker}
                    data["last_selection"] = _selection_diagnostic(result, origin, publish_current)
                    changed = True
                if publish_current and result.get("outcome") == "selected" and usage is not None:
                    cards = result.get("cards", [])
                    if isinstance(cards, list) and cards:
                        from hooks import _valid_card
                        if (all(_valid_card(card) for card in cards)
                                and len({(card.get("db_id"), card["id"]) for card in cards}) == len(cards)):
                            db_id = result.get("db_id")
                            data["ready"] = {**pending, "cards": cards, "fence": marker,
                                             "concerns": result.get("concerns", []),
                                             **({"db_id": db_id} if isinstance(db_id, str)
                                                and re.fullmatch(r"[0-7][0-9A-HJKMNP-TV-Z]{25}", db_id)
                                                else {})}
                            _count(data, "ready")
                            changed = True
                if result.get("outcome") == "selected" and data.get("ready") is None:
                    _count(data, "dropped")
                    changed = True
                return None, changed
            _state(config, session_id, publish, create=False)
        if time.monotonic() - last_activity >= idle_seconds:
            def expire(data):
                if data.get("generation") != generation or data.get("pending") is not None:
                    return None, False
                if data["ready"] is not None:
                    _count(data, "dropped")
                data.update(active=None, ready=None, prior="", inflight=None)
                return None, True
            _state(config, session_id, expire, create=False)
    finally:
        if runtime is not None:
            try:
                runtime.close()
            except Exception:
                pass
        def exited(data):
            if data.get("worker_pid") == os.getpid() or handoff:
                data["worker_pid"] = None
                return None, True
            return None, False
        _state(config, session_id, exited, create=False)
        if slot is not None:
            os.close(slot)
        os.close(fd)
        if handoff:
            # One bounded wake only after releasing the old lease. It admits no
            # new source job and cannot reset counters or reuse old ready cards.
            result = background(config, {"hook_event_name": "SessionEnd", "session_id": session_id})
            _state(config, session_id, lambda data: (data.update(handoff=result["outcome"]), True), create=False)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--serve", action="store_true")
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--session-id", required=True)
    parser.add_argument("--workspace-binding")
    args = parser.parse_args(argv)
    if not args.serve:
        return 2
    from hooks import _config
    try:
        binding = None
        if args.workspace_binding is not None:
            if len(args.workspace_binding.encode()) > MAX_WORKSPACE_BINDING:
                return 2
            from rollout_primitives import decode_json
            binding = decode_json(args.workspace_binding.encode())
            if not isinstance(binding, dict):
                return 2
        config = _config(args.config, workspace_binding=binding) if binding is not None else _config(args.config)
        if args.workspace_binding is not None and config.get("memory_scope") != "misc":
            return 2
        serve(config, args.session_id)
    except Exception:
        pass  # Optional reader never becomes a hook failure.
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Opt-in Codex lifecycle adapter for Mneme's selective, agent-authored memory.

Command hooks receive Codex event JSON on stdin. Legacy modes never read a
transcript. Explicit recording opt-in locally admits one bounded source turn;
it persists identity/anchors, never the prompt or full observation. A bounded
passive-read helper may send the current prompt to the configured local Mneme
service; the journal retains only hashes and metadata. Project/workshop roots are
explicit; the device misc default requires a checked per-session workspace
binding. No hook registration or trust change is performed here.
"""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import sys
import tempfile
import time
from typing import Any
from profile import select_for_cwd
from reader_contract import (EPISODE_FIELDS as EPISODE_CARD_FIELDS,
                             EPISODE_OPTIONAL_FIELDS as EPISODE_OPTIONAL_CARD_FIELDS,
                             validate_episode_projection_fields)

CONFIG_SCHEMA = "mneme.codex-hooks.config.v1"
CONFIG_SCHEMA_V2 = "mneme.codex-hooks.config.v2"
CONFIG_SCHEMA_V3 = "mneme.codex-hooks.config.v3"
CONFIG_SCHEMA_V4 = "mneme.codex-hooks.config.v4"
CONFIG_SCHEMA_V5 = "mneme.codex-hooks.config.v5"
CONFIG_SCHEMA_V6 = "mneme.codex-hooks.config.v6"
CONFIG_SCHEMA_V7 = "mneme.codex-hooks.config.v7"
CONFIG_SCHEMA_V8 = "mneme.codex-hooks.config.v8"
CONFIG_SCHEMA_V9 = "mneme.codex-hooks.config.v9"
CONFIG_SCHEMA_V10 = "mneme.codex-hooks.config.v10"
CONFIG_SCHEMA_V11 = "mneme.codex-hooks.config.v11"
STATE_SCHEMA = "mneme.codex-hooks.state.v2"
MAX_STDIN = 64_000
# Wire transport is not a prompt/model allowance. Tool results can be several
# MiB; only the small lifecycle projection below survives admission. The stdlib
# decoder temporarily materializes this bounded frame (including tool bodies),
# then releases it before config, owner, transcript or model work. This is a wire
# fuse, not an 8 MiB heap promise: Python objects can amplify the transient cost.
MAX_WIRE_BYTES = 8 * 1024 * 1024
MAX_WIRE_DEPTH = 64
HOOK_EVENTS = frozenset(("SessionStart", "UserPromptSubmit", "PostToolUse", "Stop", "Interrupt", "SessionEnd"))
EVENT_FIELDS = {
    "SessionStart": ("source",),
    "UserPromptSubmit": ("prompt",),
    "PostToolUse": (),
    "Stop": ("stop_hook_active", "last_assistant_message"),
    "Interrupt": (),
    "SessionEnd": ("reason",),
}
# Transcript identity belongs to lifecycle evidence, not to a tool body. Keep it
# even on closure events; recording admission still validates its actual source.
LIFECYCLE_FIELDS = ("hook_event_name", "cwd", "session_id", "turn_id", "transcript_path")
MAX_CONFIG = 8_000
MAX_STATE = 256_000
MAX_TURNS = 128
MAX_CONTEXT_BYTES = 4096
MAX_SEEN = 32
MEMORY_CHOICE = (
    "Consider 0–few source-bound memories: use save with kind episode for a short event worth "
    "remembering, even without a lesson; kind note for a reusable lesson. Neither is required. "
    "If the installed tool catalog lacks save, deliberately use its capture or episode append compatibility path. "
    "No duplicate retelling, transcript archive or secrets."
)
OVERFLOW_NOTE = " One or more cards exceeded the context budget; omitted cards remain eligible for later turns."
TTL_SECONDS = 30 * 86400
ID_RE = re.compile(r"^[A-Za-z0-9_:.\-]{1,160}$")
ULID_RE = re.compile(r"^[0-7][0-9A-HJKMNP-TV-Z]{25}$")
CONTINUATION_RE = re.compile(r"^MNEME_CHECKPOINT_CONTINUATION source_session_id=([A-Za-z0-9_:.\-]{1,160}) source_turn_id=([A-Za-z0-9_:.\-]{1,160}) token=([0-9a-f]{32})(?:\n|$)")
ACK = re.compile(r"(?is)^\s*(?:ok(?:ay)?|yes|no|yep|nope|thanks?|thank you|got it|sounds good|cool|👍|\+1|continue|go ahead|please proceed)[.!\s]*$")
TRIVIAL = re.compile(r"(?is)^\s*(?:what(?:'s| is) the (?:time|date)(?: now| today)?\??|translate [^\n]{1,90}|(?:rewrite|rephrase|format) (?:this|the following)[: ]?[^\n]{0,90})\s*$")
WORK = re.compile(r"(?i)\b(?:implement|fix|debug|review|audit|design|research|investigate|test|refactor|compare|build|write|plan|analyze|analyse|remember|recall|decide|trace|verify|migrate)\b|[/\\][\w.-]+|\b(?:repo|module|PR|issue|bug|codebase|benchmark)\b")


class HookError(Exception):
    pass


class ProjectFocusError(HookError):
    """Policy admission failed, with a fixed safe hook recovery diagnostic."""


class HookInputError(HookError):
    """One omitted lifecycle event, not a service/store availability verdict."""


def _check_wire_depth(raw: bytes) -> None:
    """Constant-space decoder guard, not JSON syntax validation.

    Python decoder recursion limits differ by version (3.14 can decode very deep
    arrays). Ignore quoted/escaped brackets; leave all other syntax to json.loads.
    """
    depth, quoted, escaped = 0, False, False
    for byte in raw:
        if quoted:
            if escaped:
                escaped = False
            elif byte == 92:  # Backslash.
                escaped = True
            elif byte == 34:
                quoted = False
        elif byte == 34:  # Quote.
            quoted = True
        elif byte in (91, 123):  # Opening array/object.
            depth += 1
            if depth > MAX_WIRE_DEPTH:
                raise HookInputError("wire_depth")
        elif byte in (93, 125):
            depth -= 1


def _read_event(stream) -> dict[str, Any] | None:
    """Admit one bounded wire frame into a tight, scalar lifecycle contract.

    Child/unknown events are intentionally ignored, including their large tool
    bodies. Relevant root events retain no tool input/output or arbitrary fields.
    Prompt/answer and total retained JSON remain bounded by the old 64k envelope;
    never truncate text or silently strip a transcript/Stop witness to fit it.
    """
    raw = stream.read(MAX_WIRE_BYTES + 1)
    if len(raw) > MAX_WIRE_BYTES:
        raise HookInputError("wire_limit")
    _check_wire_depth(raw)
    try:
        event = json.loads(raw.decode("utf-8"))
    except (ValueError, UnicodeError, RecursionError) as exc:
        raise HookInputError("invalid_json") from exc
    if not isinstance(event, dict):
        raise HookInputError("invalid_event")
    name = event.get("hook_event_name")
    if _subagent(event):
        return None
    if not isinstance(name, str):
        raise HookInputError("invalid_event")
    if name not in HOOK_EVENTS:
        return None
    projected = {key: event[key] for key in (*LIFECYCLE_FIELDS, *EVENT_FIELDS[name]) if key in event}
    # Drop the wire object before further processing; a model or retained ledger
    # must never receive a tool result through the lifecycle adapter.
    del event, raw
    if not _valid_id(projected.get("session_id")) or not isinstance(projected.get("cwd"), str):
        raise HookInputError("invalid_event")
    for key, value in projected.items():
        if key == "stop_hook_active":
            valid = type(value) is bool
        elif value is None and (key in ("transcript_path", "last_assistant_message")
                                or key == "turn_id" and name in ("SessionStart", "SessionEnd")):
            valid = True
        else:
            valid = isinstance(value, str)
        if not valid:
            raise HookInputError("invalid_event")
    if (name not in ("SessionStart", "SessionEnd") and not _valid_id(projected.get("turn_id"))
            or (projected.get("turn_id") is not None and not _valid_id(projected["turn_id"]))):
        raise HookInputError("invalid_event")
    if name == "SessionStart" and projected.get("source") not in ("startup", "resume", "clear", "compact"):
        return None
    if name == "UserPromptSubmit" and "prompt" not in projected:
        raise HookInputError("invalid_event")
    try:
        # Check actual text as well as its complete encoded retained contract.
        # A huge prompt cannot sneak in by dropping unrelated wire fields.
        if any(len(value.encode("utf-8")) > MAX_STDIN
               for value in projected.values() if isinstance(value, str)):
            raise HookInputError("retained_limit")
        if len(json.dumps(projected, ensure_ascii=False).encode("utf-8")) > MAX_STDIN:
            raise HookInputError("retained_limit")
    except UnicodeError as exc:
        raise HookInputError("invalid_event") from exc
    return projected


def _input_warning(reason: str) -> dict[str, str]:
    # All reasons are fixed local codes, never payload or parser exception text.
    return {"systemMessage": f"Mneme lifecycle event omitted ({reason}); automatic memory delivery/recording for this event was not attempted. This does not establish store availability or an empty store. Continue normally; use explicit scoped recall if needed delivery cannot wait."}


def _bounded_json(path: Path, limit: int, *, with_raw=False) -> Any:
    with path.open("rb") as file:
        raw = file.read(limit + 1)
    if len(raw) > limit:
        raise HookError("local JSON exceeds size limit")
    try:
        value = json.loads(raw)
        return (value, raw) if with_raw else value
    except (ValueError, UnicodeDecodeError) as exc:
        raise HookError("invalid local JSON") from exc


def _valid_id(value: Any) -> bool:
    return isinstance(value, str) and ID_RE.fullmatch(value) is not None


def _config(path: Path, *, workspace_binding=None) -> dict[str, Any]:
    data, parsed_raw = _bounded_json(path, MAX_CONFIG, with_raw=True)
    if not isinstance(data, dict):
        raise HookError("invalid hook config schema")
    if data.get("schema") == CONFIG_SCHEMA_V11:
        from misc_config import validate_config
        from misc_binding import validate_binding
        try:
            data = validate_config(data)
            binding = validate_binding(workspace_binding, excluded_roots=data["excluded_roots"])
        except (ValueError, OSError, TypeError) as error:
            raise HookError("invalid misc configuration or workspace binding") from error
        data["project_root"] = binding["workspace_root"]
        data["workspace_binding"] = binding
    elif workspace_binding is not None:
        raise HookError("workspace binding requires misc configuration")
    if data.get("schema") == CONFIG_SCHEMA:
        expected = {"schema", "project_root", "state_dir"}
        mode = "reminder"
    elif data.get("schema") == CONFIG_SCHEMA_V2:
        expected = {"schema", "project_root", "state_dir", "service_config", "recall_mode"}
        mode = data.get("recall_mode")
        if mode not in ("automatic", "reminder"):
            raise HookError("invalid recall_mode")
    elif data.get("schema") == CONFIG_SCHEMA_V3:
        expected = {"schema", "project_root", "state_dir", "service_config",
                    "memory_mode", "shadow_model", "shadow_codex", "shadow_codex_sha256"}
        if data.get("memory_mode") != "shadow":
            raise HookError("invalid memory_mode")
        if data.get("shadow_model") not in ("gpt-5.6-sol", "gpt-5.6-terra"):
            raise HookError("invalid shadow_model")
        codex = data.get("shadow_codex")
        if not isinstance(codex, str) or not Path(codex).is_absolute():
            raise HookError("shadow_codex must be an absolute path")
        digest = data.get("shadow_codex_sha256")
        if not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest) is None:
            raise HookError("invalid shadow_codex_sha256")
        mode = "automatic"
    elif data.get("schema") in (CONFIG_SCHEMA_V4, CONFIG_SCHEMA_V5, CONFIG_SCHEMA_V6, CONFIG_SCHEMA_V7):
        raise HookError("legacy async reader configuration requires repreparation for schema v8; quiesce old workers and start a fresh session before switching hooks")
    elif data.get("schema") in (CONFIG_SCHEMA_V8, CONFIG_SCHEMA_V9, CONFIG_SCHEMA_V10, CONFIG_SCHEMA_V11):
        expected = {"schema", "project_root", "state_dir", "service_config",
                    "memory_mode", "reader_model", "reader_codex", "reader_codex_sha256",
                    "librarian_effort", "recording_mode"}
        if data["schema"] == CONFIG_SCHEMA_V9:
            expected |= {"memory_scope", "store_target"}
        if data["schema"] == CONFIG_SCHEMA_V10:
            expected |= {"global_preferences"}
        if data["schema"] == CONFIG_SCHEMA_V11:
            expected |= {"memory_scope", "store_target", "excluded_roots", "workspace_binding"}
        if data.get("recording_mode") not in ("off", "automatic"):
            raise HookError("invalid recording_mode")
        if data.get("memory_mode") != "async":
            raise HookError("invalid memory_mode")
        from librarian_policy import resolve
        try:
            resolve(data)
        except ValueError as error:
            raise HookError(str(error)) from error
        codex = data.get("reader_codex")
        if not isinstance(codex, str) or not Path(codex).is_absolute():
            raise HookError("reader_codex must be an absolute path")
        digest = data.get("reader_codex_sha256")
        if not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest) is None:
            raise HookError("invalid reader_codex_sha256")
        auth = data.get("reader_auth")
        if auth is not None and (not isinstance(auth, str) or not Path(auth).is_absolute()):
            raise HookError("reader_auth must be an absolute path")
        if auth is not None:
            expected = expected | {"reader_auth"}
        mode = "automatic"
    else:
        raise HookError("invalid hook config schema")
    if set(data) not in (expected, expected | {"memory_scope"}):
        raise HookError("invalid hook config fields")
    scope = data.get("memory_scope", "project")
    if scope not in ("project", "workshop", "misc"):
        raise HookError("invalid memory_scope")
    if (scope == "misc") != (data["schema"] == CONFIG_SCHEMA_V11):
        raise HookError("misc scope requires device schema v11")
    if scope == "workshop" and mode != "reminder" and data["schema"] != CONFIG_SCHEMA_V9:
        raise HookError("workshop memory_scope requires reminder recall_mode")
    if data["schema"] == CONFIG_SCHEMA_V9 and scope != "workshop":
        raise HookError("schema v9 requires explicit workshop scope")
    if data["schema"] == CONFIG_SCHEMA_V10 and scope != "project":
        raise HookError("schema v10 requires project scope")
    for name in ("project_root", "state_dir"):
        value = data[name]
        if not isinstance(value, str) or not Path(value).is_absolute():
            raise HookError(f"{name} must be an absolute path")
    root = Path(data["project_root"])
    if not root.is_dir():
        raise HookError("configured project root does not exist")
    data["project_root"] = root.resolve()
    data["state_dir"] = Path(data["state_dir"])
    if data["schema"] in (CONFIG_SCHEMA_V2, CONFIG_SCHEMA_V3, CONFIG_SCHEMA_V8, CONFIG_SCHEMA_V9, CONFIG_SCHEMA_V10, CONFIG_SCHEMA_V11):
        service_config = data["service_config"]
        if not isinstance(service_config, str) or not Path(service_config).is_absolute():
            raise HookError("service_config must be an absolute path")
        data["service_config"] = Path(service_config).resolve()
    if data["schema"] == CONFIG_SCHEMA_V3:
        data["shadow_codex"] = Path(data["shadow_codex"]).resolve()
    if data["schema"] in (CONFIG_SCHEMA_V8, CONFIG_SCHEMA_V9, CONFIG_SCHEMA_V10, CONFIG_SCHEMA_V11):
        data["reader_codex"] = Path(data["reader_codex"]).resolve()
        if "reader_auth" in data:
            data["reader_auth"] = Path(data["reader_auth"]).resolve()
    data["recall_mode"] = mode
    data["memory_scope"] = scope
    data["_config_path"] = path.resolve()
    if data["schema"] == CONFIG_SCHEMA_V11:
        from target_policy import misc_policy
        try:
            misc_policy(data)
        except (ValueError, OSError, TypeError) as error:
            raise HookError("misc target unavailable or workspace no longer eligible") from error
    if data["schema"] in (CONFIG_SCHEMA_V8, CONFIG_SCHEMA_V9, CONFIG_SCHEMA_V10, CONFIG_SCHEMA_V11):
        if data["schema"] == CONFIG_SCHEMA_V9:
            from target_policy import workshop_policy
            try:
                workshop_policy(data)
            except ValueError as error:
                raise HookError(str(error)) from error
        if data["schema"] == CONFIG_SCHEMA_V10:
            from target_policy import global_preferences_policy
            try:
                global_preferences_policy(data)
            except (ValueError, OSError, TypeError) as error:
                raise HookError(str(error)) from error
        from recording_jobs import _config_digest, read_regular, load_project_focus
        try:
            data["_project_focus"] = load_project_focus(data)
        except ValueError as error:
            raise ProjectFocusError(str(error)) from error
        try:
            pin = _config_digest(data)
            if load_project_focus(data) != data["_project_focus"]:
                raise HookError("project capture policy changed during load; reload before work")
            if read_regular(data["_config_path"], MAX_CONFIG) != parsed_raw:
                raise HookError("async hook configuration changed during load; reload before work")
            data["_reader_config_pin"] = pin
        except (OSError, ValueError, TypeError) as error:
            raise HookError("async configuration snapshot unavailable or changed; reload before work") from error
    return data


def _in_scope(event: dict[str, Any], root: Path) -> bool:
    cwd = event.get("cwd")
    if not isinstance(cwd, str) or not Path(cwd).is_absolute():
        return False
    try:
        Path(cwd).resolve().relative_to(root)
        return True
    except ValueError:
        return False


def _subagent(event: dict[str, Any]) -> bool:
    # Codex shares session_id with subagents. Do not let an agent child close or
    # continue the parent's turn when an agent identifier is available.
    return bool(event.get("agent_id") or event.get("agent_type"))


def _substantive(prompt: str) -> bool:
    text = prompt.strip()
    if not text or ACK.fullmatch(text) or (len(text) <= 160 and TRIVIAL.fullmatch(text)):
        return False
    if WORK.search(text):
        return True
    return len(text) >= 100 or "\n" in text


def _state_path(state_dir: Path, session_id: str) -> Path:
    return state_dir / (hashlib.sha256(session_id.encode()).hexdigest() + ".json")


def _atomic_write(path: Path, data: Any) -> None:
    raw = json.dumps(data, separators=(",", ":"), ensure_ascii=False).encode()
    if len(raw) > MAX_STATE:
        raise HookError("hook state exceeds size limit")
    fd, name = tempfile.mkstemp(prefix=".mneme-hook-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as file:
            file.write(raw)
            file.flush()
            os.fsync(file.fileno())
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


def _with_state(state_dir: Path, session_id: str, mutate, *, blocking: bool = True):
    state_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    path = _state_path(state_dir, session_id)
    lock = path.with_suffix(".lock")
    with lock.open("a+b") as file:
        try:
            fcntl.flock(file, fcntl.LOCK_EX | (0 if blocking else fcntl.LOCK_NB))
        except BlockingIOError as exc:
            raise HookError("hook state busy") from exc
        if path.exists():
            data = _bounded_json(path, MAX_STATE)
            if not isinstance(data, dict) or data.get("schema") != STATE_SCHEMA or not isinstance(data.get("turns"), dict):
                raise HookError("invalid hook state schema")
        else:
            data = {"schema": STATE_SCHEMA, "turns": {}}
        epoch = data.get("context_epoch", 0)
        if not isinstance(epoch, int) or isinstance(epoch, bool) or epoch < 0:
            raise HookError("invalid context epoch")
        data["context_epoch"] = epoch
        for field in ("seen_ids", "seen_fingerprints"):
            values = data.get(field, [])
            if not isinstance(values, list) or len(values) > MAX_SEEN or not all(isinstance(v, str) and len(v) <= 160 for v in values):
                raise HookError("invalid seen-card metadata")
            data[field] = values
        if len(data["seen_ids"]) != len(data["seen_fingerprints"]):
            raise HookError("misaligned seen-card metadata")
        now = time.time()
        data["turns"] = {k: v for k, v in data["turns"].items()
                         if _valid_id(k) and isinstance(v, dict) and isinstance(v.get("at"), (int, float))
                         and 0 <= now - v["at"] <= TTL_SECONDS}
        result, changed = mutate(data, now)
        if changed:
            if len(data["turns"]) > MAX_TURNS:
                oldest = sorted(data["turns"], key=lambda k: data["turns"][k]["at"])
                for key in oldest[:len(data["turns"]) - MAX_TURNS]:
                    del data["turns"][key]
            _atomic_write(path, data)
        return result


def _context(event_name: str, message: str) -> dict[str, Any]:
    if len(message.encode("utf-8")) > MAX_CONTEXT_BYTES:
        raise HookError("hook context exceeds size limit")
    return {"hookSpecificOutput": {"hookEventName": event_name, "additionalContext": message}}


def _warning() -> dict[str, str]:
    return {"systemMessage": "Mneme hook unavailable; memory was not checked or recorded. Continue normally and mention the gap if relevant."}


def _memory_choice(config: dict[str, Any]) -> str:
    if config["memory_scope"] == "workshop":
        routing = ("Use the configured global user store only for personal/shared continuity. "
                   "Project details belong in their explicitly configured project store; "
                   "without one, keep them in artifacts, not global memory.")
    elif config["memory_scope"] == "misc":
        routing = "Keep these memories in the explicitly selected shared misc store, retaining their workspace provenance."
    else:
        routing = "Keep these memories in this configured project store."
    return MEMORY_CHOICE + " " + routing


def _recall_guidance(config: dict[str, Any]) -> str:
    scope = "this project" if config["memory_scope"] == "project" else "the correctly scoped store"
    return (f"Recall a few task-relevant memories from {scope} at wake/resume, meaningful task changes, "
            "and before decisions; verify mutable claims. Reuse context already loaded; "
            "no per-tool retrieval or repeated dumps.")


def _checkpoint_command(config: dict[str, Any], session_id: str, turn_id: str) -> str:
    config_path = config.get("_config_path")
    if not isinstance(config_path, Path) or not config_path.is_absolute():
        raise HookError("hook config path unavailable")
    words = [sys.executable, str(Path(__file__).resolve()), "checkpoint", "--config", str(config_path),
             "--session-id", session_id, "--turn-id", turn_id]
    if config.get("memory_scope") == "misc":
        words.extend(["--workspace-binding", json.dumps(config["workspace_binding"], ensure_ascii=False,
                                                        separators=(",", ":"))])
    return " ".join(shlex.quote(word) for word in words)


def _recall_cards(config: dict[str, Any], prompt: str) -> dict[str, Any]:
    """Load the sibling passive-read helper only for an admitted automatic turn."""
    module_path = Path(__file__).with_name("hook_recall.py")
    spec = importlib.util.spec_from_file_location("mneme_codex_hook_recall", module_path)
    if spec is None or spec.loader is None:
        raise HookError("recall helper unavailable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    if config.get("memory_mode") == "shadow":
        return module.recall_cards(config["service_config"], prompt, config["project_root"],
                                   timeout=1.5, observe=True)
    return module.recall_cards(config["service_config"], prompt, config["project_root"], timeout=1.5)


def _valid_card(card: Any) -> bool:
    basic = (isinstance(card, dict) and all(isinstance(card.get(k), str) for k in
                                          ("id", "summary", "status", "source", "fingerprint"))
            and ULID_RE.fullmatch(card["id"]) is not None
            and 0 < len(card["fingerprint"]) <= 160
            and card["status"] == "active" and 0 < len(card["source"].encode("utf-8")) <= 256
            and card.get("kind", "semantic") in ("semantic", "episode")
            and (card.get("kind") != "episode"
                 or (all(key in card for key in EPISODE_CARD_FIELDS)
                     and card["edition_id"] == card["id"])))
    if isinstance(card, dict) and "db_id" in card and (not isinstance(card["db_id"], str) or ULID_RE.fullmatch(card["db_id"]) is None):
        return False
    if isinstance(card, dict) and "scope" in card and (card["scope"] not in ("project", "global_preference")
            or card["scope"] == "global_preference" and (card.get("kind", "semantic") != "semantic"
                or "db_id" not in card or "touchstone" in card
                or any(k in card for k in ("routing_binding", "conditional_binding", "entry_kind")))):
        return False
    if not basic:
        return False
    if card.get("kind", "semantic") == "semantic":
        if any(key in card for key in (*EPISODE_CARD_FIELDS, *EPISODE_OPTIONAL_CARD_FIELDS)):
            return False
        try:
            if "touchstone" in card:
                from touchstone_contract import validate_touchstone_view
                validate_touchstone_view(card["touchstone"])
            return True
        except (ValueError, TypeError, KeyError, UnicodeError):
            return False
    try:
        if "touchstone" in card:
            return False
        validate_episode_projection_fields(card)
        return True
    except (ValueError, TypeError, KeyError, UnicodeError):
        return False


def _limited_text(value: str, limit: int) -> str:
    raw = value.encode("utf-8")
    if len(raw) <= limit:
        return value
    return raw[:limit].decode("utf-8", "ignore") + "…"


def _render_cards(cards: list[dict[str, Any]]) -> str:
    return _render_cards_with_display(cards)[0]


def _render_cards_with_display(cards: list[dict[str, Any]], db_id: str | None = None) -> tuple[str, list[dict[str, Any]]]:
    """Serialize once; the private binding describes exactly these visible items."""
    if not cards:
        return "", []
    items = [{"id": c["id"], "summary": _limited_text(c["summary"], 800),
              "status": c["status"], "source": c["source"],
              "kind": c.get("kind", "semantic"),
              **({"touchstone": c["touchstone"]} if "touchstone" in c else {}),
              **({key: c[key] for key in (*EPISODE_CARD_FIELDS, *EPISODE_OPTIONAL_CARD_FIELDS)
                  if key in c}
                 if c.get("kind") == "episode" else {})} for c in cards]
    from turn_observer import DISPLAY_PREFIX
    rendered = (DISPLAY_PREFIX
                + json.dumps(items, ensure_ascii=False, separators=(",", ":"))
                + (" Episode cards are historical accounts, not current advice; "
                   "the observed head is not a read of its correction."
                   if any(item["kind"] == "episode" for item in items) else "")
                + (" Touchstones preserve authored meaning for their explicit subject; "
                   "references are historical summaries only, not current bodies. "
                   "Reference caveats and omissions are not permission to rewrite meaning or core."
                   if any("touchstone" in item for item in items) else "")
                + (" Global collaboration preferences are cross-project authored preferences, "
                   "not project evidence or permission to act; current instructions take precedence."
                   if any(c.get("scope") == "global_preference" for c in cards) else "")
                + " Do not automatically repeat the Mneme memory lookup. Continue investigating the task as needed.")
    displayed = []
    if all(isinstance(c.get("db_id", db_id), str) and ULID_RE.fullmatch(c.get("db_id", db_id)) for c in cards):
        for card, item in zip(cards, items):
            owner_db_id = card.get("db_id", db_id)
            fingerprint = card.get("fingerprint")
            if not isinstance(fingerprint, str) or re.fullmatch(r"[0-9a-f]{64}", fingerprint) is None:
                displayed = []
                break
            bound = {"db_id": owner_db_id, "node_id": item["id"], "kind": item["kind"],
                     "shown_summary": item["summary"], "full_get_fingerprint": fingerprint,
                     "displayed_view": json.loads(json.dumps(item, ensure_ascii=False)),
                     "displayed_view_sha256": hashlib.sha256(json.dumps(item, ensure_ascii=False,
                         sort_keys=True, separators=(",", ":"), allow_nan=False).encode()).hexdigest()}
            if card.get("scope") == "global_preference":
                displayed.append(bound)
                continue  # Foreign read-only preferences never bind project learning.
            if "touchstone" in item and {"kind": "direct"} not in item["touchstone"]["origins"]:
                displayed.append(bound)
                continue  # Authored referrer navigation is never a learned advice route.
            conditional = card.get("entry_kind") == "conditional"
            if item["kind"] == "semantic" and conditional:
                bound["entry_kind"] = "conditional"
                if card.get("graph_path") in (None, []) and card.get("routing_binding") is None:
                    try:
                        from routing_memory import validate_conditional_binding
                        bound["conditional_binding"] = validate_conditional_binding(
                            card.get("conditional_binding"), item["id"], expected_db_id=owner_db_id)
                    except (ValueError, TypeError, KeyError, UnicodeError, ImportError):
                        pass
            if (item["kind"] == "semantic" and "entry_kind" not in card
                    and "conditional_binding" not in card and card.get("routing_binding") is not None):
                try:
                    from routing_memory import validate_binding
                    routing_binding = validate_binding(card["routing_binding"], expected_db_id=owner_db_id)
                    if routing_binding["route"]["target"] == item["id"]:
                        bound["routing_binding"] = routing_binding
                except (ValueError, TypeError, KeyError, UnicodeError, ImportError):
                    pass
            displayed.append(bound)
    return rendered, displayed


ASYNC_OVERFLOW_NOTE = (" Selected-batch cards exceeded the context budget; "
                       "omitted cards remain eligible for later turns.")


def _pack_async_delivery(cards: list[dict[str, Any]], session_id: str, turn_id: str,
                         db_id: str | None = None, *, concerns=None,
                         prior_budget_omitted: int = 0) -> dict[str, Any]:
    """One exact envelope: whole cards and atomic pair/caveat components.

    Optional private cases never replace useful cards or imply corpus completeness.
    Both worker consumption and foreground rendering use this identical packer.
    """
    if (not _valid_id(session_id) or not _valid_id(turn_id)
            or not isinstance(cards, list) or not all(_valid_card(c) for c in cards)
            or len({(c.get("db_id", db_id), c["id"]) for c in cards}) != len(cards)
            or type(prior_budget_omitted) is not int or not 0 <= prior_budget_omitted <= MAX_STDIN):
        raise ValueError("invalid async delivery")
    from turn_observer import validate_delivery_packet, validate_delivery_concerns, render_delivery_concern, DELIVERY_SCHEMA
    concerns = [] if concerns is None else concerns
    # Existing concerns are same-project pairs. A raw-ID collision with a
    # foreign preference cannot be resolved by their legacy ID-only endpoints.
    duplicate_ids = {c["id"] for c in cards if sum(other["id"] == c["id"] for other in cards) > 1}
    offered_concern_count = len(concerns) if isinstance(concerns, list) else 0
    if isinstance(concerns, list):
        concerns = [c for c in concerns if not isinstance(c, dict) or
                    not duplicate_ids.intersection(c.get("displayed_endpoint_ids", []))]
    _, offered_displayed = _render_cards_with_display(cards, db_id)
    # No database binding means warning-only context can still be delivered.
    if concerns and not offered_displayed:
        # Older catalogs may lack a database identity: keep the visible warning,
        # but never manufacture private delivery/maintenance authority.
        concerns = [{**c, "expected_row": None} for c in concerns]
        shape_displayed = [{"node_id": c["id"], "db_id": None} for c in cards]
        concerns = validate_delivery_concerns(concerns, shape_displayed)
    else:
        concerns = validate_delivery_concerns(concerns, offered_displayed)
    prefix = (f"Mneme async reader emission for source session_id={session_id}, "
              f"turn_id={turn_id} (emitted by this hook, not acknowledged as seen). ")

    def render(chosen, cases, omitted):
        text, displayed = _render_cards_with_display(chosen, db_id)
        context = prefix + text + "".join(" " + render_delivery_concern(c) for c in cases)
        context += ASYNC_OVERFLOW_NOTE if omitted else ""
        if len(context.encode("utf-8")) > MAX_CONTEXT_BYTES:
            return context, displayed, False
        if displayed:
            try:
                packet = validate_delivery_packet({
                    "schema": DELIVERY_SCHEMA, "session_id": session_id, "turn_id": turn_id,
                    "rendered_text": context,
                    "rendered_sha256": hashlib.sha256(context.encode()).hexdigest(),
                    "displayed": displayed, "concerns": cases})
            except ValueError as exc:
                if str(exc) == "delivery_packet_limit":
                    return context, displayed, False
                raise
            displayed = packet["displayed"]
        elif len(json.dumps({"context": context, "concerns": cases}, ensure_ascii=False,
                            separators=(",", ":")).encode()) > 8192:
            return context, displayed, False
        return context, displayed, True

    # Establish useful ordinary delivery first. Optional cases cannot crowd it out.
    packed = cards
    if not render(cards, [], bool(prior_budget_omitted))[2]:
        packed = []
        for card in cards:
            if render(packed + [card], [], True)[2]:
                packed.append(card)
    omitted = len(cards) - len(packed) + prior_budget_omitted
    retained = []
    for case in concerns:
        if not set(case["displayed_endpoint_ids"]) <= {c["id"] for c in packed}:
            continue
        candidate = case
        if not render(packed, retained + [candidate], bool(omitted))[2] and case["expected_row"] is not None:
            candidate = {**case, "expected_row": None}  # Private authority loses before visible warning.
        if render(packed, retained + [candidate], bool(omitted))[2]:
            retained.append(candidate)
    context, displayed, fits = render(packed, retained, bool(omitted))
    if not fits:
        raise ValueError("async envelope exceeds context budget")
    return {"cards": packed, "context": context, "displayed": displayed,
            "concerns": retained, "concern_omitted_count": offered_concern_count - len(retained),
            "budget_omitted_count": omitted}


def _finalize_recall(config: dict[str, Any], session_id: str, turn_id: str, epoch: int,
                     result: dict[str, Any], base: str) -> tuple[str, list[dict[str, Any]], int]:
    outcome = result.get("outcome")
    if outcome not in ("ok", "empty", "timeout", "unavailable", "skipped"):
        outcome = "unavailable"
    elapsed = result.get("elapsed_ms")
    elapsed = elapsed if isinstance(elapsed, int) and not isinstance(elapsed, bool) and 0 <= elapsed <= 10000 else None
    supplied = result.get("cards")
    cards = [c for c in supplied[:2] if _valid_card(c)] if isinstance(supplied, list) else []
    def finalize(data, _now):
        turn = data["turns"].get(turn_id)
        if turn is None or data["context_epoch"] != epoch:
            return ("stale_context", [], 0), False
        seen_ids = data["seen_ids"]
        seen_fps = data["seen_fingerprints"]
        seen_pairs = set(zip(seen_ids, seen_fps))
        fresh = []
        dropped = 0
        for card in cards:
            from touchstone_contract import delivery_cache_fingerprint
            cache_fingerprint = delivery_cache_fingerprint(card)
            pair = (card["id"], cache_fingerprint)
            if pair in seen_pairs:
                continue
            # Only commit cards that fit as whole JSON objects in the exact
            # model-facing context, including a possible overflow notice.
            trial = fresh + [card]
            if len((base + " " + _render_cards(trial) + OVERFLOW_NOTE).encode("utf-8")) > MAX_CONTEXT_BYTES:
                dropped += 1
                continue
            fresh.append(card)
            seen_pairs.add(pair)
            seen_ids.append(card["id"])
            seen_fps.append(cache_fingerprint)
        data["seen_ids"] = seen_ids[-MAX_SEEN:]
        data["seen_fingerprints"] = seen_fps[-MAX_SEEN:]
        turn["recall"] = {"outcome": outcome, "elapsed_ms": elapsed,
                          "card_ids": [c["id"] for c in fresh],
                          "fingerprints": [c["fingerprint"] for c in fresh],
                          "overflow_dropped": dropped}
        return (outcome, fresh, dropped), True
    return _with_state(config["state_dir"], session_id, finalize)


def _own_continuation(prompt: str, config: dict[str, Any], session_id: str,
                      *, blocking: bool = True) -> bool:
    match = CONTINUATION_RE.match(prompt)
    if match is None or match.group(1) != session_id:
        return False
    source_turn, token = match.group(2), match.group(3)
    def check_source(data, _now):
        turn = data["turns"].get(source_turn)
        return bool(turn and turn.get("continued") is True and turn.get("continuation_token") == token), False
    return _with_state(config["state_dir"], session_id, check_source, blocking=blocking)


def _task_fragment(prompt: str, limit: int = 360) -> str:
    """A small task-window cue, not a retained prompt or transcript."""
    clean = " ".join("".join(c if ord(c) >= 32 and ord(c) != 127 else " "
                             for c in prompt).split())
    return clean.encode("utf-8")[:limit].decode("utf-8", "ignore")


def _shadow_launch(config: dict[str, Any], session_id: str, turn_id: str,
                   cue: str, answer: str, cards: list[dict[str, str]]) -> bool:
    module_path = Path(__file__).with_name("shadow.py")
    spec = importlib.util.spec_from_file_location("mneme_codex_shadow", module_path)
    if spec is None or spec.loader is None:
        return False
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.launch(config["state_dir"], session_id, turn_id, cue, answer,
                         cards, config["shadow_codex"], config["shadow_codex_sha256"],
                         config["shadow_model"])


def _handle_shadow(event: dict[str, Any], config: dict[str, Any]) -> dict[str, Any]:
    name = event.get("hook_event_name")
    session_id = event.get("session_id")
    if not _valid_id(session_id):
        return {}
    if name == "SessionStart":
        if event.get("source") not in ("startup", "resume", "clear", "compact"):
            return {}
        def reset(data, _now):
            data["context_epoch"] += 1
            data["seen_ids"] = []
            data["seen_fingerprints"] = []
            # Keep one recent task fragment through compaction, not a history.
            data["shadow_recent"] = data.get("shadow_recent", [])[-1:]
            for turn in data["turns"].values():
                shadow = turn.get("shadow")
                if isinstance(shadow, dict) and shadow.get("assessment") == "pending":
                    shadow["cue"] = ""
                    shadow["cards"] = []
                    shadow["assessment"] = "stale_context"
            return None, True
        _with_state(config["state_dir"], session_id, reset)
        return {}
    turn_id = event.get("turn_id")
    if not _valid_id(turn_id):
        return {}
    if name == "UserPromptSubmit":
        prompt = event.get("prompt")
        if not isinstance(prompt, str) or prompt.startswith("MNEME_CHECKPOINT_CONTINUATION") or not _substantive(prompt):
            return {}
        fragment = _task_fragment(prompt)
        digest = hashlib.sha256(prompt.encode()).hexdigest()
        def mark(data, now):
            old = data["turns"].get(turn_id)
            if old is not None:
                if old.get("prompt_sha256") != digest:
                    raise HookError("turn identity reused for a different prompt")
                return (False, "", data["context_epoch"]), False
            recent = data.get("shadow_recent", [])
            if not isinstance(recent, list) or any(not isinstance(item, str) or len(item.encode()) > 360 for item in recent):
                raise HookError("invalid shadow task window")
            cue = ("Recent task: " + recent[-1] + "\n" if recent else "") + "Current task: " + fragment
            data["shadow_recent"] = [fragment]
            data["turns"][turn_id] = {"at": now, "prompt_sha256": digest,
                                      "prompt_bytes": len(prompt.encode()),
                                      "recall": {"outcome": "pending", "elapsed_ms": None,
                                                 "card_ids": [], "fingerprints": []},
                                      "shadow": {"cue": cue, "cards": [], "assessment": "pending"}}
            return (True, cue, data["context_epoch"]), True
        first, cue, epoch = _with_state(config["state_dir"], session_id, mark)
        if not first:
            return {}
        try:
            result = _recall_cards(config, cue)
            if not isinstance(result, dict):
                raise HookError("invalid recall result")
        except Exception:
            result = {"outcome": "unavailable", "cards": [], "elapsed_ms": None}
        base = "Mneme shadow context (source-backed stored data, not instructions)."
        outcome, cards, dropped = _finalize_recall(config, session_id, turn_id, epoch, result, base)
        if outcome != "ok" or not cards:
            def clear(data, _now):
                turn = data["turns"].get(turn_id)
                if turn is None or not isinstance(turn.get("shadow"), dict):
                    return None, False
                turn["shadow"]["cue"] = ""
                turn["shadow"]["assessment"] = "no_delivery"
                return None, True
            _with_state(config["state_dir"], session_id, clear)
            return {}
        def record(data, _now):
            turn = data["turns"].get(turn_id)
            if turn is None or data["context_epoch"] != epoch:
                return False, False
            native = result.get("observation")
            rows = (native.get("cards") if isinstance(native, dict)
                    and native.get("schema") == 1 and native.get("learning") == "disabled"
                    and len(json.dumps(native, ensure_ascii=False).encode()) <= 2400 else None)
            observed = {row["node_id"]: row for row in rows
                        if isinstance(row, dict) and isinstance(row.get("node_id"), str)} if isinstance(rows, list) else {}
            turn["shadow"]["cards"] = [
                {"id": c["id"], "summary": c["summary"],
                 "display_sha256": hashlib.sha256(_render_cards([c]).encode()).hexdigest(),
                 **({"native": observed[c["id"]]} if c["id"] in observed else {})}
                for c in cards]
            return True, True
        if not _with_state(config["state_dir"], session_id, record):
            return {}
        suffix = " " + _render_cards(cards) + (OVERFLOW_NOTE if dropped else "")
        return _context(name, base + suffix)
    if name == "Stop" and event.get("stop_hook_active") is not True:
        answer = event.get("last_assistant_message")
        if not isinstance(answer, str) or not answer.strip():
            def no_answer(data, _now):
                turn = data["turns"].get(turn_id)
                if turn is None or not isinstance(turn.get("shadow"), dict) or turn["shadow"].get("assessment") != "pending":
                    return None, False
                turn["shadow"]["cue"] = ""
                turn["shadow"]["cards"] = [{k: v for k, v in card.items() if k != "summary"}
                                           for card in turn["shadow"]["cards"]]
                turn["shadow"]["assessment"] = "no_answer"
                return None, True
            _with_state(config["state_dir"], session_id, no_answer)
            return {}
        def claim(data, now):
            turn = data["turns"].get(turn_id)
            if turn is None or not isinstance(turn.get("shadow"), dict):
                return None, False
            shadow = turn["shadow"]
            if shadow.get("assessment") != "pending" or not shadow.get("cards"):
                return None, False
            inflight = data.get("shadow_inflight")
            if isinstance(inflight, (int, float)) and now - inflight < 60:
                shadow["assessment"] = "busy"
                shadow["cue"] = ""
                shadow["cards"] = [{k: v for k, v in card.items() if k != "summary"}
                                   for card in shadow["cards"]]
                return None, True
            data["shadow_inflight"] = now
            shadow["assessment"] = "queued"
            job = (shadow["cue"], shadow["cards"])
            shadow["cue"] = ""
            shadow["cards"] = [{k: v for k, v in card.items() if k != "summary"}
                               for card in shadow["cards"]]
            return job, True
        claimed = _with_state(config["state_dir"], session_id, claim)
        if claimed is not None:
            cue, cards = claimed
            try:
                launched = _shadow_launch(config, session_id, turn_id, cue, answer, cards)
            except Exception:
                launched = False
            if not launched:
                def unavailable(data, _now):
                    turn = data["turns"].get(turn_id)
                    if turn is None or not isinstance(turn.get("shadow"), dict):
                        return None, False
                    turn["shadow"]["assessment"] = "unavailable"
                    data["shadow_inflight"] = None
                    return None, True
                _with_state(config["state_dir"], session_id, unavailable)
        return {}
    return {}


def _reader_worker():
    """Load the optional async reader without importing it in legacy modes."""
    path = Path(__file__).with_name("reader_worker.py")
    spec = importlib.util.spec_from_file_location("mneme_codex_reader_worker", path)
    if spec is None or spec.loader is None:
        raise HookError("reader worker unavailable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _recording_jobs():
    import recording_jobs
    return recording_jobs


def _opportunity_base(config: dict[str, Any], session_id: str, turn_id: str) -> str:
    if config.get("recording_mode") == "automatic":
        if config.get("memory_scope") == "misc":
            return "Optional off-thread misc recording is enabled; workspace provenance is retained. No actor memory checkpoint is required. Verify mutable claims."
        return "Optional off-thread project recording is enabled; no actor memory checkpoint is required. Verify mutable claims."
    command = _checkpoint_command(config, session_id, turn_id)
    return (f"Mneme {config['memory_scope']}-memory opportunity. Source identity: "
            f"session_id={session_id}, turn_id={turn_id}. Verify mutable claims. "
            f"{_memory_choice(config)} Checkpoint this turn with `{command} --outcome none` "
            "if nothing merits saving, `--outcome deferred` on service error, or "
            "`--outcome captured --id ACTUAL_ULID` using read-back note IDs or episode "
            "edition_ids. Checkpoint metadata alone is not proof that memory was saved.")


def _mark_opportunity(config: dict[str, Any], session_id: str, turn_id: str,
                      prompt: str, *, blocking: bool = True) -> tuple[bool, int]:
    digest = hashlib.sha256(prompt.encode()).hexdigest()
    def mark(data, now):
        turns = data["turns"]
        old = turns.get(turn_id)
        if old is not None:
            if old.get("prompt_sha256") != digest:
                raise HookError("turn identity reused for a different prompt")
            return (False, data["context_epoch"]), False
        turns[turn_id] = {"at": now, "prompt_sha256": digest,
                          "prompt_bytes": len(prompt.encode()),
                          "checkpoint": None, "continued": False,
                          "recall": {"outcome": "pending" if config["recall_mode"] == "automatic" else "skipped",
                                     "elapsed_ms": None, "card_ids": [], "fingerprints": []}}
        return (True, data["context_epoch"]), True
    return _with_state(config["state_dir"], session_id, mark, blocking=blocking)


def _stop_checkpoint(config: dict[str, Any], session_id: str, turn_id: str,
                     *, blocking: bool = True) -> dict[str, Any]:
    def maybe_continue(data, _now):
        turn = data["turns"].get(turn_id)
        if turn is None or turn.get("checkpoint") is not None or turn.get("continued") is True:
            return None, False
        turn["continued"] = True
        token = secrets.token_hex(16)
        turn["continuation_token"] = token
        return token, True
    token = _with_state(config["state_dir"], session_id, maybe_continue, blocking=blocking)
    if token is None:
        return {}
    command = _checkpoint_command(config, session_id, turn_id)
    return {"decision": "block", "reason": f"MNEME_CHECKPOINT_CONTINUATION source_session_id={session_id} source_turn_id={turn_id} token={token}\nOne memory checkpoint pass only for ORIGINAL source turn {turn_id}. {_memory_choice(config)} After saving, run `{command} --outcome captured --id ACTUAL_ULID` using read-back note IDs or episode edition_ids; otherwise run `{command} --outcome none`. On service error run `{command} --outcome deferred` and say memory was not saved. Do not repeat this continuation."}


def _handle_async(event: dict[str, Any], config: dict[str, Any],
                  *, background: bool = False) -> dict[str, Any]:
    name = event.get("hook_event_name")
    session_id = event.get("session_id")
    if not _valid_id(session_id):
        return {}
    # A new async config does not authorize replacing a legacy session ledger
    # or creating a new reader allowance for that same opaque session identity.
    prior_path = _state_path(config["state_dir"], session_id)
    try:
        if prior_path.is_symlink():
            raise HookError("invalid prior hook state")
        if prior_path.exists() and _bounded_json(prior_path, MAX_STATE).get("schema") != STATE_SCHEMA:
            return {} if background else _warning()
    except (HookError, OSError, ValueError, AttributeError, TypeError):
        return {} if background else _warning()
    if background:
        recording = config.get("recording_mode") == "automatic"
        if recording and name in ("Stop", "SessionEnd"):
            try:
                _reader_worker().background(config, event)
            except Exception:
                pass
            return {}
        if name != "UserPromptSubmit" or not _valid_id(event.get("turn_id")):
            return {}
        prompt = event.get("prompt")
        if (not isinstance(prompt, str) or prompt.startswith("MNEME_CHECKPOINT_CONTINUATION")
                or (not recording and not _substantive(prompt))):
            return {}
        try:
            _reader_worker().background(config, event)
        except Exception:
            pass
        return {}
    if name == "SessionStart":
        if event.get("source") not in ("startup", "resume", "clear", "compact"):
            return {}
        try:
            _reader_worker().reset(config, session_id)
            if config.get("recording_mode") == "automatic":
                _recording_jobs().session_start(config, event)
        except Exception:
            pass
        def reset(data, _now):
            data["context_epoch"] += 1
            data["seen_ids"] = []
            data["seen_fingerprints"] = []
            return None, True
        try:
            _with_state(config["state_dir"], session_id, reset, blocking=False)
        except (HookError, OSError):
            pass
        recording_guidance = ("Optional off-thread recording is enabled; no actor checkpoint is required."
                              if config.get("recording_mode") == "automatic" else f"At a natural checkpoint: {_memory_choice(config)}")
        scope_label = ("workshop user store" if config.get("memory_scope") == "workshop" else
                       "shared misc store for unconfigured work" if config.get("memory_scope") == "misc" else "project")
        if config.get("memory_scope") == "workshop":
            recording_guidance += " Retain personal/shared continuity and agent practice only; no private project extraction into global memory."
        selection_label = "selected" if config.get("memory_scope") == "misc" else "allowlisted"
        return _context(name, f"Mneme is configured for this {selection_label} {scope_label}; service availability is unverified. Async {scope_label} memory may appear at a later tool boundary; a missed boundary means no automatic delivery, not an empty store. Continue normal work; do not routinely call status or duplicate recall at wake/resume. If a history-dependent decision cannot wait for delivery, or needed delivery failed or missed its boundary, use explicit scoped recall; check service availability if that read fails. {recording_guidance} On compact, resume this selective workflow.")
    turn_id = event.get("turn_id")
    if name == "SessionEnd":
        try:
            _reader_worker().end_session(config, session_id)
        except Exception:
            pass
        if config.get("recording_mode") == "automatic":
            try:
                # SessionEnd is synchronous in Codex. This bounded wake only
                # detaches/reuses the existing worker; never assess inline.
                _reader_worker().background(config, event)
            except Exception:
                pass
        return {}
    if not _valid_id(turn_id):
        return {}
    if name == "UserPromptSubmit":
        prompt = event.get("prompt")
        if not isinstance(prompt, str):
            return {}
        if prompt.startswith("MNEME_CHECKPOINT_CONTINUATION"):
            try:
                return {} if _own_continuation(prompt, config, session_id, blocking=False) else _warning()
            except (HookError, OSError):
                return {}
        substantive = _substantive(prompt)
        try:
            _reader_worker().notice(config, event, substantive)
            if config.get("recording_mode") == "automatic":
                _recording_jobs().notice(config, event)
        except Exception:
            pass
        if not substantive:
            return {}
        base = _opportunity_base(config, session_id, turn_id)
        suffix = (" Optional async project-memory cards, if available, "
                  "arrive only at a later tool boundary, not in this prompt hook. "
                  "Continue normal work; do not duplicate pending recall. Use explicit scoped recall "
                  "if a history-dependent decision cannot wait or needed delivery failed or missed its boundary. "
                  "No cards yet does not prove an empty store.")
        if len((base + suffix).encode("utf-8")) > MAX_CONTEXT_BYTES:
            return _warning()
        try:
            first, _epoch = _mark_opportunity(config, session_id, turn_id, prompt, blocking=False)
        except (HookError, OSError):
            return {}
        return _context(name, base + (suffix if first else " Async reader was already attempted for this turn; replay did not re-inject cards."))
    if name == "PostToolUse":
        try:
            result = _reader_worker().consume(config, event)
            if not isinstance(result, dict) or result.get("outcome") not in ("emitted", "omitted"):
                return {}
            cards = result.get("cards")
            if not isinstance(cards, list) or (not cards and result.get("outcome") != "omitted"):
                return {}
            packed = _pack_async_delivery(cards, session_id, turn_id, result.get("db_id"),
                                          prior_budget_omitted=result.get("budget_omitted_count", 0),
                                          concerns=result.get("concerns"))
            # Consumption already packed before marking emitted. Do not accept a
            # second, different accounting decision at the hook boundary.
            if packed["cards"] != cards:
                return {}
            context = result.get("context", packed["context"])
            if context != packed["context"]:
                return {}
            displayed = packed["displayed"]
            output = _context(name, context)
            if displayed and config.get("recording_mode") == "automatic":
                try:
                    from turn_observer import DELIVERY_SCHEMA
                    _recording_jobs().attach_delivery(config, {
                        "schema": DELIVERY_SCHEMA,
                        "concerns": packed["concerns"],
                        "session_id": session_id, "turn_id": turn_id,
                        "rendered_text": context,
                        "rendered_sha256": hashlib.sha256(context.encode("utf-8")).hexdigest(),
                        "displayed": displayed})
                except Exception:
                    pass  # Optional evidence may fail; the exact recall output cannot.
            return output
        except Exception:
            return {}
    if name in ("Stop", "Interrupt"):
        try:
            _reader_worker().close_turn(config, session_id, turn_id)
            if config.get("recording_mode") == "automatic":
                _recording_jobs().close_turn(config, session_id, turn_id, cancel=name == "Interrupt")
        except Exception:
            pass
        if config.get("recording_mode") == "automatic" or name == "Interrupt" or event.get("stop_hook_active") is True:
            return {}
        try:
            return _stop_checkpoint(config, session_id, turn_id, blocking=False)
        except (HookError, OSError):
            return {}
    return {}


def handle_event(event: Any, config: dict[str, Any], *, reader_background: bool = False) -> dict[str, Any]:
    if config.get("memory_scope") == "misc":
        if (not isinstance(event, dict) or _subagent(event)
                or not _valid_id(event.get("session_id"))):
            return {}
        from target_policy import misc_policy
        try:
            misc_policy(config).validate_workspace(event.get("cwd"))
            def bind(data, _now):
                previous = data.get("workspace_binding")
                if previous is not None and previous != config["workspace_binding"]:
                    raise HookError("misc session workspace cannot change")
                data["workspace_binding"] = config["workspace_binding"]
                return None, previous is None
            _with_state(config["state_dir"], event["session_id"], bind, blocking=False)
        except (ValueError, OSError, TypeError, HookError):
            return {} if reader_background else _warning()
    if config.get("schema") == CONFIG_SCHEMA_V9:
        from target_policy import workshop_policy
        try:
            workshop_policy(config).validate_workspace(event.get("cwd") if isinstance(event, dict) else None)
        except (ValueError, OSError, TypeError):
            return {}
    if isinstance(event, dict) and isinstance(event.get("cwd"), str):
        selection = select_for_cwd(event["cwd"])
        if selection["configured"] and selection["root"] != config["project_root"]:
            return {}
        if selection["mode"] == "isolated" and config["memory_scope"] == "workshop":
            # A personal workshop reminder must not cross an isolated boundary,
            # even when that profile is on the allowlisted workshop root itself.
            return {}
        if selection["mode"] == "isolated" and config.get("memory_mode") not in ("shadow", "async") and config["recall_mode"] == "automatic":
            # The isolated workflow never sends prompts to an ambient
            # automatic reader. Deliberate project recall remains available.
            config = {**config, "recall_mode": "reminder"}
    if not isinstance(event, dict) or not _in_scope(event, config["project_root"]) or _subagent(event):
        return {}
    if reader_background:
        return _handle_async(event, config, background=True) if config.get("memory_mode") == "async" else {}
    if config.get("memory_mode") == "async":
        return _handle_async(event, config)
    if config.get("memory_mode") == "shadow":
        return _handle_shadow(event, config)
    name = event.get("hook_event_name")
    if name == "SessionStart":
        if event.get("source") not in ("startup", "resume", "clear", "compact"):
            return {}
        session_id = event.get("session_id")
        if config["recall_mode"] == "automatic" and _valid_id(session_id):
            def reset(data, _now):
                data["context_epoch"] += 1
                data["seen_ids"] = []
                data["seen_fingerprints"] = []
                return None, True
            _with_state(config["state_dir"], session_id, reset)
        return _context(name, f"Mneme is configured for this allowlisted {config['memory_scope']}; check service availability before relying on it. {_recall_guidance(config)} At a natural checkpoint: {_memory_choice(config)} On compact, resume this selective workflow.")
    if name == "UserPromptSubmit":
        session_id, turn_id, prompt = event.get("session_id"), event.get("turn_id"), event.get("prompt")
        if not _valid_id(session_id) or not _valid_id(turn_id) or not isinstance(prompt, str):
            return {}
        if prompt.startswith("MNEME_CHECKPOINT_CONTINUATION"):
            # Codex mints a new turn for Stop's synthetic continuation prompt.
            # Only a matching persisted token identifies the original turn;
            # malformed/stale markers must not recursively mint opportunities.
            return {} if _own_continuation(prompt, config, session_id) else _warning()
        if not _substantive(prompt):
            return {}
        base = _opportunity_base(config, session_id, turn_id)
        reminder_suffix = " Reminder-only mode: no passive read was attempted. " + _recall_guidance(config)
        # Leave room for every bounded non-card outcome before claiming the
        # attempt, so an overlong configured path cannot spend a passive read.
        suffix_budget = (len(reminder_suffix.encode("utf-8"))
                         if config["recall_mode"] == "reminder" else 256)
        if len(base.encode("utf-8")) + suffix_budget > MAX_CONTEXT_BYTES:
            return _warning()
        first, epoch = _mark_opportunity(config, session_id, turn_id, prompt)
        if config["recall_mode"] != "automatic":
            return _context(name, base + reminder_suffix)
        if not first:
            return _context(name, base + " Passive recall was already attempted for this turn; replay did not retry or re-inject cards.")
        try:
            result = _recall_cards(config, prompt)
            if not isinstance(result, dict):
                raise HookError("invalid recall result")
        except Exception:
            result = {"outcome": "unavailable", "cards": [], "elapsed_ms": None}
        outcome, cards, dropped = _finalize_recall(config, session_id, turn_id, epoch, result, base)
        if outcome == "stale_context":
            suffix = " Passive recall finished after the context changed; its cards were not injected."
        elif outcome == "ok" and cards:
            suffix = " " + _render_cards(cards) + (OVERFLOW_NOTE if dropped else "")
        elif outcome == "ok" and dropped:
            suffix = " Passive recall cards exceeded the context budget; none were injected or marked seen."
        elif outcome == "ok":
            suffix = " Passive recall returned no NEW cards in this context; do not repeat the same search automatically."
        elif outcome == "empty":
            suffix = " Bounded passive recall returned no cards; this does not establish absence."
        elif outcome == "skipped":
            suffix = " Passive recall was skipped by input bounds; manually check project memory if needed."
        else:
            suffix = " Passive recall was unavailable or timed out; no memory cards were injected. Check service before relying on memory."
        return _context(name, base + suffix)
    if name == "Stop":
        session_id, turn_id = event.get("session_id"), event.get("turn_id")
        if not _valid_id(session_id) or not _valid_id(turn_id) or event.get("stop_hook_active") is True:
            return {}
        return _stop_checkpoint(config, session_id, turn_id)
    return {}


def checkpoint(config: dict[str, Any], session_id: str, turn_id: str, outcome: str, ids: list[str]) -> dict[str, Any]:
    if not _valid_id(session_id) or not _valid_id(turn_id):
        raise HookError("invalid checkpoint identity")
    if outcome not in ("captured", "none", "deferred") or any(not isinstance(x, str) or ULID_RE.fullmatch(x) is None for x in ids) or len(ids) != len(set(ids)):
        raise HookError("invalid checkpoint outcome or IDs")
    if (outcome == "captured" and not 1 <= len(ids) <= 8) or (outcome != "captured" and ids):
        raise HookError("checkpoint IDs do not match outcome")
    def mark(data, _now):
        if config.get("memory_scope") == "misc":
            from target_policy import misc_policy
            try:
                misc_policy(config).validate_workspace()
            except (ValueError, OSError, TypeError) as error:
                raise HookError("misc checkpoint workspace no longer eligible") from error
            if data.get("workspace_binding") != config["workspace_binding"]:
                raise HookError("misc session workspace cannot change")
        turn = data["turns"].get(turn_id)
        if turn is None:
            raise HookError("no eligible turn to checkpoint")
        existing = turn.get("checkpoint")
        value = {"outcome": outcome, "ids": ids}
        if existing is not None:
            if existing != value:
                raise HookError("checkpoint already recorded differently")
            return {"checkpoint": outcome, "already_recorded": True}, False
        turn["checkpoint"] = value
        return {"checkpoint": outcome, "already_recorded": False}, True
    return _with_state(config["state_dir"], session_id, mark)


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    # Accept either `--config PATH checkpoint ...` or the natural
    # `checkpoint --config PATH ...` without two divergent config arguments.
    if "checkpoint" in argv and "--config" in argv:
        index = argv.index("--config")
        if index > argv.index("checkpoint") and index + 1 < len(argv):
            pair = argv[index:index + 2]
            del argv[index:index + 2]
            argv[:0] = pair
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path)
    parser.add_argument("--workspace-binding", help="internal checked misc workspace binding")
    parser.add_argument("--reader-background", action="store_true",
                        help="internal async reader submission; never emits context")
    sub = parser.add_subparsers(dest="command")
    cp = sub.add_parser("checkpoint")
    cp.add_argument("--session-id", required=True)
    cp.add_argument("--turn-id", required=True)
    cp.add_argument("--outcome", required=True, choices=("captured", "none", "deferred"))
    cp.add_argument("--id", action="append", default=[])
    cp.add_argument("--workspace-binding", dest="checkpoint_workspace_binding",
                    help="internal checked misc workspace binding")
    args = parser.parse_args(argv)
    try:
        if (args.workspace_binding is not None and getattr(args, "checkpoint_workspace_binding", None) is not None
                and args.workspace_binding != args.checkpoint_workspace_binding):
            raise HookError("conflicting workspace bindings")
        raw_binding = args.workspace_binding or getattr(args, "checkpoint_workspace_binding", None)
        binding = None
        if raw_binding is not None:
            if len(raw_binding.encode()) > 16 * 1024:
                raise HookError("workspace binding exceeds size limit")
            from rollout_primitives import decode_json
            binding = decode_json(raw_binding.encode())
        if args.reader_background and args.command == "checkpoint":
            raise HookError("reader background is not a checkpoint command")
        if args.command == "checkpoint":
            config = _config(args.config, workspace_binding=binding) if binding is not None else _config(args.config)
            result = checkpoint(config, args.session_id, args.turn_id, args.outcome, args.id)
            print(json.dumps(result))
            return 0
        event = _read_event(sys.stdin.buffer)
        if event is None:
            print(json.dumps({}))
            return 0
        config = _config(args.config, workspace_binding=binding) if binding is not None else _config(args.config)
        print(json.dumps(handle_event(event, config, reader_background=args.reader_background)))
        return 0
    except HookInputError as exc:
        warning = _input_warning(str(exc))
        if args.reader_background:
            print(warning["systemMessage"], file=sys.stderr)
        print(json.dumps({} if args.reader_background else warning))
        return 0
    except (HookError, OSError, ValueError, TypeError) as exc:
        if args.command == "checkpoint":
            print(f"Mneme checkpoint failed: {exc}", file=sys.stderr)
            return 1
        if isinstance(exc, ProjectFocusError):
            from recording_contract import MAX_PROJECT_FOCUS_BYTES
            print("Mneme project capture policy refused: check the enrolled project's "
                  f".mneme/hippocampus.md (regular UTF-8 text, at most {MAX_PROJECT_FOCUS_BYTES} bytes; "
                  "no control characters or symlinks). Fix or remove the file, then reload.", file=sys.stderr)
        print(json.dumps({} if args.reader_background else _warning()))
        return 0


if __name__ == "__main__":
    raise SystemExit(main())

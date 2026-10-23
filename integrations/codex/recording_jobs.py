"""Bounded, opt-in source-turn recording lane; never a second worker or transcript archive.

Terminal jobs compact to a monotonic source-start watermark. Unfinished/ambiguous
jobs, not completed turns, consume capacity. Fresh startup can admit its first
already-started open turn; retired resumes baseline existing bytes and cannot
rediscover old completed turns. A provider reservation or write intent is never
replayed after uncertainty. Public observations live only in the worker's RAM.
"""
from __future__ import annotations

from dataclasses import asdict
import hashlib
import itertools
import json
import os
from pathlib import Path
from urllib.parse import quote
import re
import secrets
import stat
import subprocess
import sys
import time

from rollout_primitives import decode_json, digest, encoded, read_regular
import host_files
import host_process
import capture_source
from turn_observer import Limits, validate_delivery_packet

SCHEMA = "mneme.codex-recording.state.v1"
MAX_JOBS = 64
MAX_JOB_BYTES = 32 * 1024
MAX_LEDGERS = 16
MAX_RECEIPTS = 16
MAX_RECEIPT_BYTES = 8192
MAX_RECEIPTS_BYTES = MAX_RECEIPTS * MAX_RECEIPT_BYTES
MAX_UNRESOLVED_DIAGNOSTICS = 16
MAX_UNRESOLVED_DIAGNOSTIC_BYTES = 768
MAX_UNRESOLVED_DIAGNOSTICS_BYTES = 8192
MAX_CONTEXT_DIAGNOSTIC_BYTES = 512
_DELIVERY_DIAGNOSTIC_STATES = frozenset({"not_recorded", "invalid_packet", "not_admitted",
    "ambiguous", "admitted_retained", "admitted_omitted", "unknown"})
# Existing producer dispositions, not model/error text. Unknown codes remain unknown.
UNRESOLVED_REASONS = frozenset({"interrupted_external_intent", "accounting_unavailable",
    "usage_unknown", "native_receipt_unverified", "native_receipt_cap",
    "native_write_ambiguous", "native_maintenance_ambiguous", "worker_failure"})
MAX_STATE = MAX_JOBS * MAX_JOB_BYTES + MAX_RECEIPTS_BYTES + 32 * 1024
CLOSE_SECONDS = 45
FLUSH_SECONDS = 5
MAX_POLLS = 3
MAX_SCAN_WORK = 32 * 1024 * 1024
MAX_EVENT_WORK = 12288
LIFECYCLE_LOCK_SECONDS = .5
COUNTERS = ("admitted", "assessed", "abstained", "verified", "deferred", "cancelled", "unresolved", "capacity")
ID = re.compile(r"[A-Za-z0-9_:.-]{1,160}\Z")
ULID = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z")


class _ActivityTransaction(dict):
    """Ephemeral outbox; never serialized into control state."""
    def __init__(self, data):
        super().__init__(data)
        self.activity = []


def _capture_activity(data, job, outcome, reason, receipt=None):
    """Snapshot only already-retained native evidence; never infer SAVE from verified."""
    try:
        if not isinstance(data, _ActivityTransaction) or job.get("source_host", "codex") != "codex":
            return
        origin = {"turn": job["turn_id"], "generation": None, "context_generation": None,
                  "attempt": job["key"]}
        frozen = job.get("frozen_selection")
        selection = frozen.get("digest") if isinstance(frozen, dict) else None
        selection = selection if isinstance(selection, str) and re.fullmatch(r"[0-9a-f]{64}", selection) else None
        common = {"origin": origin, "selection_digest": selection, "at": int(time.time())}
        event = {"session_id": data["session_id"], "turn_id": job["turn_id"],
                 "cwd": frozen.get("cwd") if isinstance(frozen, dict) else None}
        def add(status, suffix, *, owner=None, nodes=None, why=None):
            data.activity.append((event, "recording", status, {**common, "owner": owner,
                "nodes": nodes or [], "count": len(nodes or []), "reason": why,
                "key": "recording:" + job["key"] + ":" + suffix}))
        native = receipt or job.get("save_outcome", {}).get("native")
        operation = ({"lesson": "save_note_readback_verified", "possibility": "save_note_readback_verified",
                      "episode": "save_episode_append_readback_verified"}.get(job.get("proposal_kind")))
        confirmed = (operation is not None and not _is_nomination_job(job) and isinstance(native, dict)
                     and native.get("readback_status") == "verified"
                     and native.get("db_id") == job.get("db_id")
                     and native.get("db") == job.get("db_alias", "project"))
        owner_id = job.get("db_id")
        owner = ({"name": job.get("destination", "project"), "id": owner_id,
                  "db": job.get("db_alias", "project")}
                 if isinstance(owner_id, str) and ULID.fullmatch(owner_id) else None)
        if confirmed and owner is not None:
            nodes = list(dict.fromkeys(native[k] for k in ("id", "episode_id", "edition_id")
                                       if isinstance(native.get(k), str) and ULID.fullmatch(native[k])))
            add("save_confirmed", "save", owner=owner, nodes=nodes, why=operation)
        for index, item in enumerate(job.get("maintenance", [])):
            if item.get("status") in ("applied", "unchanged") and owner is not None:
                payload = item.get("payload", {})
                expected = payload.get("expected", {}) if isinstance(payload, dict) else {}
                binding = expected.get("notice", {}).get("binding", {}) if isinstance(expected, dict) else {}
                nodes = [endpoint["id"] for endpoint in binding.get("endpoints", [])
                         if isinstance(endpoint, dict) and isinstance(endpoint.get("id"), str)
                         and ULID.fullmatch(endpoint["id"])]
                add("maintenance_verified", "maintenance:" + str(index), owner=owner, nodes=nodes,
                    why="record_finding_" + item["status"])
        # This is a job disposition, never an additional confirmed memory.
        allowed = {"abstained", "deferred", "cancelled", "unresolved", "verified", "unverified"}
        known_reason = reason if reason in UNRESOLVED_REASONS | {
            "native_verified", "receipt_omitted", "shared_budget", "job_deadline", "config_changed",
            "interrupted_or_invalid_fence", "observation_work_cap", "no_available_destination",
            "native_read_budget", "native_read_deadline", "maintenance_refused", "no_frozen_intent",
            "assessment_abstained", "source_turn_not_closed", "source_prompt_pending"} else "other"
        add(outcome if outcome in allowed else "unverified", "terminal:" + outcome,
            why=known_reason)
    except Exception:
        pass


def _emit_activity(config, observations):
    try:
        if capture_source.source_host(config) != "codex":
            return
        from session_activity import emit
        for event, kind, outcome, projection in observations:
            try:
                emit(event, kind, outcome, **projection)
            except Exception:
                pass
    except Exception:
        pass


def _capture_admission(data, job, observation):
    """Observe the normal recorder's packet; selector origin is unavailable."""
    try:
        packet = job.get("delivery")
        frozen = job.get("frozen_selection")
        if (not isinstance(data, _ActivityTransaction) or not isinstance(packet, dict)
                or not isinstance(frozen, dict) or not isinstance(frozen.get("cwd"), str)):
            return  # No existing historical workspace witness means no coverage.
        state = observation["coverage"]["memory_delivery"]
        if state not in _DELIVERY_DIAGNOSTIC_STATES or state == "not_recorded":
            return
        names = {}
        snapshot = frozen.get("snapshot") if frozen.get("schema") == "mneme.native-store-selection.v1" else None
        for row in snapshot.get("active", []) if isinstance(snapshot, dict) else []:
            if not isinstance(row, dict) or not isinstance(row.get("store"), dict):
                continue
            store = row["store"]
            owner = store.get("owner")
            if (not isinstance(store.get("id"), str) or not ULID.fullmatch(store["id"])
                    or not isinstance(owner, dict) or any(not isinstance(value, str)
                        or not re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", value)
                        for value in (row.get("name"), owner.get("db")))):
                continue
            label = (row["name"], owner["db"])
            names[store["id"]] = label if store["id"] not in names else None
        groups = {}
        for card in packet["displayed"]:
            owner_id, node_id = card["db_id"], card["node_id"]
            if (not isinstance(owner_id, str) or not ULID.fullmatch(owner_id)
                    or not isinstance(node_id, str) or not ULID.fullmatch(node_id)):
                continue
            label = names.get(owner_id) or ("unknown", "unknown")
            group = groups.setdefault(owner_id, {"owner": {"name": label[0], "id": owner_id, "db": label[1]},
                                                 "nodes": [], "count": 0, "omitted": 0})
            group["count"] += 1
            if len(group["nodes"]) < 256:
                group["nodes"].append(node_id)
            else:
                group["omitted"] += 1
        event = {"session_id": data["session_id"], "turn_id": job["turn_id"], "cwd": frozen["cwd"]}
        origin = {"turn": job["turn_id"], "generation": None, "context_generation": None,
                  "attempt": "recording:" + job["key"]}
        at = int(time.time())
        for group in groups.values():
            data.activity.append((event, "host_admission", state, {**group, "origin": origin,
                                  "reason": "selector_origin_unavailable", "at": at}))
    except Exception:
        pass


def enabled(config):
    try:
        capture_source.source_host(config)
        capture_source.assessor(config)
    except (ValueError, TypeError):
        return False
    if config.get("recording_mode") != "automatic":
        return False
    if "frozen_selection" in config:
        from registry_recording import selection
        try:
            return bool(selection(config).recordable)
        except (ValueError, OSError, TypeError, KeyError):
            return False
    if config.get("memory_scope", "project") == "project":
        return True
    from target_policy import workshop_policy, misc_policy
    try:
        (misc_policy if config.get("memory_scope") == "misc" else workshop_policy)(config)
        return True
    except (ValueError, OSError, TypeError, KeyError):
        return False


def _directory(config):
    return capture_source.directory(config, "recording")


def _path(config, session):
    return _directory(config) / (digest(session.encode()) + ".json")


def _cancel_path(config, session, turn=None):
    if capture_source.source_host(config) == "claude_code" and turn is not None:
        if not isinstance(turn, str) or not ID.fullmatch(turn):
            raise ValueError("invalid_cancel_turn")
        return _directory(config) / (digest(session.encode()) + "-" + digest(turn.encode()) + ".cancel")
    return _path(config, session).with_suffix(".cancel")


def _read_cancel_marker(path):
    try:
        value = decode_json(read_regular(path, 256))
        if (not isinstance(value, dict) or set(value) != {"token", "turn_id"}
                or not isinstance(value["token"], str) or not re.fullmatch(r"[0-9a-f]{64}", value["token"])
                or (value["turn_id"] is not None and (not isinstance(value["turn_id"], str)
                    or not ID.fullmatch(value["turn_id"])))):
            return None
        return value
    except FileNotFoundError:
        return {"token": "", "turn_id": None}
    except (OSError, ValueError, TypeError, UnicodeError):
        return None


def _cancel_marker(config, session, turn=None):
    global_marker = _read_cancel_marker(_cancel_path(config, session))
    if capture_source.source_host(config) == "codex" or turn is None or global_marker is None:
        return global_marker
    targeted = _read_cancel_marker(_cancel_path(config, session, turn))
    if targeted is None or targeted["turn_id"] not in (None, turn):
        return None
    # Each turn retains its own out-of-lock fence; cancelling p1 cannot clear
    # or rotate p2's fence, even if their lifecycle lock acquisitions time out.
    return {"token": digest(encoded([global_marker["token"], targeted["token"]])),
            "turn_id": targeted["turn_id"]}


def _clean_turn_cancels(config, session, data):
    if capture_source.source_host(config) != "claude_code":
        return
    prefix = digest(session.encode()) + "-"
    active = {_cancel_path(config, session, job["turn_id"]).name for job in data["jobs"]}
    cap = MAX_LEDGERS * (MAX_JOBS + 3) + 2
    for candidate in itertools.islice(_directory(config).iterdir(), cap):
        if (candidate.name.startswith(prefix) and re.fullmatch(r"[a-f0-9]{64}-[a-f0-9]{64}\.cancel", candidate.name)
                and candidate.name not in active):
            candidate.unlink(missing_ok=True)


def _signal_cancel(config, session, turn):
    """Fence outside the optional queue lock; contention must not lose Interrupt."""
    path = _cancel_path(config, session, turn)
    # No admitted work exists without its ledger; avoid lifetime orphan markers.
    if not _path(config, session).exists():
        return
    if capture_source.source_host(config) == "claude_code" and turn is not None:
        data = _load(_path(config, session), session, config)
        if not any(job["turn_id"] == turn for job in data["jobs"]):
            return  # A stale StopFailure cannot mint an orphan or affect another turn.
    host_files.atomic_write(path, encoded({"token": secrets.token_hex(32), "turn_id": turn}))


def _cancelled(config, session, job):
    marker = _cancel_marker(config, session, job["turn_id"])
    return marker is None or "cancel_token" not in job or job["cancel_token"] != marker["token"]


def load_project_focus(config):
    """Read only the enrolled recording project's optional policy; never discover."""
    if (config.get("memory_scope", "project") != "project"
            or config.get("recording_mode") != "automatic"
            or config.get("memory_mode", config.get("recall_mode")) != "async"):
        return None
    from recording_contract import MAX_PROJECT_FOCUS_BYTES, validate_project_focus
    path = Path(config["project_root"]) / ".mneme" / "hippocampus.md"
    directory = fd = None
    try:
        try:
            directory = host_files.open_directory(path.parent)
        except FileNotFoundError:
            return None
        try:
            if host_files.WINDOWS:
                # Windows pins every ancestor and rejects reparse points while
                # opening; Unix retains the exact directory-relative identity.
                fd = host_files.open_regular(path)
            else:
                fd = os.open(path.name, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW, dir_fd=directory)
        except FileNotFoundError:
            return None
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode):
            raise ValueError("policy must be a regular file")
        with os.fdopen(fd, "rb", closefd=False) as stream:
            raw = stream.read(MAX_PROJECT_FOCUS_BYTES + 1)
        if len(raw) > MAX_PROJECT_FOCUS_BYTES:
            raise ValueError(f"policy exceeds {MAX_PROJECT_FOCUS_BYTES} UTF-8 bytes; shorten it")
        try:
            return validate_project_focus(raw.decode("utf-8"))
        except (ValueError, UnicodeError) as error:
            raise ValueError("policy must be UTF-8 text without control characters") from error
    except (OSError, ValueError) as error:
        raise ValueError(f"project capture policy {path}: {error}; fix or remove this file, then reload") from error
    finally:
        if fd is not None:
            os.close(fd)
        if directory is not None:
            os.close(directory)



def project_focus_witness(root):
    """Reuse the existing hippocampus file reader for an actual project owner.

    Background callers bind this local instruction file separately from owner
    purpose/tag guidance. A missing file is also frozen; adding one changes the
    capture instructions rather than silently blessing an older assessment.
    """
    root = Path(root)
    if not root.is_absolute():
        raise ValueError("project capture root must be absolute")
    text = load_project_focus({"memory_scope": "project", "recording_mode": "automatic",
                               "memory_mode": "async", "project_root": root})
    return text, {"root": str(root), "sha256": digest(encoded({"present": text is not None,
                                                              "content": text}))}


def revalidate_project_focus(store, job):
    """Exact local-file stale-intent check; not an atomic native SAVE guard."""
    frozen = job.get("project_focus_witness")
    if store.workspace_root is None:
        if frozen is not None:
            raise ValueError("project capture policy scope changed")
        return
    if (not isinstance(frozen, dict) or set(frozen) != {"root", "sha256"}
            or frozen["root"] != store.workspace_root):
        raise ValueError("project capture policy binding changed; reload before work")
    _, current = project_focus_witness(store.workspace_root)
    if current != frozen:
        raise ValueError("project capture policy changed; reload before work")


def _config_digest(config):
    host = capture_source.source_host(config)
    assessor = capture_source.assessor(config)
    if "frozen_selection" in config:
        from registry_recording import config_digest
        value = config_digest(config)
        return value if host == "codex" else digest(encoded([host, assessor, value]))
    raw = read_regular(Path(config["service_config"]), 8192)
    if len(raw) > 8192:
        raise ValueError("service_config_cap")
    hook_sha = None
    if config.get("_config_path") is not None:
        hook_raw = read_regular(Path(config["_config_path"]), 8192)
        if len(hook_raw) > 8192:
            raise ValueError("hook_config_cap")
        hook_sha = digest(hook_raw)
    binding = {"project_root": str(config["project_root"]),
                           "service_config": str(config["service_config"]),
                           "service_sha256": digest(raw), "hook_sha256": hook_sha,
                           "recording_mode": config.get("recording_mode"),
                           "memory_scope": config.get("memory_scope", "project"),
                           "tag_stewardship": config.get("tag_stewardship", False),
                           "tag_guide_id": config.get("tag_guide_id")}
    if config.get("core_cleanup") is False:
        binding["core_cleanup"] = False
    if (config.get("memory_scope", "project") == "project"
            and config.get("recording_mode") == "automatic"
            and config.get("memory_mode", config.get("recall_mode")) == "async"):
        focus = load_project_focus(config)
        if "_project_focus" in config and config["_project_focus"] != focus:
            raise ValueError("project capture policy changed; reload before work")
        binding["project_focus_sha256"] = digest(encoded({"present": focus is not None, "content": focus}))
    if config.get("memory_scope") == "workshop":
        from target_policy import workshop_policy
        binding["store_target"] = workshop_policy(config).canonical()
    if config.get("memory_scope") == "misc":
        from target_policy import misc_policy
        binding["store_target"] = misc_policy(config).canonical()
        binding["workspace_binding"] = config["workspace_binding"]
    if "global_preferences" in config:
        from target_policy import global_preferences_policy
        preference = global_preferences_policy(config)
        preference_raw = read_regular(Path(preference.service_config), 8192)
        if len(preference_raw) > 8192:
            raise ValueError("service_config_cap")
        binding["global_preferences"] = preference.canonical()
        binding["global_service_sha256"] = digest(preference_raw)
    if host != "codex":
        binding.update(source_host=host, assessor=assessor)
    return digest(encoded(binding))


def _misc_provenance(payload, origin):
    """Keep the exact workspace in authored body and a bounded native source ref."""
    reference = payload["source"]["reference"] + "?scope=misc&workspace=" + quote(origin, safe="")
    if len(reference.encode()) > 2048:  # Native CaptureSource reference envelope.
        reference = payload["source"]["reference"] + "?scope=misc&workspace_sha256=" + digest(origin.encode())
    payload["source"]["reference"] = reference
    payload["body"] = "Workspace: " + origin + "\n\n" + payload["body"]


def _misc_routing_provenance(routing, origin, *, db_id):
    """Preserve the routing codec and exact source URI; provenance is ordinary note text."""
    try:
        from routing_memory import validate_witness, encoded as encode_witness
        witness = validate_witness(decode_json(routing["body"].encode()), expected_db_id=db_id)
        witness["note"] = "Workspace: " + origin + "\n\n" + witness["note"]
        checked = validate_witness(witness, expected_db_id=db_id)
        return {**routing, "body": encode_witness(checked).decode()}, None
    except ValueError as error:
        reason = ("routing_workspace_provenance_cap" if str(error) in ("witness_note", "witness_bytes")
                  else "routing_workspace_provenance_refused")
        return None, reason
    except (ImportError, TypeError, KeyError, AttributeError, UnicodeError, RecursionError):
        return None, "routing_workspace_provenance_refused"


def _unchanged(config, job):
    """Recheck live eligibility as well as exact config and frozen origin."""
    try:
        if _config_digest(config) != job["config_sha256"]:
            return False
        if "frozen_selection" in config:
            return encoded(job.get("frozen_selection")) == encoded(config["frozen_selection"])
        if config.get("memory_scope") == "misc":
            from target_policy import misc_policy
            policy = misc_policy(config)
            return (job.get("workspace_binding") == config["workspace_binding"]
                    and job.get("store_target") == policy.canonical()
                    and job.get("db_alias") == policy.db_alias)
        return True
    except (OSError, ValueError, TypeError, KeyError):
        return False


def _valid(data, session, config=None):
    expected = SCHEMA if config is None else capture_source.schema(config, SCHEMA, "recording")
    host = "codex" if config is None else capture_source.source_host(config)
    return (isinstance(data, dict) and data.get("schema") == expected and data.get("session_id") == session
            and isinstance(data.get("jobs"), list) and len(data["jobs"]) <= MAX_JOBS
            and all(isinstance(j, dict) and len(encoded(j)) <= MAX_JOB_BYTES
                    and j.get("source_host", "codex") == host
                    and j.get("assessor", "codex") in capture_source.HOSTS for j in data["jobs"])
            and (host == "codex" or "watermark_exclusive" not in data
                 or type(data["watermark_exclusive"]) is bool
                 and isinstance(data.get("baseline_turn_id"), str) and ID.fullmatch(data["baseline_turn_id"]))
            and type(data.get("watermark")) is int and data["watermark"] >= -1
            and type(data.get("fresh")) is bool and type(data.get("ended")) is bool
            and isinstance(data.get("counts"), dict) and set(data["counts"]) == set(COUNTERS)
            and all(type(v) is int and 0 <= v <= 1_000_000 for v in data["counts"].values())
            and isinstance(data.get("receipts"), list) and len(data["receipts"]) <= MAX_RECEIPTS)


def _load(path, session, config=None):
    raw = read_regular(path, MAX_STATE)
    if len(raw) > MAX_STATE:
        raise ValueError("recording_state_cap")
    data = decode_json(raw)
    if not _valid(data, session, config):
        raise ValueError("invalid_recording_state")
    return data


def _write(path, data):
    # Optional delivery/diagnostics cannot make an otherwise persistable state
    # fail after phase/reason/receipt growth. Enforce this at the final projection,
    # not merely when the earlier assessment result was attached.
    for row in data["jobs"] + data["receipts"]:
        _normalize_context_in_place(row)
    for row in data["receipts"]:
        _normalize_context_in_place(row, max_bytes=MAX_RECEIPT_BYTES)
    for job in data["jobs"]:
        if "delivery" in job and len(encoded(job)) > MAX_JOB_BYTES:
            job.pop("delivery")
        _normalize_context_in_place(job, max_bytes=MAX_JOB_BYTES)
        if "assessment_diagnostic" in job:
            job["assessment_diagnostic"] = sanitize_assessment_diagnostic(job["assessment_diagnostic"])
            if len(encoded(job)) > MAX_JOB_BYTES:
                job.pop("assessment_diagnostic")
    raw = encoded(data)
    if len(raw) > MAX_STATE:
        for row in data["jobs"] + data["receipts"]:
            _normalize_context_in_place(row, force_omit=True)
        raw = encoded(data)
    if len(raw) > MAX_STATE:
        for row in data["jobs"] + data["receipts"]:
            row.pop("context_diagnostic_omitted", None)
        raw = encoded(data)
    if len(raw) > MAX_STATE:
        for job in data["jobs"]:
            job.pop("delivery", None)
        for row in data["jobs"] + data["receipts"]:
            row.pop("assessment_diagnostic", None)
        raw = encoded(data)
    if len(raw) > MAX_STATE or any(len(encoded(j)) > MAX_JOB_BYTES for j in data["jobs"]):
        raise ValueError("recording_state_cap")
    host_files.atomic_write(path, raw)


def _new(session, fresh=False, config=None):
    return {"schema": SCHEMA if config is None else capture_source.schema(config, SCHEMA, "recording"), "session_id": session, "pin": None, "watermark": -1,
            "fresh": fresh, "ended": False, "jobs": [], "receipts": [],
            "counts": {key: 0 for key in COUNTERS}, "usage_unknown": False}


def _bind_workspace(config, data):
    """Pin a misc session's origin independently of mutable policy/config digests."""
    binding = config.get("workspace_binding") if config.get("memory_scope") == "misc" else None
    if "workspace_binding" in data:
        if data["workspace_binding"] != binding:
            raise ValueError("session_workspace_changed")
        return False
    if binding is None:
        return False
    # A paid/unresolved pre-binding state must not acquire a new workspace identity.
    if data.get("config_pin") is not None or data.get("pin") is not None or data.get("jobs"):
        raise ValueError("session_workspace_missing")
    from misc_binding import validate_binding
    checked = validate_binding(binding, excluded_roots=config.get("excluded_roots", []))
    data["workspace_binding"] = dict(checked)
    return True


def _transaction(config, session, mutate, *, create=False, fresh=False, lock_timeout=0):
    """One directory lock; lifecycle callers may wait briefly for contention."""
    # Revoking a registry destination stops admission/writes, not accounting for
    # work already paid for. Otherwise a config edit after inference strands a
    # known result as an apparent interrupted intent and loses its cancellation.
    retained_registry = "frozen_selection" in config and not create
    if ((not retained_registry and not enabled(config))
            or not isinstance(session, str) or not ID.fullmatch(session)):
        return False, None
    fd = None
    observations = []
    try:
        directory = _directory(config)
        if retained_registry and not _path(config, session).exists():
            return True, None
        host_files.ensure_state_directory(directory)
        fd = host_files.open_regular(directory / ".lock", os.O_CREAT | os.O_RDWR)
        if lock_timeout > 0:
            host_files.lock_file_bounded(fd, timeout=lock_timeout)
        else:
            host_files.lock_file(fd, blocking=False)
        path = _path(config, session)
        if path.exists():
            data = _load(path, session, config)
        elif create:
            scan_cap = 130 if capture_source.source_host(config) == "codex" else MAX_LEDGERS * (MAX_JOBS + 3) + 2
            entries = list(itertools.islice(directory.iterdir(), scan_cap))
            scan_full = len(entries) >= scan_cap
            # Cancellation can race clean retirement. Remove only orphan markers,
            # never a fence belonging to outstanding or ambiguous work.
            for candidate in list(entries):
                orphan = (re.fullmatch(r"[a-f0-9]{64}\.cancel", candidate.name)
                          and not candidate.with_suffix(".json").exists())
                if capture_source.source_host(config) == "claude_code" and re.fullmatch(r"[a-f0-9]{64}-[a-f0-9]{64}\.cancel", candidate.name):
                    orphan = not (directory / (candidate.name[:64] + ".json")).exists()
                if orphan:
                    candidate.unlink(); entries.remove(candidate)
            if scan_full:
                return False, None
            ledgers = [p for p in entries if re.fullmatch(r"[a-f0-9]{64}\.json", p.name)]
            if len(ledgers) >= MAX_LEDGERS:
                for candidate in ledgers:
                    try:
                        value = decode_json(read_regular(candidate, MAX_STATE))
                        if (_valid(value, value.get("session_id"), config) and value["ended"] and not value["jobs"]):
                            _clean_turn_cancels(config, value["session_id"], value)
                            candidate.unlink(); candidate.with_suffix(".cancel").unlink(missing_ok=True)
                            ledgers.remove(candidate)
                            break
                    except (OSError, ValueError, TypeError):
                        continue
                if len(ledgers) >= MAX_LEDGERS:
                    return False, None
            data = _new(session, fresh, config)
        else:
            return True, None
        data = _ActivityTransaction(data)
        bound = _bind_workspace(config, data)
        value, changed = mutate(data)
        if changed or bound:
            _write(path, data)
            _clean_turn_cancels(config, session, data)
            observations = data.activity
        return True, value
    except (OSError, ValueError, TypeError, KeyError, UnicodeError, RecursionError):
        return False, None
    finally:
        if fd is not None:
            os.close(fd)
        # Never acquire the diagnostics lock while holding a recording lock.
        _emit_activity(config, observations)


def _count(data, name):
    data["counts"][name] = min(1_000_000, data["counts"][name] + 1)


def _boot_id():
    """Local OS boot witness; missing witness means no renewable deadline."""
    try:
        if host_files.WINDOWS:
            # An exact same-boot witness, never wall-clock-minus-uptime. Missing
            # CIM access leaves closed jobs nonrenewable rather than replayable.
            return host_process.boot_identity()
        if sys.platform == "linux":
            raw = read_regular(Path("/proc/sys/kernel/random/boot_id"), 128)
        elif sys.platform == "darwin":
            raw = subprocess.run(["/usr/sbin/sysctl", "-n", "kern.boottime"],
                                 capture_output=True, timeout=.25, check=True).stdout
        else:
            return None
        return digest(raw) if 0 < len(raw) <= 128 else None
    except (OSError, ValueError, subprocess.SubprocessError):
        return None


def _remaining(job):
    if not job.get("boot_id") or _boot_id() != job["boot_id"]:
        return 0
    now, monotonic = time.time(), time.monotonic()
    if now < job["closed_at"] or monotonic < job["closed_monotonic"]:
        return 0
    return min(job["deadline"] - now, job["monotonic_deadline"] - monotonic)


def sanitize_assessment_diagnostic(value):
    """Pure optional metadata projection; malformed diagnostics never change policy."""
    from assessment import assessment_diagnostic
    if (type(value) is not dict
            or set(value) != {"runtime_reason", "validation_reason"}):
        return assessment_diagnostic(None)
    return assessment_diagnostic({"reason": value["runtime_reason"],
                                "validation_reason": value["validation_reason"]})


def sanitize_context_diagnostic(value):
    """Counts describe final prepared bindings, never receipt, sufficiency or usefulness."""
    unknown = {"observer_memory_delivery": "unknown", "prompt_memory_delivery": "unknown",
               "shown_semantic_targets": None, "shown_routing_targets": None, "routing_enabled": None}
    if type(value) is not dict or set(value) != set(unknown):
        return unknown
    if (any(type(value[key]) is not str or value[key] not in _DELIVERY_DIAGNOSTIC_STATES
            for key in ("observer_memory_delivery", "prompt_memory_delivery"))
            or any(value[key] is not None and (type(value[key]) is not int or not 0 <= value[key] <= 4096)
                   for key in ("shown_semantic_targets", "shown_routing_targets"))
            or (value["routing_enabled"] is not None and type(value["routing_enabled"]) is not bool)):
        return unknown
    return dict(value)


def normalize_context_diagnostic_fields(row, *, max_bytes=None, force_omit=False):
    """Copy only optional private fields; old absence stays unknown, core stays intact.

    Byte pressure removes this diagnostic/its omission marker before core. The
    caller still decides whether an oversized *core* is valid; this never repairs it.
    """
    if not isinstance(row, dict):
        return row
    result = dict(row)
    fields = {"context_diagnostic", "context_diagnostic_omitted"}
    if not fields.intersection(result):
        return result
    if "context_diagnostic" in result:
        original = result["context_diagnostic"]
        result["context_diagnostic"] = sanitize_context_diagnostic(original)
        if original != result["context_diagnostic"]:
            result["context_diagnostic_omitted"] = True
    if "context_diagnostic_omitted" in result:
        if result["context_diagnostic_omitted"] is False:
            result.pop("context_diagnostic_omitted")
        else:
            result["context_diagnostic_omitted"] = True
    if force_omit or (max_bytes is not None and len(encoded(result)) > max_bytes):
        result.pop("context_diagnostic", None)
        result["context_diagnostic_omitted"] = True
        if max_bytes is not None and len(encoded(result)) > max_bytes:
            result.pop("context_diagnostic_omitted", None)
    return result


def _normalize_context_in_place(row, **kwargs):
    value = normalize_context_diagnostic_fields(row, **kwargs)
    row.clear()
    row.update(value)


def _context_diagnostic(observation, context=None):
    value = sanitize_context_diagnostic(None)
    try:
        observer = observation["coverage"]["memory_delivery"]
        if type(observer) is str and observer in _DELIVERY_DIAGNOSTIC_STATES:
            value["observer_memory_delivery"] = observer
        if context is not None:
            from recording_contract import ValidationContext
            if isinstance(context, ValidationContext):
                value.update(prompt_memory_delivery=json.loads(context.coverage_json)["memory_delivery"],
                    shown_semantic_targets=sum(target.origin == "delivery" and target.kind == "semantic"
                                               for target in context.association_bindings),
                    shown_routing_targets=sum(target.origin == "delivery" and target.routing_binding_json is not None
                                              for target in context.association_bindings),
                    routing_enabled=context.routing_enabled)
    except (ValueError, TypeError, KeyError, AttributeError, RecursionError):
        pass
    return sanitize_context_diagnostic(value)


def sanitize_unresolved_diagnostic(value):
    """Optional static private metadata; never a raw job or cost reconstruction."""
    required = {"session_id", "source_key", "phase", "reason", "ledger_usage_unknown"}
    if (type(value) is not dict or not required <= set(value) <= required | {
            "assessment_diagnostic", "context_diagnostic", "context_diagnostic_omitted"}
            or type(value["session_id"]) is not str or not ID.fullmatch(value["session_id"])
            or type(value["source_key"]) is not str or not re.fullmatch(r"[0-9a-f]{64}", value["source_key"])
            or value["phase"] != "unresolved" or type(value["ledger_usage_unknown"]) is not bool):
        return None
    reason = value["reason"]
    result = {key: value[key] for key in required}
    result["reason"] = reason if type(reason) is str and reason in UNRESOLVED_REASONS else "unknown"
    if "assessment_diagnostic" in value:
        result["assessment_diagnostic"] = sanitize_assessment_diagnostic(value["assessment_diagnostic"])
    for key in ("context_diagnostic", "context_diagnostic_omitted"):
        if key in value:
            result[key] = value[key]
    result = normalize_context_diagnostic_fields(result, max_bytes=MAX_UNRESOLVED_DIAGNOSTIC_BYTES)
    if len(encoded(result)) > MAX_UNRESOLVED_DIAGNOSTIC_BYTES:
        result.pop("assessment_diagnostic", None)
    return result if len(encoded(result)) <= MAX_UNRESOLVED_DIAGNOSTIC_BYTES else None


def normalize_unresolved_export(value, rows, *, omitted=False, sessions=None):
    """Optional evidence loses room before receipts; absence also means unknown.

    Shared by the producer and private adapter. Never changes core receipts,
    unknown/truncated status, the ledger, accounting, or admission.
    """
    result = {k: v for k, v in value.items() if k not in (
        "unresolved_diagnostics", "unresolved_diagnostics_omitted")}
    retained = []
    omitted = omitted is not False
    if type(rows) is not list:
        rows, omitted = [], True
    if len(rows) > MAX_UNRESOLVED_DIAGNOSTICS:
        omitted = True
    for row in rows[:MAX_UNRESOLVED_DIAGNOSTICS]:
        item = sanitize_unresolved_diagnostic(row)
        if item is None or (sessions is not None and item["session_id"] not in sessions):
            omitted = True
            continue
        if (item["reason"] == "unknown" or "assessment_diagnostic" not in item
                or type(row.get("assessment_diagnostic")) is not dict
                or row["assessment_diagnostic"] != item["assessment_diagnostic"]):
            omitted = True  # Missing/normalized detail is unknown, never a clean bill of health.
        if item.get("context_diagnostic_omitted") is True or (
                "context_diagnostic" in row and "context_diagnostic" not in item):
            omitted = True
        if len(encoded(retained + [item])) > MAX_UNRESOLVED_DIAGNOSTICS_BYTES:
            omitted = True
            for prior in retained:
                _normalize_context_in_place(prior, force_omit=True)
                prior.pop("context_diagnostic_omitted", None)
            item = normalize_context_diagnostic_fields(item, force_omit=True)
            item.pop("context_diagnostic_omitted", None)
        if len(encoded(retained + [item])) > MAX_UNRESOLVED_DIAGNOSTICS_BYTES:
            omitted = True
            continue
        retained.append(item)
    result.update(unresolved_diagnostics=retained, unresolved_diagnostics_omitted=omitted)
    if len(encoded(result)) > MAX_RECEIPTS_BYTES:
        for item in retained:
            _normalize_context_in_place(item, force_omit=True)
            item.pop("context_diagnostic_omitted", None)
        result["unresolved_diagnostics_omitted"] = True
        if isinstance(result.get("receipts"), list):
            result["receipts"] = [normalize_context_diagnostic_fields(row, force_omit=True)
                                  for row in result["receipts"]]
    if len(encoded(result)) > MAX_RECEIPTS_BYTES:
        for row in result.get("receipts", []):
            if isinstance(row, dict):
                row.pop("context_diagnostic_omitted", None)
    if len(encoded(result)) > MAX_RECEIPTS_BYTES:
        result.update(unresolved_diagnostics=[], unresolved_diagnostics_omitted=True)
        if len(encoded(result)) > MAX_RECEIPTS_BYTES:
            result.pop("unresolved_diagnostics")
            result.pop("unresolved_diagnostics_omitted")
    return result


def _maintenance_summary(item):
    summary = {key:item[key] for key in ("target","status","reason","native_result_sha256","details_omitted") if key in item}
    if "payload" in item:
        summary["key"] = item["payload"]["expected"]["notice"]["binding"]["key"]
    if "native" in item:
        summary["native_result_sha256"] = digest(encoded(item["native"]))
        summary["details_omitted"] = True  # Native row stays private in bounded ledger.
    return summary


def _is_nomination_job(job):
    """A nomination marker can never be interpreted as ordinary write authority."""
    return ("nomination_envelope" in job or "nomination_outcome" in job
            or job.get("phase") == "nomination_frozen"
            or job.get("proposal_kind") in ("touchstone_nomination", "touchstone", "review_nomination"))


def _nomination_summary(value):
    """Outbox receipts prove local handoff only; never export candidate prose."""
    allowed = {"queued", "replay", "expired", "capacity", "refused", "unavailable"}
    if not isinstance(value, dict) or value.get("outcome") not in allowed:
        return {"outcome": "unavailable"}
    result = {"outcome": value["outcome"]}
    if isinstance(value.get("id"), str) and re.fullmatch(r"[0-9a-f]{64}", value["id"]):
        result["id"] = value["id"]
    if value.get("status") in ("pending", "emitted", "suppressed"):
        result["status"] = value["status"]
    if result["outcome"] in ("queued", "replay") and ("id" not in result or "status" not in result):
        return {"outcome": "unavailable"}
    return result


def _partial_receipt(job):
    if _is_nomination_job(job):
        return {"source_key": job["key"], "turn_id": job["turn_id"], "outcome": "unresolved",
                "reason": job.get("reason", "interrupted_external_intent"),
                "nomination": _nomination_summary(job.get("nomination_outcome"))}
    summary = {"source_key":job["key"],"turn_id":job["turn_id"],"outcome":"unresolved","reason":job.get("reason","interrupted_external_intent"),
               "maintenance":[_maintenance_summary(item) for item in job["maintenance"]]}
    if "save_outcome" in job:
        summary["save_outcome"] = job["save_outcome"]
    if "maintenance_omitted_count" in job:
        summary["maintenance_omitted_count"] = job["maintenance_omitted_count"]
    if "intent_omissions" in job:
        summary["intent_omissions"] = job["intent_omissions"]
    return summary


def _finish(data, job, outcome, reason, receipt=None, diagnostic=None, context_diagnostic=None):
    job.pop("delivery", None)  # Raw hook text never enters terminal receipts.
    if context_diagnostic is not None:
        job["context_diagnostic"] = context_diagnostic
        _normalize_context_in_place(job, max_bytes=MAX_JOB_BYTES)
    if diagnostic is not None:
        value = sanitize_assessment_diagnostic(diagnostic)
        projected = normalize_context_diagnostic_fields({**job, "assessment_diagnostic": value},
                                                       max_bytes=MAX_JOB_BYTES)
        for key in ("context_diagnostic", "context_diagnostic_omitted"):
            job.pop(key, None)
            if key in projected:
                job[key] = projected[key]
        if len(encoded({**job, "assessment_diagnostic": value})) <= MAX_JOB_BYTES:
            job["assessment_diagnostic"] = value
    _count(data, outcome)
    if outcome == "unresolved":
        job.update(phase="unresolved", reason=reason)
        if receipt and job.get("save_outcome",{}).get("native") != receipt:
            job["receipt"] = receipt
        if len(encoded(job)) > MAX_JOB_BYTES:
            job.pop("observation", None)
        _normalize_context_in_place(job, max_bytes=MAX_JOB_BYTES)
        _capture_activity(data, job, outcome, reason, receipt)
        return
    data["jobs"].remove(job)
    summary = {"source_key": job["key"], "turn_id": job["turn_id"], "outcome": outcome, "reason": reason}
    admission = job["admission"]
    summary["source"] = {"session_id": admission["session_id"], "turn_id": job["turn_id"],
                         "prompt_sha256": admission["expected_prompt_sha256"],
                         "start_ref": job.get("source_start_ref", admission.get("start_record"))}
    if job.get("source_host", "codex") != "codex":
        summary.update(source_host=job["source_host"], assessor=job["assessor"])
        summary["source"].update(source_host=job["source_host"], start_offset=admission["start_offset"])
    if "closure_ref" in job:
        summary["closure_ref"] = job["closure_ref"]
    if "source_snapshot" in job:
        # Mandatory provenance, unlike optional selection diagnostics. A verified
        # native write never upgrades this read horizon into source finality.
        summary["source_snapshot"] = job["source_snapshot"]
    if "destination" in job:
        summary["destination"] = job["destination"]
        if "frozen_selection" in job:
            from registry_recording import receipt_target
            summary["target"] = receipt_target(job)
        else:
            summary["target"] = job.get("store_target", {
                "scope": job["destination"], "db_alias": job.get("db_alias", "project"), "db_id": job["db_id"]})
    nomination = _is_nomination_job(job)
    if nomination:
        summary["nomination"] = _nomination_summary(job.get("nomination_outcome"))
    if "payload" in job and not nomination:
        summary["proposal"] = {"kind": job["proposal_kind"], "summary": job["payload"]["summary"],
                               "body": job.get("note_body", job["payload"]["body"])}
        summary["citations"] = job["citations"]
        if "tag_context" in job:
            summary["proposal"]["tags"] = job["payload"].get("tags", [])
            summary["tag_context"] = job["tag_context"]
        elif "frozen_selection" in job:
            summary["proposal"]["tags"] = job["payload"].get("tags", [])
    if "save_outcome" in job and not nomination:
        summary["save_outcome"] = {k:v for k,v in job["save_outcome"].items() if k != "native"}
    if "maintenance" in job and not nomination:
        summary["maintenance"] = [_maintenance_summary(item) for item in job["maintenance"]]
    if "intent_omissions" in job:
        summary["intent_omissions"] = job["intent_omissions"]
    if "project_focus_witness" in job:
        summary["project_focus"] = job["project_focus_witness"]
    if "guidance_snapshot" in job:
        summary["guidance"] = guidance_receipt(job["guidance_snapshot"])
    if "store_context" in job:
        summary["store_context"] = job["store_context"]
    if "maintenance_omitted_count" in job:
        summary["maintenance_omitted_count"] = job["maintenance_omitted_count"]
    if "association" in job and not nomination:
        summary["association"] = job["association"]
    if "routing" in job and not nomination:
        summary["routing"] = job["routing"]
    if "observation" in job:
        summary["observation"] = job["observation"]
    for key in ("context_diagnostic", "context_diagnostic_omitted"):
        if key in job:
            summary[key] = job[key]
    if receipt and not nomination:
        summary["native"] = receipt
    _normalize_context_in_place(summary, max_bytes=MAX_RECEIPT_BYTES - 192)
    if "observation" in summary and len(encoded(summary)) > MAX_RECEIPT_BYTES - 192:
        summary.pop("observation")  # Optional coverage never erases source/native proof.
    if "association" in summary and len(encoded(summary)) > MAX_RECEIPT_BYTES - 192:
        summary.pop("association")  # Optional metadata never erases source/native proof.
    if "routing" in summary and len(encoded(summary)) > MAX_RECEIPT_BYTES - 192:
        summary.pop("routing")
    if "assessment_diagnostic" in job:
        summary["assessment_diagnostic"] = sanitize_assessment_diagnostic(job["assessment_diagnostic"])
        _normalize_context_in_place(summary, max_bytes=MAX_RECEIPT_BYTES - 192)
        if len(encoded(summary)) > MAX_RECEIPT_BYTES - 192:
            summary.pop("assessment_diagnostic")  # Optional absence means unknown, not lost core proof.
    _normalize_context_in_place(summary, max_bytes=MAX_RECEIPT_BYTES - 192)
    if "maintenance" in summary and len(encoded(summary)) > MAX_RECEIPT_BYTES - 192:
        summary["maintenance"] = [{"target":item["target"], "status":item["status"],
                                   **({"reason":item["reason"]} if "reason" in item else {})}
                                  for item in summary["maintenance"]]
        summary["maintenance_details_omitted"] = True
    if len(encoded(summary)) > MAX_RECEIPT_BYTES - 192:
        data["receipt_omissions"] = data.get("receipt_omissions", 0) + 1
        _capture_activity(data, {k: v for k, v in job.items() if k not in {"save_outcome", "maintenance"}},
                          "unverified", "receipt_omitted")
        return
    if len(data["receipts"]) >= MAX_RECEIPTS:
        data["receipt_omissions"] = data.get("receipt_omissions", 0) + 1
    data["receipts"] = (data["receipts"] + [summary])[-MAX_RECEIPTS:]
    _capture_activity(data, job, outcome, reason, receipt)


def session_start(config, event):
    """Remember trusted lifecycle meaning, not a claim that EOF precedes task_start."""
    source = event.get("source")
    if source not in ("startup", "resume", "clear", "compact"):
        return
    def start(data):
        # Existing state never receives another fresh-first-turn exception.
        data["ended"] = False
        return None, True
    _transaction(config, event.get("session_id"), start, create=True, fresh=source == "startup",
                 lock_timeout=LIFECYCLE_LOCK_SECONDS)


def notice(config, event):
    """Foreground: local file/descriptor work only. Never native/provider calls."""
    # Supporting callers must preserve the same root-turn boundary as hooks.
    # Children can share session_id; never inspect/admit their source or state.
    try:
        source_host = capture_source.source_host(config)
    except (ValueError, TypeError):
        return {"outcome": "skipped"}
    if event.get("agent_id") or source_host == "codex" and event.get("agent_type"):
        return {"outcome": "skipped"}
    if not enabled(config):
        return {"outcome": "skipped"}
    if "frozen_selection" in config:
        from registry_recording import selection
        try:
            current = selection(config)
            if event.get("cwd") is not None and Path(event["cwd"]).resolve() != Path(current.cwd).resolve():
                return {"outcome": "skipped"}
        except (ValueError, TypeError, OSError):
            return {"outcome": "skipped"}
    if config.get("memory_scope") in ("workshop", "misc"):
        from target_policy import workshop_policy, misc_policy
        try:
            (misc_policy if config.get("memory_scope") == "misc" else workshop_policy)(config).validate_workspace(event.get("cwd"))
        except (ValueError, TypeError, OSError):
            return {"outcome": "skipped"}
    session, turn, prompt = event.get("session_id"), event.get("turn_id"), event.get("prompt")
    if (not isinstance(session, str) or not ID.fullmatch(session)
            or not isinstance(turn, str) or not ID.fullmatch(turn) or not isinstance(prompt, str)
            or prompt.startswith("MNEME_CHECKPOINT_CONTINUATION") or len(prompt.encode()) > 8192):
        return {"outcome": "skipped"}
    try:
        marker = _cancel_marker(config, session, turn)
        if marker is None or marker["turn_id"] == turn:
            return {"outcome": "deferred", "reason": "cancelled_or_invalid_fence"}
        host = capture_source.source_host(config)
        capture_source.assessor(config)
        result = capture_source.admit(config, event)
        if result["status"] != "admitted":
            return {"outcome": "deferred", "reason": result["reason"]}
        admission = result["admission"]
        current = capture_source.observe(config, admission, limits=Limits(scan_bytes=256 * 1024, events=512))
        open_reasons = {"source_turn_not_closed"} | ({"source_prompt_pending"} if host == "claude_code" else set())
        if current["reason"] not in open_reasons:
            return {"outcome": "deferred", "reason": "not_current_open_turn"}
        snapshot_end = capture_source.snapshot_end(config, admission, current)
        start_offset = capture_source.start_offset(config, admission)
        binding = _config_digest(config)
        key = digest(encoded(([session, turn, admission.expected_prompt_sha256] if host == "codex"
                              else [host, session, turn, admission.expected_prompt_sha256])))
        pin = capture_source.pin(config, admission)
        def accept(data):
            if _cancel_marker(config, session, turn) != marker:
                return "cancelled_or_changed_fence", False
            for job in data["jobs"]:
                if job.get("turn_id") == turn:
                    return ("replay" if job["key"] == key else "identity_conflict"), False
            if data["pin"] is not None and data["pin"] != pin:
                return "source_changed", False
            if data["ended"]:
                return "ended", False
            first = data["fresh"] and capture_source.first_turn(config, admission)
            if data["pin"] is None:
                data["pin"] = pin
                if not first:
                    data["watermark"] = snapshot_end
                    if host == "claude_code":
                        data.update(watermark_exclusive=True, baseline_turn_id=turn)
                    data["fresh"] = False
                    return "baseline_only", True
            if not first:
                # Codex retains its original inclusive source-start watermark.
                # A Claude baseline is an exclusive EOF fence: a new preflush
                # prompt starts AT that offset, not after an invented byte.
                exclusive = host == "claude_code" and data.get("watermark_exclusive") is True
                if (start_offset < data["watermark"] if exclusive else start_offset <= data["watermark"]):
                    return "replay", False
                if exclusive and data.get("baseline_turn_id") == turn:
                    return "replay", False
            data["fresh"] = False
            data["watermark"] = max(data["watermark"], start_offset)
            if host == "claude_code":
                data.pop("watermark_exclusive", None)
                data.pop("baseline_turn_id", None)
            if host == "claude_code":
                for prior in list(data["jobs"]):
                    if prior["phase"] == "admitted":
                        _finish(data, prior, "cancelled", "interrupted_before_stop")
            if len(data["jobs"]) >= MAX_JOBS:
                _count(data, "capacity")
                return "capacity", True
            data["jobs"].append({"key": key, "turn_id": turn, "admission": asdict(admission),
                                 "cancel_token": marker["token"],
                                 "config_sha256": binding, "phase": "admitted", "polls": 0,
                                 "bytes_read": 0, "records_parsed": 0})
            if host != "codex":
                data["jobs"][-1].update(source_host=host, assessor=capture_source.assessor(config),
                    source_start_ref=capture_source.start_record(config, admission))
            if "frozen_selection" in config:
                data["jobs"][-1]["frozen_selection"] = json.loads(encoded(config["frozen_selection"]))
            if config.get("memory_scope") == "workshop":
                from target_policy import workshop_policy
                data["jobs"][-1]["store_target"] = workshop_policy(config).canonical()
                data["jobs"][-1]["db_alias"] = "user"
            if config.get("memory_scope") == "misc":
                from target_policy import misc_policy
                policy = misc_policy(config)
                data["jobs"][-1].update(store_target=policy.canonical(), db_alias=policy.db_alias,
                                        workspace_binding=dict(data["workspace_binding"]))
            _count(data, "admitted")
            return "admitted", True
        # Do not wait with this earlier open-turn snapshot: a Stop could close
        # first, find no job, and then be overtaken by stale admission.
        ok, outcome = _transaction(config, session, accept, create=True)
        return {"outcome": outcome if ok else "busy_or_capacity"}
    except (OSError, ValueError, TypeError, UnicodeError):
        return {"outcome": "deferred", "reason": "invalid_locator_or_config"}


def attach_delivery(config, packet):
    """Best-effort one-turn snapshot; never creates a job or blocks recall."""
    if not enabled(config):
        return {"outcome": "skipped"}
    try:
        packet = validate_delivery_packet(packet)
        session, turn = packet["session_id"], packet["turn_id"]
        binding = _config_digest(config)
        def attach(data):
            if data["ended"]:
                return "ended", False
            job = next((j for j in data["jobs"] if j["turn_id"] == turn), None)
            if job is None or job.get("phase") != "admitted":
                return "missing_or_closed", False
            if "delivery" in job:
                return "replay", False
            if (job.get("config_sha256") != binding or _cancelled(config, session, job)
                    or job.get("cancel_requested")):
                return "stale", False
            if len(encoded({**job, "delivery": packet})) > MAX_JOB_BYTES:
                return "capacity", False
            job["delivery"] = packet
            return "attached", True
        ok, outcome = _transaction(config, session, attach, create=False)
        return {"outcome": outcome if ok and outcome is not None else "busy_or_missing"}
    except (OSError, ValueError, TypeError, KeyError, UnicodeError, RecursionError):
        return {"outcome": "invalid_or_unavailable"}

def interrupt_open_turns(config, session):
    """Retire only unclosed prompts; trusted completed captures may finish later."""
    def interrupt(data):
        changed = False
        for job in list(data["jobs"]):
            if job["phase"] == "admitted":
                _finish(data, job, "cancelled", "interrupted_before_stop")
                changed = True
        return None, changed
    return _transaction(config, session, interrupt, lock_timeout=LIFECYCLE_LOCK_SECONDS)[0]


def record_stop(config, session, turn, stop_receipt):
    """A Claude Stop is only a response candidate, never provider admission.

    Peer hooks may continue the same prompt. Keep only the latest exact digest;
    a later root prompt, SessionEnd or legitimate resume requests observation.
    None is a transcript-flush/finality proof. Repeated Stop never buys more work.
    """
    if capture_source.source_host(config) != "claude_code":
        raise ValueError("provisional_stop_requires_claude")
    from claude_observer import validate_stop_receipt
    try:
        receipt = validate_stop_receipt(stop_receipt, session_id=session, turn_id=turn)
    except (ValueError, TypeError, KeyError, UnicodeError):
        return False
    def provisional(data):
        job = next((j for j in data["jobs"] if j["turn_id"] == turn), None)
        if job is None or job["phase"] != "admitted" or receipt["transcript_path"] != job["admission"]["path"]:
            return None, False
        if job.get("stop_receipt") == receipt:
            return None, False
        job["stop_receipt"] = receipt
        return None, True
    return _transaction(config, session, provisional, lock_timeout=LIFECYCLE_LOCK_SECONDS)[0]


def _confirm_claude_boundary(config, event, kind):
    from claude_observer import validate_boundary_receipt
    session, next_turn = event.get("session_id"), event.get("turn_id")
    if (capture_source.source_host(config) != "claude_code" or not isinstance(session, str)
            or not ID.fullmatch(session) or event.get("agent_id")):
        return False
    if kind == "next_prompt" and (not isinstance(next_turn, str) or not ID.fullmatch(next_turn)
                                   or not isinstance(event.get("prompt"), str)):
        return False
    now, monotonic, boot = time.time(), time.monotonic(), _boot_id()
    def confirm(data):
        if kind == "next_prompt":
            known = {j["turn_id"] for j in data["jobs"]} | {r["turn_id"] for r in data["receipts"]}
            if data.get("baseline_turn_id") is not None:
                known.add(data["baseline_turn_id"])
            if next_turn in known or data["ended"]:
                return None, False
        changed = False
        if kind == "session_end" and not data["ended"]:
            data["ended"] = True
            changed = True
        for job in list(data["jobs"]):
            if job["phase"] != "admitted":
                continue
            if event.get("transcript_path") != job["admission"]["path"]:
                continue
            if not _unchanged(config, job):
                _finish(data, job, "deferred", "config_changed")
            elif _cancelled(config, session, job):
                _finish(data, job, "cancelled", "interrupted_or_invalid_fence")
            elif job.get("stop_receipt") is None:
                _finish(data, job, "cancelled", "interrupted_before_stop")
            elif boot is None:
                _finish(data, job, "deferred", "clock_witness_unavailable")
            else:
                receipt = validate_boundary_receipt({"schema": "mneme.claude-boundary-receipt.v1",
                    "session_id": session, "turn_id": job["turn_id"], "kind": kind,
                    "next_turn_id": next_turn if kind == "next_prompt" else None,
                    "prompt_sha256": digest(event["prompt"].encode()) if kind == "next_prompt" else None,
                    "transcript_path": event["transcript_path"]}, session_id=session, turn_id=job["turn_id"])
                job.update(phase="observing", boundary_receipt=receipt, closed_at=now,
                           deadline=now + CLOSE_SECONDS, next_poll=now, closed_monotonic=monotonic,
                           monotonic_deadline=monotonic + CLOSE_SECONDS, boot_id=boot)
            changed = True
        return None, changed
    return _transaction(config, session, confirm, lock_timeout=LIFECYCLE_LOCK_SECONDS)[0]


def confirm_prompt_boundary(config, event):
    """A new trusted hook is a witness to verify, independent of new capture fit."""
    return _confirm_claude_boundary(config, event, "next_prompt")


def confirm_session_end(config, event):
    return _confirm_claude_boundary(config, event, "session_end")


def recover_stopped_turns(config, event):
    """Resume only retained unstarted candidates, never paid/frozen intents.

    SessionStart is an explicit host opportunity, not a daemon or a repair of
    unknown accounting. The old admission/path/config/fence must still match.
    Repeated resume does not renew an observing job's budget or deadline.
    """
    return _confirm_claude_boundary(config, event, "resume")


def close_turn(config, session, turn=None, *, cancel=False, ended=False, stop_receipt=None) -> bool:
    """Close retained work durably, or report refusal after a bounded lock wait.

    A duplicate Stop never renews a closed job's deadlines. Cancellation writes
    its fence once before waiting, rather than replaying the entire operation
    and invalidating work that a newer prompt admitted during contention.
    """
    if capture_source.source_host(config) == "claude_code" and not cancel:
        return record_stop(config, session, turn, stop_receipt) if stop_receipt is not None else False
    if (("frozen_selection" not in config and not enabled(config))
            or not isinstance(session, str) or not ID.fullmatch(session)
            or (turn is not None and (not isinstance(turn, str) or not ID.fullmatch(turn)))):
        return False
    fenced = True
    if cancel:
        try:
            _signal_cancel(config, session, turn)
        except (OSError, ValueError):
            # Still attempt the normal locked cancellation. A failed fence write
            # cannot be represented as successful durable cancellation.
            fenced = False
    now = time.time()
    monotonic, boot = time.monotonic(), _boot_id()
    def close(data):
        changed = False
        if ended and not data["ended"]:
            data["ended"] = True
            changed = True
        for job in list(data["jobs"]):
            if turn is not None and job["turn_id"] != turn:
                continue
            if cancel and job["phase"] == "assessing":
                if not job.get("cancel_requested"):
                    job["cancel_requested"] = True
                    changed = True
            elif cancel and job["phase"] not in ("write_intent", "unresolved"):
                _finish(data, job, "cancelled", "interrupted")
                changed = True
            elif job["phase"] == "admitted":
                if boot is None:
                    _finish(data, job, "deferred", "clock_witness_unavailable")
                else:
                    job.update(phase="observing", closed_at=now, deadline=now + CLOSE_SECONDS, next_poll=now,
                               closed_monotonic=monotonic, monotonic_deadline=monotonic + CLOSE_SECONDS, boot_id=boot)
                changed = True
        return None, changed
    ok, _ = _transaction(config, session, close, lock_timeout=LIFECYCLE_LOCK_SECONDS)
    return ok and fenced


def has_work(config, session, *, ready_only=None):
    if ready_only is None:
        ready_only = capture_source.source_host(config) == "claude_code"
    ok, value = _transaction(config, session, lambda d: (any(
        j["phase"] != "unresolved" and (not ready_only or j["phase"] != "admitted")
        for j in d["jobs"]), False))
    return bool(ok and value)


def _update(config, session, key, mutation):
    def change(data):
        job = next((j for j in data["jobs"] if j["key"] == key), None)
        if job is None:
            return None, False
        return mutation(data, job), True
    return _transaction(config, session, change)


def _native_write(config, job, timeout):
    if _is_nomination_job(job):
        raise ValueError("nomination_is_not_native_write_authority")
    from mcp_client import McpClient
    from service import client_for_service, load_config
    deadline = time.monotonic() + timeout
    from target_policy import policy_for, target_kwargs, target_service_config
    destination = job.get("destination")
    target = target_kwargs(config, destination=destination) if destination else target_kwargs(config)
    service_path = target_service_config(config, destination)
    if target:
        policy, service = policy_for(service_path, config["project_root"], **target)
    else:
        policy, service = None, load_config(service_path)
    if policy is not None and (job.get("db_alias") != policy.db_alias
            or job.get("store_target") != policy.canonical()):
        raise ValueError("frozen_store_target_changed")
    if _config_digest(config) != job["config_sha256"]:
        raise ValueError("config_changed")
    if config.get("memory_scope") == "misc" and not _unchanged(config, job):
        raise ValueError("frozen_workspace_changed")
    registry = "frozen_selection" in config
    if registry:
        from registry_recording import validate_payload, ReadBudgetRefused
        from librarian_policy import resolve
        registry_store = validate_payload(config, job)
        native_charge = job.get("registry_native_bytes")
        native_limit = resolve(config).native_read_bytes
        if type(native_charge) is not int or not 0 <= native_charge < native_limit:
            raise ReadBudgetRefused("native_read_budget")
    if not registry and destination == "global_preference":
        from target_policy import GLOBAL_PREFERENCE_NAMESPACE, GLOBAL_PREFERENCE_TAG
        from recording_contract import MAX_PREFERENCE_SUMMARY_BYTES, MAX_PREFERENCE_BODY_BYTES
        payload = job["payload"]
        source = payload.get("source", {})
        if (set(payload) != {"source", "summary", "body", "tags"}
                or payload.get("tags") != [GLOBAL_PREFERENCE_TAG]
                or set(source) != {"namespace", "key", "reference"}
                or source.get("namespace") != GLOBAL_PREFERENCE_NAMESPACE
                or not isinstance(source.get("key"), str) or not re.fullmatch(r"[0-9a-f]{64}", source["key"])
                or source.get("reference") != capture_source.provenance(config) + "-global-preference://sha256/" + source["key"]
                or not isinstance(payload.get("summary"), str)
                or not 0 < len(payload["summary"].encode()) <= MAX_PREFERENCE_SUMMARY_BYTES
                or not isinstance(payload.get("body"), str)
                or not 0 < len(payload["body"].encode()) <= MAX_PREFERENCE_BODY_BYTES):
            raise ValueError("global_preference_payload")
        if (job.get("proposal_kind") != "lesson" or job.get("maintenance")
                or job.get("association") or job.get("routing")
                or job["payload"].get("links") or "action" in job["payload"]):
            raise ValueError("global_preference_operation")
    if job.get("proposal_kind") == "possibility":
        from tag_context import ordinary_tags
        possibility_tags = job["payload"].get("tags")
        if not isinstance(possibility_tags, list) or "possibility" not in possibility_tags:
            raise ValueError("possibility_operation")
        try:
            ordinary_tags([tag for tag in possibility_tags if tag != "possibility"
                           and (not registry or tag not in registry_store.record.required_tags)], max_count=31)
        except ValueError as error:
            raise ValueError("possibility_operation") from error
        if (not registry and (destination == "global_preference" or config.get("memory_scope", "project") not in ("project", "misc"))
                or len(possibility_tags) > 32 or len(set(possibility_tags)) != len(possibility_tags)
                or not registry and possibility_tags != ["possibility"] and "tag_context" not in job
                or "action" in job["payload"] or job["payload"].get("links")
                or job.get("routing") or job.get("association")):
            raise ValueError("possibility_operation")
    token = os.environ.get(service.token_env) if service.token_env else None
    if service.token_env and not token:
        raise ValueError("native_unavailable")
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("native_deadline")
    # The native child pins its transport ceiling at startup. Give it the
    # write allowance now; the mutable Python timeout still bounds each phase.
    client = (service.native_client(timeout=min(30, remaining), operation="record")
              if policy is not None and getattr(policy, "native", False)
              else client_for_service(service, token=token, timeout=min(30, remaining)) if registry
              else McpClient(service.url, token=token, timeout=min(30, remaining)))
    try:
        client.connect()
        if policy is not None:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("native_deadline")
            client.timeout = min(2, remaining)
            if registry and getattr(policy, "native", False):
                client.max_native_bytes = native_limit-native_charge
            work_before = getattr(client, "work_bytes", None)
            catalog = client.call_tool("databases", {})
            if registry:
                native_charge += (client.work_bytes - work_before if type(work_before) is int else len(encoded(catalog)))
                if native_charge > native_limit:
                    raise ReadBudgetRefused("native_read_budget")
            policy.catalog_identity(catalog, job["db_id"])
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("native_deadline")
        client.timeout = min(30, remaining)
        # Retain existing frozen jobs/source proofs; the new envelope is only
        # transport vocabulary. Native SAVE strips it before the same write.
        payload = dict(job["payload"])
        if job["proposal_kind"] == "episode":
            if payload.pop("action", None) != "append":
                raise ValueError("recording_save_requires_episode_append")
            kind = "episode"
        else:
            kind = "note"
        if "kind" in payload:
            raise ValueError("unexpected_recording_save_kind")
        payload["kind"] = kind
        if policy is not None:
            policy.validate_workspace()
        if _config_digest(config) != job["config_sha256"]:
            raise ValueError("config_changed")
        if registry:
            validate_payload(config, job)
        if registry and getattr(policy, "native", False):
            from guidance import revalidate_guidance
            from types import SimpleNamespace
            if "guidance_snapshot" not in job:
                raise ReadBudgetRefused("guidance_witness_missing")
            bounded = SimpleNamespace(db_id=job["db_id"], policy=policy,
                                      deadline=deadline, remaining_bytes=native_limit-native_charge)
            def guidance_call(name, args):
                before = getattr(client, "work_bytes", None)
                before_remaining = bounded.remaining_bytes
                left = deadline - time.monotonic()
                if left <= 0 or bounded.remaining_bytes <= 0:
                    raise ReadBudgetRefused("native_read_budget")
                client.timeout = min(30, left)
                client.max_native_bytes = bounded.remaining_bytes
                try:
                    result = client.call_tool(name, args)
                    charged = client.work_bytes-before if type(before) is int else len(encoded(result))
                    bounded.remaining_bytes -= charged
                    if bounded.remaining_bytes < 0:
                        raise ReadBudgetRefused("native_read_budget")
                    return result
                except Exception:
                    current = getattr(client, "work_bytes", None)
                    if type(before) is int and type(current) is int and current > before:
                        bounded.remaining_bytes = before_remaining-(current-before)
                        if bounded.remaining_bytes < 0:
                            raise ReadBudgetRefused("native_read_budget")
                    else:
                        bounded.remaining_bytes = 0
                    raise
            bounded.call = guidance_call
            try:
                revalidate_guidance(job["guidance_snapshot"], bounded, registry_store.guidance,
                                    owner_binding=policy.canonical())
            except ValueError as error:
                raise ReadBudgetRefused(str(error)) from error
            # SAVE has no native multi-guide guard. This fresh exact reread is
            # deliberately a non-atomic stale-intent check, not policy locking.
            validate_payload(config, job)
            try:
                revalidate_project_focus(registry_store, job)
            except (ValueError, OSError) as error:
                raise ReadBudgetRefused(str(error)) from error
            if bounded.remaining_bytes <= 0:
                raise ReadBudgetRefused("native_read_budget")
            client.max_native_bytes = bounded.remaining_bytes
        receipt = client.save_verified(policy.db_alias if policy is not None else "project", payload, expected_db_id=job["db_id"])
        # The synchronous verified receipt is historical evidence of this
        # already-completed write. Later registry revocation cannot make the
        # observed acknowledgement disappear or authorize a replay elsewhere.
        if not registry and _config_digest(config) != job["config_sha256"]:
            raise ValueError("config_changed_after_write")
        return receipt
    finally:
        client.timeout = min(.1, max(.001, deadline - time.monotonic()))
        client.close()


def _native_maintenance(config, job, item, timeout):
    if _is_nomination_job(job):
        raise ValueError("nomination_is_not_native_write_authority")
    if "frozen_selection" in config or job.get("destination") == "global_preference":
        raise ValueError("global_preference_operation")
    from mcp_client import McpClient
    from service import load_config
    deadline = time.monotonic() + timeout
    from target_policy import policy_for, target_kwargs
    target = target_kwargs(config)
    if target:
        policy, service = policy_for(config["service_config"], config["project_root"], **target)
    else:
        policy, service = None, load_config(config["service_config"])
    if policy is not None and (job.get("db_alias") != policy.db_alias
            or job.get("store_target") != policy.canonical()):
        raise ValueError("frozen_store_target_changed")
    if _config_digest(config) != job["config_sha256"]:
        raise ValueError("config_changed")
    if config.get("memory_scope") == "misc" and not _unchanged(config, job):
        raise ValueError("frozen_workspace_changed")
    token = os.environ.get(service.token_env) if service.token_env else None
    if service.token_env and not token:
        raise ValueError("native_unavailable")
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("native_deadline")
    client = McpClient(service.url, token=token, timeout=min(30, remaining))
    try:
        client.connect()
        if policy is not None:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("native_deadline")
            client.timeout = min(2, remaining)
            policy.catalog_identity(client.call_tool("databases", {}), job["db_id"])
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("native_deadline")
        client.timeout = min(30, remaining)
        if policy is not None:
            policy.validate_workspace()
        if _config_digest(config) != job["config_sha256"]:
            raise ValueError("config_changed")
        return client.concern_checked(policy.db_alias if policy is not None else "project", item["payload"], expected_db_id=job["db_id"])
    finally:
        client.timeout = min(.1, max(.001, deadline - time.monotonic()))
        client.close()


def _freeze_maintenance(items, context, *, db_id, session, turn):
    if items == []:
        return [], 0
    from recording_contract import Maintenance, concern_evidence
    from turn_observer import validate_concern_row
    if not isinstance(items, list):
        return [], 1
    frozen, omitted, seen = [], 0, set()
    sensitive = re.compile(r"(?i)-----BEGIN .*PRIVATE KEY-----|\b(?:sk-[A-Za-z0-9]{16,}|bearer [A-Za-z0-9._-]{16,})")
    for item in items:
        try:
            if (not isinstance(item, Maintenance) or item.target not in context.concern_bindings
                    or item.target.db_id != db_id or item.target.opaque_id in seen
                    or sensitive.search(item.scope + "\n" + item.observation)):
                raise ValueError("maintenance_binding")
            if context.concern_source != (session,turn):
                raise ValueError("maintenance_source")
            expected = validate_concern_row(json.loads(item.target.expected_row_json))
            if (not any(c.kind == "memory_delivery" for c in item.evidence)
                    or not any(c.kind in ("user_statement", "tool_result") for c in item.evidence)):
                raise ValueError("maintenance_evidence")
            finding = {"scope": item.scope, "observation": item.observation,
                       "evidence": concern_evidence(item.evidence, context, session=session, turn=turn)}
            desired = {**expected, "finding": finding}
            validate_concern_row(desired)
            frozen.append({"target": item.target.opaque_id, "payload": {
                "action": "record_finding", "expected": expected, "finding": finding},
                "citations": [asdict(c) for c in item.evidence], "status": "pending"})
            seen.add(item.target.opaque_id)
        except (ValueError, TypeError, KeyError, AttributeError, UnicodeError):
            omitted += 1
    return frozen, omitted


def _checked_maintenance_result(receipt, job, item):
    from turn_observer import validate_concern_row
    if (not isinstance(receipt, dict) or set(receipt) != {"db", "db_id", "action", "outcome"}
            or receipt["db"] != job.get("db_alias", "project") or receipt["db_id"] != job["db_id"]
            or receipt["action"] != "record_finding"):
        raise ValueError("maintenance_receipt_owner")
    outcome = receipt["outcome"]
    if not isinstance(outcome, dict) or outcome.get("status") not in ("applied", "unchanged", "refused"):
        raise ValueError("maintenance_receipt_shape")
    if outcome["status"] in ("applied", "unchanged"):
        if set(outcome) != {"status", "row"}:
            raise ValueError("maintenance_receipt_shape")
        row = validate_concern_row(outcome["row"])
        if row != {**item["payload"]["expected"], "finding": item["payload"]["finding"]}:
            raise ValueError("maintenance_receipt_meaning")
    else:
        if (set(outcome) != {"status", "reason", "row"} or outcome["reason"] not in
                ("wrong_key", "stale_meanings", "missing_row", "stale_row", "missing_endpoint", "inactive_endpoint")):
            raise ValueError("maintenance_receipt_shape")
        if outcome["row"] is not None:
            row = validate_concern_row(outcome["row"])
            if row["notice"]["binding"]["key"] != item["payload"]["expected"]["notice"]["binding"]["key"]:
                raise ValueError("maintenance_receipt_meaning")
    return receipt


def _routing_payload(proposal, context, *, db_id, session, turn):
    """Format one optional annotation without another call, note, or edge write.

    Bindings describe advice at delivery time, not its current truth. Native
    routing later checks current meaning before using it. A correction names the
    exact previously read body; editing that body invalidates the reference.
    """
    judgment = getattr(proposal, "routing_judgment", None)
    if judgment is None:
        return None, getattr(proposal, "routing_reason", None)
    try:
        from recording_contract import RoutingJudgment
        from routing_memory import encode_witness, validate_binding, validate_conditional_binding, NAMESPACE, TAG
        if (not isinstance(judgment, RoutingJudgment) or proposal.kind != "lesson"
                or not context.routing_enabled
                or judgment.target not in context.association_bindings
                or judgment.target.origin != "delivery" or judgment.target.kind != "semantic"
                or judgment.target.routing_binding_json is None):
            raise ValueError("routing_target")
        if judgment.target.entry_kind not in (None, "conditional"):
            raise ValueError("routing_target")
        binding = (validate_conditional_binding(json.loads(judgment.target.routing_binding_json),
                                               judgment.target.native_id, expected_db_id=db_id)
                   if judgment.target.entry_kind == "conditional" else
                   validate_binding(json.loads(judgment.target.routing_binding_json), expected_db_id=db_id))
        if binding["route"]["target"] != judgment.target.native_id:
            raise ValueError("routing_target")
        correction = None
        if judgment.corrects is not None:
            target = judgment.corrects
            if (target not in context.association_bindings or target.origin != "overlap"
                    or target.routing_witness_json is None):
                raise ValueError("routing_correction")
            old = json.loads(target.routing_witness_json)
            if old["witness"]["binding"] != binding:
                raise ValueError("routing_correction")
            correction = {k: old[k] for k in ("node_id", "body_sha256")}
        by_id = {item.evidence_id: item for item in context.bindings}
        evidence = []
        for citation in proposal.evidence:
            source = by_id.get(citation.evidence_id)
            if (source is None or any(getattr(citation, k) != getattr(source, k)
                                     for k in ("kind", "source_ref_json", "source_field", "rendering"))):
                raise ValueError("routing_evidence")
            reference = {"record": json.loads(source.source_ref_json), "field": source.source_field}
            evidence.append({"kind": citation.kind, "reference": encoded(reference).decode()})
        body = encode_witness(note=proposal.body, binding=binding, sign=judgment.sign,
                              conditions=judgment.conditions, rationale=judgment.rationale,
                              shown_summary=judgment.target.summary, session=session, turn=turn,
                              evidence=evidence, corrects=correction,
                              entry_kind=judgment.target.entry_kind)
        return {"namespace": NAMESPACE, "tags": [TAG], "body": body}, "prepared"
    except (ImportError, ValueError, TypeError, KeyError, AttributeError, UnicodeError, RecursionError):
        return None, "routing_invalid"


def _publish_nomination(config, session, key):
    """Replay only the persisted local handoff, never inference or native SAVE.

    The recorder lock spans enqueue and the terminal receipt. If the outbox
    commits but the recorder write fails, its original frozen envelope survives
    and enqueue's source identity makes the next attempt an exact replay.
    """
    from touchstone_handoff import enqueue, validate_envelope

    def publish(data, job):
        if (job.get("phase") != "nomination_frozen"
                or "frozen_selection" not in job
                or any(field in job for field in ("payload", "proposal_kind", "citations",
                    "maintenance", "save_outcome", "routing", "association", "active_branch"))):
            _finish(data, job, "deferred", "nomination_intent_refused")
            return
        if _cancelled(config, session, job):
            _finish(data, job, "cancelled", "interrupted_or_invalid_fence")
            return
        if _remaining(job) <= 0 or not _unchanged(config, job):
            _finish(data, job, "deferred", "deadline_or_config_changed")
            return
        from registry_recording import selection
        try:
            envelope = validate_envelope(job["nomination_envelope"])
            if (envelope["source_key"] != key or envelope["session_id"] != session
                    or envelope["source_turn"] != job["turn_id"]
                    or envelope["source_start_ref"] != job.get("source_start_ref", job["admission"].get("start_record"))
                    or envelope["cancel_token"] != job["cancel_token"]
                    or envelope["owner"]["name"] != job["destination"]
                    or envelope["owner"]["db_id"] != job["db_id"]
                    or envelope["owner"]["db_alias"] != job["db_alias"]):
                raise ValueError("nomination_source_binding")
            store = selection(config).named(job["destination"])
            revalidate_project_focus(store, job)
        except (ValueError, KeyError, TypeError, OSError):
            _finish(data, job, "deferred", "nomination_intent_refused")
            return
        try:
            result = _nomination_summary(enqueue(config, envelope))
        except (ValueError, KeyError, TypeError, OSError):
            result = {"outcome": "unavailable"}
        job["nomination_outcome"] = result
        reason = ("foreground_review_" + result["status"] if result["outcome"] in ("queued", "replay")
                  and result["status"] in ("pending", "emitted")
                  else "nomination_handoff_suppressed" if result.get("status") == "suppressed"
                  else "nomination_handoff_" + result["outcome"])
        _finish(data, job, "deferred", reason)

    _update(config, session, key, publish)


def step(config, session, runtime, reserve, account, *, native_write=None, native_maintenance=None, reserve_packet=None):
    """Do not let optional journal I/O spend this step's later native budget."""
    scope = None
    try:
        from session_activity import defer
        scope = defer()
        scope.__enter__()
    except Exception:
        scope = None
    try:
        return _step(config, session, runtime, reserve, account,
                     native_write=native_write, native_maintenance=native_maintenance, reserve_packet=reserve_packet)
    finally:
        if scope is not None:
            try:
                scope.__exit__(*sys.exc_info())
            except Exception:
                pass  # Failure to drain diagnostics never changes functional completion.


def _step(config, session, runtime, reserve, account, *, native_write=None, native_maintenance=None, reserve_packet=None):
    """One bounded worker step. Callback accounting is shared with reader usage."""
    now = time.time()
    def choose(data):
        for job in data["jobs"]:
            if job["phase"] == "admitted" and _cancelled(config, session, job):
                return json.loads(json.dumps(job)), False
            clock_reversed = now < job.get("closed_at", now) or time.monotonic() < job.get("closed_monotonic", 0)
            if job["phase"] not in ("admitted", "unresolved") and (job.get("next_poll", 0) <= now or clock_reversed):
                return json.loads(json.dumps(job)), False
        return None, False
    ok, job = _transaction(config, session, choose)
    if not ok or job is None:
        return False
    key = job["key"]
    host = capture_source.source_host(config)
    if job.get("source_host", "codex") != host or job.get("assessor", "codex") != capture_source.assessor(config):
        _update(config, session, key, lambda d, j: _finish(d, j, "deferred", "capture_host_changed"))
        return True
    diagnostic = None
    context_diagnostic = None
    def finish(outcome, reason, receipt=None):
        _update(config, session, key, lambda d, j: _finish(d, j, outcome, reason, receipt, diagnostic, context_diagnostic))
    if job["phase"] in ("assessing", "write_intent"):
        if job["phase"] == "assessing":
            account(key, {"provider_attempt": True, "usage": None})
            _transaction(config, session, lambda d: (d.update(usage_unknown=True), True))
        if job["phase"] == "write_intent" and "active_branch" in job:
            def interrupted(_data,current):
                branch=current["active_branch"]
                outcome={"status":"unresolved","reason":"interrupted_external_intent"}
                if branch == "save":
                    current["save_outcome"]=outcome
                elif type(branch) is int and 0 <= branch < len(current.get("maintenance",[])):
                    current["maintenance"][branch].update(outcome)
            _update(config,session,key,interrupted)
        finish("unresolved", "interrupted_external_intent")
        return True
    if _cancelled(config, session, job):
        finish("cancelled", "interrupted_or_invalid_fence"); return True
    if _remaining(job) <= 0:
        finish("deferred", "job_deadline")
        return True
    try:
        if not _unchanged(config, job):
            finish("deferred", "config_changed"); return True
        if _is_nomination_job(job):
            _publish_nomination(config, session, key)
            return True
        if job["phase"] == "observing":
            # Charge worst-case parser work before touching the source. A crash
            # or failed progress write cannot refund an already performed poll.
            limits = Limits()
            admission = capture_source.restore(config, job["admission"])
            byte_charge = capture_source.byte_ceiling(config, admission, limits=limits)
            def reserve_poll(data, current):
                if current["phase"] != "observing":
                    return None
                if (current["polls"] >= MAX_POLLS
                        or current.get("reserved_bytes", 0) + byte_charge > MAX_SCAN_WORK
                        or current.get("reserved_records", 0) + limits.events > MAX_EVENT_WORK):
                    _finish(data, current, "deferred", "observation_work_cap")
                    return None
                current["polls"] += 1
                current["reserved_bytes"] = current.get("reserved_bytes", 0) + byte_charge
                current["reserved_records"] = current.get("reserved_records", 0) + limits.events
                return json.loads(json.dumps(current))
            ok, reserved = _update(config, session, key, reserve_poll)
            if not ok or reserved is None:
                return True
            job = reserved
            observation = capture_source.observe(config, admission, limits=limits,
                delivery=job.get("delivery"), stop_receipt=job.get("stop_receipt"),
                boundary_receipt=job.get("boundary_receipt"), snapshot=job.get("source_snapshot"),
                allow_partial=host == "claude_code" and (
                    job["polls"] >= MAX_POLLS or time.time() >= job["closed_at"] + FLUSH_SECONDS))
            job["bytes_read"] += observation["work"]["bytes_read"]
            job["records_parsed"] += observation["work"]["records_parsed"]
            def progress(_data, current):
                if current["phase"] != "observing" or current["polls"] != job["polls"]:
                    return False
                current.update({k: job[k] for k in ("polls", "bytes_read", "records_parsed")})
                _capture_admission(_data, current, observation)
                if capture_source.assessable(observation):
                    if host == "codex":
                        current["closure_ref"] = observation["boundary"]
                    else:
                        current["source_snapshot"] = observation["coverage"]["snapshot"]
                return True
            ok, saved = _update(config, session, key, progress)
            if not ok or not saved:
                return True
            if host == "claude_code" and capture_source.assessable(observation):
                job["source_snapshot"] = observation["coverage"]["snapshot"]
            if (observation["work"]["bytes_read"] > byte_charge
                    or observation["work"]["records_parsed"] > limits.events
                    or job["bytes_read"] > MAX_SCAN_WORK
                    or job["records_parsed"] > MAX_EVENT_WORK):
                finish("deferred", "observation_work_cap"); return True
            if not capture_source.assessable(observation):
                retry_reasons = {"source_turn_not_closed", "unflushed_record"}
                if host == "claude_code":
                    retry_reasons.update({"source_incomplete_line", "source_prompt_pending", "source_boundary_pending",
                                          "source_changed_during_read", "final_transcript_pending_or_mismatch", "missing_tool_result"})
                retry = (observation["reason"] in retry_reasons
                         and job["polls"] < MAX_POLLS and time.time() < job["closed_at"] + FLUSH_SECONDS)
                if retry:
                    _update(config, session, key, lambda _d, j: j.update(next_poll=time.time() + (1.5 if host == "claude_code" else .25) * job["polls"]))
                else:
                    finish("deferred", observation["reason"])
                return True
            context_diagnostic = _context_diagnostic(observation)
            from librarian_policy import resolve
            from recording_contract import prepare
            from target_policy import target_kwargs, target_service_config
            registry_mode = "frozen_selection" in config
            acquisition = None
            if registry_mode:
                from registry_recording import collect as collect_registry
                budget = resolve(config)
                left = lambda: _remaining(job)
                acquisition = collect_registry(config, observation, budget=budget, seconds=left())
                _update(config, session, key, lambda _d, current: current.update(
                    store_context=acquisition.diagnostic))
                if not acquisition.stores:
                    finish("deferred", "no_available_destination"); return True
                if acquisition.decoded_bytes >= budget.native_read_bytes:
                    finish("deferred", "native_read_budget"); return True
                if acquisition.native_seconds_left <= 0:
                    finish("deferred", "native_read_deadline"); return True
                preference_policy, routing_target, db_id = None, {}, None
                scope_options = acquisition.scope_options
                overlap = {"cards": acquisition.cards}
                native_seconds_left = acquisition.native_seconds_left
                aggregate_native_bytes = acquisition.decoded_bytes
            else:
                from hook_recall import resolve_project_identity, collect_overlap
                from librarian_policy import resolve
                from recording_contract import prepare, overlap_plan
                from target_policy import target_kwargs, workshop_policy, global_preferences_policy, target_service_config
                preference_policy = global_preferences_policy(config)
                routing_target = target_kwargs(config)
                scope_options = ({"recording_scope": "workshop", "expected_db_id": workshop_policy(config).db_id}
                                 if config.get("memory_scope") == "workshop" else {})
                if preference_policy is not None:
                    scope_options["global_preferences_enabled"] = True
                if config.get("memory_scope", "project") == "project" and config.get("_project_focus") is not None:
                    scope_options["project_focus"] = config["_project_focus"]
                budget = resolve(config)
                read_plan, reason = overlap_plan(observation, budget=budget, **scope_options)
                if read_plan is None:
                    diagnostic = sanitize_assessment_diagnostic({"runtime_reason": reason,
                                                                "validation_reason": "unknown"})
                    finish("deferred", "assessment_input_refused"); return True
                left = lambda: _remaining(job)
                native_deadline = time.monotonic() + min(budget.native_seconds, left())
                native_left = lambda: native_deadline - time.monotonic()
                identity = resolve_project_identity(config["service_config"], config["project_root"],
                                                    timeout=min(2, native_left()), **routing_target)
                if identity.get("outcome") != "ok":
                    finish("deferred", "native_identity_unavailable"); return True
                db_id = identity.get("db_id")
                if not isinstance(db_id, str) or not ULID.fullmatch(db_id):
                    finish("deferred", "invalid_native_identity"); return True
                identity_work = identity.get("native_work")
                spent_native_bytes = (identity_work.get("decoded_bytes")
                                      if isinstance(identity_work, dict) else None)
                if type(spent_native_bytes) is not int or spent_native_bytes < 0:
                    finish("deferred", "native_identity_unavailable"); return True
                if spent_native_bytes >= budget.native_read_bytes:
                    finish("deferred", "overlap_unavailable_or_changed"); return True
                if native_left() <= 0:
                    finish("deferred", "native_read_deadline"); return True
                prompt = observation["evidence"][0]["content"][0]["text"]
                routing_options = {}
                for item in observation["evidence"]:
                    if item.get("kind") == "memory_delivery":
                        packet = validate_delivery_packet(item["packet"])
                        if any(card.get("routing_binding") or card.get("conditional_binding")
                               for card in packet["displayed"]):
                            routing_options["include_routing"] = True
                overlap = collect_overlap(config["service_config"], prompt, config["project_root"],
                                          expected_db_id=db_id, timeout=native_left(), budget=budget,
                                          read_plan=read_plan, spent_native_bytes=spent_native_bytes,
                                          **routing_options, **routing_target)
                if overlap.get("outcome") not in ("ok", "empty") or overlap.get("db_id") != db_id:
                    finish("deferred", "overlap_unavailable_or_changed"); return True
                if native_left() <= 0 or left() <= 0 or not _unchanged(config, job):
                    finish("deferred", "deadline_or_config_changed"); return True
                # Once native project identity is known, foreign delivered cards stay
                # visible but cannot become project associations, routing or maintenance.
                if preference_policy is not None:
                    scope_options["expected_db_id"] = db_id
                native_seconds_left = native_left()
                native_work = overlap.get("native_work")
                aggregate_native_bytes = (native_work.get("decoded_bytes") if isinstance(native_work, dict) else None)
                if config.get("tag_stewardship") is True:
                    from tag_context import collect
                    tag_room = (budget.native_read_bytes - aggregate_native_bytes
                                if type(aggregate_native_bytes) is int else 0)
                    if native_left() > 0 and tag_room > 0:
                        tags_context = collect(config, expected_db_id=db_id,
                                               timeout=min(30, native_left() / 2),
                                               max_bytes=min(tag_room // 2, budget.recording_hint_bytes),
                                               cue=prompt)
                        aggregate_native_bytes += tags_context.decoded_bytes
                        if tags_context.enabled:
                            scope_options["tag_context"] = tags_context
                    # Lookup errors are optional tag omissions, never failed capture.
                    native_seconds_left = native_left()
            prepared, _context = prepare(observation, overlap["cards"], budget=budget, **scope_options)
            if prepared is None:
                diagnostic = sanitize_assessment_diagnostic({"runtime_reason": _context,
                                                            "validation_reason": "unknown"})
                finish("deferred", "assessment_input_refused"); return True
            context_diagnostic = _context_diagnostic(observation, _context)
            selected_coverage = json.loads(_context.coverage_json)["public_evidence"]
            source_coverage = json.loads(_context.coverage_json)
            observation_summary = {"source_turn": source_coverage["source_turn"], **selected_coverage}
            if host == "claude_code":
                observation_summary.update(turn_finality=source_coverage["turn_finality"],
                                           remaining_transcript=source_coverage["remaining_transcript"])
            # Persist the one-shot assessment intent before the shared reservation.
            def assessment_intent(_data, current):
                if current["phase"] != "observing":
                    return False
                if _cancelled(config, session, current):
                    _finish(_data, current, "cancelled", "interrupted_or_invalid_fence")
                    return False
                current.update(phase="assessing", db_id=db_id)
                current.pop("delivery", None)
                current["observation"] = observation_summary
                current["context_diagnostic"] = context_diagnostic
                _normalize_context_in_place(current, max_bytes=MAX_JOB_BYTES)
                if len(encoded(current)) > MAX_JOB_BYTES:
                    current.pop("observation")
                return True
            ok, claimed = _update(config, session, key, assessment_intent)
            if not ok or not claimed:
                return True
            admitted = (reserve_packet(key, {"turn": job["turn_id"],
                         "request": job["admission"]["expected_prompt_sha256"]})
                        if reserve_packet is not None else reserve(key))
            if not admitted:
                finish("deferred", "shared_budget"); return True
            if (_cancelled(config, session, job)
                    or not _unchanged(config, job)):
                if not account(key, {"provider_attempt": False}):
                    _transaction(config, session, lambda d: (d.update(usage_unknown=True), True))
                    finish("unresolved", "accounting_unavailable"); return True
                finish("cancelled", "interrupted_before_assessment"); return True
            try:
                result = runtime.assess(observation, overlap["cards"], timeout=min(45, left()), **scope_options)
            except Exception:
                result = {"provider_attempt": True, "usage": None, "reason": "assessment_failed"}
            from assessment import assessment_diagnostic
            diagnostic = assessment_diagnostic(result)
            if not account(key, result):
                _transaction(config, session, lambda d: (d.update(usage_unknown=True), True))
                finish("unresolved", "accounting_unavailable"); return True
            if result.get("provider_attempt") and result.get("usage") is None:
                _transaction(config, session, lambda d: (d.update(usage_unknown=True), True))
                finish("unresolved", "usage_unknown"); return True
            _transaction(config, session, lambda d: (_count(d, "assessed"), True))
            from recording_contract import sanitize_intent_omissions
            omissions = sanitize_intent_omissions(result.get("intent_omissions"))
            if omissions is not None:
                _update(config,session,key,lambda _d,current:current.update(intent_omissions=omissions))
            if result.get("reason") == "abstained":
                finish("abstained", "no_proposal"); return True
            proposal = result.get("proposal")
            maintenance = result.get("maintenance", [])
            from recording_contract import TouchstoneNomination
            if (isinstance(proposal, TouchstoneNomination)
                    or getattr(proposal, "kind", None) in ("touchstone_nomination", "touchstone", "review_nomination")):
                finish("deferred", "nomination_refused"); return True
            if "nomination" in result:
                # Distinct operation before ordinary proposal dispatch. A mixed,
                # forged or invalid nomination never degrades into a SAVE.
                from recording_contract import check_nomination
                from registry_recording import freeze_destination
                from touchstone_handoff import freeze as freeze_nomination
                try:
                    if (not registry_mode or result.get("reason") != "proposed"
                            or proposal is not None or maintenance != []):
                        raise ValueError("nomination_operation_refused")
                    nomination = check_nomination(result["nomination"], _context)
                    if re.search(r"(?i)-----BEGIN .*PRIVATE KEY-----|\b(?:sk-[A-Za-z0-9]{16,}|bearer [A-Za-z0-9._-]{16,})",
                                 nomination.summary + "\n" + nomination.body
                                 + ("\n" + encoded(nomination.effect.to_handoff()).decode()
                                    if hasattr(nomination, "effect") else "")):
                        raise ValueError("sensitive_nomination")
                    _, _, destination = freeze_destination(acquisition, nomination.destination, seconds=left())
                    envelope = freeze_nomination(config, session, job["turn_id"], key,
                        nomination, _context, source_start_ref=job.get("source_start_ref", job["admission"].get("start_record")))
                    frozen = {"phase": "nomination_frozen", "nomination_envelope": envelope,
                              "assessment_diagnostic": diagnostic,
                              **{name: value for name, value in destination.items() if name in
                                 ("destination", "db_id", "db_alias", "project_focus_witness")}}
                except (ValueError, TypeError, AttributeError, KeyError, OSError):
                    finish("deferred", "nomination_refused"); return True

                def freeze_handoff(data, current):
                    if current["phase"] != "assessing":
                        return False
                    if current.get("cancel_requested") or _cancelled(config, session, current):
                        _finish(data, current, "cancelled", "interrupted_after_assessment", diagnostic=diagnostic)
                        return False
                    if not _unchanged(config, current) or _remaining(current) <= 0:
                        _finish(data, current, "deferred", "deadline_or_config_changed", diagnostic=diagnostic)
                        return False
                    candidate = {**current, **frozen}
                    # Reserve space for a compact local handoff acknowledgement.
                    _normalize_context_in_place(candidate, max_bytes=MAX_JOB_BYTES - 512)
                    if len(encoded(candidate)) > MAX_JOB_BYTES - 512:
                        candidate.pop("observation", None)
                        candidate.pop("assessment_diagnostic", None)
                    if len(encoded(candidate)) > MAX_JOB_BYTES - 512:
                        _finish(data, current, "deferred", "nomination_intent_cap", diagnostic=diagnostic)
                        return False
                    current.clear()
                    current.update(candidate)
                    return True

                ok, changed = _update(config, session, key, freeze_handoff)
                if ok and changed:
                    _publish_nomination(config, session, key)
                return True
            if result.get("reason") != "proposed" or (proposal is None and not maintenance):
                finish("deferred", "assessment_refused"); return True
            destination = proposal.destination if proposal is not None else "project"
            registry_frozen, selected_store = {}, None
            if registry_mode:
                from registry_recording import check_proposal, freeze_destination
                try:
                    if maintenance or proposal is None:
                        raise ValueError("registry_operation_refused")
                    check_proposal(proposal, _context)
                    selected_store, destination_policy, registry_frozen = freeze_destination(
                        acquisition, destination, seconds=left())
                    db_id = selected_store.db_id
                    routing_target = {"store_target": destination_policy}
                except (ValueError, TypeError, AttributeError, KeyError):
                    finish("deferred", "assessment_refused"); return True
            elif destination not in ("project", "global_preference"):
                finish("deferred", "assessment_refused"); return True
            if not registry_mode and destination == "global_preference":
                from recording_contract import validate_global_preference
                try:
                    validate_global_preference(proposal, _context)
                    if preference_policy is None or maintenance:
                        raise ValueError("global_preference_operation")
                except (ValueError, TypeError, AttributeError, KeyError):
                    finish("deferred", "assessment_refused"); return True
                # No extra assessor or preference-overlap pass. Resolve only the
                # independently selected owner, within the remaining native envelope.
                if (type(aggregate_native_bytes) is not int or aggregate_native_bytes < 0
                        or aggregate_native_bytes >= budget.native_read_bytes or native_seconds_left <= 0):
                    finish("deferred", "global_identity_budget"); return True
                global_native_deadline = time.monotonic() + min(native_seconds_left, max(0, left()))
                global_identity = resolve_project_identity(target_service_config(config, destination),
                    config["project_root"], timeout=min(2, native_seconds_left, max(0, left())),
                    **target_kwargs(config, destination=destination))
                work = global_identity.get("native_work")
                charged = work.get("decoded_bytes") if isinstance(work, dict) else None
                if (global_identity.get("outcome") != "ok"
                        or global_identity.get("db_id") != preference_policy.db_id
                        or type(charged) is not int or charged < 0
                        or aggregate_native_bytes + charged > budget.native_read_bytes):
                    finish("deferred", "global_identity_unavailable"); return True
                db_id = preference_policy.db_id
            frozen = {"phase": "frozen", "db_id": db_id, "assessment_diagnostic": diagnostic}
            frozen.update(registry_frozen)
            if registry_mode and _context.root_user_ref_json is not None:
                frozen["root_user_ref_json"] = _context.root_user_ref_json
            if preference_policy is not None:
                frozen["destination"] = destination
            if not registry_mode and destination == "global_preference":
                frozen.update(db_alias=preference_policy.db_alias, store_target=preference_policy.canonical(),
                              native_deadline_monotonic=global_native_deadline)
            if proposal is not None:
                try:
                    from recording_contract import AssociationTarget
                    target = proposal.associate_with
                    if proposal.kind == "possibility":
                        if (not registry_mode and (destination != "project" or config.get("memory_scope", "project") not in ("project", "misc"))
                                or target is not None or proposal.routing_judgment is not None):
                            raise ValueError("possibility_operation")
                    if (target is not None and
                            (not isinstance(target, AssociationTarget)
                             or proposal.kind != "lesson" or target.kind != "semantic"
                             or target not in _context.association_bindings)):
                        raise ValueError("assessment_refused")
                    routing, routing_reason = _routing_payload(proposal, _context, db_id=db_id,
                                                                session=session, turn=job["turn_id"])
                    # Screen only obvious unsafe payloads, not a privacy/entailment certificate.
                    sensitive = re.compile(r"(?i)-----BEGIN .*PRIVATE KEY-----|\b(?:sk-[A-Za-z0-9]{16,}|bearer [A-Za-z0-9._-]{16,})")
                    if sensitive.search(proposal.summary + "\n" + proposal.body):
                        raise ValueError("sensitive_proposal")
                    if routing is not None and sensitive.search(routing["body"]):
                        routing, routing_reason = None, "routing_sensitive"
                    if config.get("memory_scope") == "misc" and routing is not None:
                        routing, workspace_reason = _misc_routing_provenance(
                            routing, job["workspace_binding"]["workspace_origin"], db_id=db_id)
                        if workspace_reason is not None:
                            routing_reason = workspace_reason
                    host_prefix = capture_source.provenance(config)
                    payload = {"source": {"namespace": host_prefix + "-acquisition.v1", "key": key,
                               "reference": f"{host_prefix}://{session}/{job['turn_id']}", "session": session},
                               "summary": proposal.summary, "body": proposal.body}
                    if "source_snapshot" in job:
                        payload["source"]["reference"] += "?snapshot=" + job["source_snapshot"]["observed_sha256"]
                    if proposal.tags:
                        from tag_context import ordinary_tags
                        if registry_mode:
                            tags = ordinary_tags([tag for tag in proposal.tags
                                                 if tag not in selected_store.record.required_tags], max_count=32)
                            payload["tags"] = sorted(set(tags) | set(selected_store.record.required_tags))
                        else:
                            payload["tags"] = list(ordinary_tags(list(proposal.tags), max_count=32))
                    if proposal.tag_context_json is not None and destination == "project":
                        frozen["tag_context"] = json.loads(proposal.tag_context_json)
                    if not registry_mode and destination == "global_preference":
                        from target_policy import GLOBAL_PREFERENCE_NAMESPACE, GLOBAL_PREFERENCE_TAG
                        coordinate = {"session": session, "turn": job["turn_id"], "key": key,
                                      "ordinals": [json.loads(c.source_ref_json)["ordinal"] for c in proposal.evidence]}
                        payload["source"] = {"namespace": GLOBAL_PREFERENCE_NAMESPACE,
                            "key": digest(encoded(coordinate)),
                            "reference": capture_source.provenance(config) + "-global-preference://sha256/" + digest(encoded(coordinate))}
                        payload["tags"] = [GLOBAL_PREFERENCE_TAG]
                    elif config.get("memory_scope") == "workshop" and routing is None:
                        payload["source"]["reference"] += f"?scope=workshop&db_id={db_id}"
                    if routing is not None:
                        payload["source"]["namespace"] = routing["namespace"]
                        payload.update(body=routing["body"], tags=routing["tags"])
                    if config.get("memory_scope") == "misc" and routing is None:
                        origin = job["workspace_binding"]["workspace_origin"]
                        _misc_provenance(payload, origin)
                    if proposal.kind == "possibility":
                        payload["tags"] = sorted(set(payload.get("tags", [])) | {"possibility"})
                    if proposal.kind == "episode":
                        payload["action"] = "append"
                    association = None
                    if target is not None:
                        from hook_recall import check_association_target
                        # Reserve write time inside the same one-shot job deadline.
                        association_seconds = min(2, max(0, left()) / 3)
                        if registry_mode:
                            association_seconds = min(association_seconds,
                                max(0, registry_frozen["native_deadline_monotonic"] - time.monotonic()))
                            checked = (check_association_target(Path(selected_store.service_config), config["project_root"],
                                       target, expected_db_id=db_id, timeout=association_seconds, with_work=True,
                                       budget=budget, spent_native_bytes=aggregate_native_bytes, **routing_target)
                                       if association_seconds > 0 and aggregate_native_bytes < budget.native_read_bytes else None)
                            work = checked.get("native_work") if isinstance(checked, dict) else None
                            charged = work.get("decoded_bytes") if isinstance(work, dict) else None
                            disposition = (checked.get("outcome", "unavailable") if type(charged) is int
                                           and aggregate_native_bytes <= charged <= budget.native_read_bytes else "unavailable")
                            frozen["registry_native_bytes"] = charged if type(charged) is int else None
                        else:
                            disposition = (check_association_target(config["service_config"], config["project_root"],
                                                                    target, expected_db_id=db_id, timeout=association_seconds,
                                                                    **routing_target)
                                           if association_seconds > 0 else "unavailable")
                        association = {"target": target.native_id,
                                       "outcome": "requested" if disposition == "kept" else "omitted",
                                       "reason": disposition}
                        if registry_mode:
                            association["db_id"] = db_id
                        if disposition == "kept":
                            payload["links"] = [{"to": target.native_id, "kind": "associative", "weight": 0.5}]
                    frozen.update({"proposal_kind": proposal.kind,
                              "payload": payload, "citations": [asdict(c) for c in proposal.evidence],
                              "assessment_diagnostic": diagnostic})
                    if association is not None:
                        frozen["association"] = association
                    if routing_reason is not None:
                        frozen["routing"] = {"outcome": "included" if routing else "omitted",
                                             "reason": routing_reason}
                    if routing is not None:
                        # Receipt preserves the same ordinary note, without duplicating opaque routing metadata.
                        frozen["note_body"] = (json.loads(routing["body"])["note"]
                                               if config.get("memory_scope") == "misc" else proposal.body)
                except (ValueError, TypeError, AttributeError, KeyError):
                    frozen = {"phase": "frozen", "db_id": db_id, "assessment_diagnostic": diagnostic,
                              "save_outcome": {"status": "omitted", "reason": "proposal_refused"}}
            findings, omitted = _freeze_maintenance(maintenance, _context, db_id=db_id,
                                                     session=session, turn=job["turn_id"])
            if findings:
                frozen["maintenance"] = findings
            if omitted:
                frozen["maintenance_omitted_count"] = omitted
            def freeze(_data, current):
                if current["phase"] != "assessing":
                    return False
                if current.get("cancel_requested") or _cancelled(config, session, current):
                    _finish(_data, current, "cancelled", "interrupted_after_assessment", diagnostic=diagnostic)
                    return False
                current.update(frozen)
                retained = []
                for item in current.pop("maintenance", []):
                    candidate = {**current, "maintenance": retained + [item]}
                    # Reserve exact-row result growth; optional findings lose
                    # room before an otherwise valid ordinary SAVE.
                    reserve_bytes = sum(len(encoded(value["payload"]["expected"]))
                                        + len(encoded(value["payload"]["finding"])) + 256
                                        for value in retained + [item])
                    # SAVE's existing2048-byte receipt plus fixed status/framing
                    # must fit even when findings fill the remaining job envelope.
                    save_reserve = 2304 if "payload" in current else 0
                    projected = normalize_context_diagnostic_fields(candidate,
                        max_bytes=MAX_JOB_BYTES - 256 - save_reserve - reserve_bytes)
                    for field in ("context_diagnostic", "context_diagnostic_omitted"):
                        current.pop(field, None)
                        if field in projected:
                            current[field] = projected[field]
                    candidate = projected
                    if len(encoded(candidate)) + reserve_bytes > MAX_JOB_BYTES - 256 - save_reserve:
                        current["maintenance_omitted_count"] = current.get("maintenance_omitted_count", 0) + 1
                    else:
                        retained.append(item)
                if retained:
                    current["maintenance"] = retained
                if len(encoded(current)) > MAX_JOB_BYTES:
                    _normalize_context_in_place(current, max_bytes=MAX_JOB_BYTES)
                if len(encoded(current)) > MAX_JOB_BYTES:
                    current.pop("observation", None)
                if len(encoded(current)) > MAX_JOB_BYTES:
                    current.pop("assessment_diagnostic", None)
                if len(encoded(current)) > MAX_JOB_BYTES:
                    current.pop("association", None)
                return True
            ok, changed = _update(config, session, key, freeze)
            if not ok or not changed:
                return True
            ok, refreshed = _update(config, session, key, lambda _d, current: json.loads(json.dumps(current)))
            if not ok or refreshed is None:
                return True
            job = refreshed
        if _is_nomination_job(job):
            _publish_nomination(config, session, key)
            return True
        # Each branch has a frozen intent and independent acknowledgement.
        pending = (["save"] if "payload" in job and "save_outcome" not in job else [])
        pending += [index for index,item in enumerate(job.get("maintenance", [])) if item["status"] == "pending"]
        for branch in pending:
            if "frozen_selection" in job:
                from librarian_policy import resolve
                native_charge = job.get("registry_native_bytes")
                if type(native_charge) is not int or not 0 <= native_charge < resolve(config).native_read_bytes:
                    finish("deferred", "native_read_budget"); return True
            if (("frozen_selection" in job or job.get("destination") == "global_preference")
                    and time.monotonic() >= job.get("native_deadline_monotonic", 0)):
                finish("deferred", "native_deadline"); return True
            if (_cancelled(config, session, job) or _remaining(job) <= 0
                    or not _unchanged(config, job)):
                finish("cancelled" if _cancelled(config, session, job) else "deferred", "deadline_or_config_changed")
                return True
            def intent(_data, current):
                if current["phase"] != "frozen" or _cancelled(config, session, current):
                    return False
                # Older frozen jobs may be packed to their former byte ceiling.
                # Display-only prose cannot prevent a durable external intent.
                if len(encoded(current)) > MAX_JOB_BYTES - 4096:
                    _normalize_context_in_place(current, max_bytes=MAX_JOB_BYTES - 4096)
                    current.pop("assessment_diagnostic",None)
                    current.pop("observation",None)
                current["phase"] = "write_intent"
                current["active_branch"] = branch
                return True
            ok, claimed = _update(config, session, key, intent)
            if not ok or not claimed:
                return True
            if (_cancelled(config, session, job) or _remaining(job) <= 0
                    or not _unchanged(config, job)):
                finish("cancelled" if _cancelled(config, session, job) else "deferred", "interrupted_before_native_send")
                return True
            if branch == "save":
                try:
                    write_job = {k:v for k,v in job.items() if k != "observation"}
                    timeout = _remaining(job)
                    if "frozen_selection" in job or job.get("destination") == "global_preference":
                        timeout = min(timeout, job["native_deadline_monotonic"] - time.monotonic())
                    receipt = (native_write or _native_write)(config, write_job, timeout)
                    if (not isinstance(receipt, dict) or receipt.get("readback_status") != "verified"
                            or receipt.get("db") != job.get("db_alias", "project") or receipt.get("db_id") != job["db_id"]):
                        outcome = {"status": "unresolved", "reason": "native_receipt_unverified"}
                    else:
                        small = {k:receipt[k] for k in ("db","db_id","id","episode_id","edition_id","revision","replayed","readback_status") if k in receipt}
                        outcome = ({"status":"verified","reason":"native_verified","native":small}
                                   if len(encoded(small)) <= 2048 else {"status":"unresolved","reason":"native_receipt_cap"})
                        if outcome["status"] == "verified" and "frozen_selection" in job:
                            try:
                                from registry_runtime import selection, config_for_store
                                from stewardship import enqueue
                                selected = selection(config)
                                store = selected.named(job["destination"])
                                if store.maintenance.enabled:
                                    ids = [small[k] for k in ("id", "edition_id") if isinstance(small.get(k), str)]
                                    enqueue(config_for_store(config, store), job["db_id"], ids)
                            except Exception:
                                pass  # Durable SAVE remains successful after an optional queue miss.
                        elif (outcome["status"] == "verified" and config.get("tag_stewardship") is True
                                and job.get("destination", "project") == "project"):
                            # Native acceptance is already durable; a failed
                            # optional queue handoff cannot make it unsuccessful.
                            try:
                                from stewardship import enqueue
                                ids = [small[k] for k in ("id", "edition_id") if isinstance(small.get(k), str)]
                                enqueue(config, job["db_id"], ids)
                            except Exception:
                                pass
                except Exception as error:
                    from registry_recording import ReadBudgetRefused
                    outcome = ({"status":"deferred","reason":"native_read_budget"}
                               if "frozen_selection" in job and isinstance(error, ReadBudgetRefused)
                               else {"status":"unresolved","reason":"native_write_ambiguous"})
                    details = getattr(error,"details",None)
                    accepted = details.get("accepted") if isinstance(details,dict) else None
                    if isinstance(accepted,dict):
                        small = {k:accepted[k] for k in ("db","db_id","id","episode_id","edition_id","revision","replayed","readback_status","retryable") if k in accepted}
                        if len(encoded(small)) <= 2048:
                            outcome["native"] = small
                if not job.get("maintenance"):
                    # Named historical note-only branch retains its receipt and
                    # uncertainty semantics, including maximally packed old jobs.
                    finish(outcome["status"],outcome["reason"],outcome.get("native"))
                    return True
                job["save_outcome"] = outcome
            else:
                item = job["maintenance"][branch]
                try:
                    receipt = (native_maintenance or _native_maintenance)(config,job,item,_remaining(job))
                    receipt = _checked_maintenance_result(receipt,job,item)
                    outcome = {"status":receipt["outcome"]["status"],"native":receipt}
                    if receipt["outcome"]["status"] == "refused":
                        outcome["reason"] = receipt["outcome"]["reason"]
                    projected = {**job,"maintenance":[{**value,**outcome} if index==branch else value
                                 for index,value in enumerate(job["maintenance"])]}
                    if len(encoded(projected)) > MAX_JOB_BYTES - 256:
                        outcome.pop("native")
                        outcome.update(native_result_sha256=digest(encoded(receipt)),details_omitted=True)
                except Exception:
                    outcome = {"status":"unresolved","reason":"native_maintenance_ambiguous"}
                item.update(outcome)
            def progress(_data,current):
                if current["phase"] != "write_intent" or current.get("active_branch") != branch:
                    return False
                if branch == "save":
                    current["save_outcome"] = outcome
                else:
                    current["maintenance"][branch].update(outcome)
                current["phase"] = "frozen"
                current.pop("active_branch",None)
                return True
            ok,saved = _update(config,session,key,progress)
            if not ok or not saved:
                return True  # Persisted intent stays ambiguous, never replay it.
        outcomes = [job["save_outcome"]] if "save_outcome" in job else []
        outcomes += job.get("maintenance", [])
        unresolved = next((item for item in outcomes if item["status"] == "unresolved"),None)
        native = job.get("save_outcome",{}).get("native")
        if unresolved:
            finish("unresolved",unresolved["reason"],native)
        elif any(item["status"] in ("verified","applied","unchanged") for item in outcomes):
            finish("verified","native_verified",native)
        else:
            finish("deferred","maintenance_refused" if outcomes else "no_frozen_intent",native)
    except Exception:
        # Never erase a reservation/write intent when an unexpected error occurs.
        def failure(data, current):
            _finish(data, current, "unresolved" if current["phase"] in ("assessing", "write_intent") else "deferred", "worker_failure")
        _update(config, session, key, failure)
    return True


def drain_status(config, session_ids):
    result = {"schema": "mneme." + capture_source.provenance(config) + "-recording.drain.v1", "pending": 0, "unresolved": 0,
              "usage_unknown": False, "counts": {k: 0 for k in COUNTERS}, "ledgers": [], "truncated": False}
    if "frozen_selection" not in config and not enabled(config):
        return result
    if not isinstance(session_ids, (list, tuple)) or len(session_ids) > MAX_LEDGERS:
        result.update(truncated=True, usage_unknown=True); return result
    for session in dict.fromkeys(session_ids):
        def status(data):
            pending = sum(j["phase"] != "unresolved" for j in data["jobs"])
            unresolved = len(data["jobs"]) - pending
            return {"session_id": session, "pending": pending, "unresolved": unresolved,
                    "counts": dict(data["counts"]), "usage_unknown": data.get("usage_unknown") is True}, False
        ok, row = _transaction(config, session, status)
        if not ok:
            result.update(truncated=True, usage_unknown=True)
        elif row is not None:
            result["pending"] += row["pending"]; result["unresolved"] += row["unresolved"]
            result["usage_unknown"] |= row.pop("usage_unknown")
            result["ledgers"].append(row)
            for key in COUNTERS:
                result["counts"][key] += row["counts"][key]
    return result


def export_receipts(config, session_ids):
    """Bounded private harness projection; never actor context or transcript export."""
    result = {"schema": "mneme." + capture_source.provenance(config) + "-recording.receipts.v1", "receipts": [],
              "truncated": False, "unknown": False}
    if "frozen_selection" not in config and not enabled(config):
        return result
    if not isinstance(session_ids, (list, tuple)) or len(session_ids) > MAX_LEDGERS:
        result.update(truncated=True, unknown=True)
        return result
    diagnostics = []
    diagnostics_omitted = False
    for session in dict.fromkeys(session_ids):
        ok, value = _transaction(config, session, lambda d: (
            {"receipts": d["receipts"] + [_partial_receipt(j) for j in d["jobs"]
                 if j.get("phase") == "unresolved" and j.get("maintenance")],
             "omissions": d.get("receipt_omissions", 0),
             "pending": bool(d["jobs"]),
             "diagnostics": [{"session_id": session, "source_key": j.get("key"),
                 "phase": "unresolved", "reason": j.get("reason"),
                 "ledger_usage_unknown": d.get("usage_unknown") is not False,
                 **({"assessment_diagnostic": j["assessment_diagnostic"]}
                    if "assessment_diagnostic" in j else {}),
                 **{key:j[key] for key in ("context_diagnostic", "context_diagnostic_omitted") if key in j}}
                 for j in d["jobs"] if j.get("phase") == "unresolved"]}, False))
        if not ok or value is None:
            result["unknown"] = True
            continue
        result["unknown"] |= value["pending"]
        result["truncated"] |= bool(value["omissions"])
        remaining = MAX_UNRESOLVED_DIAGNOSTICS - len(diagnostics)
        diagnostics_omitted |= len(value["diagnostics"]) > remaining
        diagnostics.extend(value["diagnostics"][:remaining])
        for receipt in value["receipts"]:
            item = {"session_id": session, **receipt}
            item = normalize_context_diagnostic_fields(item, max_bytes=MAX_RECEIPT_BYTES)
            if "assessment_diagnostic" in item:
                item["assessment_diagnostic"] = sanitize_assessment_diagnostic(item["assessment_diagnostic"])
                if len(encoded(item)) > MAX_RECEIPT_BYTES:
                    item.pop("assessment_diagnostic")
            if "observation" in item and len(encoded(item)) > MAX_RECEIPT_BYTES:
                item.pop("observation")
            if len(result["receipts"]) >= MAX_RECEIPTS or len(encoded(item)) > MAX_RECEIPT_BYTES:
                result["truncated"] = True
                continue
            result["receipts"].append(item)
            if len(encoded(result)) > MAX_RECEIPTS_BYTES:
                for retained in result["receipts"]:
                    _normalize_context_in_place(retained, force_omit=True)
                if len(encoded(result)) > MAX_RECEIPTS_BYTES:
                    for retained in result["receipts"]:
                        retained.pop("context_diagnostic_omitted", None)
            if len(encoded(result)) > MAX_RECEIPTS_BYTES:
                for retained in result["receipts"]:
                    retained.pop("assessment_diagnostic", None)
                    retained.pop("observation", None)
                if len(encoded(result)) > MAX_RECEIPTS_BYTES:
                    result["receipts"].pop()
                    result["truncated"] = True
    if diagnostics or diagnostics_omitted:
        result = normalize_unresolved_export(result, diagnostics, omitted=diagnostics_omitted)
    return result


def guidance_receipt(snapshot):
    """Qualified revision evidence, not duplicated authored guide prose."""
    from guidance import validate_snapshot
    value = validate_snapshot(snapshot)
    roles = {}
    for role, row in value["roles"].items():
        witness = row.get("witness")
        roles[role] = {"outcome": row["outcome"],
            **({"selector": row["selector"]} if "selector" in row else {}),
            **({"witness": {key: val for key, val in witness.items() if key != "text"}}
               if witness is not None else {})}
    return {"schema": value["schema"], "db": value["db"], "db_id": value["db_id"],
            "owner_binding_sha256": digest(encoded(value["owner_binding"])), "roles": roles,
            "save_revalidation": "host_non_atomic"}

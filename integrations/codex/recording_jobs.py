"""Bounded, opt-in source-turn recording lane; never a second worker or transcript archive.

Terminal jobs compact to a monotonic source-start watermark. Unfinished/ambiguous
jobs, not completed turns, consume capacity. Fresh startup can admit its first
already-started open turn; retired resumes baseline existing bytes and cannot
rediscover old completed turns. A provider reservation or write intent is never
replayed after uncertainty. Public observations live only in the worker's RAM.
"""
from __future__ import annotations

from dataclasses import asdict
import fcntl
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
import tempfile
import time

from rollout_primitives import decode_json, digest, encoded, read_regular
from turn_observer import (Limits, RecordAnchor, TurnAdmission, admit_source_turn,
                           observation_byte_ceiling, observe_source_turn,
                           validate_delivery_packet)

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
COUNTERS = ("admitted", "assessed", "abstained", "verified", "deferred", "cancelled", "unresolved", "capacity")
ID = re.compile(r"[A-Za-z0-9_:.-]{1,160}\Z")
ULID = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z")


def enabled(config):
    if config.get("recording_mode") != "automatic":
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
    return Path(config["state_dir"]) / "recording"


def _path(config, session):
    return _directory(config) / (digest(session.encode()) + ".json")


def _cancel_path(config, session):
    return _path(config, session).with_suffix(".cancel")


def _cancel_marker(config, session):
    try:
        value = decode_json(read_regular(_cancel_path(config, session), 256))
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


def _signal_cancel(config, session, turn):
    """Fence outside the optional queue lock; contention must not lose Interrupt."""
    path = _cancel_path(config, session)
    # No admitted work exists without its ledger; avoid lifetime orphan markers.
    if not _path(config, session).exists():
        return
    fd, temporary = tempfile.mkstemp(prefix=".cancel-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(encoded({"token": secrets.token_hex(32), "turn_id": turn}))
            stream.flush(); os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def _cancelled(config, session, job):
    marker = _cancel_marker(config, session)
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
            directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        except FileNotFoundError:
            return None
        try:
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


def _config_digest(config):
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
                           "memory_scope": config.get("memory_scope", "project")}
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
        if config.get("memory_scope") == "misc":
            from target_policy import misc_policy
            policy = misc_policy(config)
            return (job.get("workspace_binding") == config["workspace_binding"]
                    and job.get("store_target") == policy.canonical()
                    and job.get("db_alias") == policy.db_alias)
        return True
    except (OSError, ValueError, TypeError, KeyError):
        return False


def _valid(data, session):
    return (isinstance(data, dict) and data.get("schema") == SCHEMA and data.get("session_id") == session
            and isinstance(data.get("jobs"), list) and len(data["jobs"]) <= MAX_JOBS
            and all(isinstance(j, dict) and len(encoded(j)) <= MAX_JOB_BYTES for j in data["jobs"])
            and type(data.get("watermark")) is int and data["watermark"] >= -1
            and type(data.get("fresh")) is bool and type(data.get("ended")) is bool
            and isinstance(data.get("counts"), dict) and set(data["counts"]) == set(COUNTERS)
            and all(type(v) is int and 0 <= v <= 1_000_000 for v in data["counts"].values())
            and isinstance(data.get("receipts"), list) and len(data["receipts"]) <= MAX_RECEIPTS)


def _load(path, session):
    raw = read_regular(path, MAX_STATE)
    if len(raw) > MAX_STATE:
        raise ValueError("recording_state_cap")
    data = decode_json(raw)
    if not _valid(data, session):
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
    fd, temporary = tempfile.mkstemp(prefix=".recording-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(raw); stream.flush(); os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def _new(session, fresh=False):
    return {"schema": SCHEMA, "session_id": session, "pin": None, "watermark": -1,
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


def _transaction(config, session, mutate, *, create=False, fresh=False):
    """Single nonblocking directory lock also serializes bounded ledger retirement."""
    if not enabled(config) or not isinstance(session, str) or not ID.fullmatch(session):
        return False, None
    fd = None
    try:
        directory = _directory(config)
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        fd = os.open(directory / ".lock", os.O_CREAT | os.O_RDWR | getattr(os, "O_NOFOLLOW", 0), 0o600)
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        path = _path(config, session)
        if path.exists():
            data = _load(path, session)
        elif create:
            entries = list(itertools.islice(directory.iterdir(), 130))
            scan_full = len(entries) >= 130
            # Cancellation can race clean retirement. Remove only orphan markers,
            # never a fence belonging to outstanding or ambiguous work.
            for candidate in list(entries):
                if (re.fullmatch(r"[a-f0-9]{64}\.cancel", candidate.name)
                        and not candidate.with_suffix(".json").exists()):
                    candidate.unlink(); entries.remove(candidate)
            if scan_full:
                return False, None
            ledgers = [p for p in entries if re.fullmatch(r"[a-f0-9]{64}\.json", p.name)]
            if len(ledgers) >= MAX_LEDGERS:
                for candidate in ledgers:
                    try:
                        value = decode_json(read_regular(candidate, MAX_STATE))
                        if (_valid(value, value.get("session_id")) and value["ended"] and not value["jobs"]):
                            candidate.unlink(); candidate.with_suffix(".cancel").unlink(missing_ok=True)
                            ledgers.remove(candidate)
                            break
                    except (OSError, ValueError, TypeError):
                        continue
                if len(ledgers) >= MAX_LEDGERS:
                    return False, None
            data = _new(session, fresh)
        else:
            return True, None
        bound = _bind_workspace(config, data)
        value, changed = mutate(data)
        if changed or bound:
            _write(path, data)
        return True, value
    except (OSError, ValueError, TypeError, KeyError, UnicodeError, RecursionError):
        return False, None
    finally:
        if fd is not None:
            os.close(fd)


def _pin(admission):
    value = asdict(admission)
    return {k: value[k] for k in ("path", "sessions_root", "session_id", "device", "inode", "session_record")}


def _admission(value):
    return TurnAdmission(**{**value, "session_record": RecordAnchor(**value["session_record"]),
                            "start_record": RecordAnchor(**value["start_record"])})


def _count(data, name):
    data["counts"][name] = min(1_000_000, data["counts"][name] + 1)


def _boot_id():
    """Local OS boot witness; missing witness means no renewable deadline."""
    try:
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
    from reader_runtime import assessment_diagnostic
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


def _partial_receipt(job):
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
        return
    data["jobs"].remove(job)
    summary = {"source_key": job["key"], "turn_id": job["turn_id"], "outcome": outcome, "reason": reason}
    admission = job["admission"]
    summary["source"] = {"session_id": admission["session_id"], "turn_id": job["turn_id"],
                         "prompt_sha256": admission["expected_prompt_sha256"],
                         "start_ref": admission["start_record"]}
    if "closure_ref" in job:
        summary["closure_ref"] = job["closure_ref"]
    if "destination" in job:
        summary["destination"] = job["destination"]
        summary["target"] = job.get("store_target", {
            "scope": job["destination"], "db_alias": job.get("db_alias", "project"), "db_id": job["db_id"]})
    if "payload" in job:
        summary["proposal"] = {"kind": job["proposal_kind"], "summary": job["payload"]["summary"],
                               "body": job.get("note_body", job["payload"]["body"])}
        summary["citations"] = job["citations"]
    if "save_outcome" in job:
        summary["save_outcome"] = {k:v for k,v in job["save_outcome"].items() if k != "native"}
    if "maintenance" in job:
        summary["maintenance"] = [_maintenance_summary(item) for item in job["maintenance"]]
    if "intent_omissions" in job:
        summary["intent_omissions"] = job["intent_omissions"]
    if "maintenance_omitted_count" in job:
        summary["maintenance_omitted_count"] = job["maintenance_omitted_count"]
    if "association" in job:
        summary["association"] = job["association"]
    if "routing" in job:
        summary["routing"] = job["routing"]
    if "observation" in job:
        summary["observation"] = job["observation"]
    for key in ("context_diagnostic", "context_diagnostic_omitted"):
        if key in job:
            summary[key] = job[key]
    if receipt:
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
        return
    if len(data["receipts"]) >= MAX_RECEIPTS:
        data["receipt_omissions"] = data.get("receipt_omissions", 0) + 1
    data["receipts"] = (data["receipts"] + [summary])[-MAX_RECEIPTS:]


def session_start(config, event):
    """Remember trusted lifecycle meaning, not a claim that EOF precedes task_start."""
    source = event.get("source")
    if source not in ("startup", "resume", "clear", "compact"):
        return
    def start(data):
        # Existing state never receives another fresh-first-turn exception.
        data["ended"] = False
        return None, True
    _transaction(config, event.get("session_id"), start, create=True, fresh=source == "startup")


def notice(config, event):
    """Foreground: local file/descriptor work only. Never native/provider calls."""
    # Supporting callers must preserve the same root-turn boundary as hooks.
    # Children can share session_id; never inspect/admit their source or state.
    if event.get("agent_id") or event.get("agent_type"):
        return {"outcome": "skipped"}
    if not enabled(config):
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
        marker = _cancel_marker(config, session)
        if marker is None or marker["turn_id"] == turn:
            return {"outcome": "deferred", "reason": "cancelled_or_invalid_fence"}
        root = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))) / "sessions"
        result = admit_source_turn(event.get("transcript_path"), sessions_root=root,
                                  session_id=session, turn_id=turn, expected_prompt_sha256=digest(prompt.encode()))
        if result["status"] != "admitted":
            return {"outcome": "deferred", "reason": result["reason"]}
        admission = result["admission"]
        current = observe_source_turn(admission, limits=Limits(scan_bytes=256 * 1024, events=512))
        if current["reason"] != "source_turn_not_closed":
            return {"outcome": "deferred", "reason": "not_current_open_turn"}
        snapshot_end = admission.start_record.byte_offset + current["work"]["source_bytes_read"]
        binding = _config_digest(config)
        key = digest(encoded([session, turn, admission.expected_prompt_sha256]))
        pin = _pin(admission)
        def accept(data):
            if _cancel_marker(config, session) != marker:
                return "cancelled_or_changed_fence", False
            for job in data["jobs"]:
                if job.get("turn_id") == turn:
                    return ("replay" if job["key"] == key else "identity_conflict"), False
            if data["pin"] is not None and data["pin"] != pin:
                return "source_changed", False
            if data["ended"]:
                return "ended", False
            first = (data["fresh"] and admission.start_record.byte_offset == admission.session_record.line_bytes)
            if data["pin"] is None:
                data["pin"] = pin
                if not first:
                    data["watermark"] = snapshot_end
                    data["fresh"] = False
                    return "baseline_only", True
            if not first and admission.start_record.byte_offset <= data["watermark"]:
                return "replay", False
            data["fresh"] = False
            data["watermark"] = max(data["watermark"], admission.start_record.byte_offset)
            if len(data["jobs"]) >= MAX_JOBS:
                _count(data, "capacity")
                return "capacity", True
            data["jobs"].append({"key": key, "turn_id": turn, "admission": asdict(admission),
                                 "cancel_token": marker["token"],
                                 "config_sha256": binding, "phase": "admitted", "polls": 0,
                                 "bytes_read": 0, "records_parsed": 0})
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


def close_turn(config, session, turn=None, *, cancel=False, ended=False):
    if (not enabled(config) or not isinstance(session, str) or not ID.fullmatch(session)
            or (turn is not None and (not isinstance(turn, str) or not ID.fullmatch(turn)))):
        return
    if cancel:
        try:
            _signal_cancel(config, session, turn)
        except (OSError, ValueError):
            # Still attempt the normal locked cancellation. A failed fence write
            # cannot be represented as successful durable cancellation.
            pass
    now = time.time()
    monotonic, boot = time.monotonic(), _boot_id()
    def close(data):
        if ended:
            data["ended"] = True
        for job in list(data["jobs"]):
            if turn is not None and job["turn_id"] != turn:
                continue
            if cancel and job["phase"] == "assessing":
                job["cancel_requested"] = True
            elif cancel and job["phase"] not in ("write_intent", "unresolved"):
                _finish(data, job, "cancelled", "interrupted")
            elif job["phase"] == "admitted":
                if boot is None:
                    _finish(data, job, "deferred", "clock_witness_unavailable")
                else:
                    job.update(phase="observing", closed_at=now, deadline=now + CLOSE_SECONDS, next_poll=now,
                               closed_monotonic=monotonic, monotonic_deadline=monotonic + CLOSE_SECONDS, boot_id=boot)
        return None, True
    _transaction(config, session, close)


def has_work(config, session):
    ok, value = _transaction(config, session, lambda d: (any(j["phase"] != "unresolved" for j in d["jobs"]), False))
    return bool(ok and value)


def _update(config, session, key, mutation):
    def change(data):
        job = next((j for j in data["jobs"] if j["key"] == key), None)
        if job is None:
            return None, False
        return mutation(data, job), True
    return _transaction(config, session, change)


def _native_write(config, job, timeout):
    from mcp_client import McpClient
    from service import load_config
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
    if destination == "global_preference":
        from target_policy import GLOBAL_PREFERENCE_NAMESPACE, GLOBAL_PREFERENCE_TAG
        from recording_contract import MAX_PREFERENCE_SUMMARY_BYTES, MAX_PREFERENCE_BODY_BYTES
        payload = job["payload"]
        source = payload.get("source", {})
        if (set(payload) != {"source", "summary", "body", "tags"}
                or payload.get("tags") != [GLOBAL_PREFERENCE_TAG]
                or set(source) != {"namespace", "key", "reference"}
                or source.get("namespace") != GLOBAL_PREFERENCE_NAMESPACE
                or not isinstance(source.get("key"), str) or not re.fullmatch(r"[0-9a-f]{64}", source["key"])
                or source.get("reference") != "codex-global-preference://sha256/" + source["key"]
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
        if (destination == "global_preference" or config.get("memory_scope", "project") not in ("project", "misc")
                or job["payload"].get("tags") != ["possibility"]
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
        receipt = client.save_verified(policy.db_alias if policy is not None else "project", payload, expected_db_id=job["db_id"])
        if _config_digest(config) != job["config_sha256"]:
            raise ValueError("config_changed_after_write")
        return receipt
    finally:
        client.timeout = min(.1, max(.001, deadline - time.monotonic()))
        client.close()


def _native_maintenance(config, job, item, timeout):
    if job.get("destination") == "global_preference":
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


def step(config, session, runtime, reserve, account, *, native_write=None, native_maintenance=None):
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
        if job["phase"] == "observing":
            # Charge worst-case parser work before touching the source. A crash
            # or failed progress write cannot refund an already performed poll.
            limits = Limits()
            admission = _admission(job["admission"])
            byte_charge = observation_byte_ceiling(admission, limits=limits)
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
            observation = observe_source_turn(admission, limits=limits, delivery=job.get("delivery"))
            job["bytes_read"] += observation["work"]["bytes_read"]
            job["records_parsed"] += observation["work"]["records_parsed"]
            def progress(_data, current):
                if current["phase"] != "observing" or current["polls"] != job["polls"]:
                    return False
                current.update({k: job[k] for k in ("polls", "bytes_read", "records_parsed")})
                if observation["status"] == "complete":
                    current["closure_ref"] = observation["boundary"]
                return True
            ok, saved = _update(config, session, key, progress)
            if not ok or not saved:
                return True
            if (observation["work"]["bytes_read"] > byte_charge
                    or observation["work"]["records_parsed"] > limits.events
                    or job["bytes_read"] > MAX_SCAN_WORK
                    or job["records_parsed"] > MAX_EVENT_WORK):
                finish("deferred", "observation_work_cap"); return True
            if observation["status"] != "complete":
                retry = (observation["reason"] in ("source_turn_not_closed", "unflushed_record")
                         and job["polls"] < MAX_POLLS and time.time() < job["closed_at"] + FLUSH_SECONDS)
                if retry:
                    _update(config, session, key, lambda _d, j: j.update(next_poll=time.time() + .25 * job["polls"]))
                else:
                    finish("deferred", observation["reason"])
                return True
            context_diagnostic = _context_diagnostic(observation)
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
            prepared, _context = prepare(observation, overlap["cards"], budget=budget, **scope_options)
            if prepared is None:
                diagnostic = sanitize_assessment_diagnostic({"runtime_reason": _context,
                                                            "validation_reason": "unknown"})
                finish("deferred", "assessment_input_refused"); return True
            context_diagnostic = _context_diagnostic(observation, _context)
            selected_coverage = json.loads(_context.coverage_json)["public_evidence"]
            observation_summary = {"source_turn": "closed_verified", **selected_coverage}
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
            if not reserve(key):
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
            from reader_runtime import assessment_diagnostic
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
            if result.get("reason") != "proposed" or (proposal is None and not maintenance):
                finish("deferred", "assessment_refused"); return True
            destination = proposal.destination if proposal is not None else "project"
            if destination not in ("project", "global_preference"):
                finish("deferred", "assessment_refused"); return True
            if destination == "global_preference":
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
            if preference_policy is not None:
                frozen["destination"] = destination
            if destination == "global_preference":
                frozen.update(db_alias=preference_policy.db_alias, store_target=preference_policy.canonical(),
                              native_deadline_monotonic=global_native_deadline)
            if proposal is not None:
                try:
                    from recording_contract import AssociationTarget
                    target = proposal.associate_with
                    if proposal.kind == "possibility":
                        if (destination != "project" or config.get("memory_scope", "project") not in ("project", "misc")
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
                    payload = {"source": {"namespace": "codex-acquisition.v1", "key": key,
                               "reference": f"codex://{session}/{job['turn_id']}", "session": session},
                               "summary": proposal.summary, "body": proposal.body}
                    if destination == "global_preference":
                        from target_policy import GLOBAL_PREFERENCE_NAMESPACE, GLOBAL_PREFERENCE_TAG
                        coordinate = {"session": session, "turn": job["turn_id"], "key": key,
                                      "ordinals": [json.loads(c.source_ref_json)["ordinal"] for c in proposal.evidence]}
                        payload["source"] = {"namespace": GLOBAL_PREFERENCE_NAMESPACE,
                            "key": digest(encoded(coordinate)),
                            "reference": "codex-global-preference://sha256/" + digest(encoded(coordinate))}
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
                        payload["tags"] = ["possibility"]
                    if proposal.kind == "episode":
                        payload["action"] = "append"
                    association = None
                    if target is not None:
                        from hook_recall import check_association_target
                        # Reserve write time inside the same one-shot job deadline.
                        budget = min(2, max(0, left()) / 3)
                        disposition = (check_association_target(config["service_config"], config["project_root"],
                                                                target, expected_db_id=db_id, timeout=budget,
                                                                **routing_target)
                                       if budget > 0 else "unavailable")
                        association = {"target": target.native_id,
                                       "outcome": "requested" if disposition == "kept" else "omitted",
                                       "reason": disposition}
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
        # Each branch has a frozen intent and independent acknowledgement.
        pending = (["save"] if "payload" in job and "save_outcome" not in job else [])
        pending += [index for index,item in enumerate(job.get("maintenance", [])) if item["status"] == "pending"]
        for branch in pending:
            if (job.get("destination") == "global_preference"
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
                    if job.get("destination") == "global_preference":
                        timeout = min(timeout, job["native_deadline_monotonic"] - time.monotonic())
                    receipt = (native_write or _native_write)(config, write_job, timeout)
                    if (not isinstance(receipt, dict) or receipt.get("readback_status") != "verified"
                            or receipt.get("db") != job.get("db_alias", "project") or receipt.get("db_id") != job["db_id"]):
                        outcome = {"status": "unresolved", "reason": "native_receipt_unverified"}
                    else:
                        small = {k:receipt[k] for k in ("db","db_id","id","episode_id","edition_id","revision","replayed","readback_status") if k in receipt}
                        outcome = ({"status":"verified","reason":"native_verified","native":small}
                                   if len(encoded(small)) <= 2048 else {"status":"unresolved","reason":"native_receipt_cap"})
                except Exception as error:
                    outcome = {"status":"unresolved","reason":"native_write_ambiguous"}
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
    result = {"schema": "mneme.codex-recording.drain.v1", "pending": 0, "unresolved": 0,
              "usage_unknown": False, "counts": {k: 0 for k in COUNTERS}, "ledgers": [], "truncated": False}
    if not enabled(config):
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
    result = {"schema": "mneme.codex-recording.receipts.v1", "receipts": [],
              "truncated": False, "unknown": False}
    if not enabled(config):
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

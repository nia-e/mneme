"""Best-effort metadata receipts; never an authority or operational ledger.

Only this module writes the diagnostic namespace. Failures, unknown formats and
contention disable observation, not memory work. No operational helper imports.
"""
from __future__ import annotations

import contextlib
import contextvars
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import time

import host_files
import host_platform

SCHEMA = "mneme.session-activity.v1"
CONFIG_SCHEMA = "mneme.session-activity.config.v1"
QUOTA_SCHEMA = "mneme.session-activity.quota.v1"
RECORD_BYTES = 64 * 1024
PAYLOAD_BYTES = 64 * 1024 * 1024
QUOTA_BYTES = 256 * 1024
RETENTION_SECONDS = 30 * 24 * 60 * 60
ADMISSION_BYTES = 8 * 1024 * 1024
ADMISSION_ENTRIES = 4096
ADMISSION_SECONDS = 0.25
DEFER_BYTES = 256 * 1024
DEFER_SECONDS = 0.25
_DEFERRED = contextvars.ContextVar("mneme_activity_batch", default=None)
LOCK = ".activity.lock"
QUOTA = ".activity-quota.json"
STAGE = ".activity-stage"
KINDS = frozenset(("lifecycle", "core", "recall_admission", "recall_selection",
                   "delivery_consumed", "hook_output", "host_admission", "recording", "checkpoint"))
_CODE = re.compile(r"[a-z][a-z0-9_]{0,63}\Z", re.ASCII)
_HEX = re.compile(r"[a-f0-9]{64}\Z", re.ASCII)
_GENERATION = re.compile(r"[A-Za-z0-9_.:-]{1,128}\Z", re.ASCII)
_FILENAME = re.compile(r"codex-[a-f0-9]{64}\.json\Z", re.ASCII)
_RECORD_FIELDS = frozenset(("schema", "source", "session_id", "workspace", "cwd", "device",
                            "first_observed", "last_observed", "events", "evicted_events"))
_EVENT_FIELDS = frozenset(("key", "at", "kind", "outcome", "reason", "origin", "owner", "nodes",
                           "count", "omitted", "selection_digest"))
_ORIGIN_FIELDS = frozenset(("turn", "generation", "context_generation", "attempt"))


def _require(condition):
    if not condition:
        raise ValueError("invalid_activity_metadata")


def _text(value, limit, *, nullable=False):
    if nullable and value is None:
        return
    _require(isinstance(value, str) and 0 < len(value) <= limit
             and all(ord(c) >= 32 and not 127 <= ord(c) <= 159 for c in value))
    _require(len(value.encode("utf-8")) <= limit)


def _uint(value, *, nullable=False):
    _require((nullable and value is None) or (type(value) is int and 0 <= value <= 2**64 - 1))


def _path(value):
    _text(value, 4096)
    _require(Path(value).is_absolute())


def _generation(value):
    if isinstance(value, str):
        _require(bool(_GENERATION.fullmatch(value)))
    else:
        _uint(value, nullable=True)


def _validate_event(event):
    _require(type(event) is dict and event.keys() == _EVENT_FIELDS)
    _text(event["key"], 256)
    _uint(event["at"])
    _require(event["kind"] in KINDS and isinstance(event["outcome"], str)
             and bool(_CODE.fullmatch(event["outcome"])))
    _require(event["reason"] is None or (isinstance(event["reason"], str)
             and bool(_CODE.fullmatch(event["reason"]))))
    origin = event["origin"]
    _require(type(origin) is dict and origin.keys() == _ORIGIN_FIELDS)
    _text(origin["turn"], 256, nullable=True)
    _text(origin["attempt"], 256, nullable=True)
    _generation(origin["generation"])
    _generation(origin["context_generation"])
    owner = event["owner"]
    if owner is not None:
        _require(type(owner) is dict and owner.keys() == {"name", "id", "db"})
        for value in owner.values():
            _require(isinstance(value, str) and bool(_GENERATION.fullmatch(value)))
    _require(type(event["nodes"]) is list and len(event["nodes"]) <= 256)
    _require(not event["nodes"] or owner is not None)
    for node in event["nodes"]:
        _require(isinstance(node, str) and bool(_GENERATION.fullmatch(node)))
    _uint(event["count"], nullable=True)
    _uint(event["omitted"])
    _require(event["selection_digest"] is None or (isinstance(event["selection_digest"], str)
             and bool(_HEX.fullmatch(event["selection_digest"]))))


def _validate_record(record):
    _require(type(record) is dict and record.keys() == _RECORD_FIELDS)
    _require(record["schema"] == SCHEMA and record["source"] == "codex")
    _text(record["session_id"], 256)
    _path(record["workspace"])
    _path(record["cwd"])
    _text(record["device"], 128, nullable=True)
    _uint(record["first_observed"])
    _uint(record["last_observed"])
    _uint(record["evicted_events"])
    _require(record["first_observed"] <= record["last_observed"])
    _require(type(record["events"]) is list and len(record["events"]) <= 512)
    keys = set()
    for event in record["events"]:
        _validate_event(event)
        _require(record["first_observed"] <= event["at"] <= record["last_observed"]
                 and event["key"] not in keys)
        keys.add(event["key"])


def _encode(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True,
                      allow_nan=False).encode("utf-8")


def _unique_object(pairs):
    result = {}
    for key, value in pairs:
        _require(key not in result)
        result[key] = value
    return result


def _integer(text):
    _require(len(text) <= 20 and not text.startswith("-"))
    value = int(text)
    _uint(value)
    return value


def _decode(data):
    # Bound nesting before handing input to Python's recursive JSON decoder.
    depth = 0
    quoted = escaped = False
    for byte in data:
        if quoted:
            if escaped:
                escaped = False
            elif byte == 92:
                escaped = True
            elif byte == 34:
                quoted = False
        elif byte == 34:
            quoted = True
        elif byte in (91, 123):
            depth += 1
            _require(depth <= 8)
        elif byte in (93, 125):
            depth -= 1
            _require(depth >= 0)
    return json.loads(data.decode("utf-8"), object_pairs_hook=_unique_object,
                      parse_int=_integer, parse_float=lambda _: _require(False),
                      parse_constant=lambda _: _require(False))


def _checked_path(path):
    """Reject symlink/reparse ancestors without resolving them into permission."""
    path = Path(path)
    _require(path.is_absolute() and len(path.parts) <= 256 and ".." not in path.parts)
    for part in reversed((path, *path.parents)):
        info = part.lstat()
        _require(not stat.S_ISLNK(info.st_mode)
                 and not getattr(info, "st_file_attributes", 0) & 0x400)
    return path


class _AdmissionBytes(Exception):
    """The quota is known, but this admission cannot inspect more payloads."""


def _read(path, limit, *, budget=None):
    _checked_path(path)
    fd = host_files.open_regular(path)
    try:
        host_files.check_private_fd(fd)
        size = os.fstat(fd).st_size
        _require(size <= limit)
        read_limit = limit
        if budget is not None:
            remaining = ADMISSION_BYTES - budget.bytes
            if size > remaining:
                raise _AdmissionBytes()
            read_limit = min(limit, remaining)
        with os.fdopen(fd, "rb") as stream:
            fd = None
            data = stream.read(read_limit + 1)
        if budget is not None and len(data) > read_limit and read_limit < limit:
            raise _AdmissionBytes()
        _require(len(data) <= limit)
        if budget is not None:
            budget.bytes += len(data)
        return _decode(data)
    finally:
        if fd is not None:
            os.close(fd)


def _binding(event):
    _require(type(event) is dict)
    session = event.get("session_id")
    _text(session, 256)
    cwd = event.get("cwd")
    _path(cwd)
    actual = Path(cwd).resolve(strict=True)
    _require(actual.is_dir())
    manifest = os.environ.get("MNEME_ACTIVITY_CONFIG")
    manifest = Path(manifest) if manifest is not None else host_platform.default_config_dir() / "activity.json"
    config = _read(manifest, RECORD_BYTES)
    _require(type(config) is dict and {"schema", "root", "workspaces"} <= config.keys()
             and config.keys() <= {"schema", "root", "workspaces", "device"}
             and config["schema"] == CONFIG_SCHEMA)
    _path(config["root"])
    _text(config.get("device"), 128, nullable=True)
    _require(type(config["workspaces"]) is list and 0 < len(config["workspaces"]) <= 256)
    admitted = []
    seen = set()
    for binding in config["workspaces"]:
        _require(type(binding) is dict and binding.keys() == {"path", "source"}
                 and binding["source"] == "codex")
        _path(binding["path"])
        workspace = Path(binding["path"])
        _require(str(workspace) == binding["path"] and ".." not in workspace.parts
                 and workspace not in seen)
        seen.add(workspace)
        if actual == workspace or workspace in actual.parents:
            admitted.append(workspace)
    _require(bool(admitted))
    workspace = max(admitted, key=lambda path: len(path.parts))
    # Unrelated absent bindings are retained metadata, not failed current
    # admission. The selected binding alone must still identify its directory;
    # an invalid nearest candidate never authorizes fallback to a parent.
    _require(workspace.resolve(strict=True) == workspace and workspace.is_dir())
    boundary = actual
    while boundary != workspace:
        _require(not any((boundary / marker).exists() or (boundary / marker).is_symlink()
                         for marker in (".git", ".mneme")))
        boundary = boundary.parent
    root = _checked_path(config["root"])
    host_files.check_private_directory(root)
    return root, session, str(workspace), cwd, config.get("device")


def filename(session_id):
    return "codex-" + hashlib.sha256(session_id.encode("utf-8")).hexdigest() + ".json"


def _workspace_digest(workspace):
    return hashlib.sha256(workspace.encode("utf-8")).hexdigest()


def _validate_quota(quota):
    _require(type(quota) is dict and quota.keys() == {"schema", "reservations"}
             and quota["schema"] == QUOTA_SCHEMA and type(quota["reservations"]) is dict)
    _require(len(quota["reservations"]) <= PAYLOAD_BYTES // RECORD_BYTES)
    for name, reservation in quota["reservations"].items():
        _require(bool(_FILENAME.fullmatch(name)) and type(reservation) is dict
                 and reservation.keys() == {"workspace_digest", "reserved_at"})
        _require(isinstance(reservation["workspace_digest"], str)
                 and bool(_HEX.fullmatch(reservation["workspace_digest"])))
        _uint(reservation["reserved_at"])


def _recover_stage(root):
    try:
        stage = _read(root / STAGE, QUOTA_BYTES)
    except FileNotFoundError:
        return
    # A completed known stage is disposable, never an uncommitted authority.
    # Partial/unknown bytes are deliberately left untouched and disable logging.
    if type(stage) is dict and stage.get("schema") == SCHEMA:
        _require(len(_encode(stage)) <= RECORD_BYTES)
        _validate_record(stage)
    else:
        _validate_quota(stage)
    (root / STAGE).unlink()


def _publish(root, name, value, limit):
    data = _encode(value)
    _require(len(data) <= limit)
    stage = root / STAGE
    fd = host_files.open_regular(stage, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
    try:
        with os.fdopen(fd, "wb") as stream:
            fd = None
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        host_files.replace_file(stage, root / name)
    finally:
        if fd is not None:
            os.close(fd)
        # Ordinary errors clean our own scratch. A process crash leaves at most
        # this one bounded stage, recovered/refused before the next write.
        try:
            stage.unlink()
        except FileNotFoundError:
            pass


class _Admission:
    def __init__(self, deadline=None):
        self.deadline = time.monotonic() + ADMISSION_SECONDS
        if deadline is not None:
            self.deadline = min(self.deadline, deadline)
        self.bytes = 0

    def check(self):
        _require(time.monotonic() <= self.deadline)

    def record(self, path):
        self.check()
        if self.bytes >= ADMISSION_BYTES:
            raise _AdmissionBytes()
        record = _read(path, RECORD_BYTES, budget=self)
        _validate_record(record)
        _require(filename(record["session_id"]) == path.name)
        return record


def _catalog(root, quota, budget):
    names = []
    with os.scandir(root) as entries:
        for index, entry in enumerate(entries):
            budget.check()
            _require(index < ADMISSION_ENTRIES)
            if entry.name in (LOCK, QUOTA):
                continue
            _require(bool(_FILENAME.fullmatch(entry.name)))
            _require(entry.is_file(follow_symlinks=False))
            if quota is not None:
                _require(entry.name in quota["reservations"])
            names.append(entry.name)
    return names


def _quota(root, budget):
    try:
        quota = _read(root / QUOTA, QUOTA_BYTES)
    except FileNotFoundError:
        # Recovery is producer-only, bounded, and all-or-nothing. Unknown files
        # prevent reconstruction; they are never adopted, pruned or repaired.
        reservations = {}
        for name in _catalog(root, None, budget):
            record = budget.record(root / name)
            reservations[name] = {"workspace_digest": _workspace_digest(record["workspace"]),
                                  "reserved_at": record["first_observed"]}
        quota = {"schema": QUOTA_SCHEMA, "reservations": reservations}
        _validate_quota(quota)
        budget.check()
        _publish(root, QUOTA, quota, QUOTA_BYTES)
    _validate_quota(quota)
    return quota


def _admit(root, quota, name, workspace, now, budget):
    _catalog(root, quota, budget)
    reservations = quota["reservations"]
    candidates = []
    complete = True
    # Retention runs at session admission, never in reporting. A known quota
    # already establishes spare capacity; partial retention cannot release an
    # uninspected reservation or pretend to have found the globally oldest one.
    for other, reservation in sorted(reservations.items(), key=lambda pair: pair[1]["reserved_at"]):
        try:
            record = budget.record(root / other)
        except _AdmissionBytes:
            complete = False
            break
        except FileNotFoundError:
            candidates.append((reservation["reserved_at"], other))
        else:
            _require(_workspace_digest(record["workspace"]) == reservation["workspace_digest"])
            candidates.append((record["last_observed"], other))
    candidates.sort()
    remove = [item for item in candidates if item[0] < max(0, now - RETENTION_SECONDS)]
    if len(reservations) - len(remove) >= PAYLOAD_BYTES // RECORD_BYTES:
        _require(complete)
        remove = candidates[:len(reservations) - PAYLOAD_BYTES // RECORD_BYTES + 1]
    for _, other in remove:
        budget.check()
        try:
            (root / other).unlink()
        except FileNotFoundError:
            pass
        del reservations[other]
    _require(len(reservations) < PAYLOAD_BYTES // RECORD_BYTES)
    reservations[name] = {"workspace_digest": _workspace_digest(workspace), "reserved_at": now}
    # Reserve the complete record allowance before first publication. A crash
    # can leak a reservation, never overspend it; same-binding retries recover.
    budget.check()
    _publish(root, QUOTA, quota, QUOTA_BYTES)


class _Batch:
    """Bounded optional observations, never pending functional work."""
    def __init__(self):
        self._pending = []
        self._bytes = 0
        self.recorded = False
        self.recorded_lifecycle = False

    def enqueue(self, metadata, observation):
        size = len(_encode([metadata, observation]))
        if self._bytes + size <= DEFER_BYTES:
            self._pending.append((metadata, observation))
            self._bytes += size

    def drain(self):
        try:
            deadline = time.monotonic() + DEFER_SECONDS
            for metadata, observation in self._pending:
                if time.monotonic() >= deadline:
                    break
                try:
                    if _append(metadata, observation, deadline):
                        self.recorded = True
                        self.recorded_lifecycle |= observation["kind"] == "lifecycle"
                except Exception:
                    pass
        except Exception:
            pass
        finally:
            self._pending.clear()
            self._bytes = 0


@contextlib.contextmanager
def defer():
    """Queue safe metadata; drain only after the outer functional budget exits.

    Use ``with defer() as batch:`` *outside* the normal operation's deadline
    context. Nested scopes share the outer bounded queue. Deferred emit returns
    False: only ``batch.recorded``/``batch.recorded_lifecycle`` after exit attest
    successful publication. The drain is cooperatively deadline-bounded between
    emissions and before publication; OS filesystem calls are not preemptible.
    """
    current = _DEFERRED.get()
    if current is not None:
        yield current
        return
    batch = _Batch()
    token = _DEFERRED.set(batch)
    try:
        yield batch
    finally:
        _DEFERRED.reset(token)
        batch.drain()


def emit(event, kind, outcome, *, reason=None, origin=None, owner=None, nodes=None,
         count=None, omitted=0, selection_digest=None, key=None, at=None):
    """Observe one already-decided outcome, outside every operational lock.

    Arguments are allowlisted metadata, not serialized provider/native results.
    List order is append order; event.at and first/last are occurrence times.
    Stable keys deduplicate retained retries. Once evicted, retry deduplication
    is necessarily best effort rather than an unbounded hidden event ledger.
    """
    try:
        metadata, observation = _prepare(event, kind, outcome, reason=reason, origin=origin, owner=owner,
                                         nodes=nodes, count=count, omitted=omitted,
                                         selection_digest=selection_digest, key=key, at=at)
        batch = _DEFERRED.get()
        if batch is not None:
            batch.enqueue(metadata, observation)
            return False  # queued is not recorded; no startup receipt yet
        return _append(metadata, observation)
    except Exception:
        # In particular, never log exception text: it can contain source bodies,
        # query strings, credentials or private paths. This lane is expendable.
        return False


def _metadata(event):
    _require(type(event) is dict)
    metadata = {field: event.get(field) for field in ("session_id", "cwd", "turn_id")}
    _text(metadata["session_id"], 256)
    _path(metadata["cwd"])
    _text(metadata["turn_id"], 256, nullable=True)
    return metadata


def _prepare(event, kind, outcome, *, reason, origin, owner, nodes, count, omitted,
             selection_digest, key, at):
    now = int(time.time())
    occurrence = now if at is None else at
    metadata = _metadata(event)
    _require(origin is None or (type(origin) is dict and origin.keys() <= _ORIGIN_FIELDS))
    binding = {"turn": metadata["turn_id"], "generation": None,
               "context_generation": None, "attempt": None}
    binding.update(origin or {})
    observation = {"key": "pending", "at": occurrence, "kind": kind, "outcome": outcome,
                   "reason": reason, "origin": binding, "owner": owner,
                   "nodes": [] if nodes is None else nodes, "count": count,
                   "omitted": omitted, "selection_digest": selection_digest}
    _validate_event(observation)
    # Detach bounded caller-owned containers before filesystem work. Normal
    # producer state may change concurrently after its observation snapshot.
    observation["owner"] = dict(owner) if owner is not None else None
    observation["nodes"] = observation["nodes"][:257]
    _validate_event(observation)
    if key is None:
        identity = {field: value for field, value in observation.items() if field not in ("key", "at")}
        if binding["turn"] is None and binding["attempt"] is None:
            identity["at"] = occurrence
        key = hashlib.sha256(_encode(identity)).hexdigest()
    observation["key"] = key
    _validate_event(observation)
    return metadata, observation


def _append(event, observation, deadline=None):
    if deadline is not None:
        _require(time.monotonic() < deadline)
    root, session, workspace, cwd, device = _binding(event)
    if deadline is not None:
        _require(time.monotonic() < deadline)
    now, occurrence, key = int(time.time()), observation["at"], observation["key"]
    name = filename(session)
    lock = host_files.open_regular(root / LOCK, os.O_CREAT | os.O_RDWR)
    try:
        host_files.check_private_fd(lock)
        _require(os.fstat(lock).st_size == 0)
        host_files.lock_file(lock, blocking=False)
        _recover_stage(root)
        budget = _Admission(deadline)
        quota = _quota(root, budget)
        reservation = quota["reservations"].get(name)
        if reservation is not None:
            _require(reservation["workspace_digest"] == _workspace_digest(workspace))
        try:
            record = _read(root / name, RECORD_BYTES)
        except FileNotFoundError:
            record = {"schema": SCHEMA, "source": "codex", "session_id": session,
                      "workspace": workspace, "cwd": cwd, "device": device,
                      "first_observed": occurrence, "last_observed": occurrence,
                      "events": [], "evicted_events": 0}
        else:
            _validate_record(record)
            _require(reservation is not None and record["session_id"] == session
                     and record["workspace"] == workspace)
        if reservation is None:
            _admit(root, quota, name, workspace, now, budget)
        prior = next((item for item in record["events"] if item["key"] == key), None)
        if prior is not None:
            # Retry timestamps may differ, but a conflicting key is not a retry.
            compared = {k: v for k, v in observation.items() if k != "at"}
            if len(prior["nodes"]) < len(compared["nodes"]):
                compared["omitted"] += len(compared["nodes"]) - len(prior["nodes"])
                compared["nodes"] = compared["nodes"][:len(prior["nodes"])]
            _require({k: v for k, v in prior.items() if k != "at"} == compared)
            return True
        record["events"].append(observation)
        record["first_observed"] = min(record["first_observed"], occurrence)
        record["last_observed"] = max(record["last_observed"], occurrence)
        # Keep the newest appended envelope, trimming node overflow explicitly
        # if this envelope alone cannot fit the per-session resource envelope.
        while len(_encode(record)) > RECORD_BYTES or len(record["events"]) > 512:
            if len(record["events"]) > 1:
                record["events"].pop(0)
                record["evicted_events"] += 1
            elif observation["nodes"]:
                observation["nodes"] = observation["nodes"][:-1]
                observation["omitted"] += 1
            else:
                return False
        _validate_record(record)
        if deadline is not None:
            budget.check()
        _publish(root, name, record, RECORD_BYTES)
        return True
    finally:
        os.close(lock)  # also releases the sole activity-only advisory lease

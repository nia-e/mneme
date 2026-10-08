"""Bounded durable UTC-hour decisions; no timer, process launch, or backfill.

The workshop runner holds its own cycle lock. This independent permanent lock
also admits a busy timer contender, so an occupied hour can be skipped without
waiting for the active workshop cycle. The only retained fact is a high-water
slot; clock rollback never reopens it. Old runner releases must be stopped before
hourly mode is enabled: the separate state is not a fence understood by them.
"""

from contextlib import contextmanager
import datetime as dt
import fcntl
import json
import os
from pathlib import Path
import stat
import uuid

SCHEMA = "mneme.workshop.hourly.v1"
MAX_BYTES = 1024
GRACE = dt.timedelta(minutes=5)
DECISIONS = {"start", "skip_owner", "skip_busy", "skip_storage", "missed_window"}


class Refusal(ValueError):
    pass


def hour_slot(now):
    if not isinstance(now, dt.datetime) or now.tzinfo is None or now.utcoffset() is None:
        raise Refusal("Hourly clock must be timezone-aware")
    return now.astimezone(dt.timezone.utc).replace(minute=0, second=0, microsecond=0)


def _pairs(items):
    value = {}
    for key, item in items:
        if key in value:
            raise Refusal("Duplicate hourly state key")
        value[key] = item
    return value


def read(root):
    """Read-only atomic snapshot; absent is initial state, malformed is refusal."""
    path = root / "hourly-state.json"
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return None
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > MAX_BYTES:
            raise Refusal("Unsafe or oversized hourly-state.json; inspect before resuming")
        raw = stream.read(MAX_BYTES + 1)
    if len(raw) > MAX_BYTES:
        raise Refusal("Oversized hourly-state.json")
    try:
        value = json.loads(raw, object_pairs_hook=_pairs)
        if (not isinstance(value, dict) or set(value) != {"schema", "slot", "decision"}
                or value["schema"] != SCHEMA or not isinstance(value["slot"], str)
                or not isinstance(value["decision"], str) or value["decision"] not in DECISIONS):
            raise ValueError("shape")
        slot = dt.datetime.fromisoformat(value["slot"])
        if slot.utcoffset() != dt.timedelta(0) or slot != hour_slot(slot):
            raise ValueError("slot")
    except (ValueError, TypeError, UnicodeError) as exc:
        raise Refusal("Unknown or corrupt hourly-state.json; inspect before resuming") from exc
    return value


def inspect(root, now):
    """No mutation, even on a never-used root."""
    slot = hour_slot(now)
    previous = read(root)
    if previous is not None and dt.datetime.fromisoformat(previous["slot"]) >= slot:
        decision = "already_decided"
    elif now.astimezone(dt.timezone.utc) - slot >= GRACE:
        decision = "missed_window"
    else:
        decision = "start"
    next_slot = (slot if decision == "start" else
                 max(slot, dt.datetime.fromisoformat(previous["slot"]) if previous else slot) + dt.timedelta(hours=1))
    return {"slot": slot.isoformat(), "decision": decision, "last_decision": previous,
            "next_opportunity_at": next_slot.isoformat()}


@contextmanager
def _locked(root):
    fd = os.open(root / ".hourly.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    with os.fdopen(fd, "rb+") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            raise Refusal("Hourly lock must be a singly linked regular file")
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield


def _write(root, value):
    target = root / "hourly-state.json"
    data = (json.dumps(value, sort_keys=True) + "\n").encode()
    temp = root / (".hourly-state." + uuid.uuid4().hex + ".tmp")
    try:
        with temp.open("xb") as stream:
            os.fchmod(stream.fileno(), 0o600)
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temp, target)
        directory = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        temp.unlink(missing_ok=True)


def consume(root, now, reason=None):
    """Atomically consume one opportunity before any child launch.

    A failed/ambiguous publication must not launch. After rename, even an fsync
    error leaves a readable high-water marker, so an exact retry cannot replay
    this slot. A pre-rename error publishes nothing and is safe to retry.
    """
    if reason is not None and reason not in DECISIONS - {"start", "missed_window"}:
        raise Refusal("Invalid hourly skip reason")
    with _locked(root):
        result = inspect(root, now)
        if result["decision"] == "already_decided":
            return result
        if result["decision"] == "start" and reason is not None:
            result["decision"] = reason
        value = {"schema": SCHEMA, "slot": result["slot"], "decision": result["decision"]}
        _write(root, value)
        result["next_opportunity_at"] = (hour_slot(now) + dt.timedelta(hours=1)).isoformat()
        return result


def finish_occupied(root, started, finished):
    """A run crossing HH:00 consumes that new slot even without a timer tick."""
    if hour_slot(finished) > hour_slot(started):
        # Use slot time because it was occupied at HH:00, regardless of when the
        # process finally exited. High-water comparison also handles rollback.
        return consume(root, hour_slot(finished), "skip_busy")
    return None

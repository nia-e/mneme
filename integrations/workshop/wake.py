#!/usr/bin/env python3
"""Bounded local event hints for the workshop. Event text is data, never authority."""

import argparse
import contextlib
import datetime as dt
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys
import time
import uuid


MAX_BYTES = 256 * 1024
MAX_RECORDS = 64
MAX_ARCHIVE_BYTES = 128 * 1024
SCHEMA = "mneme.workshop.events.v4"
PREVIOUS_SCHEMA = "mneme.workshop.events.v3"
V2_SCHEMA = "mneme.workshop.events.v2"
SIGNAL_SOURCE = "signal-owner"
SIGNAL_KIND = "signal-burst"
MAILBOX_SOURCE = "private-mailbox"
MAILBOX_KIND = "message"
FIELDS = {"source": 64, "event_id": 128, "kind": 64,
          "summary": 1000, "body": 8000, "reference": 1000}
STATUSES = {"pending", "held", "delivered", "unfinished", "consumed"}
CONSUMED_ARCHIVE_SCHEMA = "mneme.workshop.event-consumed.v1"


class Refusal(ValueError):
    """Malformed input or unsafe/unknown durable state."""


class Busy(Refusal):
    """A live writer owns a cooperative lease; maintenance can safely defer."""


def _root(root):
    root = Path(root)
    if not root.is_absolute() or not root.is_dir() or root != root.resolve(strict=True):
        raise Refusal("Root must be an existing absolute canonical directory")
    return root


def _pairs(items):
    value = {}
    for key, item in items:
        if key in value:
            raise Refusal(f"Duplicate JSON key: {key}")
        value[key] = item
    return value


def _event(value):
    if not isinstance(value, dict) or set(value) != set(FIELDS):
        raise Refusal("Event must contain exactly source, event_id, kind, summary, body, reference")
    for key, width in FIELDS.items():
        field = value[key]
        minimum = 1 if key in ("source", "event_id", "kind") else 0
        if not isinstance(field, str) or not minimum <= len(field) <= width:
            raise Refusal(f"Invalid event {key}")
    return value


def _run_id(value):
    if not isinstance(value, str) or re.fullmatch(r"[A-Za-z0-9_-]{1,128}", value) is None:
        raise Refusal("Invalid run ID")


def _state_floor(root):
    path = root / "state.json"
    if not path.exists() and not path.is_symlink():
        return 0
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, "rb") as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > 64 * 1024:
                raise Refusal("Unsafe or oversized workshop state")
            data = stream.read(64 * 1024 + 1)
        if len(data) > 64 * 1024:
            raise Refusal("Oversized workshop state")
        state = json.loads(data, object_pairs_hook=_pairs)
        value = state["event_after_sequence"]
        if type(value) is not int or value < 0:
            raise Refusal("Invalid workshop event watermark")
        if "interactive" in state:
            interactive = state["interactive"]
            if not isinstance(interactive, dict):
                raise Refusal("Invalid interactive workshop state")
            other = interactive["event_after_sequence"]
            if type(other) is not int or other < 0:
                raise Refusal("Invalid interactive workshop watermark")
            value = max(value, other)
        return value
    except (OSError, KeyError, ValueError, TypeError, RecursionError) as exc:
        raise Refusal("Cannot derive sequence from workshop state") from exc


def _load_full(root):
    path = root / "events.json"
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return [], max(1, _state_floor(root) + 1), "absent"
    except OSError as exc:
        raise Refusal("Cannot open event queue safely") from exc
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > MAX_BYTES:
            raise Refusal("Unsafe or oversized event queue")
        with os.fdopen(fd, "rb") as stream:
            fd = -1
            data = stream.read(MAX_BYTES + 1)
        if len(data) > MAX_BYTES:
            raise Refusal("Oversized event queue")
        value = json.loads(data, object_pairs_hook=_pairs)
    except (ValueError, RecursionError) as exc:
        raise Refusal("Corrupt event queue; inspect before retrying") from exc
    finally:
        if fd >= 0:
            os.close(fd)
    if isinstance(value, list):
        records = value
        format_kind = "array"
        try:
            tail = records[-1]["sequence"] if records else 0
        except (TypeError, KeyError) as exc:
            raise Refusal("Invalid legacy event queue") from exc
        if type(tail) is not int:
            raise Refusal("Invalid legacy event sequence")
        next_sequence = max(tail + 1, _state_floor(root) + 1)
    elif (isinstance(value, dict) and set(value) == {"schema", "next_sequence", "records"}
          and value["schema"] in (V2_SCHEMA, PREVIOUS_SCHEMA, SCHEMA)):
        records, next_sequence = value["records"], value["next_sequence"]
        format_kind = value["schema"]
    else:
        raise Refusal("Unknown event queue")
    if (not isinstance(records, list) or len(records) > MAX_RECORDS or
            type(next_sequence) is not int or next_sequence < 1):
        raise Refusal("Unknown or oversized event queue")
    identities = set()
    last_sequence = 0
    for record in records:
        if not isinstance(record, dict) or set(record) != {"event", "status", "claimed_run", "detail", "sequence"}:
            raise Refusal("Unknown event record")
        if type(record["sequence"]) is not int or not last_sequence < record["sequence"] < next_sequence:
            raise Refusal("Invalid event sequence")
        last_sequence = record["sequence"]
        event = _event(record["event"])
        identity = event["source"], event["event_id"]
        if identity in identities:
            raise Refusal("Duplicate event identity in queue")
        identities.add(identity)
        status, claimed, detail = record["status"], record["claimed_run"], record["detail"]
        if (not isinstance(status, str) or status not in STATUSES or
                (claimed is not None and
                 (not isinstance(claimed, str) or re.fullmatch(r"[A-Za-z0-9_-]{1,128}", claimed) is None)) or
                (status in ("pending", "consumed") and claimed is not None) or
                (status not in ("pending", "consumed") and claimed is None) or
                (status == "consumed" and
                 (format_kind != SCHEMA or
                  (event["source"], event["kind"]) != (SIGNAL_SOURCE, SIGNAL_KIND))) or
                (not isinstance(detail, str) or len(detail) > 512)):
            raise Refusal("Invalid event record")
    return records, next_sequence, format_kind


def _load(root):
    records, next_sequence, _format_kind = _load_full(root)
    return records, next_sequence


def _read(root):
    return _load(root)[0]


def _load_writable(root):
    records, next_sequence, format_kind = _load_full(root)
    if format_kind not in ("absent", SCHEMA):
        raise Refusal("Legacy event queue is read-only; run explicit queue upgrade before writing")
    return records, next_sequence


def upgrade_version(root: Path) -> str:
    """Classify and validate the complete queue without taking a lease or writing."""
    return _load_full(_root(root))[2]


def upgrade(root: Path) -> dict:
    """Deliberately publish v4 under the queue lease; preserve all canonical data.

    Takes no workshop lease, so a bridge upgrader may continuously hold its own
    bridge/workshop leases across the queue and Signal-ledger upgrades.
    """
    root = _root(root)
    with _locked(root):
        records, next_sequence, format_kind = _load_full(root)
        if format_kind != SCHEMA:
            _write(root, records, next_sequence, allow_upgrade=True)
        else:
            # A prior replace may have crossed but died before its directory sync.
            _sync_dir(root)
        return {"schema": SCHEMA, "previous_schema": format_kind,
                "upgraded": format_kind != SCHEMA}


def fence_current(root: Path) -> None:
    """Admit the current writer without implicitly migrating an existing queue."""
    root = _root(root)
    with _locked(root):
        records, next_sequence = _load_writable(root)
        if not (root / "events.json").exists():
            _write(root, records, next_sequence)
        else:
            _sync_dir(root)


def active_records(root: Path) -> list:
    return [dict(record, event=record["event"].copy()) for record in _read(_root(root))]


def inbox_summary(root: Path) -> dict:
    """Read-only active inbox counts, not read tracking or individual Signal texts.

    Exact source/kind pairs exclude technical hints and unknown event kinds.
    Held/delivered records are not pending; interrupted work stays separate.
    """
    counts = {"pending_signal_bursts": 0, "pending_mailbox_messages": 0,
              "unfinished_signal_bursts": 0, "unfinished_mailbox_messages": 0}
    kinds = {(SIGNAL_SOURCE, SIGNAL_KIND): "signal_bursts",
             (MAILBOX_SOURCE, MAILBOX_KIND): "mailbox_messages"}
    for record in active_records(root):
        event = record["event"]
        kind = kinds.get((event["source"], event["kind"]))
        if kind is not None and record["status"] in ("pending", "unfinished"):
            counts[f"{record['status']}_{kind}"] += 1
    return counts


def queue_stats(root: Path) -> dict:
    """Read-only capacity snapshot; encoded bytes differ from failure reservation."""
    records, next_sequence = _load(_root(root))
    return {"active_events": len(records),
            "encoded_bytes": len(_encode(records, next_sequence)),
            "reserved_bytes": _future_size(records, next_sequence),
            "max_bytes": MAX_BYTES, "max_records": MAX_RECORDS}


def _archive_path(root, source, event_id):
    identity = json.dumps([source, event_id], ensure_ascii=False, separators=(",", ":")).encode()
    return root / "event-archive" / (hashlib.sha256(identity).hexdigest() + ".json")


def _event_digest(event):
    data = json.dumps(event, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(data).hexdigest()


def _archive_lookup(root, source, event_id):
    path = _archive_path(root, source, event_id)
    directory = path.parent
    if directory.is_symlink() or (directory.exists() and not directory.is_dir()):
        raise Refusal("Unsafe event archive directory")
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return None
    except OSError as exc:
        raise Refusal("Cannot read event archive safely") from exc
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > MAX_ARCHIVE_BYTES:
            raise Refusal("Unsafe or oversized event archive entry")
        with os.fdopen(fd, "rb") as stream:
            fd = -1
            entry = json.loads(stream.read(MAX_ARCHIVE_BYTES + 1), object_pairs_hook=_pairs)
    except (ValueError, RecursionError) as exc:
        raise Refusal("Corrupt event archive entry") from exc
    finally:
        if fd >= 0:
            os.close(fd)
    if isinstance(entry, dict) and entry.get("schema") == CONSUMED_ARCHIVE_SCHEMA:
        record = entry.get("record")
        expected = _consumed_archive_entry(record)
        if entry != expected:
            raise Refusal("Invalid consumed event tombstone")
        event = record["event"]
        if (event["source"], event["event_id"]) != (source, event_id):
            raise Refusal("Archive identity mismatch")
        return {"location": "archive", "record": record, "terminal": entry["terminal"]}
    if (not isinstance(entry, dict) or set(entry) != {"schema", "record", "terminal", "run_id",
                                                    "receipt_sha256", "result_sha256", "event_sha256"}
            or entry["schema"] != "mneme.workshop.event-archive.v1"):
        raise Refusal("Unknown event archive entry")
    record, terminal = entry["record"], entry["terminal"]
    if (not isinstance(record, dict) or set(record) != {"event", "status", "claimed_run", "detail", "sequence"}
            or record["status"] != "delivered" or type(record["sequence"]) is not int
            or record["sequence"] < 1 or not isinstance(record["detail"], str)
            or len(record["detail"]) > 512 or record["claimed_run"] != entry["run_id"]):
        raise Refusal("Invalid archived event record")
    event = _event(record["event"])
    if (event["source"], event["event_id"]) != (source, event_id):
        raise Refusal("Archive identity mismatch")
    if entry["event_sha256"] != _event_digest(event):
        raise Refusal("Archived payload digest mismatch")
    _run_id(entry["run_id"])
    if (not isinstance(terminal, dict) or set(terminal) != {"status", "reply"}
            or terminal["status"] not in ("replied", "completed_without_reply")
            or (terminal["status"] == "replied" and
                (not isinstance(terminal["reply"], str) or not 1 <= len(terminal["reply"]) <= 4000))
            or (terminal["status"] == "completed_without_reply" and terminal["reply"] is not None)):
        raise Refusal("Invalid archived terminal state")
    for key in ("receipt_sha256", "result_sha256", "event_sha256"):
        digest = entry[key]
        if not isinstance(digest, str) or re.fullmatch("[0-9a-f]{64}", digest) is None:
            raise Refusal("Invalid archive proof digest")
    return {"location": "archive", "record": record, "terminal": terminal}


def lookup(root: Path, source: str, event_id: str):
    root = _root(root)
    if (not isinstance(source, str) or not 1 <= len(source) <= 64 or
            not isinstance(event_id, str) or not 1 <= len(event_id) <= 128):
        raise Refusal("Invalid event identity")
    for record in _read(root):
        if (record["event"]["source"], record["event"]["event_id"]) == (source, event_id):
            return {"location": "active", "record": dict(record, event=record["event"].copy()),
                    "terminal": None}
    return _archive_lookup(root, source, event_id)


@contextlib.contextmanager
def _locked(root, *, wait=True):
    path = root / ".events.lock"
    fd = None
    try:
        fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            raise Refusal("Unsafe event lock")
        deadline = time.monotonic() + 1.0
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError as exc:
                if not wait or time.monotonic() >= deadline:
                    raise Busy("Event queue busy; retry after the current writer finishes") from exc
                time.sleep(0.02)
    except OSError as exc:
        if fd is not None:
            os.close(fd)
        raise Refusal("Cannot lock event queue safely") from exc
    except Refusal:
        if fd is not None:
            os.close(fd)
        raise
    try:
        yield
    finally:
        os.close(fd)


def _write(root, records, next_sequence=None, *, allow_upgrade=False):
    _current, admitted_sequence, format_kind = _load_full(root)
    if not allow_upgrade and format_kind not in ("absent", SCHEMA):
        raise Refusal("Legacy event queue is read-only; run explicit queue upgrade before writing")
    if next_sequence is None:
        next_sequence = admitted_sequence
    data = _encode(records, next_sequence)
    if len(records) > MAX_RECORDS or len(data) > MAX_BYTES:
        raise Refusal("Event queue is full; inspect or archive it explicitly")
    path = root / "events.json"
    temp = root / (".events-" + uuid.uuid4().hex + ".tmp")
    try:
        fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temp, path)
        dir_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(dir_fd)
        finally:
            os.close(dir_fd)
    finally:
        temp.unlink(missing_ok=True)


def _encode(records, next_sequence):
    envelope = {"schema": SCHEMA, "next_sequence": next_sequence, "records": records}
    return (json.dumps(envelope, ensure_ascii=False, sort_keys=True,
                       separators=(",", ":")) + "\n").encode()


def _future_size(records, next_sequence):
    """Reserve failure metadata even after orientation delivery but before run success."""
    worst = [dict(record, status="unfinished", claimed_run="\x00" * 128,
                  detail="\x00" * 512) for record in records]
    return len(_encode(worst, next_sequence))


def enqueue(root: Path, event: dict) -> dict:
    root = _root(root)
    event = _event(event)
    with _locked(root):
        records, next_sequence = _load_writable(root)
        for record in records:
            old = record["event"]
            if (old["source"], old["event_id"]) == (event["source"], event["event_id"]):
                if old != event:
                    raise Refusal("Event identity reused with conflicting payload")
                return record.copy()
        archived = _archive_lookup(root, event["source"], event["event_id"])
        if archived is not None:
            if archived["record"]["event"] != event:
                raise Refusal("Event identity reused with conflicting archived payload")
            return archived["record"].copy()
        record = {"event": event.copy(), "status": "pending", "claimed_run": None,
                  "detail": "", "sequence": next_sequence}
        if _future_size(records + [record], next_sequence + 1) > MAX_BYTES:
            raise Refusal("Event queue is full; inspect or archive it explicitly")
        _write(root, records + [record], next_sequence + 1)
        return record.copy()


def consume_signal_events(root: Path, events: list) -> dict:
    """Consume exact pending Signal bursts already proven seen by the caller.

    Only the event lease is taken. Durable consumed rows precede immutable
    tombstones, which precede active-row removal. Exact retry finishes any cut;
    no workshop claim or fabricated run receipt is created.
    """
    root = _root(root)
    if not isinstance(events, list) or len(events) > MAX_RECORDS:
        raise Refusal("Expected at most 64 exact Signal events")
    requested = {}
    for value in events:
        event = _event(value).copy()
        if (event["source"], event["kind"]) != (SIGNAL_SOURCE, SIGNAL_KIND):
            raise Refusal("Only exact Signal burst events may be consumed")
        identity = event["source"], event["event_id"]
        if identity in requested:
            raise Refusal("Duplicate requested Signal event")
        requested[identity] = event
    counts = {"consumed_events": 0, "already_consumed_events": 0,
              "unchanged_events": 0, "missing_events": 0}
    with _locked(root, wait=False):
        records, next_sequence = _load_writable(root)
        active = {(r["event"]["source"], r["event"]["event_id"]): r for r in records}
        selected = []
        # Validate the entire request before changing any queue record.
        for identity, event in requested.items():
            record = active.get(identity)
            archived = _archive_lookup(root, *identity)
            if record is None:
                record = None if archived is None else archived["record"]
            if record is None:
                counts["missing_events"] += 1
                continue
            if record["event"] != event or (archived is not None and archived["record"]["event"] != event):
                raise Refusal("Signal event identity reused with conflicting payload")
            if identity in active and record["status"] in ("pending", "consumed"):
                consumed = dict(record, status="consumed")
                if archived is not None and archived["record"] != consumed:
                    raise Refusal("Conflicting consumed event tombstone")
                selected.append(consumed)
            if record["status"] == "pending":
                counts["consumed_events"] += 1
            elif record["status"] == "consumed":
                counts["already_consumed_events"] += 1
            else:
                counts["unchanged_events"] += 1
        selected_by_id = {(r["event"]["source"], r["event"]["event_id"]): r for r in selected}
        if counts["consumed_events"]:
            records = [selected_by_id.get((r["event"]["source"], r["event"]["event_id"]), r)
                       for r in records]
            _write(root, records, next_sequence)
        else:
            # Complete a previous replace whose parent sync/ack was interrupted.
            _sync_dir(root)
        for record in selected:
            _publish_archive(root, _consumed_archive_entry(record))
        if selected:
            remaining = [r for r in records
                         if (r["event"]["source"], r["event"]["event_id"]) not in selected_by_id]
            _write(root, remaining, next_sequence)
    return counts


def _lane(record, cycle_class, mailbox_inbox, signal_inbox):
    if cycle_class is None:
        return True
    source = record["event"]["source"]
    kind = record["event"]["kind"]
    if cycle_class == "background":
        return source != SIGNAL_SOURCE and (not mailbox_inbox or source != MAILBOX_SOURCE)
    return ((signal_inbox and source == SIGNAL_SOURCE and kind == SIGNAL_KIND)
            or (mailbox_inbox and source == MAILBOX_SOURCE and kind == MAILBOX_KIND))


def _cycle_class(value):
    if value is not None and value not in ("background", "interactive"):
        raise Refusal("Invalid event cycle class")


def pending(root: Path, after_sequence: int = 0, *, cycle_class=None,
            include_unfinished: bool = False, mailbox_inbox: bool = False,
            signal_inbox: bool = True) -> bool:
    if type(after_sequence) is not int or after_sequence < 0:
        raise Refusal("Invalid sequence watermark")
    _cycle_class(cycle_class)
    if type(include_unfinished) is not bool:
        raise Refusal("Invalid retry flag")
    if type(mailbox_inbox) is not bool or type(signal_inbox) is not bool:
        raise Refusal("Invalid inbox flag")
    return any(_lane(record, cycle_class, mailbox_inbox, signal_inbox) and
               ((record["status"] == "pending" and
                 (include_unfinished or record["sequence"] > after_sequence))
                or (include_unfinished and record["status"] == "unfinished"))
               for record in _read(_root(root)))


def watermark(root: Path) -> int:
    return _load(_root(root))[1] - 1


def claim(root: Path, run_id: str, include_unfinished: bool = False,
          after_sequence: int = 0, *, cycle_class=None, mailbox_inbox: bool = False,
          signal_inbox: bool = True) -> dict:
    root = _root(root)
    _run_id(run_id)
    if type(include_unfinished) is not bool:
        raise Refusal("Invalid retry flag")
    if type(after_sequence) is not int or after_sequence < 0:
        raise Refusal("Invalid sequence watermark")
    _cycle_class(cycle_class)
    if type(mailbox_inbox) is not bool or type(signal_inbox) is not bool:
        raise Refusal("Invalid inbox flag")
    with _locked(root):
        records, next_sequence = _load_writable(root)
        high_watermark = next_sequence - 1
        pending_records = [r for r in records if r["status"] == "pending"
                           and _lane(r, cycle_class, mailbox_inbox, signal_inbox)]
        if after_sequence:
            pending_records.sort(key=lambda r: (r["sequence"] <= after_sequence, r["sequence"]))
        chosen = pending_records[:4]
        if include_unfinished and len(chosen) < 4:
            chosen += [r for r in records if r["status"] == "unfinished"
                       and _lane(r, cycle_class, mailbox_inbox, signal_inbox)][:4 - len(chosen)]
        for record in chosen:
            record.update(status="held", claimed_run=run_id)
        if chosen:
            _write(root, records)
        return {"events": [r.copy() for r in chosen], "watermark": high_watermark}


def _finish(root, run_id, status, detail):
    root = _root(root)
    _run_id(run_id)
    if not isinstance(detail, str) or len(detail) > 512:
        raise Refusal("Invalid event detail")
    with _locked(root):
        records, _next_sequence = _load_writable(root)
        changed = False
        for record in records:
            if record["status"] in ("held", "delivered") and record["claimed_run"] == run_id:
                record.update(status=status, detail=detail)
                changed = True
        if changed:
            _write(root, records)


def delivered(root: Path, run_id: str) -> None:
    """Mark orientation as seen, not as answered or acted upon."""
    _finish(root, run_id, "delivered", "")


def unfinished(root: Path, run_id: str, reason: str) -> None:
    _finish(root, run_id, "unfinished", reason)


def recover(root: Path, active_run_ids: set[str]) -> None:
    root = _root(root)
    if not isinstance(active_run_ids, set):
        raise Refusal("Invalid run ID set")
    for run_id in active_run_ids:
        _run_id(run_id)
    with _locked(root):
        records, _next_sequence = _load_writable(root)
        changed = False
        for record in records:
            if record["status"] in ("held", "delivered") and record["claimed_run"] in active_run_ids:
                record.update(status="unfinished", detail="Run recovered without confirmed delivery")
                changed = True
        if changed:
            _write(root, records)


def _sync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def _consumed_archive_entry(record):
    if (not isinstance(record, dict) or
            set(record) != {"event", "status", "claimed_run", "detail", "sequence"} or
            record["status"] != "consumed" or record["claimed_run"] is not None or
            type(record["sequence"]) is not int or record["sequence"] < 1 or
            not isinstance(record["detail"], str) or len(record["detail"]) > 512):
        raise Refusal("Invalid consumed event record")
    event = _event(record["event"])
    if (event["source"], event["kind"]) != (SIGNAL_SOURCE, SIGNAL_KIND):
        raise Refusal("Only Signal bursts have consumed tombstones")
    return {"schema": CONSUMED_ARCHIVE_SCHEMA, "record": record,
            "terminal": {"status": "consumed", "reply": None},
            "event_sha256": _event_digest(event)}


def _archive_entry(root, record, *, skip_incomplete=False):
    """Consumed Signal rows need no run; delivered rows require completed proof."""
    if record["status"] == "consumed":
        return _consumed_archive_entry(record)
    import heartbeat  # Local import avoids a module cycle in the normal heartbeat path.

    run_id = record["claimed_run"]
    run = root / "runs" / run_id
    try:
        heartbeat.directory(root / "runs")
        heartbeat.directory(run)
        receipt_bytes = heartbeat.bounded_bytes(run / "receipt.json", heartbeat.MAX_RESULT)
        receipt = heartbeat.json_object(receipt_bytes)
    except (OSError, ValueError, UnicodeError, heartbeat.Refusal) as exc:
        raise Refusal("Cannot prove completed event run " + run_id) from exc
    if (not isinstance(receipt, dict) or receipt.get("schema") not in heartbeat.REPLY_RECEIPT_SCHEMAS
            or receipt.get("run_id") != run_id
            or not isinstance(receipt.get("status"), str)
            or receipt["status"] not in heartbeat.TERMINAL | {"running"}
            or not isinstance(receipt.get("event_ids"), list)
            or type(receipt.get("event_count")) is not int
            or receipt.get("event_count") != len(receipt["event_ids"])
            or len(receipt["event_ids"]) > 4):
        raise Refusal("Event run lacks a valid reply-capable receipt")
    try:
        heartbeat.receipt_class(receipt)
        started = heartbeat.parse_utc(receipt.get("started_at"))
        if started.utcoffset() != dt.timedelta(0):
            raise ValueError("not UTC")
    except (heartbeat.Refusal, ValueError, TypeError) as exc:
        raise Refusal("Invalid event receipt cycle class or start time") from exc
    identities = []
    for item in receipt["event_ids"]:
        if (not isinstance(item, dict) or set(item) != {"source", "event_id"}
                or not isinstance(item["source"], str) or not 1 <= len(item["source"]) <= 64
                or not isinstance(item["event_id"], str) or not 1 <= len(item["event_id"]) <= 128):
            raise Refusal("Invalid event IDs in completed receipt")
        identities.append((item["source"], item["event_id"]))
    if len(set(identities)) != len(identities):
        raise Refusal("Duplicate event IDs in completed receipt")
    identity = record["event"]["source"], record["event"]["event_id"]
    if identity not in identities:
        raise Refusal("Completed receipt does not name event")
    if receipt["status"] != "completed":
        if skip_incomplete:
            # Delivery proves orientation only. Keep incomplete runs and their
            # event payloads intact without blocking unrelated completed events.
            return None
        raise Refusal("Event run lacks a completed reply-capable receipt")
    try:
        result_bytes = heartbeat.bounded_bytes(run / "result.json", heartbeat.MAX_RESULT)
        result = heartbeat.json_object(result_bytes)
    except (OSError, ValueError, UnicodeError, heartbeat.Refusal) as exc:
        raise Refusal("Cannot prove completed event result " + run_id) from exc
    # Archive preserves the terminal reply even if separate artifact paths later move.
    if (not isinstance(result, dict) or set(result) != {"status", "summary", "artifacts",
                                                   "next_step", "memory_candidates",
                                                   "next_wake_seconds", "replies"}
            or result["status"] not in ("progress", "rest", "blocked")
            or not isinstance(result["summary"], str) or not 1 <= len(result["summary"]) <= 4000
            or not isinstance(result["next_step"], str) or len(result["next_step"]) > 2000
            or type(result["next_wake_seconds"]) is not int
            or not 900 <= result["next_wake_seconds"] <= 86400
            or not isinstance(result["artifacts"], list) or len(result["artifacts"]) > 16
            or any(not isinstance(item, str) or not 1 <= len(item) <= 512
                   for item in result["artifacts"])
            or not isinstance(result["memory_candidates"], list)
            or len(result["memory_candidates"]) > 8
            or any(not isinstance(item, str) or not 1 <= len(item) <= 2000
                   for item in result["memory_candidates"])
            or not isinstance(result["replies"], list)):
        raise Refusal("Invalid completed run result")
    try:
        heartbeat.validate_replies(result["replies"],
                                   [{"event": {"source": source, "event_id": event_id}}
                                    for source, event_id in identities])
    except (heartbeat.Refusal, ValueError, TypeError) as exc:
        raise Refusal("Invalid completed run replies") from exc
    replies = [reply["text"] for reply in result["replies"]
               if (reply["source"], reply["event_id"]) == identity]
    terminal = {"status": "replied", "reply": replies[0]} if replies else {
        "status": "completed_without_reply", "reply": None}
    return {"schema": "mneme.workshop.event-archive.v1", "record": record,
            "terminal": terminal, "run_id": run_id,
            "event_sha256": _event_digest(record["event"]),
            "receipt_sha256": hashlib.sha256(receipt_bytes).hexdigest(),
            "result_sha256": hashlib.sha256(result_bytes).hexdigest()}


@contextlib.contextmanager
def _workshop_locked(root):
    path = root / ".workshop.lock"
    fd = None
    try:
        fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            raise Refusal("Unsafe workshop lock")
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError as exc:
        if fd is not None:
            os.close(fd)
        raise Busy("Workshop busy; pause and retry archival") from exc
    except OSError as exc:
        if fd is not None:
            os.close(fd)
        raise Refusal("Cannot lock workshop safely") from exc
    except Refusal:
        if fd is not None:
            os.close(fd)
        raise
    try:
        yield
    finally:
        os.close(fd)


def _publish_archive(root, entry):
    record = entry["record"]
    event = record["event"]
    path = _archive_path(root, event["source"], event["event_id"])
    directory = path.parent
    if directory.is_symlink() or (directory.exists() and not directory.is_dir()):
        raise Refusal("Unsafe event archive directory")
    if not directory.exists():
        directory.mkdir(mode=0o700)
        _sync_dir(root)
    old = _archive_lookup(root, event["source"], event["event_id"])
    if old is not None:
        with path.open("rb") as stream:
            old_data = stream.read(MAX_ARCHIVE_BYTES + 1)
        if old_data != _encode_archive(entry):
            raise Refusal("Conflicting event archive entry")
        _sync_dir(directory)
        return
    data = _encode_archive(entry)
    if len(data) > MAX_ARCHIVE_BYTES:
        raise Refusal("Archive entry exceeds byte limit")
    temp = directory / ("." + path.stem + "." + uuid.uuid4().hex + ".tmp")
    fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(fd, "wb") as stream:
            fd = -1
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        # The queue writer lock serializes both consumed and completed archives.
        # Rename gives a complete final file with one link at every crash point.
        os.rename(temp, path)
        _sync_dir(directory)
    finally:
        if fd >= 0:
            os.close(fd)
        temp.unlink(missing_ok=True)


def _encode_archive(entry):
    return (json.dumps(entry, ensure_ascii=False, sort_keys=True, separators=(",", ":")) + "\n").encode()


def _relocate_runs(root, active):
    runs = root / "runs"
    if not runs.exists():
        return 0
    if runs.is_symlink() or not runs.is_dir():
        raise Refusal("Unsafe runs directory")
    destination = root / "run-archive"
    if destination.is_symlink() or (destination.exists() and not destination.is_dir()):
        raise Refusal("Unsafe run archive directory")
    if not destination.exists():
        destination.mkdir(mode=0o700)
        _sync_dir(root)
    pinned = {r["claimed_run"] for r in active if r["claimed_run"] is not None}
    for record in active:
        pinned.update(re.findall(r"runs/([0-9]{8}T[0-9]{12}Z-[0-9a-f]{8})", record["detail"]))
    moved = 0
    now = dt.datetime.now(dt.timezone.utc)
    today = now.date()
    import heartbeat
    for run in sorted(runs.iterdir()):
        if run.name in pinned:
            continue
        if run.is_symlink() or not run.is_dir():
            raise Refusal("Unsafe run entry")
        try:
            receipt = heartbeat.json_object(heartbeat.bounded_bytes(run / "receipt.json", heartbeat.MAX_RESULT))
            if (not isinstance(receipt, dict) or receipt.get("run_id") != run.name
                    or not isinstance(receipt.get("status"), str)
                    or receipt["status"] not in heartbeat.TERMINAL | {"running"}):
                raise Refusal("Invalid run receipt during archive")
            heartbeat.receipt_class(receipt)
            started = heartbeat.parse_utc(receipt.get("started_at"))
        except (OSError, heartbeat.Refusal, ValueError, TypeError) as exc:
            raise Refusal("Unknown run receipt during archive") from exc
        if (started.date() >= today or started >= now - dt.timedelta(hours=1)
                or receipt.get("status") not in heartbeat.TERMINAL):
            continue
        target = destination / run.name
        if target.exists() or target.is_symlink():
            raise Refusal("Run archive destination already exists: " + run.name)
        os.rename(run, target)
        _sync_dir(runs)
        _sync_dir(destination)
        moved += 1
    # Also flush a relocation completed just before a prior process died.
    _sync_dir(runs)
    _sync_dir(destination)
    return moved


def _archive_locked(root, *, skip_incomplete=False):
    """Publish complete proofs before pruning, under workshop then queue leases."""
    records, next_sequence = _load_writable(root)
    entries = []
    skipped = 0
    for record in records:
        if record["status"] not in ("delivered", "consumed"):
            continue
        entry = _archive_entry(root, record, skip_incomplete=skip_incomplete)
        if entry is None:
            skipped += 1
        else:
            entries.append(entry)
    # Validate every selected receipt/result before publishing any new archive
    # evidence. Publication remains exact-retry safe at each durable cut.
    if entries:
        # In particular, a consumed queue replace may have crossed just before
        # its caller died. Fence that disposition before publishing tombstones.
        _sync_dir(root)
    for entry in entries:
        _publish_archive(root, entry)
    archived = {entry["record"]["sequence"] for entry in entries}
    active = [r for r in records if r["sequence"] not in archived]
    if entries:
        _write(root, active, next_sequence)
    moved = _relocate_runs(root, active)
    result = {"archived_events": len(entries), "relocated_runs": moved,
              "active_events": len(active), "watermark": next_sequence - 1}
    if skip_incomplete:
        result["skipped_incomplete_events"] = skipped
    return result


def archive(root: Path) -> dict:
    """Paused operator maintenance: publish immutable proofs, then prune and relocate."""
    root = _root(root)
    with _workshop_locked(root):
        pause = root / "PAUSED"
        if pause.is_symlink() or not pause.is_file():
            raise Refusal("Pause the workshop before archival")
        with _locked(root):
            return _archive_locked(root)


def maintain(root: Path) -> dict:
    """Automatic nondeleting maintenance; never wait for or compete with a session."""
    root = _root(root)
    pause = root / "PAUSED"
    # Never remove or toggle an operator's pause, including unfamiliar markers.
    if pause.exists() or pause.is_symlink():
        return {"status": "skipped", "reason": "paused"}
    try:
        with _workshop_locked(root):
            with _locked(root, wait=False):
                if pause.exists() or pause.is_symlink():
                    return {"status": "skipped", "reason": "paused"}
                return {"status": "maintained", **_archive_locked(root, skip_incomplete=True)}
    except Busy:
        return {"status": "skipped", "reason": "busy"}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, epilog=(
        "archive requires PAUSED; maintain preserves completed events in the archive "
        "and skips paused/busy workshops without starting a session. "
        "upgrade deliberately migrates supported prior queues to v4; normal writers refuse legacy queues."))
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("action", choices=("enqueue", "status", "archive", "maintain", "upgrade"))
    parser.add_argument("--summary")
    parser.add_argument("--source")
    parser.add_argument("--id", dest="event_id")
    parser.add_argument("--kind")
    parser.add_argument("--body")
    parser.add_argument("--reference")
    args = parser.parse_args(argv)
    try:
        if args.action == "enqueue":
            if args.summary is not None:
                value = {"source": args.source if args.source is not None else "local-shell",
                         "event_id": args.event_id if args.event_id is not None else uuid.uuid4().hex,
                         "kind": args.kind if args.kind is not None else "notification",
                         "summary": args.summary, "body": args.body if args.body is not None else "",
                         "reference": args.reference if args.reference is not None else ""}
            else:
                if any(value is not None for value in (args.source, args.event_id, args.kind,
                                                       args.body, args.reference)):
                    raise Refusal("--summary is required with event flags")
                data = sys.stdin.buffer.read(64 * 1024 + 1)
                if len(data) > 64 * 1024:
                    raise Refusal("Event input too large")
                value = json.loads(data, object_pairs_hook=_pairs)
            result = enqueue(args.root, value)
        elif args.action in ("archive", "maintain", "upgrade"):
            if any(value is not None for value in (args.summary, args.source, args.event_id,
                                                   args.kind, args.body, args.reference)):
                raise Refusal("Event flags are only valid with enqueue")
            result = {"archive": archive, "maintain": maintain, "upgrade": upgrade}[args.action](args.root)
        else:
            if any(value is not None for value in (args.summary, args.source, args.event_id,
                                                   args.kind, args.body, args.reference)):
                raise Refusal("Event flags are only valid with enqueue")
            records = _read(_root(args.root))
            result = {status: sum(r["status"] == status for r in records) for status in sorted(STATUSES)}
        print(json.dumps(result, ensure_ascii=False, sort_keys=True))
        return 0
    except (Refusal, OSError, UnicodeError, json.JSONDecodeError) as exc:
        print(json.dumps({"error": str(exc)}), file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())

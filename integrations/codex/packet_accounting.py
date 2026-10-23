"""Durable packet spend, separate from the bounded optional delivery JSON.

Caller holds the session's reader lock throughout open/commit/publication. SQLite
is the sole accounting owner: JSON mirrors never write spend back into this file.
Packet and call rows deliberately have no eviction policy.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3

import host_files

SCHEMA = "mneme.packet-accounting.v1"
FIELDS = ("attempts", "input_tokens", "output_tokens", "cached_input_tokens",
          "reasoning_output_tokens", "unknown_usage")
ID = re.compile(r"^[A-Za-z0-9_:.-]{1,160}$")
DIGEST = re.compile(r"^[0-9a-f]{64}$")


def empty_usage():
    return dict(attempts=0, input_tokens=0, output_tokens=0, cached_input_tokens=0,
                reasoning_output_tokens=0, unknown_usage=False)


def encoded(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True)


def usage_result(result):
    value = result.get("usage")
    if not isinstance(value, dict):
        return None
    if not all(type(value.get(k)) is int and 0 <= value[k] <= 1_000_000
               for k in ("input_tokens", "output_tokens")):
        return None
    projected = {name: value[name] for name in ("input_tokens", "output_tokens")}
    for name, total in (("cached_input_tokens", "input_tokens"), ("reasoning_output_tokens", "output_tokens")):
        amount = value.get(name)
        projected[name] = amount if type(amount) is int and 0 <= amount <= projected[total] else None
    return projected


def add_result(usage, result):
    if not result.get("provider_attempt"):
        return
    value = usage_result(result)
    if value is None:
        usage.update(unknown_usage=True, cached_input_tokens=None, reasoning_output_tokens=None)
        return
    for name in ("input_tokens", "output_tokens"):
        usage[name] += value[name]
    for name, total in (("cached_input_tokens", "input_tokens"), ("reasoning_output_tokens", "output_tokens")):
        previous, amount = usage.get(name), value.get(name)
        usage[name] = (previous + amount if type(previous) is int and type(amount) is int
                       and 0 <= amount <= value[total] else None)


class Ledger:
    """One lock-scoped transaction; successful commit precedes JSON publication."""
    def __init__(self, path, host, session, data, *, legacy_raw=None, fresh=False):
        if host not in ("codex", "claude_code") or not isinstance(session, str) or not ID.fullmatch(session):
            raise ValueError("invalid_accounting_owner")
        self.path, self.dirty = Path(path).absolute(), False
        self.owner = (host, session)
        self.db = None
        exists = os.path.lexists(self.path)
        if not exists and not (fresh or legacy_raw is not None):
            raise ValueError("current_reader_missing_accounting")
        if not exists and any(os.path.lexists(str(self.path) + suffix) for suffix in ("-journal", "-wal", "-shm")):
            raise ValueError("accounting_sidecar_without_database")
        # SQLite also opens these paths; admit their identity before it does so.
        for suffix in ("", "-journal", "-wal", "-shm"):
            target = Path(str(self.path) + suffix)
            if os.path.lexists(target):
                fd = host_files.open_regular(target)
                os.close(fd)
        if not exists:
            fd = host_files.open_regular(self.path, os.O_RDWR | os.O_CREAT | os.O_EXCL)
            os.close(fd)
        try:
            self.db = sqlite3.connect(self.path.as_uri() + "?mode=rw", uri=True, timeout=0)
            self.db.execute("PRAGMA synchronous=FULL")
            self.db.execute("PRAGMA foreign_keys=ON")
            self.db.execute("BEGIN IMMEDIATE")
            if not exists:
                for statement in (
                    "CREATE TABLE meta (id INTEGER PRIMARY KEY CHECK(id=1), schema TEXT NOT NULL, host TEXT NOT NULL, session TEXT NOT NULL, legacy_hash TEXT, legacy_raw BLOB, aggregate TEXT NOT NULL, routing TEXT, latest_end TEXT, latest_user TEXT)",
                    "CREATE TABLE packets (id TEXT PRIMARY KEY, kind TEXT NOT NULL, turn TEXT NOT NULL, digest TEXT NOT NULL, usage TEXT NOT NULL, UNIQUE(kind,turn))",
                    "CREATE TABLE calls (id TEXT PRIMARY KEY, packet TEXT NOT NULL REFERENCES packets(id), stage TEXT NOT NULL, payload TEXT NOT NULL, status TEXT NOT NULL CHECK(status IN ('reserved','settled','abandoned')), receipt TEXT)",
                    "CREATE UNIQUE INDEX single_reservation ON calls(status) WHERE status='reserved'",
                ):
                    self.db.execute(statement)
                legacy = json.loads(legacy_raw) if legacy_raw is not None else None
                aggregate = {k: legacy[k] for k in FIELDS} if legacy is not None else empty_usage()
                routing = legacy.get("routing_usage") if legacy is not None else None
                self.db.execute("INSERT INTO meta VALUES (1,?,?,?,?,?,?,?,NULL,NULL)",
                    (SCHEMA, host, session, hashlib.sha256(legacy_raw).hexdigest() if legacy_raw is not None else None,
                     legacy_raw, encoded(aggregate), encoded(routing) if routing is not None else None))
                self.dirty = True
                if legacy_raw is not None:
                    active = legacy.get("active")
                    if isinstance(active, dict):
                        packet, _ = self.bind("user_message", active.get("turn"), active.get("request"))
                        poisoned = empty_usage()
                        add_result(poisoned, {"provider_attempt": True, "usage": None})
                        self.db.execute("UPDATE packets SET usage=? WHERE id=?", (encoded(poisoned), packet))
                        if active.get("closed") is True:
                            ended, _ = self.bind("turn_end", active["turn"], active["request"], publish_end=False)
                            self.db.execute("UPDATE packets SET usage=? WHERE id=?", (encoded(poisoned), ended))
            row = self.db.execute("SELECT schema,host,session,legacy_hash FROM meta WHERE id=1").fetchone()
            if row is None or row[:3] != (SCHEMA, host, session):
                raise ValueError("accounting_owner_or_schema_changed")
            if legacy_raw is not None and row[3] != hashlib.sha256(legacy_raw).hexdigest():
                raise ValueError("legacy_accounting_preimage_changed")
            if fresh and exists:
                raise ValueError("accounting_without_reader_state")
        except BaseException:
            self.close()
            raise

    def close(self):
        if self.db is not None:
            self.db.close()
            self.db = None

    def commit(self):
        self.db.commit()
        # SQLite syncs its rollback journal and DB; directory creation also needs
        # a namespace barrier. Windows uses write-through publication for the JSON
        # sibling, rather than pretending POSIX directory fsync exists there.
        if self.dirty and not host_files.WINDOWS:
            host_files.sync_directory(self.path.parent)

    def bind(self, kind, turn, digest, *, publish_end=True):
        if kind not in ("user_message", "turn_end") or not isinstance(turn, str) or not ID.fullmatch(turn) or not isinstance(digest, str) or not DIGEST.fullmatch(digest):
            raise ValueError("invalid_packet_identity")
        row = self.db.execute("SELECT id,digest FROM packets WHERE kind=? AND turn=?", (kind, turn)).fetchone()
        if row:
            if row[1] != digest:
                raise ValueError("packet_prompt_changed")
            return row[0], False
        # The DB's owner binding supplies source_host/session to this identity.
        packet = hashlib.sha256(encoded([*self.owner, kind, turn]).encode()).hexdigest()
        self.db.execute("INSERT INTO packets VALUES (?,?,?,?,?)", (packet, kind, turn, digest, encoded(empty_usage())))
        if kind == "turn_end" and publish_end:
            self.db.execute("UPDATE meta SET latest_end=? WHERE id=1", (packet,))
        elif kind == "user_message":
            self.db.execute("UPDATE meta SET latest_user=? WHERE id=1", (packet,))
        self.dirty = True
        return packet, True

    def lookup(self, kind, turn, digest):
        row = self.db.execute("SELECT id,digest FROM packets WHERE kind=? AND turn=?", (kind, turn)).fetchone()
        return row[0] if row and row[1] == digest else None

    def latest_user(self, turn):
        row = self.db.execute("SELECT p.id,p.digest FROM packets p JOIN meta m ON p.id=m.latest_user WHERE p.turn=?", (turn,)).fetchone()
        return row

    def latest_end(self):
        return self.db.execute("SELECT latest_end FROM meta WHERE id=1").fetchone()[0]

    def publish_end(self, packet):
        """Matching lifecycle close, including a previously observed recording end."""
        row = self.db.execute("SELECT kind FROM packets WHERE id=?", (packet,)).fetchone()
        if row != ("turn_end",):
            raise ValueError("invalid_end_packet")
        if self.latest_end() != packet:
            self.db.execute("UPDATE meta SET latest_end=? WHERE id=1", (packet,))
            self.dirty = True

    def usage(self, packet):
        row = self.db.execute("SELECT usage FROM packets WHERE id=?", (packet,)).fetchone()
        return json.loads(row[0]) if row else None

    def allowed(self, packet, budget, *, calls=1, input_room=0, output_room=0):
        usage = self.usage(packet)
        return bool(usage and not usage["unknown_usage"] and usage["attempts"] + calls <= budget.attempts
                    and usage["input_tokens"] < budget.input_tokens - input_room
                    and usage["output_tokens"] < budget.output_tokens - output_room)

    def reserve(self, packet, call_id, payload, budget, *, stage="selector", calls=1, input_room=0, output_room=0):
        if self.db.execute("SELECT 1 FROM calls WHERE status='reserved' OR id=? LIMIT 1", (call_id,)).fetchone():
            return False
        if not self.allowed(packet, budget, calls=calls, input_room=input_room, output_room=output_room):
            return False
        self.db.execute("INSERT INTO calls VALUES (?,?,?,?, 'reserved', NULL)", (call_id, packet, stage, encoded(payload)))
        usage = self.usage(packet)
        usage["attempts"] += 1
        self.db.execute("UPDATE packets SET usage=? WHERE id=?", (encoded(usage), packet))
        aggregate, routing = self._totals()
        aggregate["attempts"] += 1
        if stage == "routing":
            routing = routing or empty_usage()
            routing["attempts"] += 1
        self._save_totals(aggregate, routing)
        return True

    def _totals(self):
        row = self.db.execute("SELECT aggregate,routing FROM meta WHERE id=1").fetchone()
        return json.loads(row[0]), json.loads(row[1]) if row[1] is not None else None

    def _save_totals(self, aggregate, routing):
        self.db.execute("UPDATE meta SET aggregate=?,routing=? WHERE id=1", (encoded(aggregate), encoded(routing) if routing is not None else None))
        self.dirty = True

    def settle(self, call_id, result):
        row = self.db.execute("SELECT packet,stage,status,receipt FROM calls WHERE id=?", (call_id,)).fetchone()
        if row is None:
            return not result.get("provider_attempt")
        receipt = encoded({"provider_attempt": bool(result.get("provider_attempt")), "usage": usage_result(result)})
        if row[2] == "settled":
            return row[3] == receipt
        # An abandoned call may settle late, but its packet remains poisoned.
        usage = self.usage(row[0])
        aggregate, routing = self._totals()
        for target in (usage, aggregate):
            add_result(target, result)
        if row[1] == "routing":
            add_result(routing, result)
        self.db.execute("UPDATE packets SET usage=? WHERE id=?", (encoded(usage), row[0]))
        self.db.execute("UPDATE calls SET status='settled',receipt=? WHERE id=?", (receipt, call_id))
        self._save_totals(aggregate, routing)
        return True

    def abandon(self):
        """Caller must own the worker lease, proving the previous worker retired."""
        row = self.db.execute("SELECT id,packet,stage FROM calls WHERE status='reserved'").fetchone()
        if row is None:
            return False
        unknown = {"provider_attempt": True, "usage": None}
        usage = self.usage(row[1])
        aggregate, routing = self._totals()
        for target in (usage, aggregate):
            add_result(target, unknown)
        if row[2] == "routing":
            add_result(routing, unknown)
        self.db.execute("UPDATE packets SET usage=? WHERE id=?", (encoded(usage), row[1]))
        self.db.execute("UPDATE calls SET status='abandoned' WHERE id=?", (row[0],))
        self._save_totals(aggregate, routing)
        return True

    def mirror(self, data):
        aggregate, routing = self._totals()
        data.update(aggregate)
        if routing is not None:
            data["routing_usage"] = routing
        else:
            data.pop("routing_usage", None)
        row = self.db.execute("SELECT id,payload FROM calls WHERE status='reserved'").fetchone()
        data["reservation"] = json.loads(row[1]) if row else None
        data["reservation_call_id"] = row[0] if row else None

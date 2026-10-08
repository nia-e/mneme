"""Private, versioned Signal conversation ledger. No Signal RPC or workshop dispatch here.

A caller holds the bridge's single-process lease for the entire Store lifetime. This
ledger deliberately has no pruning: exhausted capacity is visible and needs human
retention policy, not a surprise deletion of unanswered conversation or dedup IDs.
"""

from __future__ import annotations

from contextlib import contextmanager
import hashlib
import json
import math
import os
from pathlib import Path
import re
import sqlite3
import stat

SIGNAL_SOURCE = "signal-owner"
SIGNAL_KIND = "signal-burst"
SCHEMA = "mneme.signal.ledger.v1"
MAX_INPUT = 4000
MAX_REPLY = 4000
MAX_BODY = 8000
MAX_MESSAGES = 8192
MAX_BURSTS = 8192
MAX_OUTBOX = 8192
# All retained variable-size columns, including copied message-ID arrays, count.
MAX_TEXT_BYTES = 16_000_000
MAX_DB_BYTES = 64 * 1024 * 1024
QUIET_SECONDS = 10
MAX_BURST_SECONDS = 45
_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.:-]{0,255}\Z")
_RUN = re.compile(r"[A-Za-z0-9_-]{1,128}\Z")
_TABLES = {"metadata", "messages", "bursts", "outbox"}
_DDL = (
    "CREATE TABLE metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    "CREATE TABLE messages(message_id TEXT PRIMARY KEY, timestamp_ms INTEGER NOT NULL, "
    "text TEXT NOT NULL, arrival REAL NOT NULL, generation INTEGER NOT NULL, "
    "answered_event_id TEXT)",
    "CREATE TABLE bursts(event_id TEXT PRIMARY KEY, generation INTEGER NOT NULL UNIQUE, "
    "body TEXT NOT NULL, message_ids TEXT NOT NULL, enqueued INTEGER NOT NULL DEFAULT 0)",
    "CREATE TABLE outbox(id TEXT PRIMARY KEY, event_id TEXT NOT NULL UNIQUE, "
    "run_id TEXT NOT NULL, generation INTEGER NOT NULL, text TEXT NOT NULL, "
    "status TEXT NOT NULL, timestamp_ms INTEGER, "
    "FOREIGN KEY(event_id) REFERENCES bursts(event_id))",
)


class StoreError(ValueError):
    """Unsafe, unsupported, or invalid ledger operation."""


class CapacityError(StoreError):
    """Admission or publication would exceed an explicit ledger bound."""


class ConflictError(StoreError):
    """An identity was replayed with different contents."""


def _time(value):
    if type(value) not in (int, float) or not math.isfinite(value) or value < 0:
        raise StoreError("now must be finite nonnegative Unix seconds")
    return float(value)


def _identity(value, pattern=_ID):
    if not isinstance(value, str) or pattern.fullmatch(value) is None:
        raise StoreError("invalid bounded identity")
    return value


def _text(value, limit, *, empty=False):
    if not isinstance(value, str) or len(value) > limit or (not empty and not value):
        raise StoreError("invalid bounded text")
    if "\x00" in value:
        raise StoreError("NUL in text")
    return value


def _row_burst(row):
    return {"event_id": row[0], "generation": row[1], "body": row[2],
            "message_ids": json.loads(row[3])}


def _row_outbox(row):
    return {"id": row[0], "event_id": row[1], "generation": row[2],
            "text": row[3], "status": row[4]}


def _is_full(exc):
    code = getattr(exc, "sqlite_errorcode", None)
    return (code is not None and code & 0xff == 13) or str(exc) == "database or disk is full"


class Store:
    def __init__(self, path, *, owner_id, create=False):
        path = Path(path)
        if not path.is_absolute() or not path.parent.is_dir():
            raise StoreError("ledger needs an existing absolute parent")
        self.path = path.parent.resolve(strict=True) / path.name
        _identity(owner_id)
        self.owner_id = owner_id
        if type(create) is not bool:
            raise StoreError("create must be boolean")
        if not self.path.parent.is_dir():
            raise StoreError("ledger needs an existing absolute canonical parent")
        self.conn = None
        if create:
            self._create()
        else:
            self._admit_existing()

    def _safe_file(self):
        try:
            info = self.path.lstat()
        except FileNotFoundError as exc:
            raise StoreError("ledger absent; run explicit init") from exc
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > MAX_DB_BYTES:
            raise StoreError("unsafe or oversized ledger file")
        return info

    def _uri(self, mode):
        return self.path.as_uri() + "?mode=" + mode

    def _connect(self, mode):
        return sqlite3.connect(self._uri(mode), uri=True, isolation_level=None, timeout=0)

    def _classify(self, conn):
        try:
            schema_rows = list(conn.execute("SELECT type,name,tbl_name,sql FROM sqlite_master"))
            names = {row[1] for row in schema_rows if row[0] == "table"}
            if names != _TABLES or conn.execute("PRAGMA user_version").fetchone()[0] != 1:
                raise StoreError("unknown ledger schema; inspect before recovery")
            stored_ddl = {" ".join(row[3].split()) for row in schema_rows if row[0] == "table" and row[3] is not None}
            if stored_ddl != {" ".join(ddl.split()) for ddl in _DDL}:
                raise StoreError("unknown ledger table definition; inspect before recovery")
            if any(row[0] != "table" and
                   (row[0] != "index" or row[3] is not None or
                    not row[1].startswith("sqlite_autoindex_")) for row in schema_rows):
                raise StoreError("unknown ledger schema object; inspect before recovery")
            meta = dict(conn.execute("SELECT key, value FROM metadata"))
            if set(meta) != {"schema", "owner_id", "generation"} or meta["schema"] != SCHEMA or meta["owner_id"] != self.owner_id:
                raise StoreError("ledger schema or owner binding mismatch")
            if not meta["generation"].isdigit():
                raise StoreError("invalid ledger generation")
            columns = {
                "metadata": {"key", "value"},
                "messages": {"message_id", "timestamp_ms", "text", "arrival", "generation", "answered_event_id"},
                "bursts": {"event_id", "generation", "body", "message_ids", "enqueued"},
                "outbox": {"id", "event_id", "run_id", "generation", "text", "status", "timestamp_ms"},
            }
            for name, expected in columns.items():
                if {row[1] for row in conn.execute(f"PRAGMA table_info({name})")} != expected:
                    raise StoreError("unknown ledger columns; inspect before recovery")
        except sqlite3.DatabaseError as exc:
            raise StoreError("unreadable ledger; inspect before recovery") from exc

    def _admit_existing(self):
        self._safe_file()
        # Read-only classification happens before any write-capable SQLite handle or PRAGMA.
        try:
            probe = self._connect("ro")
            try:
                self._classify(probe)
            finally:
                probe.close()
            self._safe_file()
            self.conn = self._connect("rw")
            self._classify(self.conn)
            self.conn.execute("PRAGMA synchronous=FULL")
            self.conn.execute("PRAGMA foreign_keys=ON")
            self._limit_pages()
        except (sqlite3.DatabaseError, OSError, StoreError) as exc:
            if self.conn is not None:
                self.conn.close()
                self.conn = None
            if isinstance(exc, StoreError):
                raise
            raise StoreError("cannot admit existing ledger") from exc

    def _create(self):
        if self.path.exists() or self.path.is_symlink():
            raise StoreError("ledger already exists; init never overwrites")
        fd = None
        owned = False
        try:
            fd = os.open(self.path, os.O_CREAT | os.O_EXCL | os.O_RDWR | os.O_NOFOLLOW, 0o600)
            owned = True
            os.close(fd)
            fd = None
            conn = self._connect("rw")
            self.conn = conn
            conn.execute("PRAGMA synchronous=FULL")
            conn.execute("PRAGMA foreign_keys=ON")
            self._limit_pages()
            conn.executescript("BEGIN;\n" + ";\n".join(_DDL) + ";\n"
                               "INSERT INTO metadata VALUES('schema', 'mneme.signal.ledger.v1');\n"
                               "INSERT INTO metadata VALUES('generation', '0');\n"
                               "PRAGMA user_version=1;\nCOMMIT;")
            conn.execute("INSERT INTO metadata VALUES('owner_id', ?)", (self.owner_id,))
            self._classify(conn)
            database = os.open(self.path, os.O_RDONLY)
            try:
                os.fsync(database)
            finally:
                os.close(database)
            directory = os.open(self.path.parent, os.O_RDONLY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
        except BaseException:
            if fd is not None:
                os.close(fd)
            if self.conn is not None:
                self.conn.close()
                self.conn = None
            if owned:
                try:
                    self.path.unlink()
                except FileNotFoundError:
                    pass
            raise

    @contextmanager
    def _write(self):
        if self.conn is None:
            raise StoreError("ledger closed")
        self.conn.execute("BEGIN IMMEDIATE")
        try:
            yield
        except BaseException as exc:
            if self.conn.in_transaction:
                self.conn.execute("ROLLBACK")
            if isinstance(exc, sqlite3.DatabaseError) and _is_full(exc):
                raise CapacityError("ledger physical capacity reached; no data was accepted") from exc
            raise
        else:
            try:
                self.conn.execute("COMMIT")
            except sqlite3.DatabaseError as exc:
                if self.conn.in_transaction:
                    self.conn.execute("ROLLBACK")
                if _is_full(exc):
                    raise CapacityError("ledger physical capacity reached; no data was accepted") from exc
                raise StoreError("ledger commit failed; inspect before retry") from exc

    def _limit_pages(self):
        page_size = self.conn.execute("PRAGMA page_size").fetchone()[0]
        pages = MAX_DB_BYTES // page_size
        actual = self.conn.execute(f"PRAGMA max_page_count={pages}").fetchone()[0]
        if actual > pages:
            raise StoreError("ledger already exceeds physical capacity")

    def _generation(self):
        return int(self.conn.execute("SELECT value FROM metadata WHERE key='generation'").fetchone()[0])

    def _count(self, table):
        return self.conn.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]

    def _bytes(self):
        result = self.conn.execute("SELECT COALESCE(SUM(length(CAST(message_id AS BLOB))+length(CAST(text AS BLOB))),0) FROM messages").fetchone()[0]
        result += self.conn.execute("SELECT COALESCE(SUM(length(CAST(event_id AS BLOB))+length(CAST(body AS BLOB))+length(CAST(message_ids AS BLOB))),0) FROM bursts").fetchone()[0]
        result += self.conn.execute("SELECT COALESCE(SUM(length(CAST(id AS BLOB))+length(CAST(event_id AS BLOB))+length(CAST(run_id AS BLOB))+length(CAST(text AS BLOB))),0) FROM outbox").fetchone()[0]
        return result

    def ingest(self, message_id, timestamp_ms, text, now):
        _identity(message_id)
        if type(timestamp_ms) is not int or not 0 <= timestamp_ms <= 2**63 - 1:
            raise StoreError("invalid Signal timestamp")
        _text(text, MAX_INPUT)
        now = _time(now)
        with self._write():
            old = self.conn.execute("SELECT timestamp_ms,text FROM messages WHERE message_id=?", (message_id,)).fetchone()
            if old is not None:
                if old != (timestamp_ms, text):
                    raise ConflictError("Signal message identity conflicts with retained message")
                return {"accepted": False, "duplicate": True, "generation": self._generation()}
            if self._count("messages") >= MAX_MESSAGES or self._bytes() + len(message_id.encode("utf-8")) + len(text.encode("utf-8")) > MAX_TEXT_BYTES:
                raise CapacityError("ledger message capacity reached; raw frame remains in receiver spool")
            generation = self._generation() + 1
            self.conn.execute("INSERT INTO messages VALUES(?,?,?,?,?,NULL)", (message_id, timestamp_ms, text, now, generation))
            self.conn.execute("UPDATE metadata SET value=? WHERE key='generation'", (str(generation),))
            self.conn.execute("UPDATE outbox SET status='superseded' WHERE status='pending' AND generation<?", (generation,))
            return {"accepted": True, "duplicate": False, "generation": generation}

    def _body(self, generation):
        pending = self.conn.execute("SELECT message_id,text FROM messages WHERE answered_event_id IS NULL AND generation<=? ORDER BY generation", (generation,)).fetchall()
        if not pending:
            return None
        new_lines = ["Owner: " + text for _, text in pending]
        core = "\n\n".join(new_lines)
        if len(core) > MAX_BODY:
            raise CapacityError("all unanswered messages cannot fit one burst; conversation held for review")
        context = []
        rows = list(reversed(self.conn.execute("SELECT m.answered_event_id,m.text,o.text FROM messages m LEFT JOIN outbox o ON o.event_id=m.answered_event_id WHERE m.answered_event_id IS NOT NULL ORDER BY m.generation DESC LIMIT 3").fetchall()))
        for index, (event_id, received, replied) in enumerate(rows):
            context.append("Owner: " + received)
            if index + 1 == len(rows) or rows[index + 1][0] != event_id:
                context.append("Assistant: " + (replied or ""))
        if context:
            prefix = "Recent answered context:\n" + "\n".join(context) + "\n\nUnanswered:\n"
            if len(prefix) + len(core) <= MAX_BODY:
                core = prefix + core
        return core, [row[0] for row in pending]

    def prepare_burst(self, now):
        now = _time(now)
        with self._write():
            generation = self._generation()
            prior = self.conn.execute("SELECT event_id,generation,body,message_ids,enqueued FROM bursts WHERE generation=?", (generation,)).fetchone()
            if prior is not None:
                return None if prior[4] else _row_burst(prior[:4])
            last_burst = self.conn.execute("SELECT COALESCE(MAX(generation),0) FROM bursts").fetchone()[0]
            arrivals = self.conn.execute("SELECT MIN(arrival),MAX(arrival) FROM messages WHERE generation>?", (last_burst,)).fetchone()
            if arrivals[0] is None or (now - arrivals[1] < QUIET_SECONDS and now - arrivals[0] < MAX_BURST_SECONDS):
                return None
            assembled = self._body(generation)
            if assembled is None:
                return None
            body, ids = assembled
            packed = json.dumps(ids, ensure_ascii=False, separators=(",", ":"))
            digest = hashlib.sha256(f"{self.owner_id}:{generation}".encode()).hexdigest()[:32]
            event_id = "signal-" + digest
            encoded = sum(len(item.encode("utf-8")) for item in (body, packed, event_id))
            if self._count("bursts") >= MAX_BURSTS or self._bytes() + encoded > MAX_TEXT_BYTES:
                raise CapacityError("ledger burst capacity reached; conversation remains unanswered")
            self.conn.execute("INSERT INTO bursts VALUES(?,?,?,?,0)", (event_id, generation, body, packed))
            return {"event_id": event_id, "generation": generation, "body": body, "message_ids": ids}

    def pending_bursts(self):
        rows = self.conn.execute("SELECT event_id,generation,body,message_ids FROM bursts WHERE enqueued=0 ORDER BY generation LIMIT ?", (MAX_BURSTS,)).fetchall()
        return [_row_burst(row) for row in rows]

    def awaiting_replies(self):
        """Enqueued events still lacking a completed-run disposition."""
        rows = self.conn.execute("""SELECT b.event_id,b.generation,b.body,b.message_ids
            FROM bursts b LEFT JOIN outbox o ON o.event_id=b.event_id
            WHERE b.enqueued=1 AND o.id IS NULL ORDER BY b.generation LIMIT ?""",
            (MAX_BURSTS,)).fetchall()
        return [_row_burst(row) for row in rows]

    def mark_enqueued(self, event_id):
        _identity(event_id)
        with self._write():
            if self.conn.execute("UPDATE bursts SET enqueued=1 WHERE event_id=?", (event_id,)).rowcount != 1:
                raise StoreError("unknown burst event")

    def queue_reply(self, event_id, run_id, text):
        _identity(event_id)
        _identity(run_id, _RUN)
        _text(text, MAX_REPLY, empty=True)
        with self._write():
            burst = self.conn.execute("SELECT generation FROM bursts WHERE event_id=?", (event_id,)).fetchone()
            if burst is None:
                raise StoreError("unknown burst event")
            existing = self.conn.execute("SELECT id,event_id,generation,text,status,run_id FROM outbox WHERE event_id=?", (event_id,)).fetchone()
            if existing is not None:
                if existing[1] != event_id or existing[5] != run_id or existing[3] != text:
                    raise ConflictError("reply event/run identity conflict")
                return _row_outbox(existing[:5])
            generation = burst[0]
            status = "empty" if not text else ("superseded" if generation != self._generation() else "pending")
            ident = "reply-" + hashlib.sha256((event_id + ":" + run_id).encode()).hexdigest()[:32]
            encoded = sum(len(item.encode("utf-8")) for item in (ident, event_id, run_id, text))
            if self._count("outbox") >= MAX_OUTBOX or self._bytes() + encoded > MAX_TEXT_BYTES:
                raise CapacityError("ledger outbox capacity reached")
            self.conn.execute("INSERT INTO outbox VALUES(?,?,?,?,?,?,NULL)", (ident, event_id, run_id, generation, text, status))
            return {"id": ident, "event_id": event_id, "generation": generation, "text": text, "status": status}

    def pending_outbox(self):
        rows = self.conn.execute("SELECT id,event_id,generation,text,status FROM outbox WHERE status='pending' AND generation=(SELECT CAST(value AS INTEGER) FROM metadata WHERE key='generation') ORDER BY rowid LIMIT ?", (MAX_OUTBOX,)).fetchall()
        return [_row_outbox(row) for row in rows]

    def begin_send(self, outbox_id):
        _identity(outbox_id)
        with self._write():
            row = self.conn.execute("SELECT status,generation FROM outbox WHERE id=?", (outbox_id,)).fetchone()
            if row is None or row[0] != "pending":
                return False
            if row[1] != self._generation():
                self.conn.execute("UPDATE outbox SET status='superseded' WHERE id=?", (outbox_id,))
                return False
            self.conn.execute("UPDATE outbox SET status='sending' WHERE id=?", (outbox_id,))
            return True

    def finish_send(self, outbox_id, outcome, *, timestamp_ms=None):
        _identity(outbox_id)
        if outcome not in ("accepted", "failed", "unknown"):
            raise StoreError("invalid send outcome")
        if timestamp_ms is not None and (type(timestamp_ms) is not int or not 0 <= timestamp_ms <= 2**63 - 1):
            raise StoreError("invalid send timestamp")
        with self._write():
            row = self.conn.execute("SELECT event_id,generation,status FROM outbox WHERE id=?", (outbox_id,)).fetchone()
            if row is None or row[2] != "sending":
                raise StoreError("send completion needs a sending outbox item")
            if outcome == "accepted" and row[1] != self._generation():
                # RPC may have crossed while a new inbound was accepted. The sent
                # reply still answers its own generation, never newer messages.
                pass
            self.conn.execute("UPDATE outbox SET status=?,timestamp_ms=? WHERE id=?", (outcome, timestamp_ms, outbox_id))
            if outcome == "accepted":
                self.conn.execute("UPDATE messages SET answered_event_id=? WHERE answered_event_id IS NULL AND generation<=?", (row[0], row[1]))

    def recover_uncertain_sends(self):
        with self._write():
            return self.conn.execute("UPDATE outbox SET status='unknown' WHERE status='sending'").rowcount

    def status(self):
        counts = {name: self._count(name) for name in ("messages", "bursts", "outbox")}
        counts.update({"generation": self._generation(), "unanswered": self.conn.execute("SELECT COUNT(*) FROM messages WHERE answered_event_id IS NULL").fetchone()[0],
                       "pending_bursts": self.conn.execute("SELECT COUNT(*) FROM bursts WHERE enqueued=0").fetchone()[0],
                       "pending_outbox": self.conn.execute("SELECT COUNT(*) FROM outbox WHERE status='pending'").fetchone()[0],
                       "unknown_outbox": self.conn.execute("SELECT COUNT(*) FROM outbox WHERE status='unknown'").fetchone()[0],
                       "max_messages": MAX_MESSAGES, "max_bursts": MAX_BURSTS, "max_outbox": MAX_OUTBOX,
                       "max_retained_bytes": MAX_TEXT_BYTES, "retained_bytes": self._bytes(),
                       "max_db_bytes": MAX_DB_BYTES})
        try:
            self._body(self._generation())
            counts["body_capacity_blocked"] = False
        except CapacityError:
            counts["body_capacity_blocked"] = True
        return counts

    def close(self):
        if self.conn is not None:
            self.conn.close()
            self.conn = None

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

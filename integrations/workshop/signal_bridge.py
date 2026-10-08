#!/usr/bin/env python3
"""Private Signal-to-workshop bridge: bounded receive spool, wake, and replies.

The bridge never starts Codex. The owner/account pins are private configuration,
not model context; Signal receipt acknowledgments precede our durable spool.
"""

import argparse
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import stat
import sys
import time
import uuid

import heartbeat
import signal_jsonrpc
import signal_tools
import signal_store
import wake


CONFIG_SCHEMA = "mneme.workshop.signal-bridge.v1"
CONFIG_KEYS = {"schema", "workshop_root", "state_dir", "store_path", "signal_cli",
               "signal_data_dir", "signal_rpc_config", "account", "owner_id"}
PROFILE_KEYS = {"asset_roots", "portrait_path"}
MAX_CONFIG = 8192
MAX_RAW = 64 * 1024
MAX_SPOOL = 128
MAX_SPOOL_BYTES = 2 * 1024 * 1024
MAX_REPLY = 4000
RUN_ID = re.compile(r"[A-Za-z0-9_-]{1,128}\Z")
SPOOL_NAME = re.compile(r"([0-9]{12})-[0-9a-f]{32}\.json\Z")
SPOOL_TEMP = re.compile(r"\.([0-9]{12}-[0-9a-f]{32}\.json)\.tmp\Z")
SOURCE = signal_store.SIGNAL_SOURCE
KIND = signal_store.SIGNAL_KIND


class Refusal(ValueError):
    """Unsafe local state or invalid private configuration."""


def _pairs(items):
    value = {}
    for key, item in items:
        if key in value:
            raise Refusal("duplicate JSON key")
        value[key] = item
    return value


def _read_json(path, limit, *, private=False):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    except OSError as exc:
        raise Refusal("required private file is unavailable") from exc
    try:
        info = os.fstat(fd)
        if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > limit
                or (private and (info.st_uid != os.getuid() or info.st_mode & 0o077))):
            raise Refusal("unsafe or oversized private file")
        with os.fdopen(fd, "rb") as stream:
            fd = -1
            data = stream.read(limit + 1)
        if len(data) > limit:
            raise Refusal("oversized private file")
        return json.loads(data.decode("utf-8"), object_pairs_hook=_pairs)
    except (UnicodeError, ValueError, RecursionError) as exc:
        raise Refusal("invalid private JSON file") from exc
    finally:
        if fd >= 0:
            os.close(fd)


def _absolute(value, label):
    if not isinstance(value, str) or not value or len(value) > 1024:
        raise Refusal("invalid " + label)
    path = Path(value)
    if not path.is_absolute() or ".." in path.parts:
        raise Refusal("invalid " + label)
    return path


def _uuid(value, label):
    if not isinstance(value, str) or len(value) != 36:
        raise Refusal("invalid " + label)
    try:
        if str(uuid.UUID(value)) != value:
            raise ValueError("noncanonical UUID")
    except ValueError as exc:
        raise Refusal("invalid " + label) from exc
    return value


class Config:
    def __init__(self, path):
        path = _absolute(str(path), "config path")
        data = _read_json(path, MAX_CONFIG, private=True)
        if (not isinstance(data, dict) or set(data) not in (CONFIG_KEYS, CONFIG_KEYS | {"agent_tools"})
                or data["schema"] != CONFIG_SCHEMA):
            raise Refusal("unknown bridge configuration")
        self.config_path = path
        self.workshop_root = _absolute(data["workshop_root"], "workshop root")
        self.state_dir = _absolute(data["state_dir"], "state directory")
        self.store_path = _absolute(data["store_path"], "store path")
        self.signal_cli = _absolute(data["signal_cli"], "Signal CLI")
        self.signal_data_dir = _absolute(data["signal_data_dir"], "Signal data directory")
        self.signal_rpc_config = _absolute(data["signal_rpc_config"], "Signal RPC configuration")
        self.account = _uuid(data["account"], "account ACI")
        self.owner_id = _uuid(data["owner_id"], "owner ACI")
        self.agent_tools = None
        if "agent_tools" in data:
            tools = data["agent_tools"]
            if not isinstance(tools, dict) or set(tools) != PROFILE_KEYS:
                raise Refusal("invalid agent tools configuration")
            roots = tools["asset_roots"]
            if not isinstance(roots, list) or not 1 <= len(roots) <= 8:
                raise Refusal("invalid profile asset roots")
            asset_roots = tuple(_absolute(value, "profile asset root") for value in roots)
            if len(set(asset_roots)) != len(asset_roots):
                raise Refusal("duplicate profile asset root")
            for root in asset_roots:
                if root.is_symlink() or not root.is_dir() or root.resolve(strict=True) != root:
                    raise Refusal("unavailable canonical profile asset root")
            portrait = _absolute(tools["portrait_path"], "profile portrait")
            if not any(portrait.is_relative_to(root) for root in asset_roots):
                raise Refusal("profile portrait outside admitted roots")
            try:
                signal_tools.validate_image_path(portrait, asset_roots)
            except ValueError as exc:
                raise Refusal("unavailable admitted profile portrait") from exc
            self.agent_tools = {"asset_roots": asset_roots, "portrait_path": portrait}
        if self.store_path.parent != self.state_dir or self.state_dir == self.workshop_root:
            raise Refusal("bridge store must be directly under separate state directory")
        if self.state_dir.is_relative_to(self.workshop_root):
            raise Refusal("bridge state must be outside model-writable workshop")
        for item, label in ((self.workshop_root, "workshop root"),
                            (self.signal_data_dir, "Signal data directory")):
            if item.is_symlink() or not item.is_dir() or item.resolve(strict=True) != item:
                raise Refusal("unavailable canonical " + label)
        if self.signal_cli.is_symlink() or not self.signal_cli.is_file() or not os.access(self.signal_cli, os.X_OK):
            raise Refusal("pinned Signal CLI unavailable")
        signal_jsonrpc.validate_config(self.signal_rpc_config)


def _sync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


@contextlib.contextmanager
def _lock(state_dir, *, create=False, shared=False):
    path = state_dir / ".bridge.lock"
    flags = os.O_NOFOLLOW | (os.O_RDWR | os.O_CREAT if create else os.O_RDONLY)
    try:
        fd = os.open(path, flags, 0o600)
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            raise Refusal("unsafe bridge lock")
        fcntl.flock(fd, (fcntl.LOCK_SH if shared else fcntl.LOCK_EX) | fcntl.LOCK_NB)
        yield
    except BlockingIOError as exc:
        raise Refusal("bridge already running") from exc
    finally:
        if "fd" in locals():
            os.close(fd)


class Spool:
    def __init__(self, directory):
        if directory.is_symlink() or not directory.is_dir() or directory.resolve(strict=True) != directory:
            raise Refusal("unsafe receive spool")
        info = directory.stat()
        if info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise Refusal("nonprivate receive spool")
        self.directory = directory

    def inspect(self):
        """Read-only count for status; never recover or consume an interrupted write."""
        ready = interrupted = 0
        for path in self.directory.iterdir():
            if SPOOL_NAME.fullmatch(path.name):
                ready += 1
            elif SPOOL_TEMP.fullmatch(path.name):
                interrupted += 1
            else:
                raise Refusal("unknown receive spool entry")
            if path.is_symlink() or not path.is_file() or path.stat().st_nlink != 1:
                raise Refusal("unsafe receive spool entry")
            if path.stat().st_size > MAX_RAW:
                raise Refusal("unsafe receive spool entry")
        if ready + interrupted > MAX_SPOOL:
            raise Refusal("receive spool capacity exceeded")
        return ready, interrupted

    def _files(self):
        entries = []
        used = 0
        for path in self.directory.iterdir():
            if SPOOL_TEMP.fullmatch(path.name) is not None:
                self._recover_temp(path)
                path = self.directory / SPOOL_TEMP.fullmatch(path.name).group(1)
            if SPOOL_NAME.fullmatch(path.name) is None or path.is_symlink() or not path.is_file():
                raise Refusal("unknown receive spool entry")
            info = path.stat()
            if info.st_nlink != 1 or info.st_size > MAX_RAW:
                raise Refusal("unsafe receive spool entry")
            entries.append(path)
            used += info.st_size
            if len(entries) > MAX_SPOOL or used > MAX_SPOOL_BYTES:
                raise Refusal("receive spool capacity exceeded")
        return sorted(entries), used

    def _recover_temp(self, path):
        """Promote a complete interrupted write; retain an incomplete one for repair."""
        match = SPOOL_TEMP.fullmatch(path.name)
        if match is None:
            raise Refusal("unknown receive spool entry")
        target = self.directory / match.group(1)
        if target.exists() or target.is_symlink():
            raise Refusal("ambiguous interrupted receive spool entry")
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        try:
            info = os.fstat(fd)
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > MAX_RAW:
                raise Refusal("unsafe interrupted receive spool entry")
            chunks = []
            remaining = MAX_RAW + 1
            while remaining:
                chunk = os.read(fd, remaining)
                if not chunk:
                    break
                chunks.append(chunk)
                remaining -= len(chunk)
            data = b"".join(chunks)
            if len(data) <= MAX_RAW and data.endswith(b"\n"):
                os.fsync(fd)
        finally:
            os.close(fd)
        if not data.endswith(b"\n"):
            raise Refusal("incomplete receive spool entry; preserve for operator recovery")
        try:
            raw = json.loads(data.decode("utf-8"), object_pairs_hook=_pairs)
        except (UnicodeError, ValueError, RecursionError) as exc:
            raise Refusal("invalid interrupted receive spool entry; preserve for operator recovery") from exc
        if not isinstance(raw, dict):
            raise Refusal("invalid interrupted receive spool entry; preserve for operator recovery")
        os.replace(path, target)
        _sync_dir(self.directory)

    def append(self, raw):
        if not isinstance(raw, dict):
            raise Refusal("invalid Signal notification")
        data = (json.dumps(raw, ensure_ascii=False, allow_nan=False, separators=(",", ":")) + "\n").encode()
        entries, used = self._files()
        if len(data) > MAX_RAW or len(entries) >= MAX_SPOOL or used + len(data) > MAX_SPOOL_BYTES:
            raise Refusal("receive spool capacity exceeded")
        sequence = max((int(SPOOL_NAME.fullmatch(path.name).group(1)) for path in entries), default=0) + 1
        if sequence > 999999999999:
            raise Refusal("receive spool sequence exhausted")
        path = self.directory / (f"{sequence:012d}-" + uuid.uuid4().hex + ".json")
        temporary = self.directory / ("." + path.name + ".tmp")
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        _sync_dir(self.directory)
        return path

    def replay(self, consumer):
        entries, _used = self._files()
        for path in entries:
            raw = _read_json(path, MAX_RAW)
            consumer(raw)
            path.unlink()
            _sync_dir(self.directory)
        return len(entries)


def _message(raw, config):
    """Return authenticated direct text or None; never trust sender labels alone."""
    if not isinstance(raw, dict) or raw.get("method") != "receive":
        return None
    params = raw.get("params")
    if isinstance(params, dict) and "exception" in params:
        return None
    result = params.get("result") if isinstance(params, dict) else None
    if not isinstance(result, dict) or result.get("account") != config.account:
        return None
    if "exception" in result:
        return None
    envelope = result.get("envelope")
    if not isinstance(envelope, dict) or envelope.get("sourceUuid") != config.owner_id:
        return None
    if "exception" in envelope:
        return None
    data = envelope.get("dataMessage")
    if not isinstance(data, dict):
        return None
    if any("group" in key.lower() for key in (*envelope.keys(), *data.keys())):
        return None
    message = data.get("message")
    if not isinstance(message, str) or not message.strip():
        return None
    # A legitimate but overlong owner message remains spooled, visibly blocked.
    if len(message) > signal_store.MAX_INPUT:
        raise signal_store.CapacityError("owner text exceeds bounded input")
    timestamp = envelope.get("timestamp")
    if type(timestamp) is not int or not 0 <= timestamp < 2 ** 63:
        return None
    if "timestamp" in data and data["timestamp"] != timestamp:
        return None
    device = envelope.get("sourceDevice")
    if type(device) is not int or not 0 <= device <= 255:
        return None
    return {"message_id": f"{timestamp}:{device}", "timestamp_ms": timestamp, "text": message}


def _event_for_burst(burst):
    return {"source": SOURCE, "event_id": burst["event_id"], "kind": KIND,
            "summary": "Private owner conversation burst", "body": burst["body"],
            "reference": ""}


def _reply_proof(root, burst, *, active_allowed):
    event_id = burst["event_id"]
    found = wake.lookup(root, SOURCE, event_id)
    if found is None:
        return None
    record = found["record"]
    if record["event"] != _event_for_burst(burst):
        raise Refusal("Signal burst payload mismatch")
    # Reading a pending burst consumes its wake-up, not its reply obligation.
    # It is never a synthetic completed run or permission to send an answer.
    if record["status"] == "consumed":
        return None
    run_id = record["claimed_run"]
    if found["location"] == "archive":
        terminal = found["terminal"]
        if not isinstance(run_id, str) or RUN_ID.fullmatch(run_id) is None:
            raise Refusal("unsafe archived run identity")
        if terminal["status"] == "replied":
            return run_id, terminal["reply"]
        if terminal["status"] == "completed_without_reply":
            return run_id, ""
        raise Refusal("unknown archived terminal outcome")
    if found["location"] != "active" or record["status"] != "delivered" or run_id is None or not active_allowed:
        return None
    if not isinstance(run_id, str) or RUN_ID.fullmatch(run_id) is None:
        raise Refusal("unsafe run identity")
    run = root / "runs" / run_id
    if (root / "runs").is_symlink() or run.is_symlink() or not run.is_dir():
        return None
    receipt_path = run / "receipt.json"
    if not receipt_path.exists() and not receipt_path.is_symlink():
        return None
    receipt = heartbeat.json_object(heartbeat.bounded_bytes(receipt_path, heartbeat.MAX_RESULT))
    if (not isinstance(receipt, dict) or receipt.get("schema") not in heartbeat.REPLY_RECEIPT_SCHEMAS
            or receipt.get("status") not in heartbeat.TERMINAL | {"running"}
            or receipt.get("run_id") != run_id):
        raise Refusal("unknown run receipt")
    if receipt["status"] != "completed":
        return None
    cycle_class = heartbeat.receipt_class(receipt)
    if receipt["schema"] == heartbeat.RECEIPT_SCHEMA and cycle_class != "interactive":
        raise Refusal("Signal reply came from noninteractive run")
    identities = receipt.get("event_ids")
    if (not isinstance(identities, list) or len(identities) > 4
            or receipt.get("event_count") != len(identities)):
        raise Refusal("invalid completed event identities")
    events = []
    seen = set()
    for item in identities:
        if (not isinstance(item, dict) or set(item) != {"source", "event_id"}
                or not isinstance(item["source"], str) or not isinstance(item["event_id"], str)):
            raise Refusal("invalid completed event identity")
        identity = item["source"], item["event_id"]
        if identity in seen:
            raise Refusal("duplicate completed event identity")
        seen.add(identity)
        events.append({"event": item})
    if (SOURCE, event_id) not in seen:
        raise Refusal("completed receipt omits Signal burst")
    result = heartbeat.json_object(heartbeat.bounded_bytes(run / "result.json", heartbeat.MAX_RESULT))
    heartbeat.validate_result(result, root, events)
    matches = [item["text"] for item in result["replies"]
               if item["source"] == SOURCE and item["event_id"] == event_id]
    return run_id, matches[0] if matches else ""


class Bridge:
    def __init__(self, config, store, spool, *, clock=time.time):
        self.config, self.store, self.spool, self.clock = config, store, spool, clock
        self.ignored = 0
        self.accepted = 0

    def _consume(self, raw):
        message = _message(raw, self.config)
        if message is None:
            self.ignored += 1
            return
        result = self.store.ingest(message["message_id"], message["timestamp_ms"],
                                   message["text"], self.clock())
        if result["accepted"]:
            self.accepted += 1

    def receive(self, raw):
        self.spool.append(raw)  # fsync before interpreting or acknowledging locally
        self.spool.replay(self._consume)

    def _reconcile_seen(self):
        """Consume only fully seen pending bursts; the queue rechecks exact payloads.

        The ledger commit precedes this separate queue operation. A busy queue or
        a crash can leave a redundant wake-up, never erase an unseen message.
        Active consumed residue is included so interrupted archival can finish.
        """
        candidates = {record["event"]["event_id"] for record in
                      wake.active_records(self.config.workshop_root)
                      if record["event"]["source"] == SOURCE
                      and record["event"]["kind"] == KIND
                      and record["status"] in ("pending", "consumed")}
        if not candidates:
            return True
        events = [_event_for_burst(burst) for burst in self.store.fully_seen_bursts()
                  if burst["event_id"] in candidates]
        try:
            for offset in range(0, len(events), 64):
                wake.consume_signal_events(self.config.workshop_root, events[offset:offset + 64])
        except wake.Busy:
            return False  # durable seen IDs will be reconciled by the next tick
        return True

    def tick(self, rpc):
        self.spool.replay(self._consume)
        if not self._reconcile_seen():
            return
        if (self.config.workshop_root / "PAUSED").exists():
            return
        self.store.prepare_burst(self.clock())
        for burst in self.store.pending_bursts():
            try:
                wake.enqueue(self.config.workshop_root, _event_for_burst(burst))
            except wake.Refusal as exc:
                if str(exc) in ("Event queue busy; retry after the current writer finishes",
                                "Event queue is full; inspect or archive it explicitly"):
                    break  # durable burst stays pending; receiver and outbox stay live
                raise
            self.store.mark_enqueued(burst["event_id"])
        if self.config.agent_tools is not None:
            # In this mode inbound only wakes ordinary sessions. Sending is an
            # explicit tool action, including when the session answers a burst.
            return
        # Archived proofs are immutable. Active proofs are read only while the
        # inherited runner lock is idle, so stage drafts cannot impersonate final.
        lock_path = self.config.workshop_root / ".workshop.lock"
        idle_fd = None
        try:
            if lock_path.exists() or lock_path.is_symlink():
                idle_fd = os.open(lock_path, os.O_RDONLY | os.O_NOFOLLOW)
                info = os.fstat(idle_fd)
                if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                    raise Refusal("unsafe workshop lock")
                active_allowed = False
                try:
                    fcntl.flock(idle_fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
                    active_allowed = True
                except BlockingIOError:
                    pass
            else:
                active_allowed = False
            for burst in self.store.awaiting_replies():
                proof = _reply_proof(self.config.workshop_root, burst,
                                     active_allowed=active_allowed)
                if proof is not None:
                    self.store.queue_reply(burst["event_id"], proof[0], proof[1])
        finally:
            if idle_fd is not None:
                os.close(idle_fd)
        for item in self.store.pending_outbox():
            if (self.config.workshop_root / "PAUSED").exists():
                return
            rpc.poll(timeout=0)  # deliver/ingest newly arrived owner texts first
            self.spool.replay(self._consume)
            if not rpc.caught_up:
                return  # finite poll left buffered receive data; retry next tick
            if (self.config.workshop_root / "PAUSED").exists():
                return
            if not self.store.begin_send(item["id"]):
                continue
            try:
                result = rpc.send(self.config.owner_id, item["text"])
            except signal_jsonrpc.TransportError:
                self.store.finish_send(item["id"], "unknown")
                raise
            self.store.finish_send(item["id"], result["outcome"],
                                   timestamp_ms=result.get("timestamp_ms"))

    def send_message(self, rpc, arguments):
        """One explicit, durable, fixed-recipient send; never drain on a later tick."""
        request_id = arguments["request_id"]
        try:
            item = self.store.queue_direct(request_id, arguments["text"],
                                           reply_to=arguments.get("reply_to"))
        except signal_store.ConflictError:
            return {"outcome": "failed", "reason": "request_id_conflict"}
        except signal_store.StoreError:
            return {"outcome": "failed", "reason": "direct_send_refused"}
        if item["status"] != "pending":
            return {"outcome": item["status"], "timestamp_ms": item["timestamp_ms"]}
        if (self.config.workshop_root / "PAUSED").exists():
            return {"outcome": "pending", "reason": "workshop_paused"}
        try:
            rpc.poll(timeout=0)
            self.spool.replay(self._consume)
        except signal_jsonrpc.TransportError:
            return {"outcome": "pending", "reason": "receive_transport_unavailable"}
        if not rpc.caught_up:
            return {"outcome": "pending", "reason": "receive_not_caught_up"}
        if (self.config.workshop_root / "PAUSED").exists():
            return {"outcome": "pending", "reason": "workshop_paused"}
        if not self.store.begin_direct_send(request_id):
            item = self.store.get_direct(request_id)
            return {"outcome": item["status"], "timestamp_ms": item["timestamp_ms"]}
        try:
            result = rpc.send(self.config.owner_id, arguments["text"])
            self.store.finish_direct_send(request_id, result["outcome"],
                                          timestamp_ms=result.get("timestamp_ms"))
        except Exception:
            # Once intent is durable, even a missing result may have crossed the
            # Signal boundary. Refuse replay rather than risk a duplicate DM.
            self.store.finish_direct_send(request_id, "unknown")
            return {"outcome": "unknown", "reason": "send_result_unavailable"}
        return {"outcome": result["outcome"], "timestamp_ms": result.get("timestamp_ms")}

    def read_history(self, arguments):
        """Read and mark only incoming messages selected into this bounded result.

        Seen is not answered. A lost tool response may still have committed seen
        state; repeating the read is safe, and history is never removed.
        """
        result = signal_tools._history_result(self.store.read_history(limit=arguments["limit"]))
        message_ids = [item["id"] for item in result["entries"]
                       if item["direction"] == "incoming"]
        self.store.mark_seen(message_ids)
        self._reconcile_seen()
        return result


def _state(config, *, create=False):
    state = config.state_dir
    if create:
        if state.exists() or state.is_symlink():
            raise Refusal("bridge state already exists; inspect rather than reinitialize")
        if not state.parent.is_dir() or state.parent.is_symlink():
            raise Refusal("unsafe bridge state parent")
        state.mkdir(mode=0o700)
        _sync_dir(state.parent)
        (state / "spool").mkdir(mode=0o700)
        _sync_dir(state)
    if state.is_symlink() or not state.is_dir() or state.resolve(strict=True) != state:
        raise Refusal("bridge state unavailable")
    info = state.stat()
    if info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise Refusal("nonprivate bridge state")
    spool = Spool(state / "spool")
    return state, spool


def init(config):
    # Do not leave a fresh ledger beside a known incompatible queue. Creating a
    # new bridge never implicitly migrates an existing workshop queue.
    if wake.upgrade_version(config.workshop_root) not in ("absent", wake.SCHEMA):
        raise Refusal("Upgrade the existing workshop queue before initializing the Signal bridge")
    state, _spool = _state(config, create=True)
    with _lock(state, create=True):
        with signal_store.Store(config.store_path, owner_id=config.owner_id, create=True):
            wake.fence_current(config.workshop_root)
    return {"status": "initialized"}


def upgrade(config):
    """Explicit queue/ledger upgrade while reception and workshop work are idle.

    Both formats are positively classified before either write. These are two
    independent durable publications, not a cross-file transaction: after an
    interruption, rerun this command to finish a supported old/current pair.
    """
    state, _spool = _state(config)
    with _lock(state):
        if not (config.workshop_root / "PAUSED").exists():
            raise Refusal("Pause the workshop and stop the Signal bridge before upgrade")
        with wake._workshop_locked(config.workshop_root):
            signal_store.upgrade_version(config.store_path, owner_id=config.owner_id)
            wake.upgrade_version(config.workshop_root)
            queue = wake.upgrade(config.workshop_root)
            changed = signal_store.upgrade(config.store_path, owner_id=config.owner_id)
    return {"status": "upgraded" if changed or queue["upgraded"] else "already_current",
            "ledger_schema": signal_store.SCHEMA, "queue_schema": wake.SCHEMA}


def status(config):
    state, spool = _state(config)
    try:
        with _lock(state, shared=True):
            with signal_store.Store(config.store_path, owner_id=config.owner_id) as store:
                summary = store.status()
                ready, interrupted = spool.inspect()
                return {"status": "paused" if (config.workshop_root / "PAUSED").exists() else "ready",
                        "store": summary, "spooled_notifications": ready,
                        "interrupted_spool_entries": interrupted}
    except Refusal as exc:
        if str(exc) == "bridge already running":
            return {"status": "running"}
        raise


def run(config, *, rpc_factory=signal_jsonrpc.SignalRpc, clock=time.time):
    state, spool = _state(config)
    with _lock(state):
        with signal_store.Store(config.store_path, owner_id=config.owner_id) as store:
            # Fence before replay/receive or retry-state mutation, even when no
            # events exist yet. An old queue needs an explicit idle upgrade.
            wake.fence_current(config.workshop_root)
            store.recover_uncertain_sends()
            bridge = Bridge(config, store, spool, clock=clock)
            spool.replay(bridge._consume)
            rpc = rpc_factory(config.signal_cli, config.account, config.signal_data_dir,
                              config.signal_rpc_config, bridge.receive)
            stopped = [False]
            handlers = {}
            def stop(_signum, _frame):
                stopped[0] = True
            for number in (signal.SIGTERM, signal.SIGINT):
                handlers[number] = signal.signal(number, stop)
            try:
                with rpc:
                    rpc.start()
                    agent_server = (signal_tools.SignalToolsServer(
                        state / "signal-tools.sock", asset_roots=config.agent_tools["asset_roots"],
                        portrait_path=config.agent_tools["portrait_path"])
                        if config.agent_tools is not None else contextlib.nullcontext(None))
                    with agent_server as agent:
                        while not stopped[0]:
                            rpc.poll(timeout=1)
                            if agent is not None:
                                agent.poll(rpc, paused=(config.workshop_root / "PAUSED").exists(),
                                           send_message=lambda args: bridge.send_message(rpc, args),
                                           read_history=bridge.read_history)
                            bridge.tick(rpc)
            finally:
                for number, handler in handlers.items():
                    signal.signal(number, handler)
            return {"status": "stopped", "accepted_notifications": bridge.accepted,
                    "ignored_notifications": bridge.ignored}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, epilog=(
        "upgrade: pause the workshop, stop the Signal bridge, then upgrade the "
        "known queue and ledger formats. An interrupted upgrade is safe to rerun."))
    parser.add_argument("action", choices=("init", "upgrade", "run", "status"))
    parser.add_argument("--config", required=True, type=Path)
    args = parser.parse_args(argv)
    os.umask(0o077)
    try:
        config = Config(args.config)
        result = {"init": init, "upgrade": upgrade, "run": run, "status": status}[args.action](config)
        print(json.dumps(result, ensure_ascii=False, sort_keys=True))
        return 0
    except (Refusal, signal_store.StoreError, wake.Refusal, heartbeat.Refusal,
            signal_jsonrpc.TransportError, signal_tools.SignalToolsError,
            OSError, ValueError, TypeError) as exc:
        # Neither payloads, account identities nor recipient values are logged.
        print(json.dumps({"status": "refused", "reason": type(exc).__name__}), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

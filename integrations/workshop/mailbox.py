#!/usr/bin/env python3
"""Private, bounded stdio MCP mailbox for one local workshop root.

SSH authenticates the operating-system account. This server does not authenticate
names inside messages, run a listener, start Codex, or grant workshop authority.
"""

import argparse
import contextlib
import fcntl
import json
import math
import os
from pathlib import Path
import re
import stat
import sys

import heartbeat
import wake


VERSIONS = ("2025-11-25", "2025-06-18", "2024-11-05")
SOURCE = "private-mailbox"
MAX_FRAME = 64 * 1024
MAX_RESPONSE = 64 * 1024
MAX_TEXT = 4000
ID_PATTERN = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]{0,95}\Z")
RUN_PATTERN = re.compile(r"[A-Za-z0-9_-]{1,128}\Z")
METHODS = frozenset(("initialize", "notifications/initialized", "ping", "tools/list", "tools/call"))
CATALOG = [
    {"name": "send_message", "description": "Durably queue private task data for Pi's next eligible workshop cycle; acceptance is not an answer or execution.",
     "inputSchema": {"type": "object", "additionalProperties": False,
                     "properties": {"request_id": {"type": "string", "minLength": 1, "maxLength": 96,
                                                   "pattern": "^[A-Za-z0-9][A-Za-z0-9_-]*$"},
                                    "text": {"type": "string", "minLength": 1, "maxLength": MAX_TEXT}},
                     "required": ["request_id", "text"]}},
    {"name": "read_reply", "description": "Read one request's status and only a reply published by a completed valid workshop cycle.",
     "inputSchema": {"type": "object", "additionalProperties": False,
                     "properties": {"request_id": {"type": "string", "minLength": 1, "maxLength": 96,
                                                   "pattern": "^[A-Za-z0-9][A-Za-z0-9_-]*$"}},
                     "required": ["request_id"]}},
    {"name": "workshop_status", "description": "Read bounded workshop counters and last-observed cycle policy; current runner configuration is unknown here. Does not start or alter a cycle.",
     "inputSchema": {"type": "object", "additionalProperties": False, "properties": {}}},
]


class Refusal(ValueError):
    """Bounded caller error or unsafe local state."""


def _pairs(items):
    value = {}
    for key, item in items:
        if key in value:
            raise Refusal("duplicate JSON key")
        value[key] = item
    return value


def _reject_constant(_token):
    raise Refusal("non-finite JSON number")


def _bounded_tree(value):
    stack = [(value, 0)]
    nodes = 0
    while stack:
        item, depth = stack.pop()
        nodes += 1
        if nodes > 128 or depth > 12:
            raise Refusal("JSON request is too complex")
        if isinstance(item, dict):
            stack.extend((child, depth + 1) for child in item.values())
        elif isinstance(item, list):
            stack.extend((child, depth + 1) for child in item)
        elif isinstance(item, float) and not math.isfinite(item):
            raise Refusal("non-finite JSON number")


def _parse(raw):
    try:
        value = json.loads(raw.decode("utf-8"), object_pairs_hook=_pairs,
                           parse_constant=_reject_constant)
        _bounded_tree(value)
    except (UnicodeError, ValueError, RecursionError) as exc:
        raise Refusal("invalid bounded JSON-RPC frame") from exc
    if not isinstance(value, dict) or value.get("jsonrpc") != "2.0":
        raise Refusal("invalid JSON-RPC envelope")
    method = value.get("method")
    if not isinstance(method, str) or not 1 <= len(method) <= 64:
        raise Refusal("invalid JSON-RPC method")
    ident = value.get("id")
    if "id" in value and (type(ident) not in (str, int) or len(str(ident)) > 128):
        raise Refusal("invalid JSON-RPC id")
    if method == "notifications/initialized":
        if "id" in value:
            raise Refusal("initialized notification must not have an id")
    elif "id" not in value:
        raise Refusal("JSON-RPC request needs an id")
    if not isinstance(value.get("params", {}), dict):
        raise Refusal("JSON-RPC params must be an object")
    return value


def _wire(ident, *, result=None, error=None):
    payload = {"jsonrpc": "2.0", "id": ident,
               "error" if error is not None else "result": error if error is not None else result}
    encoded = (json.dumps(payload, ensure_ascii=False, separators=(",", ":"), allow_nan=False) + "\n").encode()
    if len(encoded) > MAX_RESPONSE:
        return _wire(ident, error={"code": -32603, "message": "bounded MCP response exceeded limit"})
    return encoded


def _error(ident, code, message):
    return _wire(ident, error={"code": code, "message": message[:300]})


def _tool(value):
    return {"content": [{"type": "text", "text": json.dumps(value, ensure_ascii=False, separators=(",", ":"))}],
            "structuredContent": value}


def _tool_error(message):
    return {"content": [{"type": "text", "text": message[:300]}], "isError": True}


def _arguments(value, names):
    if not isinstance(value, dict) or set(value) != names:
        raise Refusal("tool arguments must contain exactly " + ", ".join(sorted(names)))
    return value


def _request_id(value):
    if not isinstance(value, str) or ID_PATTERN.fullmatch(value) is None:
        raise Refusal("request_id must be 1..96 ASCII letters, digits, underscore, or hyphen")
    return value


def _metadata(params):
    """Admit MCP request metadata, never tool arguments or authority."""
    if "_meta" not in params:
        return
    meta = params["_meta"]
    if not isinstance(meta, dict):
        raise Refusal("MCP request metadata must be an object")
    if "progressToken" not in meta:
        return
    token = meta["progressToken"]
    if type(token) is int and -(2 ** 63) <= token < 2 ** 63:
        return
    if isinstance(token, str) and 1 <= len(token) <= 128:
        return
    raise Refusal("progressToken must be a bounded string or integer")


def send_message(root, arguments):
    args = _arguments(arguments, {"request_id", "text"})
    request_id = _request_id(args["request_id"])
    message = args["text"]
    if not isinstance(message, str) or not 1 <= len(message) <= MAX_TEXT or not message.strip():
        raise Refusal("text must be nonblank and at most 4000 characters")
    event = {"source": SOURCE, "event_id": request_id, "kind": "message",
             "summary": message[:160], "body": message, "reference": ""}
    record = wake.enqueue(root, event)
    return {"request_id": request_id, "accepted": True, "event_status": record["status"],
            "note": "Durably recorded; a new request is queued, not necessarily seen, answered, or executed. Retry with the same request_id and identical text."}


def _identity(value, request_id):
    return (isinstance(value, dict) and set(value) == {"source", "event_id"}
            and value["source"] == SOURCE and value["event_id"] == request_id)


@contextlib.contextmanager
def _runner_idle(root):
    """The child inherits this lock: no stage draft can masquerade as final."""
    try:
        fd = os.open(root / ".workshop.lock", os.O_RDONLY | os.O_NOFOLLOW)
    except FileNotFoundError:
        yield False
        return
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            raise Refusal("unsafe workshop lock")
        try:
            fcntl.flock(fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
        except BlockingIOError:
            yield False
            return
        yield True
    finally:
        os.close(fd)


def _completed_reply(root, record, request_id):
    run_id = record["claimed_run"]
    if run_id is None:
        return None
    if not isinstance(run_id, str) or RUN_PATTERN.fullmatch(run_id) is None:
        raise Refusal("unsafe run identity in event queue")
    run = root / "runs" / run_id
    if (root / "runs").is_symlink() or run.is_symlink():
        raise Refusal("unsafe run directory")
    if not run.is_dir():
        return None
    receipt_path = run / "receipt.json"
    if not receipt_path.exists() and not receipt_path.is_symlink():
        return None
    try:
        receipt = heartbeat.json_object(heartbeat.bounded_bytes(receipt_path, heartbeat.MAX_RESULT))
    except (OSError, UnicodeError, ValueError, RecursionError, heartbeat.Refusal) as exc:
        raise Refusal("unreadable run receipt; inspect the local workshop before retrying") from exc
    if (not isinstance(receipt, dict) or receipt.get("schema") not in heartbeat.REPLY_RECEIPT_SCHEMAS
            or receipt.get("run_id") != run_id or receipt.get("status") not in heartbeat.TERMINAL | {"running"}):
        raise Refusal("unknown run receipt; inspect the local workshop before retrying")
    try:
        heartbeat.receipt_class(receipt)
    except (heartbeat.Refusal, ValueError, TypeError) as exc:
        raise Refusal("invalid run receipt cycle class") from exc
    if receipt["status"] != "completed":
        return None
    identities = receipt.get("event_ids")
    if (not isinstance(identities, list) or len(identities) > 4
            or not any(_identity(item, request_id) for item in identities)):
        raise Refusal("completed receipt lacks this event identity")
    try:
        result = heartbeat.json_object(heartbeat.bounded_bytes(run / "result.json", heartbeat.MAX_RESULT))
    except (OSError, UnicodeError, ValueError, RecursionError, heartbeat.Refusal) as exc:
        raise Refusal("unreadable completed result; inspect the local workshop before retrying") from exc
    # The runner validates and atomically publishes this final result before the
    # adjacent completed receipt. Stage results are never consulted here.
    replies = result.get("replies") if isinstance(result, dict) else None
    if not isinstance(replies, list) or len(replies) > 4:
        raise Refusal("invalid completed reply set")
    found = []
    for reply in replies:
        if (not isinstance(reply, dict) or set(reply) != {"source", "event_id", "text"}
                or not isinstance(reply["source"], str) or not isinstance(reply["event_id"], str)
                or not isinstance(reply["text"], str) or not 1 <= len(reply["text"]) <= MAX_TEXT):
            raise Refusal("invalid completed reply")
        if reply["source"] == SOURCE and reply["event_id"] == request_id:
            found.append(reply["text"])
    if len(found) > 1:
        raise Refusal("duplicate completed reply")
    return found[0] if found else ""


def read_reply(root, arguments):
    request_id = _request_id(_arguments(arguments, {"request_id"})["request_id"])
    with _runner_idle(root) as idle:
        found = wake.lookup(root, SOURCE, request_id)
        if found is None:
            return {"request_id": request_id, "status": "unknown", "reply": None}
        record = found["record"]
        if found["location"] == "archive":
            terminal = found["terminal"]
            if (not isinstance(terminal, dict) or set(terminal) != {"status", "reply"}
                    or terminal["status"] not in ("replied", "completed_without_reply")
                    or (terminal["status"] == "replied" and
                        (not isinstance(terminal["reply"], str) or
                         not 1 <= len(terminal["reply"]) <= MAX_TEXT))
                    or (terminal["status"] == "completed_without_reply" and terminal["reply"] is not None)):
                raise Refusal("invalid archived reply outcome")
            return {"request_id": request_id, "status": terminal["status"],
                    "reply": terminal["reply"]}
        if found["location"] != "active" or found["terminal"] is not None:
            raise Refusal("unknown mailbox lookup state")
        reply = _completed_reply(root, record, request_id) if idle else None
    if reply is None:
        return {"request_id": request_id, "status": record["status"], "reply": None}
    return {"request_id": request_id, "status": "replied" if reply else "completed_without_reply",
            "reply": reply or None}


def workshop_status(root, arguments):
    _arguments(arguments, set())
    entries, _used = heartbeat.ledger(root)
    state = heartbeat.read_state(root)
    counts = heartbeat.quota_counts(entries, heartbeat.utc_now())
    observed = None
    if entries:
        run, receipt = max(entries, key=lambda entry: (heartbeat.parse_utc(entry[1]["started_at"]), entry[0].name))
        # These values describe that admission, not the service's current launch
        # flags. Missing legacy fields remain absent rather than inventing a
        # default cap or claiming a missing cap means unlimited.
        policy_fields = ("background_schedule_mode", "interactive_budget_mode",
                         "max_starts_utc_day", "max_starts_rolling_hour")
        observed = {"source": "latest_retained_run_receipt", "run_id": run.name,
                    "started_at": receipt["started_at"], "status": receipt["status"],
                    "cycle_class": heartbeat.receipt_class(receipt),
                    "policy": {key: receipt[key] for key in policy_fields if key in receipt}}
    return {"paused": (root / "PAUSED").exists(),
            "starts_today": counts["background_today"],
            "interactive_starts_today": counts["interactive_today"],
            "interactive_starts_last_hour": counts["interactive_last_hour"],
            "runner_configuration_known": False,
            "max_starts_utc_day": None,
            "last_observed_cycle": observed,
            "running_receipts": sum(receipt["status"] == "running" for _, receipt in entries),
            "next_due_at": None,
            "stored_relative_next_due_at": state["next_due_at"],
            "note": "Current runner flags are unknown to the mailbox: configured cap and next due time are null. Last-observed policy is historical; stored relative due time may not control hourly scheduling. eligible_events uses only the legacy background watermark.",
            "eligible_events": wake.pending(root, state["event_after_sequence"], cycle_class="background"),
            "queued_mailbox_messages": wake.inbox_summary(root)["pending_mailbox_messages"]}


class Server:
    def __init__(self, root):
        self.root = wake._root(root)
        self.initialized = False

    def handle(self, message):
        method, ident, params = message["method"], message.get("id"), message.get("params", {})
        if method not in METHODS:
            return _error(ident, -32601, "method not found")
        if method == "initialize":
            if set(params) != {"protocolVersion", "capabilities", "clientInfo"} or not isinstance(params["capabilities"], dict) or not isinstance(params["clientInfo"], dict):
                return _error(ident, -32602, "invalid initialize parameters")
            version = params["protocolVersion"]
            if version not in VERSIONS:
                return _error(ident, -32602, "unsupported MCP protocol version")
            self.initialized = True
            return _wire(ident, result={"protocolVersion": version, "capabilities": {"tools": {}},
                                        "serverInfo": {"name": "mneme-private-workshop-mailbox", "version": "1"}})
        if not self.initialized:
            return None if method == "notifications/initialized" else _error(ident, -32000, "initialize first")
        if method == "notifications/initialized":
            return None
        if method == "ping":
            return _wire(ident, result={})
        if method == "tools/list":
            if set(params) - {"_meta"}:
                return _error(ident, -32602, "tools/list has one complete page; no cursor or other parameters")
            try:
                _metadata(params)
            except Refusal as exc:
                return _error(ident, -32602, str(exc))
            return _wire(ident, result={"tools": CATALOG})
        if set(params) not in ({"name", "arguments"}, {"name", "arguments", "_meta"}) or not isinstance(params["name"], str):
            return _error(ident, -32602, "tools/call requires name and arguments")
        try:
            _metadata(params)
        except Refusal as exc:
            return _error(ident, -32602, str(exc))
        functions = {"send_message": send_message, "read_reply": read_reply, "workshop_status": workshop_status}
        action = functions.get(params["name"])
        if action is None:
            return _error(ident, -32602, "unknown mailbox tool")
        try:
            value = action(self.root, params["arguments"])
            return _wire(ident, result=_tool(value))
        except (Refusal, wake.Refusal, heartbeat.Refusal, OSError, ValueError, UnicodeError, RecursionError) as exc:
            return _wire(ident, result=_tool_error(str(exc)))


def run_stream(root, source, sink):
    server = Server(root)
    while True:
        raw = source.readline(MAX_FRAME + 2)
        if not raw:
            break
        if len(raw) > MAX_FRAME or not raw.endswith(b"\n"):
            while raw and not raw.endswith(b"\n"):
                raw = source.readline(4096)
            sink.write(_error(None, -32600, "MCP stdio frame exceeds 64 KiB"))
            sink.flush()
            continue
        try:
            message = _parse(raw)
        except Refusal as exc:
            sink.write(_error(None, -32700, str(exc)))
            sink.flush()
            continue
        response = server.handle(message)
        if response is not None:
            sink.write(response)
            sink.flush()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path, help="existing absolute canonical workshop directory")
    args = parser.parse_args(argv)
    run_stream(args.root, sys.stdin.buffer, sys.stdout.buffer)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

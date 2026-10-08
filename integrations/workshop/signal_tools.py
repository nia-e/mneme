#!/usr/bin/env python3
"""Narrow local Signal tools. The bridge alone owns signal-cli and delivery."""

import argparse
import json
import os
from pathlib import Path
import re
import select
import socket
import stat
import sys
import tempfile
import time
import uuid


SCHEMA = "mneme.signal.tools-request.v1"
PROFILE_TOOL = "set_own_signal_profile"
SEND_TOOL = "send_message"
HISTORY_TOOL = "read_history"
VERSIONS = ("2025-11-25", "2025-06-18", "2024-11-05")
MAX_FRAME = 32 * 1024
MAX_HISTORY_BYTES = 4096
MAX_IMAGE = 8 * 1024 * 1024
REQUEST_ID = re.compile(r"[a-f0-9]{32}\Z")
DURABLE_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.:-]{0,255}\Z")
PNG = b"\x89PNG\r\n\x1a\n"


class SignalToolsError(ValueError):
    """Unsafe listener state; client refusals are reported, not raised."""


def _pairs(items):
    value = {}
    for key, item in items:
        if key in value:
            raise ValueError("duplicate_key")
        value[key] = item
    return value


def _decode(raw):
    if len(raw) > MAX_FRAME or not raw.endswith(b"\n"):
        raise ValueError("invalid_frame")
    try:
        return json.loads(raw.decode("utf-8"), object_pairs_hook=_pairs,
                          parse_constant=lambda _: (_ for _ in ()).throw(ValueError("invalid_number")))
    except (UnicodeError, RecursionError, ValueError) as exc:
        raise ValueError("invalid_json") from exc


def _encode(value):
    raw = (json.dumps(value, ensure_ascii=False, allow_nan=False, separators=(",", ":")) + "\n").encode()
    if len(raw) > MAX_FRAME:
        raise SignalToolsError("response_too_large")
    return raw


def _profile_arguments(value):
    if not isinstance(value, dict) or not value or set(value) - {"given_name", "avatar_path", "restore_portrait"}:
        raise ValueError("invalid_arguments")
    name = value.get("given_name")
    if "given_name" in value and (not isinstance(name, str) or not 1 <= len(name) <= 128 or
                                  not name.strip() or "\x00" in name):
        raise ValueError("invalid_given_name")
    path = value.get("avatar_path")
    if "avatar_path" in value and (not isinstance(path, str) or not path or len(path) > 1024):
        raise ValueError("invalid_avatar_path")
    restore = value.get("restore_portrait", False)
    if "restore_portrait" in value and restore is not True:
        raise ValueError("invalid_restore_portrait")
    if path is not None and restore:
        raise ValueError("conflicting_avatar_selection")
    return name, path, restore


def _send_arguments(value):
    if not isinstance(value, dict) or set(value) not in ({"text", "request_id"},
                                                          {"text", "request_id", "reply_to"}):
        raise ValueError("invalid_send_arguments")
    text = value["text"]
    if not isinstance(text, str) or not 1 <= len(text) <= 4000:
        raise ValueError("invalid_send_text")
    for key in ("request_id", "reply_to"):
        if key in value and (not isinstance(value[key], str) or DURABLE_ID.fullmatch(value[key]) is None):
            raise ValueError("invalid_" + key)
    return value


def _history_arguments(value):
    if not isinstance(value, dict) or set(value) - {"limit"}:
        raise ValueError("invalid_history_arguments")
    limit = value.get("limit", 12)
    if type(limit) is not int or not 1 <= limit <= 20:
        raise ValueError("invalid_history_limit")
    return {"limit": limit}


def _history_result(value):
    if (not isinstance(value, dict) or set(value) != {"entries", "uncertain", "truncated"} or
            not isinstance(value["entries"], list) or not isinstance(value["uncertain"], list) or
            type(value["truncated"]) is not bool):
        raise SignalToolsError("invalid_history_result")
    if len(json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")) > MAX_HISTORY_BYTES:
        raise SignalToolsError("history_result_too_large")
    return value


def _path(value, roots):
    path = Path(value)
    if not path.is_absolute() or ".." in path.parts:
        raise ValueError("invalid_avatar_path")
    try:
        if path.is_symlink() or path.resolve(strict=True) != path or not any(path.is_relative_to(root) for root in roots):
            raise ValueError("invalid_avatar_path")
        info = path.stat()
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or not 1 <= info.st_size <= MAX_IMAGE:
            raise ValueError("invalid_avatar_file")
    except OSError as exc:
        raise ValueError("avatar_unavailable") from exc
    return path


def validate_image_path(value, roots):
    """Admit one configured source image without opening any Signal account state."""
    path = _path(value, roots)
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        try:
            header = os.read(fd, 16)
            if not (header.startswith(PNG) or header.startswith(b"\xff\xd8\xff")):
                raise ValueError("invalid_avatar_format")
        finally:
            os.close(fd)
    except OSError as exc:
        raise ValueError("avatar_unavailable") from exc
    return path


def _stage(path, directory):
    """Copy admitted image to a private stable file before upstream opens it."""
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    staged = None
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or not 1 <= info.st_size <= MAX_IMAGE:
            raise ValueError("invalid_avatar_file")
        first = os.read(fd, 16)
        if first.startswith(PNG):
            suffix = ".png"
        elif first.startswith(b"\xff\xd8\xff"):
            suffix = ".jpg"
        else:
            raise ValueError("invalid_avatar_format")
        with tempfile.NamedTemporaryFile(prefix=".profile-", suffix=suffix, dir=directory,
                                         delete=False) as output:
            staged = Path(output.name)
            total = len(first)
            output.write(first)
            while True:
                chunk = os.read(fd, 65536)
                if not chunk:
                    break
                total += len(chunk)
                if total > MAX_IMAGE:
                    raise ValueError("invalid_avatar_file")
                output.write(chunk)
            output.flush()
            os.fsync(output.fileno())
        return staged
    except BaseException:
        if staged is not None:
            staged.unlink(missing_ok=True)
        raise
    finally:
        os.close(fd)


class SignalToolsServer:
    """Bridge-owned one-client-at-a-time local socket; caller holds bridge lease."""

    def __init__(self, socket_path, *, asset_roots, portrait_path):
        self.path = Path(socket_path)
        self.roots = tuple(Path(root) for root in asset_roots)
        self.portrait = Path(portrait_path)
        self.listener = None
        self._owned_socket = False
        if not self.path.is_absolute() or not self.roots or any(not root.is_absolute() for root in self.roots):
            raise SignalToolsError("invalid_tools_configuration")

    def __enter__(self):
        parent = self.path.parent
        try:
            info = parent.stat()
            if (parent.is_symlink() or parent.resolve(strict=True) != parent or
                    not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077):
                raise SignalToolsError("unsafe_tools_socket_parent")
            if any(root.resolve(strict=True) != root for root in self.roots):
                raise SignalToolsError("invalid_asset_root")
            if any(not root.is_dir() for root in self.roots):
                raise SignalToolsError("invalid_asset_root")
            if self.path.exists() or self.path.is_symlink():
                existing = self.path.lstat()
                if not stat.S_ISSOCK(existing.st_mode):
                    raise SignalToolsError("tools_socket_path_occupied")
                # Only caller holding the bridge lease may retire its stale socket.
                probe = socket.socket(socket.AF_UNIX)
                try:
                    probe.settimeout(0.1)
                    probe.connect(str(self.path))
                except (ConnectionRefusedError, FileNotFoundError):
                    self.path.unlink()
                else:
                    raise SignalToolsError("tools_socket_already_live")
                finally:
                    probe.close()
            self.listener = socket.socket(socket.AF_UNIX)
            self.listener.bind(str(self.path))
            self._owned_socket = True
            os.chmod(self.path, 0o600)
            self.listener.listen(1)
            self.listener.setblocking(False)
            return self
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def __exit__(self, *_):
        if self.listener is not None:
            self.listener.close()
            self.listener = None
            if self._owned_socket and self.path.is_socket():
                self.path.unlink()
            self._owned_socket = False

    def poll(self, rpc, *, paused=False, send_message=None, read_history=None):
        if self.listener is None:
            raise SignalToolsError("tools_listener_closed")
        ready, _, _ = select.select([self.listener], [], [], 0.25)
        if not ready:
            return False
        client, _ = self.listener.accept()
        with client:
            deadline = time.monotonic() + 2
            request_id = None
            tool = None
            staged = None
            invoked = False
            try:
                raw = bytearray()
                while not raw.endswith(b"\n") and len(raw) <= MAX_FRAME:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise socket.timeout()
                    client.settimeout(remaining)
                    chunk = client.recv(min(4096, MAX_FRAME + 1 - len(raw)))
                    if not chunk:
                        break
                    raw.extend(chunk)
                request = _decode(bytes(raw))
                if (not isinstance(request, dict) or set(request) != {"schema", "request_id", "tool", "arguments"}
                        or request["schema"] != SCHEMA or not isinstance(request["request_id"], str)
                        or REQUEST_ID.fullmatch(request["request_id"]) is None):
                    raise ValueError("invalid_tools_request")
                request_id = request["request_id"]
                tool = request["tool"]
                if tool == PROFILE_TOOL:
                    name, image, restore = _profile_arguments(request["arguments"])
                elif tool == SEND_TOOL:
                    arguments = _send_arguments(request["arguments"])
                elif tool == HISTORY_TOOL:
                    arguments = _history_arguments(request["arguments"])
                else:
                    raise ValueError("unknown_tool")
                if paused and tool == PROFILE_TOOL:
                    response = {"request_id": request_id, "outcome": "failed", "reason": "workshop_paused"}
                elif tool == SEND_TOOL:
                    if send_message is None:
                        response = {"request_id": request_id, "outcome": "failed", "reason": "send_unavailable"}
                    else:
                        invoked = True
                        result = send_message(arguments)
                        if (not isinstance(result, dict) or result.get("outcome") not in
                                {"accepted", "failed", "unknown", "superseded", "pending"}):
                            raise SignalToolsError("invalid_send_result")
                        response = {**result, "request_id": request_id}
                elif tool == HISTORY_TOOL:
                    if read_history is None:
                        response = {"request_id": request_id, "outcome": "failed", "reason": "history_unavailable"}
                    else:
                        invoked = True
                        response = {**_history_result(read_history(arguments)), "request_id": request_id}
                else:
                    if restore:
                        image = str(self.portrait)
                    if image is not None:
                        staged = _stage(validate_image_path(image, self.roots), self.path.parent)
                    invoked = True
                    response = rpc.update_profile(given_name=name, avatar=staged)
                    if (not isinstance(response, dict) or response.get("outcome") not in
                            {"accepted", "failed", "unknown"}):
                        raise SignalToolsError("invalid_profile_actor_result")
                    response = {**response, "request_id": request_id}
            except ValueError as exc:
                unavailable = "profile_result_unavailable" if tool == PROFILE_TOOL else "result_unavailable"
                response = ({"request_id": request_id,
                             "outcome": "unknown",
                             "reason": unavailable}
                            if invoked else
                            {"request_id": request_id, "outcome": "failed", "reason": str(exc)[:80]})
            except (OSError, socket.timeout):
                unavailable = "profile_result_unavailable" if tool == PROFILE_TOOL else "result_unavailable"
                input_unavailable = "profile_input_unavailable" if tool == PROFILE_TOOL else "input_unavailable"
                response = {"request_id": request_id,
                            "outcome": "unknown" if invoked else "failed",
                            "reason": unavailable if invoked else input_unavailable}
            except Exception:
                if not invoked:
                    raise
                response = {"request_id": request_id,
                            "outcome": "unknown",
                            "reason": "profile_result_unavailable" if tool == PROFILE_TOOL else "result_unavailable"}
            finally:
                if staged is not None:
                    staged.unlink(missing_ok=True)
            try:
                client.sendall(_encode(response))
            except (BrokenPipeError, ConnectionResetError, socket.timeout):
                pass  # A lost result may follow a durable mutation, including mark-seen.
        return True


CATALOG = [{"name": PROFILE_TOOL,
            "description": "Set this configured Signal account's own given name or avatar. A lost result is unknown; do not automatically retry.",
            "inputSchema": {"type": "object", "additionalProperties": False, "minProperties": 1,
                            "properties": {"given_name": {"type": "string", "minLength": 1, "maxLength": 128},
                                           "avatar_path": {"type": "string", "minLength": 1, "maxLength": 1024},
                                           "restore_portrait": {"type": "boolean", "const": True}}}},
           {"name": SEND_TOOL,
            "description": "Send text to the bridge's fixed Signal recipient. Call read_history before composing unless you already fetched a fresh view in this session; check prior outgoing messages to avoid repetition and respond to the actual latest message. Retry an unknown send result only with the same request_id.",
            "inputSchema": {"type": "object", "additionalProperties": False,
                            "required": ["text", "request_id"],
                            "properties": {"text": {"type": "string", "minLength": 1, "maxLength": 4000},
                                           "request_id": {"type": "string", "pattern": "^[A-Za-z0-9][A-Za-z0-9_.:-]{0,255}$"},
                                           "reply_to": {"type": "string", "pattern": "^[A-Za-z0-9][A-Za-z0-9_.:-]{0,255}$"}}}},
           {"name": HISTORY_TOOL,
            "description": "Read bounded recent Signal history, including outgoing messages and uncertain sends. Marks only returned incoming messages seen and consumes fully seen pending wake-ups; it does not mark them answered or delete history. A lost result may still mark messages seen; reading again is safe.",
            "annotations": {"readOnlyHint": False},
            "inputSchema": {"type": "object", "additionalProperties": False,
                            "properties": {"limit": {"type": "integer", "minimum": 1, "maximum": 20}}}}]


def _call_socket(path, arguments, *, tool=PROFILE_TOOL):
    if tool == PROFILE_TOOL:
        _profile_arguments(arguments)
    elif tool == SEND_TOOL:
        _send_arguments(arguments)
    elif tool == HISTORY_TOOL:
        arguments = _history_arguments(arguments)
    else:
        raise ValueError("unknown_tool")
    request_id = uuid.uuid4().hex
    raw = _encode({"schema": SCHEMA, "request_id": request_id, "tool": tool, "arguments": arguments})
    written = False
    try:
        with socket.socket(socket.AF_UNIX) as client:
            deadline = time.monotonic() + 75
            client.settimeout(5)
            client.connect(str(path))
            written = True  # sendall may write a prefix before reporting failure.
            client.settimeout(max(0.001, deadline - time.monotonic()))
            client.sendall(raw)
            reply = bytearray()
            while not reply.endswith(b"\n") and len(reply) <= MAX_FRAME:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise socket.timeout()
                client.settimeout(remaining)
                chunk = client.recv(min(4096, MAX_FRAME + 1 - len(reply)))
                if not chunk:
                    break
                reply.extend(chunk)
        result = _decode(bytes(reply))
        allowed = ({"accepted", "failed", "unknown"} if tool == PROFILE_TOOL else
                   {"accepted", "failed", "unknown", "superseded", "pending"})
        if not isinstance(result, dict) or result.get("request_id") != request_id:
            raise ValueError("invalid_tools_response")
        if tool == HISTORY_TOOL:
            if result.get("outcome") not in {"failed", "unknown"}:
                _history_result({key: value for key, value in result.items() if key != "request_id"})
        elif result.get("outcome") not in allowed:
            raise ValueError("invalid_tools_response")
        return result
    except (OSError, ValueError) as exc:
        return {"request_id": request_id,
                "outcome": "unknown" if written else "failed",
                "reason": "result_unavailable" if written else "tools_bridge_unavailable"}


def _wire(ident, *, result=None, error=None):
    return _encode({"jsonrpc": "2.0", "id": ident,
                    "error" if error is not None else "result": error if error is not None else result})


class MCPServer:
    def __init__(self, socket_path):
        self.path = Path(socket_path)
        parent = self.path.parent
        try:
            info = parent.stat()
            if (not self.path.is_absolute() or ".." in self.path.parts or
                    parent.is_symlink() or parent.resolve(strict=True) != parent or
                    not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077):
                raise SignalToolsError("invalid_tools_socket_path")
        except OSError as exc:
            raise SignalToolsError("invalid_tools_socket_path") from exc
        self.initialized = False

    def handle(self, frame):
        ident = frame.get("id") if isinstance(frame, dict) else None
        if (not isinstance(frame, dict) or frame.get("jsonrpc") != "2.0" or
                not isinstance(frame.get("method"), str) or len(frame["method"]) > 64 or
                ("id" in frame and (type(ident) not in (str, int) or len(str(ident)) > 128))):
            return _wire(None, error={"code": -32600, "message": "invalid request"})
        method = frame["method"]
        if method == "notifications/initialized" and "id" in frame:
            return _wire(None, error={"code": -32600, "message": "invalid notification"})
        if method != "notifications/initialized" and "id" not in frame:
            return _wire(None, error={"code": -32600, "message": "request id required"})
        params = frame.get("params", {})
        if not isinstance(params, dict):
            return _wire(ident, error={"code": -32602, "message": "invalid parameters"})
        if method == "initialize":
            if (set(params) != {"protocolVersion", "capabilities", "clientInfo"} or
                    params["protocolVersion"] not in VERSIONS or not isinstance(params["capabilities"], dict) or
                    not isinstance(params["clientInfo"], dict)):
                return _wire(ident, error={"code": -32602, "message": "unsupported initialize"})
            self.initialized = True
            return _wire(ident, result={"protocolVersion": params["protocolVersion"], "capabilities": {"tools": {}},
                                        "serverInfo": {"name": "mneme-signal-tools", "version": "1"}})
        if not self.initialized:
            return _wire(ident, error={"code": -32000, "message": "initialize first"})
        if method == "notifications/initialized":
            return None
        if method == "ping":
            return _wire(ident, result={})
        if method == "tools/list":
            if set(params) - {"_meta"}:
                return _wire(ident, error={"code": -32602, "message": "invalid catalog parameters"})
            return _wire(ident, result={"tools": CATALOG})
        if method != "tools/call":
            return _wire(ident, error={"code": -32601, "message": "method not found"})
        if (set(params) not in ({"name", "arguments"}, {"name", "arguments", "_meta"}) or
                params["name"] not in (PROFILE_TOOL, SEND_TOOL, HISTORY_TOOL)):
            return _wire(ident, error={"code": -32602, "message": "unknown tool"})
        try:
            value = _call_socket(self.path, params["arguments"], tool=params["name"])
            return _wire(ident, result={"content": [{"type": "text", "text": json.dumps(value)}],
                                        "structuredContent": value})
        except (ValueError, TypeError):
            return _wire(ident, result={"content": [{"type": "text", "text": "invalid tool arguments"}],
                                        "isError": True})


def run_stream(socket_path, source, sink):
    server = MCPServer(socket_path)
    while True:
        raw = source.readline(MAX_FRAME + 2)
        if not raw:
            return
        if len(raw) > MAX_FRAME or not raw.endswith(b"\n"):
            while raw and not raw.endswith(b"\n"):
                raw = source.readline(4096)
            sink.write(_wire(None, error={"code": -32700, "message": "invalid bounded JSON-RPC frame"}))
            sink.flush()
            continue
        try:
            frame = _decode(raw)
        except ValueError:
            sink.write(_wire(None, error={"code": -32700, "message": "invalid bounded JSON-RPC frame"}))
            sink.flush()
            continue
        response = server.handle(frame)
        if response is not None:
            sink.write(response)
            sink.flush()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True, type=Path)
    args = parser.parse_args(argv)
    run_stream(args.socket, sys.stdin.buffer, sys.stdout.buffer)


if __name__ == "__main__":
    main()

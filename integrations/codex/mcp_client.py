"""Thin, bounded NDJSON adapter to the native ``mnemed client`` process.

The child owns MCP transport, session, tool parsing, and verified writes. This
module only owns its lifetime and the local process framing. A failed request is
never replayed.
"""

import json
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import time
from urllib.parse import urlsplit

PROTOCOL_VERSION = "2025-11-25"
MAX_REQUEST_BYTES = 128 * 1024
MAX_RESPONSE_BYTES = 512 * 1024


class McpError(Exception):
    """Base error for this local integration."""


class McpTransportError(McpError):
    """Native process or MCP host was unavailable or timed out."""


class McpProtocolError(McpError):
    """Native process returned a malformed response."""


class McpToolError(McpError):
    """Mneme reported a tool-level refusal."""


def _binary():
    override = os.environ.get("MNEME_CLIENT_BINARY")
    if override:
        return override
    sibling = Path(__file__).resolve().parent.parent / "bin" / "mnemed"
    if sibling.is_file():
        return str(sibling)
    found = shutil.which("mnemed")
    if found:
        return found
    raise McpTransportError("mnemed client binary unavailable")


class McpClient:
    def __init__(self, url, *, token=None, token_env=None, timeout=3.0,
                 expected_server_name="mneme-mcp"):
        parsed = urlsplit(url)
        if (parsed.scheme != "http" or parsed.hostname not in ("127.0.0.1", "::1")
                or parsed.username or parsed.password or parsed.query or parsed.fragment
                or not parsed.port):
            raise ValueError("Mneme integration requires a numeric loopback HTTP URL with port")
        if timeout <= 0 or timeout > 30:
            raise ValueError("timeout must be in (0, 30]")
        if token is not None and (not token or "\r" in token or "\n" in token):
            raise ValueError("invalid bearer token")
        if token_env and (not token_env.isidentifier() or token is not None):
            raise ValueError("invalid or conflicting token env")
        if expected_server_name not in ("mneme-mcp", "mneme-mcp-library"):
            raise ValueError("unexpected Mneme MCP server identity")
        self.url = url
        self.timeout = timeout
        self.token = token
        self.token_env = token_env
        self.expected_server_name = expected_server_name
        self._process = None
        self._next_id = 1
        self._buffer = b""
        self._connected = False
        self.connect_result = None
        self._native_capabilities = {}

    def __enter__(self):
        self.connect()
        return self

    def __exit__(self, *_):
        self.close()

    def _start(self):
        if self._process is not None:
            return
        env = os.environ.copy()
        token_env = self.token_env
        if self.token is not None:
            token_env = "MNEME_CLIENT_EPHEMERAL_TOKEN"
            env[token_env] = self.token
        argv = [_binary(), "client", "--endpoint", self.url,
                "--timeout-ms", str(max(1, round(self.timeout * 1000))),
                "--expected-server", self.expected_server_name, "--local-only"]
        if token_env:
            argv += ["--token-env", token_env]
        try:
            self._process = subprocess.Popen(argv, stdin=subprocess.PIPE,
                                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                             env=env, bufsize=0)
        except OSError as error:
            raise McpTransportError("mnemed client process unavailable") from error

    def _readline(self, deadline):
        process = self._process
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            while True:
                pos = self._buffer.find(b"\n")
                if pos >= 0:
                    line, self._buffer = self._buffer[:pos], self._buffer[pos + 1:]
                    return line
                if len(self._buffer) > MAX_RESPONSE_BYTES:
                    raise McpProtocolError("native client response exceeds 512 KiB")
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not selector.select(remaining):
                    raise McpTransportError("native client response timed out; request was not retried")
                chunk = os.read(process.stdout.fileno(), 8192)
                if not chunk:
                    raise McpTransportError("native client exited; request outcome may be ambiguous; not retried")
                self._buffer += chunk

    def _write(self, data, deadline):
        fd = self._process.stdin.fileno()
        os.set_blocking(fd, False)
        remaining_data = memoryview(data)
        with selectors.DefaultSelector() as selector:
            selector.register(fd, selectors.EVENT_WRITE)
            while remaining_data:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not selector.select(remaining):
                    raise McpTransportError("native client write timed out; request was not retried")
                try:
                    count = os.write(fd, remaining_data)
                except BlockingIOError:
                    continue
                if count == 0:
                    raise McpTransportError("native client pipe closed; request was not retried")
                remaining_data = remaining_data[count:]

    def _request(self, op, **fields):
        ident = self._next_id
        self._next_id += 1
        data = json.dumps({"id": ident, "op": op, **fields}, separators=(",", ":"),
                          ensure_ascii=False, allow_nan=False).encode("utf-8") + b"\n"
        if len(data) > MAX_REQUEST_BYTES:
            raise McpProtocolError("native client request exceeds 128 KiB")
        self._start()
        deadline = time.monotonic() + self.timeout
        try:
            self._write(data, deadline)
            response = self._readline(deadline)
            if len(response) > MAX_RESPONSE_BYTES:
                raise McpProtocolError("native client response exceeds 512 KiB")
            value = json.loads(response)
            if not isinstance(value, dict) or value.get("id") != ident or type(value.get("ok")) is not bool:
                raise McpProtocolError("native client response identity or shape invalid")
            if value["ok"]:
                if "result" not in value:
                    raise McpProtocolError("native client result missing")
                if op == "connect":
                    # Only local envelope metadata proves bridge support. An
                    # old bridge may forward arbitrary remote initialize data.
                    native = value.get("_mneme_client")
                    self._native_capabilities = native if isinstance(native, dict) else {}
                return value["result"]
            error = value.get("error")
            if not isinstance(error, dict) or not isinstance(error.get("message"), str):
                raise McpProtocolError("native client error malformed")
            kind = error.get("kind")
            cls = McpToolError if kind == "tool" else McpTransportError if kind == "transport" else McpProtocolError
            exception = cls(error["message"][:512])
            exception.details = error
            raise exception
        except (OSError, McpTransportError, McpProtocolError):
            self._stop()
            raise
        except (UnicodeError, ValueError) as error:
            self._stop()
            raise McpProtocolError("native client response is not JSON") from error

    def connect(self):
        if not self._connected:
            self.connect_result = self._request("connect")
            self._connected = True
        return self

    def call_tool(self, name, arguments):
        self.connect()
        return self._request("tools/call", name=name, arguments=arguments)

    def list_tools(self):
        self.connect()
        value = self._request("tools/list")
        if not isinstance(value, dict) or not isinstance(value.get("tools"), list):
            raise McpProtocolError("native tool catalog malformed")
        return value["tools"]

    def raw_rpc(self, method, params=None):
        self.connect()
        return self._request("rpc", method=method, params=params or {})

    def prepare_capture(self, payload):
        return self._request("capture/prepare", payload=payload)

    def prepare_save(self, payload):
        """Freeze native identity before connecting or submitting a mutation."""
        return self._request("save/prepare", payload=payload)

    def _verified(self, operation, db, payload, expected_db_id):
        self.connect()
        fields = {"db": db, "payload": payload}
        if expected_db_id is not None:
            version = self._native_capabilities.get("expected_db_id")
            if type(version) is not int or version != 1:
                raise McpProtocolError(
                    "native client does not support expected_db_id; guarded write was not sent")
            fields["expected_db_id"] = expected_db_id
        return self._request(operation, **fields)

    def capture_verified(self, db, payload, *, expected_db_id=None):
        """Write/read back once; an optional native ID guard never falls back."""
        return self._verified("capture/verified", db, payload, expected_db_id)

    def save_verified(self, db, payload, *, expected_db_id=None):
        """Canonical note/episode save; no legacy fallback or automatic retry."""
        self.connect()
        version = self._native_capabilities.get("save")
        if type(version) is not int or version != 1:
            raise McpProtocolError("native bridge lacks SAVE support; write was not sent")
        return self._verified("save/verified", db, payload, expected_db_id)

    def concern_checked(self, db, payload, *, expected_db_id=None):
        """One native atomic result; no rehash, readback, retry or rebind."""
        self.connect()
        version = self._native_capabilities.get("concern")
        if type(version) is not int or version != 1:
            raise McpProtocolError("native bridge lacks checked concern support; action was not sent")
        return self._verified("concern/checked", db, payload, expected_db_id)

    def prepare_episode(self, payload):
        return self._request("episode/prepare", payload=payload)

    def episode_verified(self, db, payload, *, expected_db_id=None):
        """Write/read the exact edition, optionally bound to one native DB ID."""
        return self._verified("episode/verified", db, payload, expected_db_id)

    def _stop(self):
        process, self._process = self._process, None
        self._connected = False
        self._native_capabilities = {}
        self._buffer = b""
        if process is None:
            return
        for stream in (process.stdin, process.stdout):
            try:
                stream.close()
            except OSError:
                pass
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()

    def close(self):
        if self._process is not None:
            try:
                self._request("close")
            except McpError:
                pass
            finally:
                self._stop()

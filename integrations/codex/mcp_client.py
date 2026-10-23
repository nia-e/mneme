"""Thin, bounded NDJSON adapter to the native ``mnemed client`` process.

The child owns MCP transport, session, tool parsing, and verified writes. This
module only owns its lifetime and the local process framing. A failed request is
never replayed.
"""

import json
import ipaddress
import math
import os
from pathlib import Path
import subprocess
import time
from urllib.parse import urlsplit

import host_process
from host_platform import executable_name, find_executable

PROTOCOL_VERSION = "2025-11-25"
MAX_REQUEST_BYTES = 128 * 1024
MAX_RESPONSE_BYTES = 512 * 1024


def validate_remote_endpoint(url, *, ssh_mcp_port=18766, bearer=False):
    """Validate connection syntax only; native code owns transport and SSH policy."""
    if (not isinstance(url, str) or not url or len(url.encode("utf-8")) > 4096
            or any(char.isspace() or ord(char) < 32 for char in url) or "\\" in url):
        raise ValueError("remote owner requires a bounded HTTP, HTTPS, or SSH URL")
    if type(ssh_mcp_port) is not int or not 1 <= ssh_mcp_port <= 65535:
        raise ValueError("ssh_mcp_port must be in 1..65535")
    parsed = urlsplit(url)
    # Accessing port also rejects invalid, negative and overflowing ports.
    if (parsed.scheme not in ("http", "https", "ssh") or not parsed.hostname
            or parsed.port == 0 or parsed.password is not None or parsed.query or parsed.fragment
            or "?" in url or "#" in url):
        raise ValueError("remote owner URL must have a host and no password, query, or fragment")
    if parsed.scheme == "ssh":
        username = parsed.username or ""
        if (parsed.path not in ("", "/") or parsed.hostname.startswith("-")
                or "%" in parsed.hostname or any(not (c.isascii() and (c.isalnum() or c in "_.-"))
                                               for c in username)):
            raise ValueError("SSH owner requires ssh://[user@]host[:ssh-port] without a path")
    elif parsed.username is not None:
        raise ValueError("HTTP owner URL must not contain credentials")
    if parsed.scheme == "http" and bearer:
        try:
            loopback = ipaddress.ip_address(parsed.hostname).is_loopback
        except ValueError:
            loopback = False
        if not loopback:
            raise ValueError("bearer tokens require HTTPS, SSH, or numeric loopback HTTP")
    return url


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
        if host_process.IS_WINDOWS and Path(override).suffix.lower() != ".exe":
            raise McpTransportError("native Windows mnemed client requires an .exe, not a shell wrapper")
        return override
    # Resolve for each new child, not once at import or against a retained bundle.
    found = find_executable("mnemed")
    if found:
        return found
    sibling = Path(__file__).resolve().parent.parent / "bin" / executable_name("mnemed")
    if sibling.is_file():
        return str(sibling)  # A failed chosen executable is never retried elsewhere.
    raise McpTransportError("mnemed client binary unavailable")


class McpClient:
    def __init__(self, url, *, token=None, token_env=None, timeout=3.0,
                 expected_server_name="mneme-mcp", remote_owner=False, ssh_mcp_port=18766):
        if type(remote_owner) is not bool:
            raise ValueError("remote_owner must be a boolean")
        if remote_owner:
            validate_remote_endpoint(url, ssh_mcp_port=ssh_mcp_port,
                                     bearer=token is not None or bool(token_env))
        else:
            parsed = urlsplit(url)
            if (parsed.scheme != "http" or parsed.hostname not in ("127.0.0.1", "::1")
                    or parsed.username or parsed.password or parsed.query or parsed.fragment
                    or not parsed.port):
                raise ValueError("Mneme integration requires a numeric loopback HTTP URL with port")
        if type(timeout) not in (int, float) or not math.isfinite(timeout) or timeout <= 0 or timeout > 30:
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
        self.remote_owner = remote_owner
        self.ssh_mcp_port = ssh_mcp_port
        self._process = None
        self._pipe = None
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
                "--expected-server", self.expected_server_name]
        if self.remote_owner:
            argv += ["--remote-owner", "--ssh-mcp-port", str(self.ssh_mcp_port)]
        else:
            argv += ["--local-only"]
        if token_env:
            argv += ["--token-env", token_env]
        try:
            self._process = host_process.spawn(argv, tree=True, stdin=subprocess.PIPE,
                                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                             env=env, bufsize=0)
            self._pipe = host_process.BoundedPipe(self._process)
        except OSError as error:
            self._stop()
            raise McpTransportError("mnemed client process unavailable") from error

    def _readline(self, deadline):
        while True:
            pos = self._buffer.find(b"\n")
            if pos >= 0:
                if pos > MAX_RESPONSE_BYTES:
                    raise McpProtocolError("native client response exceeds 512 KiB")
                line, self._buffer = self._buffer[:pos], self._buffer[pos + 1:]
                return line
            if len(self._buffer) > MAX_RESPONSE_BYTES:
                raise McpProtocolError("native client response exceeds 512 KiB")
            try:
                chunk = self._pipe.read(min(8192, MAX_RESPONSE_BYTES + 1 - len(self._buffer)), deadline)
            except TimeoutError as error:
                raise McpTransportError("native client response timed out; request was not retried") from error
            if not chunk:
                raise McpTransportError("native client exited; request outcome may be ambiguous; not retried")
            self._buffer += chunk

    def _write(self, data, deadline):
        try:
            self._pipe.write(data, deadline)
        except TimeoutError as error:
            raise McpTransportError("native client write timed out; request was not retried") from error

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

    def owner_capabilities(self):
        """Tiny native-validated discovery summary; never remote initialize data.

        Unknown/old/malformed support fails closed without fetching another
        catalog or trying an unguarded compatibility write.
        """
        self.connect()
        value = self._native_capabilities.get("owner_capabilities")
        value = value if isinstance(value, dict) else {}
        return {name: value.get(name) is True for name in
                ("retag_content_guards", "tag_vocabulary")}

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
        pipe, self._pipe = self._pipe, None
        try:
            host_process.terminate_tree(process, timeout=1)
        finally:
            if pipe is not None:
                pipe.close()
            else:
                for stream in (process.stdin, process.stdout):
                    if stream is not None:
                        stream.close()

    def close(self):
        try:
            if self._process is not None:
                self._request("close")
        except McpError:
            pass
        finally:
            self._stop()  # Capability cache never survives a logical close.

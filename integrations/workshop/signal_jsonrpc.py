"""Bounded, single-owner Signal stdio transport (signal-cli 0.14.9-SNAPSHOT).

The callback receives complete JSON-RPC receive notifications synchronously and
must persist them before returning. signal-cli's upstream ACK precedes this
callback: this adapter cannot promise lossless or exactly-once reception.

Only start/subscribe, fixed-owner text send, own-profile update, poll and close
are exposed. There is no read-receipt operation, arbitrary RPC escape hatch,
listener, retry or restart.
"""

import json
import math
import os
from pathlib import Path
import select
import signal
import stat
import subprocess
import time
import uuid


MAX_FRAME_BYTES = 256 * 1024
MAX_TEXT_CHARS = 4000
MAX_JSON_DEPTH = 32
MAX_POLL_FRAMES = 128
READ_CHUNK_BYTES = 16 * 1024
MINIMAL_CONFIG = {"verbose": 0, "logFile": "/dev/null", "scrubLog": True,
                  "account": None, "dbus": False, "dbusSystem": False}


class TransportError(RuntimeError):
    """Fixed, non-payload diagnostic. No original exception is exposed."""

    def __init__(self, code):
        self.code = code
        super().__init__(code)


def _uuid(value):
    try:
        if not isinstance(value, str) or str(uuid.UUID(value)) != value:
            raise ValueError
    except (ValueError, AttributeError):
        raise TransportError("invalid_account_identifier") from None
    return value


def _duration(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0:
        raise TransportError("invalid_timeout")
    return float(value)


def _pairs(items):
    result = {}
    for key, value in items:
        if key in result:
            raise TransportError("duplicate_json_key")
        result[key] = value
    return result


def _decode(raw):
    if len(raw) > MAX_FRAME_BYTES:
        raise TransportError("frame_too_large")
    try:
        text = raw.decode("utf-8")
        depth, quoted, escaped = 0, False, False
        for char in text:
            if quoted:
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == '"':
                    quoted = False
            elif char == '"':
                quoted = True
            elif char in "[{":
                depth += 1
                if depth > MAX_JSON_DEPTH:
                    raise TransportError("json_too_deep")
            elif char in "]}":
                depth -= 1
        return json.loads(text, object_pairs_hook=_pairs,
                          parse_constant=lambda _: (_ for _ in ()).throw(ValueError()))
    except (UnicodeError, ValueError, RecursionError):
        raise TransportError("invalid_json") from None


def validate_config(path):
    """Read-only admission of a canonical, private, exact minimal CLI config."""
    path = Path(path)
    try:
        if not path.is_absolute() or path != path.resolve(strict=True):
            raise TransportError("invalid_config_path")
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        try:
            info = os.fstat(fd)
            if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or
                    info.st_mode & 0o077 or info.st_nlink != 1 or info.st_size > 4096):
                raise TransportError("unsafe_config_file")
            with os.fdopen(fd, "rb") as stream:
                fd = -1
                value = _decode(stream.read(4097))
        finally:
            if fd >= 0:
                os.close(fd)
        if (not isinstance(value, dict) or set(value) != set(MINIMAL_CONFIG) or
                any(type(value[k]) is not type(v) or value[k] != v for k, v in MINIMAL_CONFIG.items())):
            raise TransportError("nonminimal_cli_config")
    except OSError:
        raise TransportError("config_unavailable") from None


class SignalRpc:
    """Synchronous actor; call from one thread. Callback must not reenter it.

    start/poll raise TransportError and stop the child on protocol/callback failure.
    send returns accepted/failed/unknown; after any possible write, transport loss
    is unknown. A failed transport stays closed. Caller owns durable send intent.
    config_path is a pre-created signal-cli JSON config, not the bridge config.
    """

    def __init__(self, cli: Path, account: str, data_dir: Path, config_path: Path,
                 on_receive, *, start_timeout=45, send_timeout=60, close_timeout=3):
        self.cli, self.data_dir, self.config_path = Path(cli), Path(data_dir), Path(config_path)
        self.account = _uuid(account)
        if not callable(on_receive):
            raise TransportError("invalid_receive_callback")
        self.on_receive = on_receive
        self.start_timeout = _duration(start_timeout)
        self.send_timeout = _duration(send_timeout)
        self.close_timeout = _duration(close_timeout)
        self._process = None
        self._buffer = bytearray()
        self._pending = None
        self._response = None
        self._sequence = 0
        self._received = 0
        self._started = False
        self._closed = False
        self._busy = False
        self._written = False
        self._owner = None
        self.subscription = None

    def _admit(self):
        validate_config(self.config_path)
        try:
            for path in (self.cli, self.data_dir):
                if not path.is_absolute() or path != path.resolve(strict=True):
                    raise TransportError("noncanonical_transport_path")
            if not self.cli.is_file() or not os.access(self.cli, os.X_OK) or not self.data_dir.is_dir():
                raise TransportError("transport_path_unavailable")
            if Path("/etc/signal-cli/config.json").exists():
                raise TransportError("ambient_system_config")
        except OSError:
            raise TransportError("transport_path_unavailable") from None

    def start(self):
        if self._started:
            return self
        if self._closed or self._busy:
            raise TransportError("transport_closed_or_busy")
        self._admit()  # No process or Signal store open before complete validation.
        command = [str(self.cli), "--data-dir", str(self.data_dir), "--log-file", "/dev/null",
                   "--scrub-log", "-a", self.account, "-o", "json", "jsonRpc",
                   "--receive-mode", "manual", "--ignore-attachments", "--ignore-avatars",
                   "--ignore-stickers", "--ignore-stories"]
        # In particular, no inherited JAVA_* options, logging overrides or credentials.
        env = {"HOME": str(Path.home()), "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8",
               "SIGNAL_CLI_CONFIG": str(self.config_path)}
        self._busy = True
        try:
            self._process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                             stderr=subprocess.DEVNULL, env=env, bufsize=0,
                                             start_new_session=True)
            os.set_blocking(self._process.stdin.fileno(), False)
            os.set_blocking(self._process.stdout.fileno(), False)
            response = self._request("subscribeReceive", None, self.start_timeout)
            subscription = response.get("result")
            if "error" in response or type(subscription) is not int or subscription < 0:
                raise TransportError("subscription_refused")
            self.subscription = subscription
            self._started = True
            return self
        except TransportError:
            self.close()
            raise
        except OSError:
            self.close()
            raise TransportError("process_start_failed") from None
        except BaseException:
            self.close()
            raise
        finally:
            self._busy = False

    def _dispatch(self, frame):
        if not isinstance(frame, dict) or frame.get("jsonrpc") != "2.0":
            raise TransportError("invalid_rpc_frame")
        if frame.get("method") == "receive" and frame.get("id") is None:
            if not isinstance(frame.get("params"), dict):
                raise TransportError("invalid_receive_frame")
            try:
                self.on_receive(frame)
            except Exception:
                raise TransportError("receive_callback_failed") from None
            self._received += 1
        elif (self._pending is not None and frame.get("id") == self._pending and
              "method" not in frame and ("result" in frame) != ("error" in frame)):
            if self._response is not None:
                raise TransportError("duplicate_rpc_response")
            self._response = frame
        else:
            raise TransportError("unexpected_rpc_frame")

    def _consume(self, budget):
        count = 0
        while count < budget:
            end = self._buffer.find(b"\n")
            if end < 0:
                if len(self._buffer) > MAX_FRAME_BYTES:
                    raise TransportError("frame_too_large")
                break
            if end > MAX_FRAME_BYTES:
                raise TransportError("frame_too_large")
            raw = bytes(self._buffer[:end])
            del self._buffer[:end + 1]
            self._dispatch(_decode(raw))
            count += 1
        return count

    def _pump(self, timeout, budget=MAX_POLL_FRAMES):
        deadline = time.monotonic() + timeout
        while True:
            count = self._consume(budget)
            if count or count >= budget:
                return count
            if self._process is None or self._closed:
                raise TransportError("transport_closed")
            fd = self._process.stdout.fileno()
            try:
                ready, _, _ = select.select([fd], [], [], max(0, deadline - time.monotonic()))
                if not ready:
                    return 0
                room = MAX_FRAME_BYTES + 1 - len(self._buffer)
                if room <= 0:
                    raise TransportError("frame_too_large")
                chunk = os.read(fd, min(READ_CHUNK_BYTES, room))
                if not chunk:
                    raise TransportError("truncated_frame" if self._buffer else "process_disconnected")
                self._buffer.extend(chunk)
            except (OSError, ValueError):
                raise TransportError("process_io_failed") from None

    def _request(self, method, params, timeout):
        self._sequence += 1
        self._pending = f"workshop-{self._sequence}"
        self._response = None
        self._written = False
        request = {"jsonrpc": "2.0", "id": self._pending, "method": method}
        if params is not None:
            request["params"] = params
        raw = json.dumps(request, ensure_ascii=False, separators=(",", ":")).encode("utf-8") + b"\n"
        if len(raw) > MAX_FRAME_BYTES:
            raise TransportError("request_too_large")
        deadline = time.monotonic() + timeout
        offset = 0
        try:
            while offset < len(raw):
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TransportError("request_timeout")
                try:
                    amount = os.write(self._process.stdin.fileno(), raw[offset:])
                    if amount <= 0:
                        raise TransportError("process_disconnected")
                    self._written = True
                    offset += amount
                except BlockingIOError:
                    self._pump(min(remaining, 0.05))
                except OSError:
                    raise TransportError("process_io_failed") from None
            while self._response is None:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TransportError("request_timeout")
                self._pump(remaining)
            return self._response
        finally:
            self._pending = None
            self._response = None

    def poll(self, timeout=0):
        timeout = _duration(timeout)
        if self._busy or not self._started or self._closed:
            raise TransportError("transport_closed_or_busy")
        before, count = self._received, 0
        self._busy = True
        try:
            while count < MAX_POLL_FRAMES:
                consumed = self._pump(timeout if count == 0 else 0, MAX_POLL_FRAMES - count)
                if not consumed:
                    break
                count += consumed
            return self._received - before
        except TransportError:
            self.close()
            raise
        except BaseException:
            self.close()
            raise
        finally:
            self._busy = False

    @property
    def caught_up(self):
        """No buffered/locally readable frames at this instant (not a network ACK).

        A bounded poll can leave a backlog or partial frame. Defer new sends until
        this is true; later arrivals can still race a send and are dispatched by it.
        """
        if self._closed or self._busy or not self._started or self._buffer:
            return False
        try:
            ready, _, _ = select.select([self._process.stdout.fileno()], [], [], 0)
            return not ready
        except (OSError, ValueError):
            return False

    @staticmethod
    def _send_result(response, owner):
        error = response.get("error")
        body = response.get("result")
        if isinstance(error, dict):
            if error.get("code") in {-32600, -32601, -32602}:
                return {"outcome": "failed", "reason": "rpc_rejected"}
            data = error.get("data")
            body = data.get("response") if isinstance(data, dict) else None
        rows = body.get("results") if isinstance(body, dict) else None
        if isinstance(rows, list) and len(rows) == 1 and isinstance(rows[0], dict):
            address = rows[0].get("recipientAddress")
            if isinstance(address, dict) and address.get("uuid") == owner:
                kind = rows[0].get("type")
                timestamp = body.get("timestamp")
                if kind == "SUCCESS" and error is None and type(timestamp) is int and 0 < timestamp < 2**63:
                    return {"outcome": "accepted", "timestamp_ms": timestamp}
                if kind in {"UNREGISTERED_FAILURE", "IDENTITY_FAILURE", "RATE_LIMIT_FAILURE", "INVALID_PRE_KEY_FAILURE"}:
                    return {"outcome": "failed", "reason": "recipient_refused"}
        return {"outcome": "unknown", "reason": "send_outcome_unconfirmed"}

    def send(self, owner_id, text):
        owner_id = _uuid(owner_id)
        if not isinstance(text, str) or not 1 <= len(text) <= MAX_TEXT_CHARS:
            raise TransportError("invalid_message_length")
        try:
            text.encode("utf-8")
        except UnicodeError:
            raise TransportError("invalid_message_encoding") from None
        if self._owner is not None and self._owner != owner_id:
            raise TransportError("recipient_binding_changed")
        if self._busy:
            raise TransportError("transport_busy")
        if not self._started or self._closed:
            return {"outcome": "failed", "reason": "transport_not_started"}
        self._owner = owner_id
        self._busy = True
        self._written = False
        try:
            response = self._request("send", {"recipient": [owner_id], "message": text}, self.send_timeout)
            return self._send_result(response, owner_id)
        except TransportError as error:
            outcome = "unknown" if self._written else "failed"
            self.close()
            return {"outcome": outcome, "reason": error.code}
        except BaseException:
            self.close()
            raise
        finally:
            self._busy = False

    def update_profile(self, *, given_name=None, avatar=None):
        """Update this actor's own profile, never another account.

        The caller admits image format/size and stages an immutable private file;
        only that canonical local path is accepted here, not a data URI. Omitted
        fields stay unchanged. Empty RPC result confirms command completion, not
        another client's visibility. Even an IO error may follow server mutation.
        """
        params = {}
        if given_name is not None:
            if not isinstance(given_name, str) or not 1 <= len(given_name) <= 128:
                return {"outcome": "failed", "reason": "invalid_profile_name"}
            try:
                given_name.encode("utf-8")
            except UnicodeError:
                return {"outcome": "failed", "reason": "invalid_profile_name"}
            params["givenName"] = given_name
        if avatar is not None:
            try:
                path = Path(avatar)
                if not path.is_absolute() or path != path.resolve(strict=True):
                    raise ValueError
                info = path.stat()
                if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or
                        info.st_mode & 0o077 or info.st_nlink != 1):
                    raise ValueError
                str(path).encode("utf-8")
            except (OSError, ValueError, TypeError, RuntimeError):
                return {"outcome": "failed", "reason": "invalid_profile_avatar"}
            params["avatar"] = str(path)
        if not params:
            return {"outcome": "failed", "reason": "empty_profile_update"}
        if self._busy:
            return {"outcome": "failed", "reason": "transport_busy"}
        if not self._started or self._closed:
            return {"outcome": "failed", "reason": "transport_not_started"}
        self._busy = True
        self._written = False
        try:
            response = self._request("updateProfile", params, self.send_timeout)
            if "error" not in response and response.get("result") == {}:
                return {"outcome": "accepted"}
            return {"outcome": "unknown", "reason": "profile_outcome_unconfirmed"}
        except TransportError as error:
            outcome = "unknown" if self._written else "failed"
            self.close()
            return {"outcome": outcome, "reason": error.code}
        except BaseException:
            self.close()
            raise
        finally:
            self._busy = False

    def close(self):
        self._closed = True
        self._started = False
        process, self._process = self._process, None
        if process is None:
            return
        try:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except (ProcessLookupError, PermissionError):
                pass
            try:
                process.wait(timeout=self.close_timeout)
            except subprocess.TimeoutExpired:
                pass
            # Also clean up children when the session leader exited first.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
            try:
                process.wait(timeout=max(self.close_timeout, 0.1))
            except subprocess.TimeoutExpired:
                pass
        finally:
            for stream in (process.stdin, process.stdout):
                if stream is not None:
                    stream.close()
            self._buffer.clear()

    def __enter__(self):
        return self.start()

    def __exit__(self, *_):
        self.close()

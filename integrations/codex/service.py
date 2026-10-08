"""Explicit single-store Mneme MCP connections and local process management.

This manages one foreground-capable daemon for one *explicit* user or project store. It
does not discover repositories, open a second CLI lease, install launchd jobs,
or edit Codex configuration. State lives in a caller-chosen private directory.
Connect-only endpoints instead probe an existing loopback host (or SSH forward)
without owning a process, local store, or service-state directory.
"""

import argparse
from dataclasses import dataclass
import fcntl
import json
import math
import os
from pathlib import Path
import plistlib
import posixpath
import signal
import subprocess
import sys
import time
from urllib.parse import urlsplit

from mcp_client import McpClient, McpError


_UNSET = object()
MAX_CONFIG_BYTES = 32768
MAX_SERVER_PATH_BYTES = 4096


def _unique_config_fields(pairs):
    data = {}
    for key, value in pairs:
        if key in data:
            raise ValueError("duplicate service configuration field: %s" % key)
        data[key] = value
    return data


def _read_config(path):
    with Path(path).open("rb") as config_file:
        encoded = config_file.read(MAX_CONFIG_BYTES + 1)
    if len(encoded) > MAX_CONFIG_BYTES:
        raise ValueError("service configuration exceeds %d bytes" % MAX_CONFIG_BYTES)
    data = json.loads(encoded.decode("utf-8"), object_pairs_hook=_unique_config_fields)
    if not isinstance(data, dict):
        raise ValueError("service configuration must be an object")
    return data


def server_database_path(value):
    """Validate, never resolve or inspect, an exact POSIX server-path identity."""
    if (not isinstance(value, str) or not value or "\x00" in value
            or len(value.encode("utf-8")) > MAX_SERVER_PATH_BYTES
            or not posixpath.isabs(value) or value.startswith("//")
            or posixpath.normpath(value) != value):
        raise ValueError("database_path must be a bounded normalized absolute POSIX server path")
    return value


@dataclass(frozen=True)
class ConnectConfig:
    """A connection capability, deliberately lacking any process-control fields."""

    url: str
    database_name: str
    database_path: str
    token_env: str = ""

    def validate_identity(self):
        if not isinstance(self.database_name, str) or self.database_name not in ("user", "project"):
            raise ValueError("database_name must be user or project")
        server_database_path(self.database_path)
        if (not isinstance(self.url, str) or len(self.url) > 128
                or any(char.isspace() or ord(char) < 32 for char in self.url)):
            raise ValueError("connect endpoint requires a numeric loopback HTTP URL")
        parsed = urlsplit(self.url)
        if (parsed.scheme != "http" or parsed.hostname not in ("127.0.0.1", "::1")
                or parsed.username is not None or parsed.password is not None
                or parsed.query or parsed.fragment or parsed.path not in ("", "/")
                or parsed.port is None or not 1024 <= parsed.port <= 65535):
            raise ValueError("connect endpoint requires a numeric loopback HTTP URL on port 1024..65535")
        if not isinstance(self.token_env, str) or (self.token_env and not self.token_env.isidentifier()):
            raise ValueError("token_env must be a variable name")
        return self

    def validate(self):
        self.validate_identity()
        if self.token_env:
            token = os.environ.get(self.token_env)
            if not token or "\r" in token or "\n" in token:
                raise ValueError("configured bearer-token environment variable is unset or invalid")
        return self

    @classmethod
    def _from_data(cls, data):
        required = {"mode", "url", "database_name", "database_path"}
        if (not required <= set(data) or set(data) - required - {"token_env"}
                or data["mode"] != "connect"):
            raise ValueError("connect config needs only mode, url, database_name/database_path and optional token_env")
        return cls(data["url"], data["database_name"], data["database_path"],
                   data.get("token_env", "")).validate_identity()


def load_config(path):
    """Read once; choose only an explicit connect mode or the shipped local schema."""
    data = _read_config(path)
    if data.get("mode") == "connect":
        return ConnectConfig._from_data(data)
    return ServiceConfig._from_data(data)


@dataclass(frozen=True, init=False)
class ServiceConfig:
    binary: Path
    database_name: str
    database_path: Path
    port: int
    state_dir: Path
    working_directory: Path
    token_env: str = ""

    def __init__(self, binary, project_db=_UNSET, port=None, state_dir=None,
                 working_directory=None, token_env="", *, database_name=_UNSET,
                 database_path=_UNSET):
        # Keep the shipped project constructor, including its second positional
        # argument. New named stores must use the complete, unambiguous pair.
        if project_db is not _UNSET:
            if database_name is not _UNSET or database_path is not _UNSET:
                raise ValueError("project_db cannot be mixed with database_name/database_path")
            database_name, database_path = "project", project_db
        elif database_name is _UNSET or database_path is _UNSET:
            raise ValueError("service config needs database_name and database_path")
        for name, value in (("binary", binary), ("database_name", database_name),
                            ("database_path", database_path), ("port", port),
                            ("state_dir", state_dir), ("working_directory", working_directory),
                            ("token_env", token_env)):
            object.__setattr__(self, name, value)

    @property
    def project_db(self):
        """Legacy project-only callers must not silently select the user store."""
        if self.database_name != "project":
            raise ValueError("project_db is only available for project service configurations")
        return self.database_path

    def validate_identity(self):
        if not isinstance(self.database_name, str) or self.database_name not in ("user", "project"):
            raise ValueError("database_name must be user or project")
        for name in ("binary", "database_path", "state_dir", "working_directory"):
            value = getattr(self, name)
            if not isinstance(value, Path) or not value.is_absolute():
                raise ValueError("%s must be an absolute path" % name)
        if type(self.port) is not int or not (1024 <= self.port <= 65535):
            raise ValueError("port must be in 1024..65535")
        if not isinstance(self.token_env, str) or (self.token_env and not self.token_env.isidentifier()):
            raise ValueError("token_env must be a variable name")
        return self

    def validate(self):
        """Launch validation; status and stop need only stable identity fields."""
        self.validate_identity()
        if not self.binary.is_file() or not os.access(self.binary, os.X_OK):
            raise ValueError("Mneme MCP binary is missing or not executable")
        if not self.database_path.is_file():
            raise ValueError("database_path must be an existing file; initialize an explicit capture store first")
        if not self.working_directory.is_dir():
            raise ValueError("working_directory must exist before launch")
        if self.token_env and not os.environ.get(self.token_env):
            raise ValueError("configured bearer-token environment variable is unset")
        return self

    @property
    def url(self):
        return "http://127.0.0.1:%d/" % self.port

    @property
    def argv(self):
        args = [str(self.binary), "--capability-profile", "operator", "--http",
                "127.0.0.1:%d" % self.port, "--db",
                "%s=%s" % (self.database_name, self.database_path)]
        if self.token_env:
            args += ["--http-token-env", self.token_env]
        return args

    @classmethod
    def from_json(cls, path):
        """Managed-local parser retained for project provisioning/legacy callers."""
        return cls._from_data(_read_config(path))

    @classmethod
    def _from_data(cls, data):
        if data.get("mode") == "connect":
            raise ValueError("connect-only config cannot manage a local service; use load_config for clients")
        if not isinstance(data, dict) or set(data) - {
            "binary", "project_db", "database_name", "database_path", "port",
            "state_dir", "working_directory", "token_env"
        }:
            raise ValueError("unknown service configuration fields")
        if "project_db" in data:
            if "database_name" in data or "database_path" in data:
                raise ValueError("project_db cannot be mixed with database_name/database_path")
            database_name, database_path = "project", data["project_db"]
        else:
            if "database_name" not in data or "database_path" not in data:
                raise ValueError("service config needs database_name and database_path")
            database_name, database_path = data["database_name"], data["database_path"]
        try:
            obj = cls(binary=Path(data["binary"]), database_name=database_name,
                      database_path=Path(database_path), port=data["port"],
                      state_dir=Path(data["state_dir"]),
                      working_directory=Path(data["working_directory"]),
                      token_env=data.get("token_env", ""))
        except (KeyError, TypeError) as error:
            raise ValueError("service config needs binary, database_name/database_path, port, state_dir, working_directory") from error
        return obj.validate_identity()


def _state_path(config):
    return config.state_dir / "mneme-codex-service.json"


def _lock(config):
    config.state_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    if config.state_dir.stat().st_mode & 0o077:
        raise ValueError("state_dir must not be accessible to group/others")
    fd = os.open(str(config.state_dir / "service.lock"), os.O_CREAT | os.O_RDWR, 0o600)
    fcntl.flock(fd, fcntl.LOCK_EX)
    return fd


def _read_state(config):
    try:
        state = json.loads(_state_path(config).read_text(encoding="utf-8"))
    except FileNotFoundError:
        return None
    if not isinstance(state, dict) or not isinstance(state.get("pid"), int):
        raise ValueError("service state is malformed; inspect it before changing process state")
    return state


def _state_matches_config(config, state):
    if "project_db" in state:
        # This is the sole supported legacy state. Mixed forms are not a
        # migration: they could describe two different store identities.
        store_matches = (config.database_name == "project"
                         and "database_name" not in state and "database_path" not in state
                         and state["project_db"] == str(config.database_path))
    else:
        store_matches = (state.get("database_name") == config.database_name
                         and state.get("database_path") == str(config.database_path))
    return (state.get("binary") == str(config.binary)
            and store_matches
            and state.get("port") == config.port
            and state.get("working_directory") == str(config.working_directory))


def _expected_catalog(config, catalog):
    return (isinstance(catalog, list) and len(catalog) == 1
            and isinstance(catalog[0], dict)
            and catalog[0].get("db") == config.database_name
            and catalog[0].get("name") == config.database_name
            and catalog[0].get("state") == "open"
            and catalog[0].get("configured_path") == str(config.database_path))


def _matches_process(config, pid):
    """True for this host, False for confirmed absence/mismatch, None if unknown."""
    if pid <= 0:
        return False
    try:
        output = subprocess.run(["ps", "-ww", "-p", str(pid), "-o", "command="],
                                check=False, capture_output=True, text=True, timeout=2)
    except (OSError, subprocess.TimeoutExpired):
        return _absent_or_unverified(pid)
    if output.returncode != 0:
        return _absent_or_unverified(pid)
    if not output.stdout.strip():
        return _absent_or_unverified(pid)
    # `ps` exposes a space-joined argv rather than NUL-separated arguments.
    # Match the *whole* intended invocation: substring checks could authorize
    # SIGTERM against a reused PID running a nearby port or store path.
    return output.stdout.rstrip("\n") == " ".join(config.argv)


def _absent_or_unverified(pid):
    # A sandbox may deny `ps` even while the process is alive. A failed ps is
    # stale state only when an independent existence probe confirms ESRCH.
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except OSError:
        pass
    return None


def _probe(config, timeout=1.0):
    token = os.environ.get(config.token_env) if config.token_env else None
    try:
        with McpClient(config.url, token=token, timeout=timeout) as client:
            return client.call_tool("databases", {})
    except McpError:
        return None


def _terminate_spawned(process):
    if process.poll() is not None:
        return
    process.terminate()
    try:
        # The HTTP host may spend 15 seconds draining requests and another 15
        # releasing database leases after SIGTERM.
        process.wait(timeout=40)
    except subprocess.TimeoutExpired:
        # Only a spawn that never reached recorded service state is force-killed.
        process.kill()
        process.wait(timeout=3)


def _require_managed(config, action):
    if isinstance(config, ConnectConfig):
        raise ValueError("connect-only endpoint cannot %s a service; manage the host and tunnel on their owning machines" % action)


def _connect_status(config, probe_timeout=0.2):
    config.validate()
    catalog = _probe(config, timeout=probe_timeout)
    readiness = ("unreachable" if catalog is None else
                 "ready" if _expected_catalog(config, catalog) else "unexpected-catalog")
    return {"state": readiness, "mode": "connect", "url": config.url,
            "db": config.database_name, "database_path": config.database_path}


def status(config):
    if isinstance(config, ConnectConfig):
        return _connect_status(config)
    config.validate_identity()
    state = _read_state(config)
    if not state:
        return {"state": "stopped", "url": config.url}
    if not _state_matches_config(config, state):
        return {"state": "foreign-state", "url": config.url}
    identity = _matches_process(config, state["pid"])
    if identity is None:
        return {"state": "identity-unverified", "url": config.url, "pid": state["pid"]}
    if not identity:
        return {"state": "stale", "url": config.url, "pid": state["pid"]}
    catalog = _probe(config)
    readiness = ("unreachable" if catalog is None else
                 "ready" if _expected_catalog(config, catalog) else "unexpected-catalog")
    return {"state": readiness,
            "url": config.url, "pid": state["pid"]}


def start(config, timeout=10.0):
    _require_managed(config, "start")
    config.validate()
    fd = _lock(config)
    try:
        existing = status(config)
        if existing["state"] == "ready":
            return existing
        if existing["state"] not in ("stopped", "stale"):
            raise RuntimeError("refusing to replace %s service state" % existing["state"])
        if _probe(config) is not None:
            raise RuntimeError("refusing to start: Mneme MCP already responds on configured port")
        if existing["state"] == "stale":
            _state_path(config).unlink()
        log_path = config.state_dir / "mneme-mcp.log"
        with open(log_path, "ab", buffering=0) as log:
            process = subprocess.Popen(config.argv, stdin=subprocess.DEVNULL,
                                       stdout=log, stderr=log, start_new_session=True,
                                       close_fds=True, cwd=config.working_directory)
        try:
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError("Mneme MCP exited during startup; inspect private log")
                catalog = _probe(config, timeout=0.5)
                if catalog is not None:
                    if not _expected_catalog(config, catalog):
                        raise RuntimeError("refusing startup: Mneme MCP has unexpected database catalog")
                    state = {"pid": process.pid, "binary": str(config.binary),
                             "database_name": config.database_name,
                             "database_path": str(config.database_path), "port": config.port,
                             "working_directory": str(config.working_directory)}
                    path = _state_path(config)
                    temp = path.with_suffix(".tmp")
                    temp.write_text(json.dumps(state, sort_keys=True), encoding="utf-8")
                    os.chmod(temp, 0o600)
                    temp.replace(path)
                    return {"state": "ready", "url": config.url, "pid": process.pid}
                time.sleep(0.1)
            raise RuntimeError("Mneme MCP startup timed out; inspect private log")
        except Exception:
            _terminate_spawned(process)
            raise
    finally:
        os.close(fd)


def ensure_ready(config, timeout=10.0):
    """Ensure an exact catalog; connect-only configs never control a process.

    HTTP reuse is not process identity evidence and never authorizes signalling.
    Only the fallback may lock, inspect processes, or spawn a host.
    """
    if isinstance(config, ConnectConfig):
        if type(timeout) not in (int, float) or not math.isfinite(timeout) or not 0 < timeout <= 30:
            raise ValueError("connect readiness timeout must be finite and in (0, 30]")
        config.validate()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            # initialize, notification, catalog, DELETE: leave room for all
            # exchanges rather than spending the entire budget on each one.
            remaining = deadline - time.monotonic()
            current = _connect_status(config, max(0.001, min(1.0, remaining / 5)))
            if current["state"] == "ready":
                return {**current, "reuse_only": True}
            if current["state"] == "unexpected-catalog":
                raise RuntimeError("connect-only Mneme %s has unexpected database catalog; verify the configured host and server path" % config.database_name)
            time.sleep(min(0.05, max(0, deadline - time.monotonic())))
        raise RuntimeError("connect-only Mneme %s unavailable; check the SSH tunnel and remote service, then retry (no local fallback)" % config.database_name)
    config.validate()
    state = _read_state(config)
    if state is not None and not _state_matches_config(config, state):
        raise RuntimeError("refusing to reuse foreign service state")
    catalog = _probe(config)
    if catalog is not None:
        if not _expected_catalog(config, catalog):
            raise RuntimeError("refusing to reuse Mneme MCP with unexpected database catalog")
        if state is not None:
            return {"state": "ready", "url": config.url, "pid": state["pid"],
                    "reuse_only": True, "process_identity": "unchecked"}
        # Another starter may have bound the port but not yet published state.
        # Wait for that atomic publication without requiring sandboxed clients
        # to write our private lock or inspect processes. An unrecorded host
        # eventually reaches the strict path, which refuses to claim it.
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            time.sleep(0.05)
            state = _read_state(config)
            if state is None:
                continue
            if not _state_matches_config(config, state):
                raise RuntimeError("refusing to reuse foreign service state")
            catalog = _probe(config)
            if catalog is not None and not _expected_catalog(config, catalog):
                raise RuntimeError("refusing to reuse Mneme MCP with unexpected database catalog")
            if catalog is not None:
                return {"state": "ready", "url": config.url, "pid": state["pid"],
                        "reuse_only": True, "process_identity": "unchecked"}
            break
    return start(config, timeout=timeout)


def stop(config, timeout=40.0):
    _require_managed(config, "stop")
    config.validate_identity()
    fd = _lock(config)
    try:
        current = status(config)
        if current["state"] == "stopped":
            return current
        if current["state"] == "stale":
            _state_path(config).unlink()
            return {"state": "stopped", "url": config.url}
        if current["state"] in ("foreign-state", "identity-unverified"):
            raise RuntimeError("refusing to stop a %s service state" % current["state"])
        pid = current["pid"]
        identity = _matches_process(config, pid)
        if identity is None:
            raise RuntimeError("service process identity became unverified; refusing to signal")
        if not identity:
            raise RuntimeError("service process identity changed; refusing to signal")
        os.kill(pid, signal.SIGTERM)
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            identity = _matches_process(config, pid)
            if identity is None:
                raise RuntimeError("service process identity became unverified after SIGTERM; state retained")
            if not identity:
                _state_path(config).unlink()
                return {"state": "stopped", "url": config.url}
            time.sleep(0.1)
        raise RuntimeError("Mneme MCP did not stop; do not remove its lease by hand")
    finally:
        os.close(fd)


def restart(config):
    _require_managed(config, "restart")
    stop(config)
    return start(config)


def launchd_plist(config, label="local.mneme.codex"):
    """Return a reviewable plist; never load it or provision a global service."""
    _require_managed(config, "generate a launchd job for")
    config.validate()
    if config.token_env:
        raise ValueError("launchd token provisioning must be designed separately")
    return plistlib.dumps({
        "Label": label,
        "ProgramArguments": config.argv,
        "RunAtLoad": True,
        "KeepAlive": True,
        "WorkingDirectory": str(config.working_directory),
        "StandardOutPath": str(config.state_dir / "mneme-mcp.log"),
        "StandardErrorPath": str(config.state_dir / "mneme-mcp.log"),
    })


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, help="explicit private JSON service or connect-only config (status only)")
    parser.add_argument("action", choices=("start", "status", "stop", "restart", "print-launchd"))
    args = parser.parse_args(argv)
    config = load_config(args.config)
    if isinstance(config, ConnectConfig) and args.action != "status":
        parser.error("connect-only config supports status, not %s; manage the host and tunnel on their owning machines" % args.action)
    if args.action == "print-launchd":
        sys.stdout.buffer.write(launchd_plist(config))
        return
    result = {"start": start, "status": status, "stop": stop, "restart": restart}[args.action](config)
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()

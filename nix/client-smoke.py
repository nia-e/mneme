#!/usr/bin/env python3
"""Finite installed-client smoke; only synthetic, disposable loopback owners."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import selectors
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer


DB = "01ARZ3NDEKTSV4RRFFQ69G5FA1"
NODE = "01ARZ3NDEKTSV4RRFFQ69G5FA2"
PROTOCOL = "2025-11-25"
TIMEOUT = 20


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def catalog():
    guard = {"type": "string", "minLength": 26, "maxLength": 26,
             "pattern": "^[0-7][0-9A-HJKMNP-TV-Z]{25}$"}
    return [{"name": "databases", "inputSchema": {"type": "object", "properties": {}}}] + [
        {"name": name, "inputSchema": {"type": "object", "properties": {
            "db": {"type": "string"}, "expected_db_id": guard,
            "id": {"type": "string"}}}} for name in ("get", "forget")
    ]


class Owner(HTTPServer):
    """A mock native owner; incorrect identity forwarding fails the smoke."""

    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.timeout = 0.1
        self.calls = []
        self.requests = []
        self.errors = []
        self.stopped = threading.Event()
        self.worker = threading.Thread(target=self.serve, daemon=True)

    def serve(self):
        while not self.stopped.is_set():
            self.handle_request()

    def handle_error(self, request, client_address):
        self.errors.append("unexpected HTTP handler failure")

    def __enter__(self):
        self.worker.start()
        return self

    def __exit__(self, *_):
        self.stopped.set()
        self.worker.join(3)
        self.server_close()
        require(not self.worker.is_alive(), "mock owner did not stop")
        require(not self.errors, f"mock owner errors: {self.errors!r}")

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server_port}/mcp"


class Handler(BaseHTTPRequestHandler):
    def setup(self):
        super().setup()
        self.connection.settimeout(2)

    def log_message(self, *_):
        pass

    def respond(self, value=None, status=200):
        body = b"" if value is None else json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Mcp-Session-Id", "nix-smoke")
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def do_DELETE(self):
        self.server.requests.append("DELETE")
        self.respond(status=202)

    def do_POST(self):
        try:
            require(self.path == "/mcp", f"unexpected owner path: {self.path}")
            length = int(self.headers.get("Content-Length", "0"))
            require(0 < length <= 65536, "owner request outside size bound")
            msg = json.loads(self.rfile.read(length))
            method = msg["method"]
            self.server.requests.append(method)
            if method == "notifications/initialized":
                self.respond(status=202)
                return
            if method == "initialize":
                result = {"protocolVersion": PROTOCOL, "capabilities": {"tools": {}},
                          "serverInfo": {"name": "mneme-mcp", "version": "smoke"},
                          "capabilityProfile": "operator"}
            elif method == "tools/list":
                result = {"tools": catalog()}
            elif method == "tools/call":
                name = msg["params"]["name"]
                args = msg["params"].get("arguments", {})
                self.server.calls.append(name)
                if name == "databases":
                    value = [{"db": "project", "db_id": DB, "state": "open",
                              "resolved_path": "/synthetic/project.db"}]
                else:
                    require(name == "get", f"unexpected owner mutation/tool: {name}")
                    require(args.get("expected_db_id") == DB, "missing/wrong identity guard")
                    require(args.get("db") == "project", "logical alias not routed to owner")
                    require(args.get("id") == NODE, "unexpected node request")
                    value = {"db": "project", "db_id": DB, "id": NODE,
                             "summary": "synthetic nix smoke", "summary_truncated": False}
                result = {"isError": False, "content": [{"type": "text", "text": json.dumps(value)}]}
            else:
                raise RuntimeError(f"unexpected owner method: {method}")
            self.respond({"jsonrpc": "2.0", "id": msg["id"], "result": result})
        except Exception as error:
            self.server.errors.append(str(error))
            self.respond(status=500)


class Stdio:
    def __init__(self, command, env, cwd):
        self.err = tempfile.TemporaryFile()
        self.process = subprocess.Popen(command, env=env, cwd=cwd, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=self.err)
        self.pending = b""

    def __enter__(self):
        return self

    def __exit__(self, *_):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=5)
        self.process.stdin.close()
        self.process.stdout.close()
        self.err.close()

    def send(self, message):
        self.process.stdin.write(json.dumps(message).encode() + b"\n")
        self.process.stdin.flush()

    def request(self, request_id, method, params):
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        deadline = time.monotonic() + TIMEOUT
        with selectors.DefaultSelector() as selector:
            selector.register(self.process.stdout, selectors.EVENT_READ)
            while b"\n" not in self.pending:
                remaining = deadline - time.monotonic()
                require(remaining > 0 and selector.select(remaining),
                        f"stdio response {request_id} timed out")
                chunk = os.read(self.process.stdout.fileno(), 65536)
                require(chunk, f"stdio EOF before response {request_id}")
                self.pending += chunk
                require(len(self.pending) <= 1024 * 1024, "stdio response too large")
        line, self.pending = self.pending.split(b"\n", 1)
        response = json.loads(line)
        require(response.get("jsonrpc") == "2.0" and response.get("id") == request_id,
                f"unexpected stdio response: {response!r}")
        require("error" not in response, f"stdio RPC error: {response!r}")
        return response["result"]

    def finish(self):
        self.process.stdin.close()  # MCP stdio shutdown is EOF, not an invented RPC.
        status = self.process.wait(timeout=5)
        self.err.seek(0)
        require(status == 0, f"stdio exit {status}: {self.err.read().decode(errors='replace')}")


def tool_value(result, is_error=False):
    require(result.get("isError") is is_error, f"unexpected tool status: {result!r}")
    blocks = result["content"]
    require(len(blocks) == 1 and blocks[0]["type"] == "text", "unexpected tool content")
    return json.loads(blocks[0]["text"])


def run(command, env, cwd, *, success=True):
    result = subprocess.run(command, env=env, cwd=cwd, stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, timeout=TIMEOUT)
    require((result.returncode == 0) == success,
            f"unexpected exit {result.returncode}: {command!r}\n{result.stdout}\n{result.stderr}")
    return result


def smoke(prefix, root, owner):
    cli, mcp = [str(prefix / "bin" / name) for name in ("mnemed", "mneme-mcp")]
    env = {"PATH": os.environ.get("PATH", ""), "HOME": str(root / "home"),
           "XDG_CONFIG_HOME": str(root / "config"), "XDG_DATA_HOME": str(root / "data"),
           "XDG_CACHE_HOME": str(root / "cache"), "TMPDIR": str(root), "LANG": "C"}
    for name in ("home", "config", "data", "cache"):
        (root / name).mkdir()
    metadata = json.loads((prefix / "share/mneme/build-info.json").read_text())
    require(metadata["defaultFeatures"] is False and metadata["features"] == [],
            "unexpected installed feature scope")
    require(metadata["binaries"] == ["mnemed", "mneme-mcp"], "unexpected installed binaries")
    version = run([cli, "--version"], env, root).stdout.strip()
    require(version == f"mnemed {metadata['version']}", "CLI/package version mismatch")
    require("--memory-config" in run([cli, "--help"], env, root).stdout, "CLI help missing configuration")
    # The current hand-written MCP parser reports help as a nonzero usage result.
    require("--memory-config" in run([mcp, "--help"], env, root, success=False).stderr,
            "MCP help missing configuration")
    for line in (prefix / "share/mneme/binary-sha256.txt").read_text().splitlines():
        expected, name = line.split()
        with (prefix / name).open("rb") as binary:
            actual = hashlib.file_digest(binary, "sha256").hexdigest()
        require(actual == expected, f"installed hash mismatch for {name}")
    target = root / "generated.toml"
    target.write_text(f'''schema = "mneme.config.v1"
read_default = "fixture"
[stores.fixture]
id = "{DB}"
owner = {{ url = "{owner.url}", db = "project" }}
active = {{ workspace = "any" }}
permissions = {{ read = true, write = false }}
''')
    config = root / "memory.toml"
    config.symlink_to(target.name)
    selected = json.loads(run([cli, "--json", "config", "select", "--config", str(config),
                               "--cwd", str(root)], env, root).stdout)
    require(selected["read_default"] == "fixture" and selected["digest"], "wrong config selection")
    require(not owner.requests, "metadata selection contacted an owner")

    args = [cli, "--memory-config", str(config), "--store", "fixture", "--json"]
    denied = run(args + ["forget", NODE], env, root, success=False)
    require("not granted" in denied.stderr, f"wrong CLI write refusal: {denied.stderr}")
    require(not owner.requests, "CLI denied write contacted the owner")
    value = json.loads(run(args + ["get", NODE], env, root).stdout)
    require(value["id"] == NODE and value["db_id"] == DB, "wrong CLI get result")

    command = [mcp, "--memory-config", str(config), "--cwd", str(root)]
    with Stdio(command, env, root) as stdio:
        initialized = stdio.request(1, "initialize", {"protocolVersion": PROTOCOL,
                                   "capabilities": {}, "clientInfo": {"name": "nix-smoke", "version": "1"}})
        require(initialized["capabilityProfile"] == "configured-databases", "wrong MCP mode")
        require(initialized["serverInfo"]["version"] == metadata["version"], "MCP/package version mismatch")
        require(initialized["protocolVersion"] == PROTOCOL, "unexpected negotiated protocol")
        stdio.send({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
        before = list(owner.requests)
        denied = tool_value(stdio.request(2, "tools/call", {"name": "forget", "arguments": {
            "db": "fixture", "id": NODE}}), is_error=True)
        require("write grant denied" in denied["result"], f"wrong MCP write refusal: {denied!r}")
        require(owner.requests == before, "MCP denied write contacted owner")
        listed = stdio.request(3, "tools/list", {})
        names = {tool["name"] for tool in listed["tools"]}
        require({"get", "databases", "config_select"} <= names and "forget" not in names,
                f"wrong configured catalog: {names!r}")
        require(not listed["omissions"], f"catalog omitted owner: {listed!r}")
        result = tool_value(stdio.request(4, "tools/call", {"name": "get", "arguments": {
            "db": "fixture", "id": NODE}}))
        require(result["db"] == "fixture" and result["result"]["id"] == NODE
                and result["result"]["db_id"] == DB, "wrong MCP get result")
        stdio.finish()
    require(owner.calls.count("get") == 2 and "forget" not in owner.calls,
            f"unexpected owner operations: {owner.calls!r}")

    unsupported = run(command + ["--http", "127.0.0.1:0"], env, root, success=False)
    require("--http needs the `http` feature" in unsupported.stderr, "HTTP feature boundary absent")
    local = root / "local"
    local.mkdir()
    unsupported = run([cli, "--json", "--db", str(local / "store.db"), "capture", "init"],
                      env, local, success=False)
    require("cozo-enabled" in unsupported.stderr, "local database backend refusal absent")
    require(not list(local.iterdir()), "client package created local store state")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("prefix", type=Path, help="installed package containing bin/mnemed and bin/mneme-mcp")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="mneme-nix-smoke-") as scratch, Owner() as owner:
        smoke(args.prefix.resolve(), Path(scratch).resolve(), owner)
    print("client smoke passed: symlink config, CLI/MCP read routing and denial, feature boundaries")


if __name__ == "__main__":
    main()

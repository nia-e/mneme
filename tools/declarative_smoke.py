#!/usr/bin/env python3
"""Finite, provider-free smoke of copied declarative-config artifacts.

No live owner/config/service is touched. Two disposable owners, a connection trap,
and copied native/Python artifacts establish only the checks listed in the receipt.
An explicit build description and exclusive-create output keep evidence immutable.
This is not Windows qualification, provider quality, or a live-migration receipt.
"""
from __future__ import annotations

import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import math
import os
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parents[1]
HELPERS = REPO / "integrations/codex"
sys.path.insert(0, str(HELPERS))
import host_process  # noqa: E402

MAX_OUTPUT = 1024 * 1024
PROTOCOL = "2025-11-25"
FAKE_IDS = ["01ARZ3NDEKTSV4RRFFQ69G5FAV", "01ARZ3NDEKTSV4RRFFQ69G5FAW"]

# Real lifecycle admits no work for an empty session. A separate, explicitly
# seeded ended-reader marker forces the real detached launch path without model
# evidence; the worker must enter serve(), release its locks, and exit immediately.
BACKGROUND_LIFECYCLE = r'''
import hashlib,json,os,sys,time
from pathlib import Path
sys.path.insert(0,sys.argv[1])
import host_files,host_process,hook_launcher,hooks,reader_worker
config_path,cwd,marker=Path(sys.argv[2]),sys.argv[3],Path(sys.argv[4])
session='declarative-bg-lifecycle'
start={'hook_event_name':'SessionStart','source':'startup','cwd':cwd,'session_id':session}
end={'hook_event_name':'SessionEnd','reason':'exit','cwd':cwd,'session_id':session}
result=hook_launcher.handle_event(start,config_path)
assert result.get('hookSpecificOutput'), 'background-enabled SessionStart failed'
config=hooks._config(config_path,cwd=cwd)
path,_=reader_worker._paths(config,session)
hook_launcher.handle_event(end,config_path)
data=json.loads(path.read_bytes())
assert data['ended'] and data['worker_pid'] is None, 'empty lifecycle created work'
assert not marker.exists(), 'provider invoked by empty lifecycle'
def seed(data):
    token=hashlib.sha256(b'').hexdigest()
    ledger=data['_accounting']
    packet,_=ledger.bind('user_message','fixture',token)
    # Synthetic fixture usage, not a provider invocation. The JSON totals are
    # read-only mirrors of this canonical reservation and settlement.
    assert ledger.reserve(packet,'smoke:ended-fixture',{'fixture':'ended-marker'},reader_worker.resolve(config)), 'fixture reservation refused'
    assert ledger.settle('smoke:ended-fixture',{'provider_attempt':True,'usage':{'input_tokens':17,'output_tokens':3}}), 'fixture settlement failed'
    data['active']={'turn':'fixture','request':token,'epoch':data['epoch'],'cue':'','closed':False}
    data['pending']={'turn':'fixture','request':token,'epoch':data['epoch']}
    return None,True
assert reader_worker._state(config,session,seed)[0], 'could not seed ended marker'
spawn=host_process.spawn
workers=[]
def capture(argv,**kwargs):
    child=spawn(argv,**kwargs)
    if '--serve' in argv and any(str(value).endswith('reader_worker.py') for value in argv):
        workers.append(child)
    return child
host_process.spawn=capture
try:
    launched=reader_worker.background(config,end)
    assert launched['outcome']=='launched' and len(workers)==1, 'v13 detached launch did not occur: '+str(launched)
    workers[0].wait(timeout=5)
    assert workers[0].returncode==0, 'background worker failed'
finally:
    host_process.spawn=spawn
    for worker in workers:
        if worker.poll() is None:
            host_process.terminate_tree(worker,timeout=1)
data=json.loads(path.read_bytes())
assert data['worker_pid'] is None and data['worker_identity'] is None, 'worker ownership not released'
locks=[path.with_suffix('.worker.lock'),*path.parent.glob('slot-*.lock')]
assert len(locks)>=2 and all(lock.is_file() for lock in locks), 'worker never entered lease/slot lifecycle'
for lock in locks:
    fd=host_files.open_regular(lock,os.O_RDWR)
    try: host_files.lock_file(fd,blocking=False)
    finally: os.close(fd)
before=[data[key] for key in ('attempts','input_tokens','output_tokens')]
assert before==[1,17,3], 'ended worker reset accounting'
hook_launcher.handle_event({**start,'source':'resume'},config_path)
hook_launcher.handle_event(end,config_path)
data=json.loads(path.read_bytes())
assert [data[key] for key in ('attempts','input_tokens','output_tokens')]==before, 'resume reset accounting'
assert not marker.exists(), 'provider unexpectedly invoked'
print(json.dumps({'empty_lifecycle':'no_work','seeded_ended_marker_launch':launched['outcome'],
    'worker_exit':workers[0].returncode,'released_locks':len(locks),'retained_accounting':before,'provider_invocations':0}))
'''


def need(condition, message):
    if not condition:
        raise RuntimeError(message)


def encoded(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False, allow_nan=False).encode()


def sha(value):
    return hashlib.sha256(value).hexdigest()


def artifact(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return {"path": str(path), "bytes": path.stat().st_size, "sha256": digest.hexdigest()}


def source_state():
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=REPO, timeout=10).decode().strip()
    # Includes edited and untracked implementation files, not ignored receipts,
    # target build products, credentials, or arbitrary memory bodies.
    names = subprocess.check_output(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=REPO, timeout=10).split(b"\0")
    files = [{"path": os.fsdecode(name), "sha256": artifact(REPO / os.fsdecode(name))["sha256"]}
             for name in sorted(set(names)) if name and (REPO / os.fsdecode(name)).is_file()]
    status = subprocess.check_output(["git", "status", "--porcelain"], cwd=REPO, timeout=10)
    return {"head": head, "dirty": bool(status), "status_sha256": sha(status),
            "tracked_and_untracked_file_digest": sha(encoded(files)), "files": files}


def isolated_env(root, cli):
    env = {key: value for key, value in os.environ.items()
           if not key.upper().startswith(("MNEME_", "XDG_", "HF_", "HUGGINGFACE_", "CODEX_", "OPENAI_", "ANTHROPIC_"))
           and not any(word in key.upper() for word in ("API_KEY", "TOKEN", "SECRET", "PASSWORD"))}
    for key in ("HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "XDG_DATA_HOME", "XDG_CONFIG_HOME",
                "XDG_CACHE_HOME", "XDG_STATE_HOME", "CODEX_HOME", "HF_HOME", "TEMP", "TMP", "TMPDIR"):
        directory = root / key.lower()
        directory.mkdir()
        env[key] = str(directory)
    env.update(MNEME_CLIENT_BINARY=str(cli), MNEME_RERANK="0", HF_HUB_OFFLINE="1",
               TRANSFORMERS_OFFLINE="1", HF_ENDPOINT="http://127.0.0.1:9", PYTHONUTF8="1",
               PYTHONDONTWRITEBYTECODE="1", GIT_CEILING_DIRECTORIES=str(root))
    for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
        env[key] = env[key.lower()] = "http://127.0.0.1:9"
    env["NO_PROXY"] = env["no_proxy"] = "127.0.0.1,localhost,::1"
    return env


class Trap:
    """Counts every accepted TCP contact; no HTTP owner is impersonated."""
    def __init__(self):
        self.socket = socket.socket()
        self.socket.bind(("127.0.0.1", 0))
        self.socket.listen()
        self.socket.settimeout(.05)
        self.count = 0
        self.closed = False
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    @property
    def url(self):
        return "http://127.0.0.1:" + str(self.socket.getsockname()[1])

    def run(self):
        while not self.closed:
            try:
                connection, _ = self.socket.accept()
                self.count += 1
                connection.close()
            except socket.timeout:
                pass
            except OSError:
                break

    def close(self):
        self.closed = True
        self.socket.close()
        self.thread.join(timeout=1)


class RefusalHTTP:
    """Local protocol/auth refusal fixture; never forwards any request."""
    def __init__(self, status):
        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(b'"not an MCP response"')
            def log_message(self, *_):
                pass
        self.server = HTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, kwargs={"poll_interval": .05}, daemon=True)
        self.thread.start()

    @property
    def url(self):
        return "http://127.0.0.1:" + str(self.server.server_address[1])

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=1)


class Smoke:
    def __init__(self, options, root, receipt):
        self.options, self.root, self.receipt = options, root, receipt
        self.deadline = time.monotonic() + options.timeout
        self.children, self.clients = [], []
        self.guides = {}
        self.project_store = False
        prefix = root / "installed"
        self.bin, self.lib = prefix / "bin", prefix / "lib"
        self.bin.mkdir(parents=True)
        self.lib.mkdir()
        suffix = ".exe" if os.name == "nt" else ""
        self.cli, self.mcp = self.bin / ("mnemed" + suffix), self.bin / ("mneme-mcp" + suffix)
        for source, target in ((options.cli, self.cli), (options.mcp, self.mcp)):
            shutil.copy2(source, target)
            need(artifact(source)["sha256"] == artifact(target)["sha256"], "copied binary mismatch")
        receipt["helper_sources"] = []
        for source in sorted(HELPERS.glob("*.py")):
            if not source.name.startswith("test_"):
                shutil.copy2(source, self.lib / source.name)
                original = artifact(source)
                need(original["sha256"] == artifact(self.lib / source.name)["sha256"], "copied helper mismatch")
                receipt["helper_sources"].append(original)
        receipt["copied_artifacts"] = [artifact(path) for path in sorted(prefix.rglob("*")) if path.is_file()]
        self.env = isolated_env(root, self.cli)
        self.cwd = root / "workspace"
        self.cwd.mkdir()
        self.config = root / "memory.toml"
        self.trap = Trap()

    def remaining(self, maximum=20):
        value = min(maximum, self.deadline - time.monotonic())
        need(value > 0, "aggregate smoke deadline exceeded")
        return value

    def run_command(self, label, args, payload=None, ok=True, *, decode=True, raw_input=None):
        child = host_process.spawn(list(map(str, args)), cwd=self.cwd, env=self.env, tree=True,
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.children.append(child)
        pipe = host_process.BoundedPipe(child, max_read_bytes=16384)
        deadline = time.monotonic() + self.remaining()
        try:
            need(payload is None or raw_input is None, "command has conflicting input forms")
            raw = raw_input if raw_input is not None else encoded(payload) if payload is not None else b""
            need(len(raw) <= 128 * 1024, "request too large")
            pipe.write(raw, deadline)
            child.stdin.close()
            output = bytearray()
            while True:
                chunk = pipe.read(min(16384, MAX_OUTPUT + 1 - len(output)), deadline)
                if not chunk:
                    break
                output.extend(chunk)
                need(len(output) <= MAX_OUTPUT, "command output too large")
            child.wait(timeout=max(.001, deadline - time.monotonic()))
            self.receipt["commands"].append({"label": label, "argv_sha256": sha(encoded(list(map(str, args)))),
                "exit": child.returncode, "output_bytes": len(output), "output_sha256": sha(output)})
            need((child.returncode == 0) == ok, "unexpected command result: " + label)
            return json.loads(output) if ok and decode else bytes(output)
        finally:
            pipe.close()
            host_process.terminate_tree(child, timeout=.2)

    def select(self, *, cwd=None):
        return self.run_command("native.select", [self.cli, "--json", "config", "select", "--config", self.config, "--cwd", cwd or self.cwd])

    def call(self, selection, name, operation, tool, arguments, *, ok=True, cwd=None):
        value = self.run_command("native." + name + "." + operation + "." + tool,
            [self.cli, "--json", "config", "call", "--config", self.config, "--cwd", cwd or self.cwd,
             "--store", name, "--operation", operation, "--expected-selection-digest", selection["digest"],
             "--timeout-ms", "3000"], {"tool": tool, "arguments": arguments})
        need(value.get("ok") is ok and value.get("selection_digest") == selection["digest"] and value.get("store") == name,
             "unexpected native bridge envelope: " + name + "." + tool + "; " + value.get("error", {}).get("message", ""))
        return value

    def mcp_selection(self, expected, *, denied=False):
        child = host_process.spawn([str(self.mcp), "--memory-config", str(self.config), "--cwd", str(self.cwd)],
            cwd=self.cwd, env=self.env, tree=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.children.append(child)
        pipe = host_process.BoundedPipe(child, max_read_bytes=16384)
        buffer, request_id = bytearray(), 0
        def rpc(method, params, *, raw=False):
            nonlocal request_id, buffer
            request_id += 1
            deadline = time.monotonic() + self.remaining()
            pipe.write(encoded({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}) + b"\n", deadline)
            while b"\n" not in buffer:
                chunk = pipe.read(min(16384, MAX_OUTPUT + 1 - len(buffer)), deadline)
                need(bool(chunk), "declarative MCP EOF before response")
                buffer.extend(chunk)
                need(len(buffer) <= MAX_OUTPUT, "MCP response exceeds bound")
            line, _, rest = buffer.partition(b"\n")
            buffer = bytearray(rest)
            value = json.loads(line)
            need(value.get("id") == request_id and (raw or "error" not in value), "declarative MCP protocol refusal")
            self.receipt["commands"].append({"label": "mcp." + method, "response_bytes": len(line), "response_sha256": sha(line)})
            return value if raw else value["result"]
        try:
            init = rpc("initialize", {"protocolVersion": PROTOCOL, "capabilities": {},
                "clientInfo": {"name": "declarative-smoke", "version": "1"}})
            self.receipt.setdefault("mcp_initializations", []).append({"transport": "stdio", "protocol": init["protocolVersion"],
                                                                       "server": init["serverInfo"]})
            pipe.write(encoded({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}) + b"\n",
                       time.monotonic() + self.remaining())
            selected = rpc("tools/call", {"name": "config_select", "arguments": {}})
            need(selected.get("isError") is False and json.loads(selected["content"][0]["text"]) == expected,
                 "MCP/native/Python config selection differs")
            if denied:
                for name, tool, args in (("deniedread", "core", {}), ("deniedwrite", "save", {
                    "kind": "note", "summary": "must not contact", "operation_id": "mcp-denied"})):
                    value = rpc("tools/call", {"name": tool, "arguments": {"db": name, **args}})
                    need(value.get("isError") is True, "MCP configured denial did not refuse")
                original = self.config.read_bytes()
                try:
                    self.config.write_bytes(original.replace(b"core = true", b"core = false"))
                    stale = rpc("tools/call", {"name": "core", "arguments": {"db": "deniedwrite"}}, raw=True)
                    need("error" in stale or stale.get("result", {}).get("isError") is True,
                         "long-lived MCP reused stale selection")
                finally:
                    self.config.write_bytes(original)
            else:
                catalog = rpc("tools/list", {})
                need({"config_select", "databases", "get", "save"} <= {tool["name"] for tool in catalog["tools"]},
                     "MCP lazy catalog lost reachable operations")
            child.stdin.close()
            while pipe.read(16384, time.monotonic() + self.remaining(10)):
                raise RuntimeError("MCP emitted unexpected shutdown output")
            child.wait(timeout=self.remaining(10))
            need(child.returncode == 0, "MCP did not gracefully shut down on EOF")
        finally:
            pipe.close()
            host_process.terminate_tree(child, timeout=.2)
        self.receipt["checks"].append("mcp_stdio_native_selection_parity" + ("_denied_no_contact" if denied else "_lazy_optional_owner_catalog"))
        return catalog["tools"] if not denied else None

    def write_config(self, entries, *, background=False, cache=None):
        lines = ['schema = "mneme.config.v1"', '[background]', 'enabled = ' + str(background).lower()]
        for name, ident, url, read, write in entries:
            lines.extend(["[stores." + name + "]", "id = " + json.dumps(ident),
                'owner = { url = ' + json.dumps(url) + ', db = "project" }',
                ('active = { workspace = "project", roots = [' + json.dumps(str(self.cwd)) + '] }'
                 if name == "first" and self.project_store else 'active = { workspace = "any" }'),
                'permissions = { read = ' + str(read).lower() + ', write = ' + str(write).lower() + ' }',
                'delivery = { core = true, context = true }',
                'background = { recall = true, record = true, maintenance = true }'])
            if name in self.guides:
                guide = json.dumps(self.guides[name])
                lines.append('guidance = { tag_guide = { id = ' + guide + ', content = "summary" } }')
            if cache is not None and name == "first":
                lines.append('cache = { library = "smokecache" }')
        if cache is not None:
            lines.extend(['[libraries.smokecache]', 'catalog = ' + json.dumps(str(cache["catalog"])),
                          'replicas.mirror = { url = ' + json.dumps(cache["url"]) + ' }'])
        self.config.write_text("\n".join(lines) + "\n")

    def metadata_checks(self):
        self.write_config([("deniedread", FAKE_IDS[0], self.trap.url, False, True),
                           ("deniedwrite", FAKE_IDS[1], self.trap.url, True, False)])
        selected = self.select()
        copied_python = "import json,sys;sys.path.insert(0,sys.argv[1]);from store_registry import native_select;print(json.dumps(native_select(sys.argv[2],sys.argv[3],sys.argv[4]).snapshot))"
        python = self.run_command("copied.python.select", [sys.executable, "-B", "-c", copied_python,
                                 self.lib, self.config, self.cli, self.cwd])
        need(selected == python, "Python and native selection differ")
        self.mcp_selection(selected, denied=True)
        flags = {row["name"]: row["eligible"] for row in selected["active"]}
        need(flags["deniedwrite"]["core"] and flags["deniedwrite"]["read"], "background off disabled manual/core reads")
        need(all(not row[key] for row in flags.values() for key in ("recall", "record", "maintenance")), "background off retained automatic work")
        denied_read = self.call(selected, "deniedread", "explicit", "core", {}, ok=False)
        denied_write = self.call(selected, "deniedwrite", "explicit", "save", {"kind": "note", "summary": "must not be sent",
                  "tags": [], "operation_id": "declarative-smoke-denied"}, ok=False)
        need(all("not granted" in value["error"]["message"] and value["work"]["decoded_bytes"] == 0
                 for value in (denied_read, denied_write)), "denied route did not refuse at configured grant boundary")
        # A stale digest must refuse before even the readable route is contacted.
        self.config.write_text(self.config.read_text().replace("core = true", "core = false"))
        stale = self.call(selected, "deniedwrite", "explicit", "core", {}, ok=False)
        need("digest changed" in stale["error"]["message"] and stale["work"]["decoded_bytes"] == 0,
             "stale call did not refuse at selection boundary")
        self.nested_operation_scope_checks()
        for invalid in ('schema = "mneme.config.v1"\nunknown = true\n',
                        'schema = "mneme.config.v1"\nschema = "mneme.config.v1"\n',
                        'schema = "mneme.config.v1"\n[background]\nenabled = "yes"\n'):
            self.config.write_text(invalid)
            self.run_command("malformed.select", [self.cli, "--json", "config", "select", "--config", self.config, "--cwd", self.cwd], ok=False)
        time.sleep(.1)  # Let the accepting thread observe any queued contact.
        self.trap.close()
        need(self.trap.count == 0, "selection/denied/stale route contacted owner")
        self.receipt["checks"].extend(["copied_python_native_selection_exact_parity", "background_off_retains_core_manual_read_grants",
            "read_write_denial_stale_selection_and_malformed_config_make_no_contact"])

    def nested_operation_scope_checks(self):
        # Preserve the existing migration boundary, not a new blanket prohibition
        # on explicit global/core access. Only positively scoped automatic lanes
        # stop at the separately configured child project.
        self.write_config([("continuity", FAKE_IDS[0], self.trap.url, True, True)], background=True)
        scoped = self.config.read_text() + "\n".join(
            "applicability." + operation + " = { roots = [" + json.dumps(str(self.cwd)) + "] }"
            for operation in ("context", "recall", "record", "maintenance")) + "\n"
        self.config.write_text(scoped)
        parent = self.select()["active"][0]["eligible"]
        need(all(parent.values()), "parent-scoped automatic fixture lost its admitted grants")
        nested = self.cwd / "separate-project"
        (nested / ".mneme").mkdir(parents=True)
        (nested / ".mneme/memory.toml").write_text('schema = "mneme.config.v1"\n')
        selected = self.select(cwd=nested)
        eligible = selected["active"][0]["eligible"]
        need(all(eligible[key] for key in ("read", "write", "core")) and
             all(not eligible[key] for key in ("context", "recall", "record", "maintenance")),
             "nested project escaped positive operation roots or narrowed broad core/manual grants")
        for operation in ("context", "recall", "record", "maintenance"):
            denied = self.call(selected, "continuity", operation, "core", {}, ok=False, cwd=nested)
            need("not granted" in denied["error"]["message"] and denied["work"]["decoded_bytes"] == 0,
                 "nested operation denial did not occur before native work")
        self.receipt["nested_operation_applicability"] = {"parent": parent, "nested": eligible,
                                                        "native_contact": "none; checked by shared TCP trap"}

    def close(self):
        self.trap.close()
        for child in reversed(self.children):
            host_process.terminate_tree(child, timeout=1)
        self.receipt["cleanup"] = {"owned_children_exited": all(child.poll() is not None for child in self.children)}

    def run(self):
        self.receipt["cli_version"] = self.run_command("copied.cli.version", [self.cli, "--version"], decode=False).decode().strip()
        self.run_command("copied.cli.config.help", [self.cli, "config", "--help"], decode=False)
        self.metadata_checks()
        self.owner_checks()

    def owner_checks(self):
        # Native reference and persistent builds both support explicit init.
        # No installer enrollment or production service manager is involved.
        owners = []
        for name in ("first", "second"):
            db = self.root / (name + ".db")
            self.run_command(name + ".initialize", [self.cli, "--json", "--db", db, "capture", "init"])
            owner, url = self.start_http(["project=" + str(db)], "operator")
            # Use the copied legitimate native transport bridge, not raw HTTP.
            script = "import json,sys;sys.path.insert(0,sys.argv[1]);from mcp_client import McpClient; c=McpClient(sys.argv[2],timeout=3);print(json.dumps(c.call_tool('databases',{})));c.close()"
            catalog = self.run_command(name + ".catalog", [sys.executable, "-B", "-c", script, self.lib,
                                                       url])
            need(len(catalog) == 1 and catalog[0]["name"] == "project", "wrong disposable owner alias")
            owners.append({"name": name, "id": catalog[0]["db_id"], "url": url,
                           "process": owner, "db": db})
        need(owners[0]["id"] != owners[1]["id"], "disposable owners share an identity")
        self.write_config([(row["name"], row["id"], row["url"], True, True) for row in owners] +
                          [("optionaldown", FAKE_IDS[0], "http://127.0.0.1:9", True, False)])
        selected = self.select()
        self.mcp_selection(selected)
        stores_view = self.run_command("native.stores.shared_tui_selection", [self.cli, "--memory-config", self.config, "--json", "stores"])
        need(stores_view == selected, "stores/TUI shared selector differs from config select")
        for row in owners:
            saved = self.call(selected, row["name"], "explicit", "save", {"kind": "note", "summary": "Disposable declarative violet lantern",
                "body": "Authored fixture café 日本語.", "tags": ["core"], "operation_id": "declarative-smoke-shared-local-id"})["result"]
            row["node"] = saved["id"]
            got = self.call(selected, row["name"], "explicit", "get", {"id": row["node"], "body": True,
                             "max_body_bytes": 128})["result"]
            need(got["db_id"] == row["id"] and got["id"] == row["node"] and got["body"] == "Authored fixture café 日本語.",
                 "routed manual read lost owner/content identity")
            core = self.call(selected, row["name"], "core", "core", {"max_body_bytes": 128})["result"]
            need(row["node"] in json.dumps(core), "background-off core omitted authored core")
            self.call(selected, row["name"], "record", "save", {"kind": "note", "summary": "background denied",
                      "tags": [], "operation_id": "declarative-background-denied"}, ok=False)
        need(owners[0]["node"] == owners[1]["node"], "same source key did not produce the intended cross-owner ID collision fixture")
        self.guides = {row["name"]: row["node"] for row in owners}
        self.write_config([(row["name"], row["id"], row["url"], True, True) for row in owners] +
                          [("optionaldown", FAKE_IDS[0], "http://127.0.0.1:9", True, False)])
        selected = self.select()
        repl = self.run_command("configured.repl.retained_session.look_abort",
            [self.cli, "--memory-config", self.config, "--store", "first", "--json", "repl", owners[0]["node"]],
            decode=False, raw_input=b"look\nabort\n")
        views = [json.loads(line) for line in repl.splitlines()]
        need(len(views) == 3 and views[-1].get("reflected") is None and owners[0]["node"] in json.dumps(views),
             "configured REPL lost retained session or implicitly reflected aborted browsing")
        self.receipt["checks"].append("configured_repl_retained_native_walk_look_abort_without_learning")
        hook_config = self.root / "hook.json"
        self.hook_adapter(self.root / "ledger", self.cli, hook_config)
        time.sleep(1.05)  # Native CORE shares the existing cold-work interval.
        event = {"hook_event_name": "SessionStart", "source": "startup", "cwd": str(self.cwd), "session_id": "declarative-smoke"}
        startup = self.run_command("copied.v13.core_hook.background_off", [sys.executable, "-B", self.lib / "core_hook.py",
                                   "--hook-config", hook_config], event)
        context = startup["hookSpecificOutput"]["additionalContext"]
        sections = [json.loads(line) for line in context.splitlines() if line.startswith("{")]
        scopes = {section["scope"]: section for section in sections}
        self.receipt["core_outcomes"] = [{"scope": section["scope"], "outcome": section["outcome"]} for section in sections]
        for row in owners:
            need(scopes[row["name"]]["outcome"] == "ok" and row["node"] in json.dumps(scopes[row["name"]]),
                 "v13 passive core lost readable owner")
            need("guidance" not in scopes[row["name"]], "background instruction/tag guidance leaked into startup core")
        need(scopes["optionaldown"]["outcome"] != "ok", "optional outage became successful empty core")
        self.receipt["checks"].append("copied_v13_startup_core_background_off_two_owners_and_outage")
        self.call(selected, "optionaldown", "explicit", "core", {}, ok=False)
        # Optional outage does not alter activation or poison a reachable owner.
        need(self.select() == selected, "owner outage changed metadata activation")
        self.call(selected, "first", "explicit", "get", {"id": owners[0]["node"]})
        self.receipt["owner_ids"] = [row["id"] for row in owners]
        self.receipt["checks"].extend(["native_stores_shared_tui_selector_exact_parity", "two_disposable_owner_ids_preserved",
            "same_store_local_node_id_keeps_two_distinct_owner_qualified_core_sections",
            "background_off_manual_save_get_and_core_work_record_refuses", "optional_owner_outage_preserves_selection_and_other_owner"])
        self.configured_http_checks(owners)
        self.background_checks(owners)
        self.inactive_explicit_check(owners)
        self.cache_checks(owners)
        for row in owners:
            host_process.terminate_tree(row["process"], timeout=3)
            self.run_command(row["name"] + ".offline_reopen_after_shutdown",
                             [self.cli, "--json", "--db", row["db"], "get", row["node"]])
        self.receipt["checks"].append("disposable_http_owner_shutdown_releases_leases")

    def background_checks(self, owners):
        self.project_store = True
        focus_dir = self.cwd / ".mneme"
        focus_dir.mkdir()
        focus = focus_dir / "hippocampus.md"
        focus.write_text("# Capture instructions\n\nKeep source-bound violet lantern decisions. Café 日本語.\n")
        self.write_config([(row["name"], row["id"], row["url"], True, True) for row in owners], background=True)
        self.config.write_text(self.config.read_text().replace("maintenance = true", "maintenance = false"))
        marker = self.root / "unexpected-provider-invocation"
        fake = self.root / "refuse-provider"
        fake.write_text("#!" + sys.executable + "\nfrom pathlib import Path\nPath(" + repr(str(marker)) + ").write_text('invoked')\nraise SystemExit(97)\n")
        fake.chmod(0o755)
        hook = self.root / "background-hook.json"
        self.hook_adapter(self.root / "background-ledger", fake, hook)
        self.record_dependency_checks(owners, hook, focus)
        time.sleep(1.05)
        core = self.run_command("copied.v13.core_excludes_existing_background_instruction_file",
            [sys.executable, "-B", self.lib / "core_hook.py", "--hook-config", hook],
            {"hook_event_name": "SessionStart", "source": "startup", "cwd": str(self.cwd), "session_id": "file-focus-core"})
        context = core["hookSpecificOutput"]["additionalContext"]
        need(focus.read_text() not in context and "Capture instructions" not in context,
             "background hippocampus file became startup foreground context")
        self.receipt["checks"].append("existing_background_instruction_file_not_rendered_at_startup")
        diagnostic = "import json\ntry:\n exec(" + repr(BACKGROUND_LIFECYCLE) + ")\nexcept Exception as error:\n print(json.dumps({'error':str(error)}))\n"
        result = self.run_command("copied.v13.background_empty_and_seeded_ended_worker_lifecycle",
            [sys.executable, "-B", "-c", diagnostic, self.lib, hook, self.cwd, marker])
        need("error" not in result, "background lifecycle: " + result.get("error", ""))
        need(result["provider_invocations"] == 0 and not marker.exists(), "background lifecycle invoked provider")
        self.receipt["background_lifecycle"] = result
        self.receipt["checks"].extend(["copied_v13_background_enabled_empty_session_no_work_or_provider",
            "seeded_ended_reader_real_detached_launch_exit_releases_slot_and_keeps_accounting"])

    def record_dependency_checks(self, owners, hook, focus):
        script = r'''
import json,sys,time
from pathlib import Path
sys.path.insert(0,sys.argv[1])
from hooks import _config
from registry_runtime import selection
from target_policy import TargetPolicy
from guidance import GuidanceOwner,collect_guidance,revalidate_guidance
config=_config(Path(sys.argv[2]),cwd=sys.argv[3]);selected=selection(config)
remaining=128*1024;deadline=time.monotonic()+10;rows=[]
for store in selected.recordable:
    policy=TargetPolicy.from_selected(store,selected)
    with GuidanceOwner(policy,store.service(),store.db_id,deadline,remaining,operation='record') as owner:
        frozen=collect_guidance(owner,store.guidance,owner_binding=policy.canonical(),roles=('tag_guide',))
        assert all(row['outcome']=='ok' for row in frozen.roles.values()), 'record dependency guidance omitted: '+store.name
        revalidate_guidance(frozen,owner,store.guidance,owner_binding=policy.canonical())
        remaining=owner.remaining_bytes
        rows.append({'store':store.name,'db_id':store.db_id,'roles':{role:row['outcome'] for role,row in frozen.roles.items()}})
assert len(rows)==2 and 0<128*1024-remaining<=128*1024, 'record dependencies lost shared accounting'
import fixture_source_turn as fixture,turn_observer,registry_recording,recording_contract
from librarian_policy import resolve
focus=Path(sys.argv[4]);text=focus.read_text()
transcript=focus.parent/'synthetic-turn.jsonl';transcript.write_bytes(fixture.encoded(fixture.complete_turn()))
admission=turn_observer.admit_source_turn(transcript,sessions_root=focus.parent,session_id=fixture.SESSION,
    turn_id=fixture.TURN,expected_prompt_sha256=fixture.sha(fixture.PROMPT.encode()))
assert admission['status']=='admitted', 'synthetic turn not source-admitted'
observed=turn_observer.observe_source_turn(admission['admission'])
budget=resolve(config)
acquired=registry_recording.collect(config,observed,budget=budget,seconds=10)
options={store['name']:store for store in acquired.scope_options['selected_stores']}
assert set(options)=={'first','second'}, 'record acquisition omitted authorized destination'
assert options['first']['project_focus']==text and options['second']['project_focus'] is None, 'file instructions lost exact project scope'
packet,context=recording_contract.prepare(observed,acquired.cards,budget=budget,**acquired.scope_options)
assert packet is not None, 'recording context refused: '+str(context)
contexts={store.name:store for store in context.selected_stores}
assert contexts['first'].project_focus==text and contexts['second'].project_focus is None, 'recording packet lost exact file instructions'
_,_,frozen=registry_recording.freeze_destination(acquired,'first',seconds=5)
assert frozen['project_focus_witness']['root']==str(focus.parent.parent), 'file focus witness wrong root'
assert text not in [binding.text for binding in context.bindings], 'file instructions became turn evidence'
print(json.dumps({'owners':rows,'decoded_native_bytes':128*1024-remaining,'allowance':128*1024,
    'acquisition_native_bytes':acquired.decoded_bytes,'acquisition_allowance':budget.native_read_bytes,
    'project_focus_sha256':__import__('hashlib').sha256(text.encode()).hexdigest(),'project_focus_scopes':['first'],
    'provider_invocations':0}))
'''
        result = self.run_command("copied.v13.two_owner_record_guidance_dependencies_shared_budget",
                                 [sys.executable, "-B", "-c", script, self.lib, hook, self.cwd, focus])
        need({row["db_id"] for row in result["owners"]} == {row["id"] for row in owners}, "record dependencies crossed owners")
        self.receipt["record_dependency_reads"] = result
        self.receipt["checks"].append("two_owner_record_tag_guidance_frozen_and_revalidated_under_shared_128k_budget_no_provider")
        need(result["project_focus_sha256"] == sha(focus.read_bytes()), "background instructions changed during acquisition")
        self.receipt["checks"].append("existing_hippocampus_file_exact_project_only_background_context_not_turn_evidence")

    def inactive_explicit_check(self, owners):
        original = self.config.read_bytes()
        try:
            self.config.write_bytes(original.replace(b'active = { workspace = "any" }', b'active = { workspace = "no-project" }'))
            selected = self.select()
            need("second" not in {row["name"] for row in selected["active"]}, "explicit-access fixture remained automatically active")
            row = next(row for row in selected["stores"] if row["name"] == "second")
            need(row["admitted"] and row["eligible"]["read"], "inactive configured inventory lost deliberate read grant")
            value = self.call(selected, "second", "explicit", "get", {"id": owners[1]["node"]})["result"]
            need(value["db_id"] == owners[1]["id"], "inactive deliberate read crossed owner")
            self.call(selected, "second", "core", "core", {}, ok=False)
        finally:
            self.config.write_bytes(original)
        self.receipt["checks"].append("inactive_admitted_store_explicit_read_does_not_enable_automatic_core")

    def hook_adapter(self, state, reader, destination):
        script = "import json,sys;sys.path.insert(0,sys.argv[1]);from registry_config import prepare_native;p=prepare_native(memory_config=sys.argv[2],native_cli=sys.argv[3],cwd=sys.argv[4],state_dir=sys.argv[5],reader_codex=sys.argv[6],hooks_program=sys.argv[7],hook_config_path=sys.argv[8]);print(json.dumps({'config':p['hook_config'],'selection':p['selection']}))"
        prepared = self.run_command("copied.v13.prepare_native", [sys.executable, "-B", "-c", script, self.lib,
            self.config, self.cli, self.cwd, state, reader, self.lib / "hook_launcher.py", destination])
        need(prepared["selection"] == self.select(), "generated hook adapter changed native selection")
        destination.write_bytes(encoded(prepared["config"]))

    def start_http(self, databases, profile):
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        args = [str(self.mcp), "--http", "127.0.0.1:" + str(port)]
        if profile is not None:
            args.extend(["--capability-profile", profile])
        if databases is None:
            args.extend(["--memory-config", str(self.config), "--cwd", str(self.cwd)])
        for database in databases or []:
            args.extend(["--db", database])
        child = host_process.spawn(args, cwd=self.cwd, env=self.env, tree=True, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.children.append(child)
        deadline = time.monotonic() + self.remaining(10)
        while True:
            need(child.poll() is None, "HTTP owner exited before ready")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.05):
                    return child, "http://127.0.0.1:" + str(port)
            except OSError:
                need(time.monotonic() < deadline, "HTTP owner readiness timed out")
                time.sleep(.025)

    def configured_http_checks(self, owners):
        time.sleep(1.05)  # Do not mistake a prior passive CORE's cooldown for routing failure.
        original = self.config.read_bytes()
        self.write_config([(row["name"], row["id"], row["url"], True, row["name"] == "first") for row in owners] +
                          [("optionaldown", FAKE_IDS[0], "http://127.0.0.1:9", True, False)])
        selected = self.select()
        stdio_catalog = self.mcp_selection(selected)
        child, url = self.start_http(None, None)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        session, request_id = None, 0
        def rpc(method, params):
            nonlocal session, request_id
            request_id += 1
            headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
            if session:
                headers.update({"mcp-session-id": session, "mcp-protocol-version": PROTOCOL})
            payload = encoded({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
            request = urllib.request.Request(url, data=payload, headers=headers, method="POST")
            with opener.open(request, timeout=self.remaining()) as response:
                session = response.headers.get("mcp-session-id") or session
                output = response.read(MAX_OUTPUT + 1)
            need(len(output) <= MAX_OUTPUT, "configured HTTP response too large")
            value = json.loads(output)
            need(value.get("id") == request_id and "error" not in value, "configured HTTP protocol refusal")
            self.receipt["commands"].append({"label": "configured.http." + method,
                "response_bytes": len(output), "response_sha256": sha(output)})
            return value["result"]
        try:
            init = rpc("initialize", {"protocolVersion": PROTOCOL, "capabilities": {},
                "clientInfo": {"name": "declarative-http-smoke", "version": "1"}})
            need(session is not None and init["selectionDigest"] == selected["digest"], "configured HTTP initialization differs")
            self.receipt.setdefault("mcp_initializations", []).append({"transport": "http", "protocol": init["protocolVersion"],
                                                                       "server": init["serverInfo"]})
            notification = urllib.request.Request(url,
                data=encoded({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
                headers={"Content-Type": "application/json", "Accept": "application/json, text/event-stream",
                         "mcp-session-id": session, "mcp-protocol-version": PROTOCOL}, method="POST")
            with opener.open(notification, timeout=self.remaining(5)) as response:
                need(response.status == 202, "configured HTTP initialized notification refused")
            snapshot = rpc("tools/call", {"name": "config_select", "arguments": {}})
            need(json.loads(snapshot["content"][0]["text"]) == selected, "configured HTTP selection differs")
            catalog = rpc("tools/list", {})
            need(catalog["tools"] == stdio_catalog, "configured stdio/HTTP catalogs differ")
            for tool, arguments in (("core", {"max_body_bytes": 128}), ("get", {"id": owners[0]["node"]})):
                value = rpc("tools/call", {"name": tool, "arguments": {"db": "first", **arguments}})
                need(value.get("isError") is False and owners[0]["node"] in value["content"][0]["text"],
                     "configured HTTP read/core lost admitted owner")
            denied = rpc("tools/call", {"name": "save", "arguments": {"db": "second", "summary": "never written",
                         "operation_id": "configured-http-denied"}})
            need(denied.get("isError") is True, "configured HTTP exceeded write ceiling")
            request = urllib.request.Request(url, headers={"mcp-session-id": session, "mcp-protocol-version": PROTOCOL}, method="DELETE")
            with opener.open(request, timeout=self.remaining(5)) as response:
                need(response.status in (200, 202, 204), "configured HTTP session shutdown refused")
        finally:
            host_process.terminate_tree(child, timeout=3)
            self.config.write_bytes(original)
        self.receipt["checks"].append("configured_http_stdio_catalog_selection_core_read_write_denial_and_shutdown_parity")

    def cache_checks(self, owners):
        first = owners[0]
        snapshots = []
        time.sleep(1.05)  # Snapshot admission shares the same native cold-work gate.
        for index in range(2):
            if index:
                time.sleep(1.05)  # Existing native snapshot cold-work interval.
            script = "import json,sys;sys.path.insert(0,sys.argv[1]);from mcp_client import McpClient;c=McpClient(sys.argv[2],timeout=5);print(json.dumps(c.call_tool('snapshot_create',{'db':'project','expected_db_id':sys.argv[3]})));c.close()"
            snapshot = self.run_command("disposable.snapshot_create." + str(index),
                [sys.executable, "-B", "-c", script, self.lib, first["url"], first["id"]])
            bundle = Path(snapshot["bundle"])
            need(bundle.is_relative_to(self.root) and snapshot["db_id"] == first["id"], "snapshot escaped disposable identity/root")
            manifest = json.loads((bundle / "manifest.json").read_bytes())
            snapshots.append({"source_device_id": "mirror", "database": "copy_" + snapshot["generation"],
                              "resolved_path": str(bundle / "database.db"), "generation": snapshot["generation"],
                              "captured_at": manifest["captured_at"]})
        replica, replica_url = self.start_http([row["database"] + "=" + row["resolved_path"] for row in snapshots], "read-only")
        catalog_path = self.root / "disposable-catalog.json"
        catalog = {"schema": "mneme.library.catalog.v1", "library_id": "smoke", "revision": 1, "entries": [
            {"project_id": "disposable", "db_id": first["id"], "owner_device_id": "owner", "display_name": "Disposable",
             "database": "project", "revision": 1, "withdrawn": False, "replicas": [snapshots[0]]}]}
        catalog_path.write_bytes(encoded(catalog))
        cache = {"catalog": catalog_path, "url": replica_url}
        entries = [(row["name"], row["id"], row["url"], True, True) for row in owners]
        self.write_config(entries, cache=cache)
        selected = self.select()
        live = self.call(selected, "first", "explicit", "get", {"id": first["node"]})["result"]
        need("_mneme_source" not in live, "available canonical owner became cache read")
        host_process.terminate_tree(first["process"], timeout=3)
        cached = self.call(selected, "first", "explicit", "get", {"id": first["node"]})["result"]
        old_source = cached["_mneme_source"]
        need(old_source == {"owner_db_id": first["id"], **snapshots[0]}, "cached read lost exact immutable source")
        catalog["entries"][0]["replicas"].append(snapshots[1])
        catalog["revision"] += 1
        catalog_path.write_bytes(encoded(catalog))
        newest = self.call(selected, "first", "explicit", "get", {"id": first["node"]})["result"]
        need(newest["_mneme_source"]["generation"] == max(snapshots, key=lambda row: (row["captured_at"], row["generation"]))["generation"],
             "new read did not select newest available cache edition")
        pinned = self.call(selected, "first", "explicit", "get", {"id": first["node"], "_mneme_source": old_source})["result"]
        need(pinned["_mneme_source"] == old_source, "cached continuation silently changed generation")
        self.call(selected, "first", "explicit", "save", {"summary": "canonical owner down", "operation_id": "no-cache-write"}, ok=False)
        self.call(selected, "first", "explicit", "save", {"summary": "cached witness is not write authority",
                  "operation_id": "no-cache-witness-write", "_mneme_source": old_source}, ok=False)
        catalog["entries"][0]["replicas"].remove(snapshots[0])
        catalog["revision"] += 1
        catalog_path.write_bytes(encoded(catalog))
        expired = self.call(selected, "first", "explicit", "get", {"id": first["node"], "_mneme_source": old_source}, ok=False)
        need("expired" in expired["error"]["message"], "expired cached reference did not refuse explicitly")
        # Reachable-but-wrong native identity, authentication and protocol errors
        # cannot be laundered into the usable cache edition.
        refusals = [RefusalHTTP(401), RefusalHTTP(200)]
        try:
            for url in (owners[1]["url"], refusals[0].url, refusals[1].url):
                self.write_config([("first", first["id"], url, True, True)], cache=cache)
                self.call(self.select(), "first", "explicit", "get", {"id": first["node"]}, ok=False)
        finally:
            for refusal in refusals:
                refusal.close()
        self.receipt["cache"] = {"canonical_owner_id": first["id"], "first_exact_reference": old_source,
                                 "last_catalog": artifact(catalog_path)}
        self.receipt["checks"].extend(["available_owner_read_then_availability_only_cache_fallback", "cached_continuation_exact_generation_and_expiry",
            "writes_canonical_owner_only_cached_witness_not_authority", "identity_auth_protocol_refusal_never_falls_back"])
        host_process.terminate_tree(replica, timeout=3)


def main(argv=None):
    need(sys.version_info >= (3, 10), "copied Mneme helpers require Python 3.10 or later")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, required=True)
    parser.add_argument("--mcp", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--build-description", required=True)
    parser.add_argument("--timeout", type=float, default=180)
    options = parser.parse_args(argv)
    need(math.isfinite(options.timeout) and 30 <= options.timeout <= 600, "timeout must be 30..600 seconds")
    for key in ("cli", "mcp", "output"):
        setattr(options, key, getattr(options, key).resolve())
    need(not options.output.exists(), "receipt already exists")
    before = source_state()
    receipt = {"schema": "mneme.declarative-smoke.v1", "status": "running", "platform": platform.platform(),
        "build_description": options.build_description, "source_start": before, "python": sys.version,
        "artifacts": {key: artifact(getattr(options, key)) for key in ("cli", "mcp")},
        "harness": artifact(Path(__file__).resolve()), "commands": [], "checks": [],
        "omissions": ["providers/models and quality", "live stores/config/services", "Windows runtime qualification",
                      "Codex hook trust/application delivery", "TUI terminal rendering"]}
    start = time.monotonic()
    try:
        with tempfile.TemporaryDirectory(prefix="mneme-declarative-smoke-") as directory:
            root = Path(directory).resolve()
            smoke = Smoke(options, root, receipt)
            try:
                smoke.run()
            finally:
                smoke.close()
        receipt["cleanup"]["disposable_tree_removed"] = not root.exists()
        need(receipt["cleanup"]["owned_children_exited"] and not root.exists(), "smoke cleanup incomplete")
        for key in ("cli", "mcp"):
            need(artifact(getattr(options, key)) == receipt["artifacts"][key], "source binary changed during smoke")
        receipt["status"] = "passed"
    except Exception as error:
        receipt["status"], receipt["error"] = "failed", str(error)
        if "cleanup" in receipt and "root" in locals():
            receipt["cleanup"]["disposable_tree_removed"] = not root.exists()
    receipt["source_end"] = source_state()
    receipt["source_changed_during_smoke"] = receipt["source_start"] != receipt["source_end"]
    receipt["elapsed_ms"] = round((time.monotonic() - start) * 1000)
    options.output.parent.mkdir(parents=True, exist_ok=True)
    with options.output.open("xb") as stream:
        stream.write(encoded(receipt) + b"\n")
    print(json.dumps({key: receipt[key] for key in ("status", "checks", "omissions", "elapsed_ms")}, indent=2))
    if receipt["status"] != "passed":
        print(receipt["error"], file=sys.stderr)
    return 0 if receipt["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())

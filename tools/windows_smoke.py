#!/usr/bin/env python3
"""Finite, provider-free native Windows smoke; Unix runs are crosschecks only.

Build the matched binaries with --locked --no-default-features --features
cozo,http -p mnemed -p mneme-mcp. This creates only disposable configuration,
stores, installed integration copies and owners. It neither enrolls an existing
project nor changes a real service. Receipts are exclusive-create, with artifact
hashes and bounded output hashes rather than memory bodies or raw subprocess logs.

Covered: ordinary init/Python discovery and extraction, copied HTTP owner and
native bridge, bounded save/get/body, lease refusal, stdio EOF/abrupt exit/reopen.
Two installed owners exercise explicit registry core and contextual recall with
one scripted selector (not a model), including automatic-write denial on extras.
Not covered: providers/models, Codex hook trust/application delivery, TUI, remote
filesystems, installer repair under every crash point, or production live stores.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace

sys.dont_write_bytecode = True
INTEGRATION = Path(__file__).resolve().parents[1] / "integrations/codex"
sys.path.insert(0, str(INTEGRATION))
import host_process  # noqa: E402
import host_files  # noqa: E402
from mcp_client import McpClient  # noqa: E402

MAX_OUTPUT = 2 * 1024 * 1024
MAX_FRAME = 512 * 1024
PROTOCOL = "2025-11-25"
BODY = "Violet lantern café 日本語: disposable Windows body."
REGISTRY_HELPERS = {"core_hook.py", "store_registry.py", "registry_config.py", "registry_runtime.py",
                    "registry_reader.py", "registry_recording.py"}

# This fixture imports only the copied installed integration. Native collection
# and the common decoded-byte ledger are real; choosing all supplied candidates
# is deliberately scripted, and cannot establish selector/model quality.
REGISTRY_RECALL = """
import json, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from hooks import _config
from registry_runtime import selection
from registry_reader import execute
class Selector:
    calls = 0
    def select(self, dialogue, cards, **kwargs):
        self.calls += 1
        return {'reason':'selected', 'selected_ids':[card['id'] for card in cards],
                'concerns':[], 'provider_attempt':False, 'usage':None, 'elapsed_ms':0}
config = _config(Path(sys.argv[2]))
selected = selection(config)
selector = Selector()
result = execute(config, sys.argv[3], selector, lambda: True, set())
print(json.dumps({'stores':[store.name for store in selected.stores],
    'recordable':[store.name for store in selected.recordable],
    'maintainable':[store.name for store in selected.maintainable],
    'selector_calls':selector.calls, 'provider_attempt':result.get('provider_attempt'),
    'outcome':result['outcome'], 'read_coverage':result['read_coverage'],
    'native_work':result['selection'].get('native_work'),
    'cards':[{key:card[key] for key in ('id','db_id','store_name')} for card in result['cards']]}))
"""


def need(condition, message):
    if not condition:
        raise RuntimeError(message)


def encode(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=True, allow_nan=False).encode()


def sha(value):
    return hashlib.sha256(value).hexdigest()


def artifact(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return {"path": str(path), "bytes": path.stat().st_size, "sha256": digest.hexdigest()}


def verify_installed_artifacts(installed, sources, binaries):
    """Fail stale bundles/copies instead of merely recording divergent hashes."""
    expected = {Path(source["path"]).name: source for source in sources}
    suffix = ".exe" if os.name == "nt" else ""
    expected.update({"mnemed" + suffix: binaries["cli"], "mneme-mcp" + suffix: binaries["mcp"]})
    for copied in installed:
        name = Path(copied["path"]).name
        source = expected.get(name)
        need(source is not None and all(copied[key] == source[key] for key in ("bytes", "sha256")),
             f"installed artifact differs from supplied source/binary: {name}")


def isolated_environment(root, cli):
    # Inherit OS/executable discovery, not a developer's API credentials or
    # memory/provider configuration. Empty caches plus loopback discard prevent
    # accidental model downloads; no provider-capable Codex executable is used.
    env = {key: value for key, value in os.environ.items()
           if not key.upper().startswith(("MNEME_", "XDG_", "HF_", "HUGGINGFACE_", "CODEX_", "OPENAI_", "ANTHROPIC_"))
           and not any(word in key.upper() for word in ("API_KEY", "TOKEN", "SECRET", "PASSWORD"))}
    directories = {"HOME": "home", "USERPROFILE": "home", "APPDATA": "roaming",
                   "LOCALAPPDATA": "local", "XDG_DATA_HOME": "data", "XDG_CONFIG_HOME": "config",
                   "XDG_CACHE_HOME": "cache", "XDG_STATE_HOME": "state", "CODEX_HOME": "codex-home",
                   "HF_HOME": "hf", "TEMP": "temp", "TMP": "temp", "TMPDIR": "temp"}
    for key, name in directories.items():
        path = root / name
        path.mkdir(exist_ok=True)
        env[key] = str(path)
    env.update(MNEME_CLIENT_BINARY=str(cli), MNEME_RERANK="0", HF_HUB_OFFLINE="1",
               TRANSFORMERS_OFFLINE="1", HF_ENDPOINT="http://127.0.0.1:9",
               GIT_CEILING_DIRECTORIES=str(root), PYTHONUTF8="1", PYTHONDONTWRITEBYTECODE="1")
    for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
        env[key] = env[key.lower()] = "http://127.0.0.1:9"
    env["NO_PROXY"] = env["no_proxy"] = "127.0.0.1,localhost,::1"
    return env


class Child:
    """One owned tree with separately bounded stdout/stderr, no pipe threads."""

    def __init__(self, argv, *, cwd, env, deadline, records, label):
        self.deadline, self.records, self.label = deadline, records, label
        self.out, self.err = bytearray(), bytearray()
        self.total, self.closed, self.eof = 0, False, set()
        self.record = {"label": label, "argv_sha256": sha(encode(list(map(str, argv))))}
        self.started = time.monotonic()
        self.process = host_process.spawn(list(map(str, argv)), cwd=cwd, env=env, tree=True,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
        try:
            self.stdout = host_process.BoundedPipe(self.process)
            self.stderr = host_process.BoundedPipe(SimpleNamespace(
                stdin=self.process.stdin, stdout=self.process.stderr, text_mode=False))
        except BaseException:
            host_process.terminate_tree(self.process)
            for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
                stream.close()
            raise
        self.hashes = [hashlib.sha256(), hashlib.sha256()]
        self.counts = [0, 0]

    def poll_io(self):
        need(time.monotonic() < self.deadline, f"subprocess deadline exceeded: {self.label}")
        for index, (pipe, output) in enumerate(((self.stdout, self.out), (self.stderr, self.err))):
            if index in self.eof:
                continue
            try:
                chunk = pipe.read(8192, min(self.deadline, time.monotonic() + 0.025))
            except TimeoutError:
                continue
            if not chunk:
                self.eof.add(index)
                continue
            self.total += len(chunk)
            need(self.total <= MAX_OUTPUT, f"subprocess output exceeds bound: {self.label}")
            self.hashes[index].update(chunk)
            self.counts[index] += len(chunk)
            output.extend(chunk)

    def write(self, data):
        need(len(data) <= MAX_FRAME, "request exceeds frame bound")
        self.stdout.write(data, self.deadline)

    def line(self):
        while b"\n" not in self.out:
            need(0 not in self.eof, f"subprocess EOF before response: {self.label}")
            need(len(self.out) <= MAX_FRAME, "response exceeds frame bound")
            self.poll_io()
        line, _, rest = self.out.partition(b"\n")
        need(len(line) <= MAX_FRAME, "response exceeds frame bound")
        self.out = bytearray(rest)
        return bytes(line)

    def finish(self):
        if not self.process.stdin.closed:
            self.process.stdin.close()
        while len(self.eof) != 2:
            self.poll_io()
        self.process.wait(timeout=max(0.001, self.deadline - time.monotonic()))
        return bytes(self.out), bytes(self.err)

    def close(self):
        if self.closed:
            return
        try:
            host_process.terminate_tree(self.process, timeout=3)
        finally:
            self.stdout.close()
            self.stderr.close()
            self.closed = True
            self.record.update(exit=self.process.poll(), stdout_bytes=self.counts[0], stderr_bytes=self.counts[1],
                stdout_sha256=self.hashes[0].hexdigest(), stderr_sha256=self.hashes[1].hexdigest(),
                elapsed_ms=round((time.monotonic() - self.started) * 1000))
            self.records.append(self.record)


class Smoke:
    def __init__(self, options, root, receipt):
        self.options, self.root, self.receipt = options, root, receipt
        self.deadline = time.monotonic() + options.timeout
        self.children, self.clients = [], []
        self.env = isolated_environment(root, options.cli)
        self.project = root / "project café 日本語 & %NAME% !"
        self.project.mkdir()
        self.db = self.project / ".mneme/codex-memory.db"
        self.prefix = self.project / ".mneme/codex-integration"
        self.service_config = self.prefix / "config/service.json"
        self.service_script = self.prefix / "lib/service.py"
        self.reference_project = root / "reference café project"
        self.reference_project.mkdir()
        self.services = {"project": {"prefix": self.prefix, "stopped": False},
            "reference": {"prefix": self.reference_project / ".mneme/codex-integration", "stopped": False}}
        self.registry_path = root / "stores.json"

    def child(self, label, argv, *, cleanup=False, env=None):
        now = time.monotonic()
        # Installed service.stop has its own 40-second identity/exit deadline.
        # Do not kill that helper before its cleanup contract has completed.
        deadline = now + 50 if cleanup else min(self.deadline, now + 120)
        need(deadline > now, "smoke total deadline exceeded")
        child = Child(argv, cwd=self.project, env=env or self.env, deadline=deadline,
                      records=self.receipt["processes"], label=label)
        self.children.append(child)
        return child

    def command(self, label, argv, *, ok=True, retain=False, cleanup=False, input_data=None, env=None):
        child = self.child(label, argv, cleanup=cleanup, env=env)
        try:
            if input_data is not None:
                child.write(input_data)
            out, err = child.finish()
            code = child.process.returncode
            if code and label.endswith("ordinary.init"):
                # Only this synthetic setup result is decoded for diagnostics;
                # arbitrary command output and body data stay out of receipts.
                try:
                    failure = json.loads(out)
                    child.record["init_failure"] = {key: failure[key] for key in ("stage", "error") if key in failure}
                except (ValueError, TypeError):
                    pass
            need(code == (0 if ok else 1), f"unexpected subprocess exit {code}: {label}; stderr {sha(err)}")
            return json.loads(out) if ok else (out, err)
        finally:
            if not retain:
                child.close()

    def cli(self, label, *args, **options):
        return self.command(label, [self.options.cli, "--json", *args], **options)

    def service(self, action, *, name="project", cleanup=False):
        prefix = self.services[name]["prefix"]
        return self.command(name + ".installed.service." + action,
            [sys.executable, "-B", prefix / "lib/service.py", "--config", prefix / "config/service.json", action], cleanup=cleanup)

    def stop_service(self, name="project"):
        entry = self.services[name]
        if (entry["prefix"] / "config/service.json").is_file() and not entry["stopped"]:
            result = self.service("stop", name=name, cleanup=True)
            need(result.get("state") == "stopped", "installed owner did not stop")
            entry["stopped"] = True

    def http(self, url=None):
        remaining = self.deadline - time.monotonic()
        need(remaining > 0, "smoke total deadline exceeded")
        client = McpClient(url or self.url, timeout=min(20, remaining))
        self.clients.append(client)
        return client

    def http_call(self, client, method):
        remaining = self.deadline - time.monotonic()
        need(remaining > 0, "smoke total deadline exceeded")
        client.timeout = min(20, remaining)
        return getattr(client, method)()

    def tool(self, client, name, args):
        remaining = self.deadline - time.monotonic()
        need(remaining > 0, "smoke total deadline exceeded")
        client.timeout = min(20, remaining)
        result = client.call_tool(name, args)
        self.receipt["tools"].append({"name": name, "arguments_sha256": sha(encode(args)),
            "result_sha256": sha(encode(result)), "result_bytes": len(encode(result))})
        return result

    def stdio(self, label):
        child = self.child(label, [self.options.mcp, "--capability-profile", "operator", "--db", f"project={self.db}"])
        child.request_id = 0
        result = self.rpc(child, "initialize", {"protocolVersion": PROTOCOL,
            "capabilities": {}, "clientInfo": {"name": "mneme-windows-smoke", "version": "1"}})
        need(result.get("protocolVersion") == PROTOCOL and result.get("capabilityProfile") == "operator",
             "wrong stdio initialization")
        child.write(encode({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}) + b"\n")
        return child

    def rpc(self, child, method, params):
        child.request_id += 1
        child.write(encode({"jsonrpc": "2.0", "id": child.request_id, "method": method, "params": params}) + b"\n")
        response = json.loads(child.line())
        need(response.get("jsonrpc") == "2.0" and response.get("id") == child.request_id and "error" not in response,
             "invalid/error stdio response")
        return response["result"]

    def stdio_get(self, child, ident):
        value = self.rpc(child, "tools/call", {"name": "get", "arguments":
            {"db": "project", "expected_db_id": self.db_id, "id": ident, "body": True, "max_body_bytes": 256}})
        need(value.get("isError") is False, "stdio get returned tool error")
        return json.loads(value["content"][0]["text"])

    def refusal(self, label):
        _, err = self.cli(label, "--db", str(self.db), "get", self.note_id, ok=False)
        need(b"already owned by another mneme process" in err, "second opener failed at wrong boundary")

    def init_project(self, name, project, registry):
        result = self.cli(name + ".ordinary.init", "init", "--root", str(project), "--no-recording", "--no-trust-hooks",
            "--python", sys.executable, "--mcp-binary", str(self.options.mcp), "--codex-binary", str(self.options.cli),
            "--library-config", str(self.root / (name + "-library") / "library.json"), "--store-registry", str(registry), retain=True)
        need(result.get("status") == "configured" and result.get("store") == "created", "ordinary init did not configure new project")
        need(result.get("recording") == "off" and result.get("published") is False and result.get("local_registration") is True and
             result.get("hook_trust", {}).get("status") == "not_requested", "init crossed provider/trust/publication boundary")
        prefix = self.services[name]["prefix"]
        need(Path(result["service_config"]) == prefix / "config/service.json", "init returned an unexpected service config")
        need(Path(result["store_registry"]) == registry, "init did not retain explicit store registry")
        need(REGISTRY_HELPERS <= {path.name for path in (prefix / "lib").glob("*.py")}, "installed bundle omitted registry helper")
        return result

    def registry_reads(self, project_client, reference_client):
        core_ids = []
        for name, identifier, client in (("project", self.db_id, project_client),
                                          ("reference", self.reference_db_id, reference_client)):
            core = self.tool(client, "save", {"db": "project", "expected_db_id": identifier,
                "kind": "note", "summary": "Authored " + name + " identity", "body": "Disposable " + name + " core.",
                "tags": ["core"], "operation_id": "windows-registry-core-" + name})
            core_ids.append(core["id"])
        library = self.prefix / "lib"
        config = self.prefix / "config/hooks.json"
        installed_cli = self.prefix / "bin" / ("mnemed.exe" if os.name == "nt" else "mnemed")
        env = {**self.env, "MNEME_CLIENT_BINARY": str(installed_cli)}
        startup = self.command("installed.registry.core", [sys.executable, "-B", library / "core_hook.py",
            "--hook-config", config], env=env, input_data=encode({"hook_event_name": "SessionStart", "source": "startup",
                "cwd": str(self.project), "session_id": "windows-registry-smoke"}))
        context = startup["hookSpecificOutput"]["additionalContext"]
        sections = [json.loads(line) for line in context.splitlines() if line.startswith("{")]
        need(len(sections) == 2 and [row["scope"] for row in sections] == ["project", "reference"],
             "registry core did not preserve project-first named selection")
        need(all(row["outcome"] == "ok" and [card["id"] for card in row["cards"]] == [expected]
                 for row, expected in zip(sections, core_ids)), "registry core crossed owners or omitted authored core")
        recalled = self.command("installed.registry.recall", [sys.executable, "-B", "-c", REGISTRY_RECALL,
            library, config, "Native Windows violet lantern"], env=env)
        need(recalled["stores"] == ["project", "reference"] and recalled["recordable"] == [] and recalled["maintainable"] == [],
             "recording-off registry selection granted automatic writes or changed owners")
        need(recalled["outcome"] == "selected" and recalled["selector_calls"] == 1 and recalled["provider_attempt"] is False,
             "registry contextual recall did not use exactly one scripted selector")
        need(recalled["read_coverage"] == {"selected": 2, "attempted": 2, "unavailable": 0, "budget_omitted": 0, "complete": True},
             "two-owner registry recall was incomplete")
        expected = {(self.db_id, self.note_id, "project"), (self.reference_db_id, self.reference_note_id, "reference")}
        actual = {(row["db_id"], row["id"], row["store_name"]) for row in recalled["cards"]}
        need(expected <= actual, "registry recall lost owner-qualified note identities")
        work = recalled["native_work"]
        need(isinstance(work["decoded_bytes"], int) and 0 < work["decoded_bytes"] <= work["read_allowance_bytes"],
             "registry recall lacks one bounded native decoded-byte ledger")
        self.receipt["registry"] = {"owner_ids": [self.db_id, self.reference_db_id], "native_work": work,
            "coverage": recalled["read_coverage"], "selector": "scripted, no provider; quality not assessed"}
        self.receipt["checks"].append("installed_registry_project_plus_named_extra_core_recall_owner_qualification_shared_budget")

    def run(self):
        # The supplied CLI also stands in for Codex: pinned, never invoked as a
        # provider. Only the passive startup-core hook is invoked explicitly.
        empty_registry = self.root / "reference-stores.json"
        host_files.atomic_write(empty_registry, encode({"schema": "mneme.stores.v1", "stores": []}))
        reference = self.init_project("reference", self.reference_project, empty_registry)
        self.reference_db_id = reference["db_id"]
        reference_db = self.reference_project / ".mneme/codex-memory.db"
        host_files.atomic_write(self.registry_path, encode({"schema": "mneme.stores.v1", "stores": [{
            "name": "reference", "service_config": str(self.services["reference"]["prefix"] / "config/service.json"),
            "db_alias": "project", "database_path": str(reference_db), "db_id": self.reference_db_id,
            "purpose": "Read-only automatic reference material for the violet lantern fixture.",
            "read": {"enabled": True, "core": True}, "record": {"enabled": False}, "maintenance": {"enabled": False}}]}))
        result = self.init_project("project", self.project, self.registry_path)
        self.url, self.db_id = result["owner_url"], result["db_id"]
        need(self.db_id != self.reference_db_id, "two disposable owners received the same identity")
        self.receipt["checks"].append("ordinary_init_no_recording_no_trust_local_registration")
        self.receipt["installed_artifacts"] = [artifact(path) for entry in self.services.values()
            for path in sorted(entry["prefix"].rglob("*")) if path.is_file() and path.parent.name in ("bin", "lib")]
        verify_installed_artifacts(self.receipt["installed_artifacts"], self.receipt["integration_sources"],
                                   self.receipt["artifacts"])
        self.receipt["checks"].append("installed_helpers_and_binaries_match_supplied_source_hashes")
        need(self.service("status").get("state") == "ready", "copied service owner not ready")
        need(self.service("status", name="reference").get("state") == "ready", "named extra owner not ready")
        reference_client = self.http(reference["owner_url"])
        reference_note = self.tool(reference_client, "save", {"db": "project", "expected_db_id": self.reference_db_id,
            "kind": "note", "summary": "Native Windows violet lantern", "body": "Separate reference fixture, never a fallback.",
            "tags": ["windows-smoke"], "operation_id": "windows-smoke-reference"})
        self.reference_note_id = reference_note["id"]
        client = self.http()
        self.http_call(client, "connect")
        names = {tool["name"] for tool in self.http_call(client, "list_tools")}
        need({"databases", "save", "get", "database_control"} <= names, "HTTP owner catalog lacks required tools")
        catalog = self.tool(client, "databases", {})
        need(len(catalog) == 1 and catalog[0]["name"] == "project" and catalog[0]["db_id"] == self.db_id,
             "HTTP owner catalog identity differs from init")
        note = self.tool(client, "save", {"db": "project", "expected_db_id": self.db_id, "kind": "note",
            "summary": "Native Windows violet lantern", "body": BODY, "tags": ["windows-smoke"], "operation_id": "windows-smoke-note"})
        self.note_id = note["id"]
        got = self.tool(client, "get", {"db": "project", "expected_db_id": self.db_id, "id": self.note_id,
            "body": True, "max_body_bytes": 256})
        need(got["body"] == BODY, "HTTP body did not round trip")
        ranged = self.cli("owner.body.range", "body", self.note_id, "--offset", "1", "--max-bytes", "6")
        need(ranged["body"] == "iolet ", "CLI owner body range differs")
        self.refusal("http.second_opener_refused")
        self.registry_reads(client, reference_client)
        client.close()
        reference_client.close()
        self.stop_service()
        self.stop_service("reference")
        self.receipt["checks"].append("installed_http_owner_catalog_save_get_bounded_body_lease_refusal_stop")
        reopened = self.cli("offline.after_installed_stop", "--db", str(self.db), "get", self.note_id, "--body")
        # Explicit offline GET uses the CLI's body projection object; MCP GET
        # has a string plus body_range. Do not silently conflate those schemas.
        need(reopened["body"]["content"] == BODY, "stored body lost after installed owner stop")
        # EOF is graceful on Windows; TerminateProcess is not SIGTERM.
        owner = self.stdio("stdio.graceful")
        need(self.stdio_get(owner, self.note_id)["body"] == BODY, "stdio body differs")
        self.refusal("stdio.second_opener_refused")
        owner.finish()
        need(owner.process.returncode == 0, "stdio EOF did not shut down successfully")
        owner.close()
        self.cli("offline.after_graceful_eof", "--db", str(self.db), "get", self.note_id)
        owner = self.stdio("stdio.abrupt")
        need(self.stdio_get(owner, self.note_id)["body"] == BODY, "reopened stdio body differs")
        owner.process.kill()
        owner.finish()
        need(owner.process.returncode != 0, "abrupt owner unexpectedly exited successfully")
        owner.close()
        final = self.cli("offline.after_abrupt_exit", "--db", str(self.db), "get", self.note_id, "--body")
        need(final["body"]["content"] == BODY, "stored body lost after abrupt owner exit")
        self.receipt["checks"].append("stdio_graceful_eof_and_abrupt_exit_both_release_lease_and_preserve_body")

    def close(self):
        errors = []
        for client in self.clients:
            try:
                client.close()
            except Exception as error:
                errors.append(str(error))
        for name in self.services:
            try:
                self.stop_service(name)
            except Exception as error:
                errors.append(name + ": " + str(error))
        for child in reversed(self.children):
            try:
                child.close()
            except Exception as error:
                errors.append(str(error))
        self.receipt["cleanup"] = {"owned_children_exited": all(child.process.poll() is not None for child in self.children),
            "installed_owner_stopped": self.services["project"]["stopped"],
            "named_extra_owner_stopped": self.services["reference"]["stopped"], "errors": errors}
        need(not errors and self.receipt["cleanup"]["owned_children_exited"], "smoke cleanup incomplete")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", required=True, type=Path)
    parser.add_argument("--mcp", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path, help="new immutable JSON receipt path")
    parser.add_argument("--timeout", type=float, default=300)
    parser.add_argument("--require-windows", action="store_true", help="fail instead of reporting a Unix crosscheck")
    parser.add_argument("--build-description", required=True, help="caller-supplied exact build command; binary hashes remain authoritative")
    options = parser.parse_args(argv)
    need(math.isfinite(options.timeout) and 30 <= options.timeout <= 900, "timeout must be 30..900 seconds")
    options.cli, options.mcp, options.output = (value.resolve() for value in (options.cli, options.mcp, options.output))
    need(not options.output.exists(), "receipt exists; choose a new path rather than rewrite evidence")
    need(not options.require_windows or os.name == "nt", "native Windows required; Unix emulation is not Windows evidence")
    receipt = {"schema": "mneme.windows-smoke.v1", "status": "running", "platform": platform.platform(),
        "native_windows_evidence": "executed" if os.name == "nt" else "missing; Unix crosscheck only",
        "build_description": options.build_description, "python": sys.version,
        "artifacts": {name: artifact(path) for name, path in
            (("cli", options.cli), ("mcp", options.mcp), ("harness", Path(__file__).resolve()))},
        "integration_sources": [artifact(path) for path in sorted(INTEGRATION.glob("*.py")) if not path.name.startswith("test_")],
        "processes": [], "tools": [], "checks": [], "omissions": ["providers/models and quality", "Codex app hook trust/delivery",
            "TUI", "non-local/unsupported filesystems", "production stores", "all installer crash permutations",
            "installed service log uses the production append-only file; finite lifetime, not a strict byte cap"]}
    original_env = dict(os.environ)
    start = time.monotonic()
    try:
        with tempfile.TemporaryDirectory(prefix="mneme-windows-smoke-") as directory:
            root = Path(directory).resolve()
            smoke = Smoke(options, root, receipt)
            os.environ.clear()
            os.environ.update(smoke.env)
            try:
                smoke.run()
            except Exception as error:
                receipt["operation_error"] = str(error)
                raise
            finally:
                smoke.close()
        receipt["cleanup"]["disposable_tree_removed"] = not root.exists()
        need(receipt["cleanup"]["disposable_tree_removed"], "disposable fixture tree still exists")
        for name in ("cli", "mcp"):
            need(artifact(getattr(options, name)) == receipt["artifacts"][name], f"{name} changed during smoke")
        receipt["status"] = "passed"
    except Exception as error:
        receipt["status"], receipt["error"] = "failed", str(error)
    finally:
        os.environ.clear()
        os.environ.update(original_env)
    receipt["elapsed_ms"] = round((time.monotonic() - start) * 1000)
    options.output.parent.mkdir(parents=True, exist_ok=True)
    with options.output.open("xb") as stream:
        stream.write(encode(receipt) + b"\n")
    print(json.dumps(receipt, indent=2, sort_keys=True))
    return 0 if receipt["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Finite installed-artifact smoke for Codex global + project core loading.

Copies the installed native pair and five Python runtime modules into a private
prefix. All stores, configs, logs, and cache writes are disposable. No Cargo,
provider, Codex settings, real memory store, or hook-trust changes are made.
This verifies native/MCP integration, not Codex UI trust or model use of context.
"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import socket
import subprocess
import sys
import threading
import time


REPO = Path(__file__).resolve().parents[1]
SCRATCH = REPO / "target" / "codex-global-memory-v1"
MODULES = ("service.py", "mcp_client.py", "launcher.py", "memory.py", "core_hook.py")
PROTOCOL = "2025-11-25"
MAX_FRAME = 512 * 1024
HOOK_TIMEOUT = 0.5
HOOK_OUTER_TIMEOUT = 5.0
TOTAL_TIMEOUT = 300.0
COLD_PACING = 1.05
SOURCE_NAMESPACE = "codex-global-core-smoke"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
                    encoding="utf-8")
    path.chmod(0o600)


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def free_ports():
    # Reserve both together so the ephemeral allocator cannot return one twice.
    with socket.socket() as first, socket.socket() as second:
        first.bind(("127.0.0.1", 0))
        second.bind(("127.0.0.1", 0))
        return first.getsockname()[1], second.getsockname()[1]


def cached_model(cache):
    """Preflight the exact installed default's fastembed/hf-hub cache inputs.

    Rust hf-hub 0.4 does not honor HF_HUB_OFFLINE. Requiring the complete cached
    BGE-base asset set prevents its ApiRepo.get cache-miss download path instead.
    This is an input preflight, not a claim of OS-enforced network isolation.
    """
    model = cache / "models--Xenova--bge-base-en-v1.5"
    with (model / "refs/main").open("rb") as stream:
        raw = stream.read(129)
    require(re.fullmatch(rb"[0-9a-f]{40}", raw) is not None,
            "default BGE-base cache ref must be one exact existing revision")
    snapshot = model / "snapshots" / raw.decode("ascii")
    files = [snapshot / name for name in ("onnx/model.onnx", "tokenizer.json", "config.json",
                                          "special_tokens_map.json", "tokenizer_config.json")]
    for path in files:
        require(path.is_file() and path.stat().st_size > 0 and path.resolve().is_relative_to(cache.resolve()),
                "required BGE-base asset is missing, empty, or escapes cache: %s" % path)
    return {"revision": raw.decode("ascii"),
            "assets": [{"path": str(path.relative_to(cache)), "bytes": path.stat().st_size}
                       for path in files]}


def sourced(key, summary, *, core=False):
    return {"source": {"namespace": SOURCE_NAMESPACE, "key": key,
                       "reference": str(Path(__file__).resolve()) + "#" + key},
            "summary": summary, "body": summary + " Synthetic smoke fixture, not a user fact.",
            "tags": ["core"] if core else ["smoke-ordinary"], "active": True}


FIXTURE = {
    "user_core": sourced("user-core", "GLOBAL_CORE_SENTINEL: synthetic cross-project preference.", core=True),
    "project_core": sourced("project-core", "PROJECT_CORE_SENTINEL: synthetic project orientation.", core=True),
    "project_ordinary": sourced("project-ordinary", "PROJECT_ORDINARY_SENTINEL: never inject through core."),
    "user_capture": sourced("user-capture", "USER_CAPTURE_SENTINEL: explicit user-store capture, never core."),
}


class Runner:
    def __init__(self, out, env):
        self.out, self.env = out, env
        self.deadline = time.monotonic() + TOTAL_TIMEOUT
        self.commands = []
        self.pacing = []
        self.lock = threading.Lock()
        self.hosts_may_be_live = False

    def remaining(self, maximum):
        remaining = self.deadline - time.monotonic()
        require(remaining > 0, "smoke exceeded its 300-second operation deadline")
        return min(maximum, remaining)

    def pace_cold(self, purpose):
        require(self.remaining(COLD_PACING) == COLD_PACING, "insufficient time for native cold-work pacing")
        time.sleep(COLD_PACING)
        self.pacing.append({"purpose": purpose, "seconds": COLD_PACING})

    def run(self, argv, *, cwd=None, payload=None, timeout=30, check=True, cleanup=False):
        argv = [str(item) for item in argv]
        started = time.monotonic()
        result = subprocess.run(argv, cwd=cwd or self.out, env=self.env, input=payload,
                                text=True, capture_output=True, check=False,
                                timeout=timeout if cleanup else self.remaining(timeout))
        elapsed = round((time.monotonic() - started) * 1000)
        require(len(result.stdout.encode()) + len(result.stderr.encode()) <= 2 * 1024 * 1024,
                "subprocess output exceeded smoke's 2 MiB result bound")
        with self.lock:
            self.commands.append({"argv": argv, "cwd": str(cwd or self.out),
                                  "timeout_seconds": timeout, "elapsed_ms": elapsed,
                                  "returncode": result.returncode,
                                  "stdout_bytes": len(result.stdout.encode()),
                                  "stderr_bytes": len(result.stderr.encode())})
        if check:
            require(result.returncode == 0,
                    "%s exited %s: %s" % (Path(argv[0]).name, result.returncode, result.stderr[-1500:]))
        return result, elapsed

    def json(self, argv, **kwargs):
        result, elapsed = self.run(argv, **kwargs)
        return json.loads(result.stdout), elapsed

    def cli(self, name, db, *args, init=False, **kwargs):
        require(not self.hosts_may_be_live, "smoke refuses a competing one-shot CLI lease")
        argv = [self.out / "prefix/bin/mnemed", "--db", db, "--json"]
        if name == "user" and not init:
            argv += ["--user"]
        return self.json(argv + list(args), **kwargs)[0]


class StdioClient:
    """One bounded actual launcher process; no direct native stdio host spawn."""

    def __init__(self, runner, config, name):
        self.runner, self.name = runner, name
        self.stderr = (runner.out / (name + ".stderr.log")).open("wb")
        self.process = subprocess.Popen(
            [sys.executable, str(runner.out / "prefix/lib/launcher.py"),
             "--service-config", str(config)], cwd=runner.out, env=runner.env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr)
        self.buffer = b""
        self.next_id = 1
        self.trace = []

    def request(self, method, params=None, *, notification=False):
        message = {"jsonrpc": "2.0", "method": method, "params": params or {}}
        if not notification:
            message["id"] = self.next_id
            self.next_id += 1
        encoded = canonical(message).encode() + b"\n"
        require(len(encoded) <= 128 * 1024, "smoke request exceeds launcher bound")
        self.process.stdin.write(encoded)
        self.process.stdin.flush()
        if notification:
            return None
        started = time.monotonic()
        deadline = started + self.runner.remaining(30)
        with selectors.DefaultSelector() as selector:
            selector.register(self.process.stdout, selectors.EVENT_READ)
            while b"\n" not in self.buffer:
                remaining = deadline - time.monotonic()
                require(remaining > 0 and selector.select(remaining), "launcher response timed out")
                data = os.read(self.process.stdout.fileno(), min(65536, MAX_FRAME + 1 - len(self.buffer)))
                require(data, "launcher closed stdout before response")
                self.buffer += data
                require(len(self.buffer) <= MAX_FRAME, "launcher frame exceeds 512 KiB")
        line, self.buffer = self.buffer.split(b"\n", 1)
        reply = json.loads(line)
        require(reply.get("jsonrpc") == "2.0" and reply.get("id") == message["id"],
                "launcher response identity mismatch")
        require("error" not in reply, "launcher returned JSON-RPC error: %r" % reply.get("error"))
        self.trace.append({"method": method, "params": params,
                           "elapsed_ms": round((time.monotonic() - started) * 1000),
                           "response": reply})
        return reply["result"]

    def initialize(self):
        result = self.request("initialize", {"protocolVersion": PROTOCOL, "capabilities": {},
                               "clientInfo": {"name": self.name, "version": "1"}})
        require(result.get("protocolVersion") == PROTOCOL
                and result.get("serverInfo", {}).get("name") == "mneme-mcp",
                "launcher initialized an unexpected server")
        self.request("notifications/initialized", notification=True)
        return result

    def tool(self, name, arguments, *, refused=False):
        if name in ("core", "status"):
            self.runner.pace_cold(self.name + ":" + name)
        result = self.request("tools/call", {"name": name, "arguments": arguments})
        require(result.get("isError") is refused, "unexpected native tool success/refusal")
        blocks = result.get("content")
        require(isinstance(blocks, list) and len(blocks) == 1 and blocks[0].get("type") == "text",
                "unexpected native tool content")
        if refused:
            return blocks[0]["text"]
        value = json.loads(blocks[0]["text"])
        return json.loads(value) if isinstance(value, str) and value.startswith(("{", "[")) else value

    def close(self):
        if self.process.poll() is None:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.terminate()
                try:
                    self.process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=3)
        self.process.stdout.close()
        self.stderr.close()
        write_json(self.runner.out / (self.name + ".trace.json"), self.trace)
        require(self.process.returncode == 0, "launcher did not close cleanly")


def host_inventory(runner, *, cleanup=False):
    result, _ = runner.run(["ps", "-axo", "pid=,command="], timeout=5, cleanup=cleanup)
    binary = str(runner.out / "prefix/bin/mneme-mcp")
    return [{"pid": int(line.strip().split(None, 1)[0]),
             "command": line.strip().split(None, 1)[1]}
            for line in result.stdout.splitlines()
            if len(line.strip().split(None, 1)) == 2
            and line.strip().split(None, 1)[1].startswith(binary + " ")]


def assert_core(value, identifier):
    require(isinstance(value, dict) and value.get("total") == 1 and value.get("truncated") is False,
            "core response must contain exactly one nontruncated fixture")
    require([node.get("id") for node in value.get("nodes", [])] == [identifier],
            "core returned ordinary memory or wrong store")


def assert_catalog(value, name, db):
    require(isinstance(value, list) and len(value) == 1, "host did not expose one store")
    row = value[0]
    require(row.get("db") == name and row.get("name") == name and row.get("state") == "open"
            and row.get("configured_path") == str(db), "host database identity mismatch")


def hook(runner, config, cwd, *, source="startup", extra=None, pace=True):
    payload = {"hook_event_name": "SessionStart", "source": source,
               "session_id": "global-core-smoke", "cwd": str(cwd)}
    payload.update(extra or {})
    if pace:
        runner.pace_cold("SessionStart:" + source)
    value, elapsed = runner.json([sys.executable, runner.out / "prefix/lib/core_hook.py",
                                 "--config", config, "--timeout", str(HOOK_TIMEOUT)],
                                payload=canonical(payload), timeout=HOOK_OUTER_TIMEOUT)
    context = value.get("hookSpecificOutput", {}).get("additionalContext", "")
    require(isinstance(context, str) and len(context.encode()) <= 18432,
            "hook additionalContext exceeded its declared envelope")
    return {"event": payload, "output": value, "context": context, "elapsed_ms": elapsed,
            "context_bytes": len(context.encode())}


def assert_context(row, ids, *, project):
    context = row["context"]
    require(ids["user_core"] in context, "hook did not load user core")
    require((ids["project_core"] in context) is project, "hook project routing mismatch")
    if project:
        require(context.index(ids["user_core"]) < context.index(ids["project_core"]),
                "hook did not load user core before project core")
    for key in ("project_ordinary", "user_capture"):
        if key in ids:
            require(ids[key] not in context and FIXTURE[key]["summary"] not in context,
                    "hook leaked ordinary memory into direct core load")


def inventory(out):
    # Cache content is a copied input, not a smoke output; do not rehash 0.5 GB.
    return [{"path": str(path.relative_to(out)), "bytes": path.stat().st_size,
             "sha256": sha256(path)}
            for path in sorted(out.rglob("*"))
            if path.is_file() and not path.is_symlink()
            and "embedding-cache" not in path.relative_to(out).parts
            and "__pycache__" not in path.relative_to(out).parts
            and path.name not in ("receipt.json", "inventory.json")]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, default=SCRATCH / ("smoke-" + time.strftime("%Y%m%d-%H%M%S")))
    parser.add_argument("--mnemed", type=Path, default=Path.home() / ".cargo/bin/mnemed")
    parser.add_argument("--mcp", type=Path, default=Path.home() / ".cargo/bin/mneme-mcp")
    parser.add_argument("--embedding-cache", type=Path,
                        default=Path.home() / ".local/share/mneme/.fastembed_cache")
    args = parser.parse_args()
    out = args.out.resolve()
    require(out.parent == SCRATCH.resolve() and out.name.startswith("smoke-"),
            "--out must be a smoke-* directory under target/codex-global-memory-v1")
    require(not out.exists(), "refusing to reuse a smoke output directory")
    for path in [args.mnemed, args.mcp] + [REPO / "integrations/codex" / name for name in MODULES]:
        require(path.is_file(), "required ready artifact is missing: %s" % path)
    require(args.embedding_cache.is_dir(), "provide an existing populated embedding cache; no downloads")
    os.umask(0o077)
    out.mkdir(parents=True, mode=0o700)
    prefix = out / "prefix"
    for name in ("bin", "lib", "config"):
        (prefix / name).mkdir(parents=True, mode=0o700)
    env = os.environ.copy()
    env.pop("HF_HOME", None)
    for key in list(env):
        if key.startswith("GIT_"):
            # In particular, inherited GIT_TRACE* can append to real-home logs.
            env.pop(key)
    env.pop("MNEME_RERANK", None)
    env.pop("MNEME_DB", None)
    # All child cache writes and Git discovery remain within disposable scratch.
    env["FASTEMBED_CACHE_DIR"] = str(out / "embedding-cache")
    env["GIT_CEILING_DIRECTORIES"] = str(out)
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    runner = Runner(out, env)
    receipt = {"schema": "mneme.codex-global-core.smoke.v1", "result": "failed",
               "evidence_tier": "installed_artifact", "provider_calls": 0,
               "token_mode": "none", "raw_environment_recorded": False,
               "deadlines_seconds": {"total_operations": TOTAL_TIMEOUT, "native_cli": 30,
                   "launcher_request": 30, "hook_mcp": HOOK_TIMEOUT,
                   "hook_process": HOOK_OUTER_TIMEOUT, "service_cleanup": 45},
               "deadline_scope": "subprocess/protocol operations; trusted local copies and hash inventory are not timer-interrupted",
               "steps": [], "cleanup": []}
    clients = []
    configs = {}
    failure = None
    try:
        artifacts = []
        sources = [(args.mnemed, prefix / "bin/mnemed"), (args.mcp, prefix / "bin/mneme-mcp")]
        sources += [(REPO / "integrations/codex" / name, prefix / "lib" / name) for name in MODULES]
        for source, target in sources:
            shutil.copy2(source.resolve(), target)
            digest = sha256(source)
            require(digest == sha256(target), "artifact changed while it was copied")
            artifacts.append({"source": str(source.resolve()), "installed": str(target), "sha256": digest})
        shutil.copy2(Path(__file__).resolve(), out / "smoke-source.py")
        # macOS clonefile-backed cp avoids a duplicate model download and never
        # gives the native process a writable path into the real user's cache.
        cache_inputs = cached_model(args.embedding_cache.resolve())
        cloned, _ = runner.run(["/bin/cp", "-cR", args.embedding_cache.resolve(), out / "embedding-cache"],
                               timeout=30, check=False)
        if cloned.returncode:
            require(not (out / "embedding-cache").exists(), "partial cache clone; inspect scratch before retry")
            runner.run(["/bin/cp", "-R", args.embedding_cache.resolve(), out / "embedding-cache"], timeout=30)
        for path in (out / "embedding-cache").rglob("*"):
            if path.is_symlink():
                require(path.resolve().is_relative_to(out / "embedding-cache"), "cache symlink escapes private clone")
        require(cached_model(out / "embedding-cache") == cache_inputs, "cloned embedding cache identity differs")
        versions = {}
        for name in ("mnemed", "mneme-mcp"):
            for flag in ("--version", "--help"):
                # The shipped MCP parser emits help as a nonzero diagnostic
                # and has no --version; its initialize serverInfo is captured
                # separately. Record this honestly rather than invent parity.
                result, _ = runner.run([prefix / "bin" / name, flag], timeout=5, check=False)
                (out / (name + flag + ".txt")).write_text(result.stdout + result.stderr, encoding="utf-8")
                versions[name + flag] = {"returncode": result.returncode,
                    "output_sha256": sha256(out / (name + flag + ".txt"))}
        revision, _ = runner.run(["git", "rev-parse", "HEAD"], cwd=REPO, timeout=5)
        write_json(out / "manifest.json", {"artifacts": artifacts, "python": sys.executable,
                   "python_version": sys.version, "versions": versions,
                   "checkout_head": revision.stdout.strip(),
                   "binary_build_revision": "not asserted; installed pair hashes and reopen behavior are evidence",
                   "embedding_cache_source": str(args.embedding_cache.resolve()),
                   "embedding_cache_private_clone": str(out / "embedding-cache"),
                   "embedding_cache_inputs": cache_inputs, "network_isolation": "not asserted",
                   "smoke_source_sha256": sha256(out / "smoke-source.py"),
                   "fixture_sha256": hashlib.sha256(canonical(FIXTURE).encode()).hexdigest(),
                   "provider_calls": 0, "token_mode": "none"})
        write_json(out / "fixture.json", FIXTURE)
        root, user_cwd = out / "project", out / "user-cwd"
        project_cwd, outside = root / "nested/subdirectory", out / "outside-project"
        for directory in (project_cwd, user_cwd, outside, out / "stores", root / ".mneme"):
            directory.mkdir(parents=True, mode=0o700)
        git_probe, _ = runner.run(["git", "rev-parse", "--show-toplevel"], cwd=user_cwd,
                                  timeout=5, check=False)
        require(git_probe.returncode != 0, "global service cwd must not inherit a project Git root")
        receipt["user_cwd_git_discovery"] = "none with explicit scratch Git ceiling"
        dbs = {"user": out / "stores/user.db", "project": root / ".mneme/codex-memory.db"}
        ids = {}
        for name, db in dbs.items():
            receipt["steps"].append({"name": name + "_init", "result": runner.cli(name, db, "capture", "init", init=True)})
            key = name + "_core"
            ids[key] = runner.cli(name, db, "capture", "add", "--input", "-",
                                  payload=canonical(FIXTURE[key]), cwd=user_cwd if name == "user" else root)["id"]
        ids["project_ordinary"] = runner.cli("project", dbs["project"], "capture", "add", "--input", "-",
                                            payload=canonical(FIXTURE["project_ordinary"]), cwd=root)["id"]
        receipt["seed_counts"] = {name: runner.cli(name, db, "status") for name, db in dbs.items()}
        ports = dict(zip(("user", "project"), free_ports()))
        for name in ("user", "project"):
            config = {"binary": str(prefix / "bin/mneme-mcp"), "port": ports[name],
                      "state_dir": str(out / (name + "-service-state")),
                      "working_directory": str(user_cwd if name == "user" else root)}
            if name == "user":
                config.update(database_name="user", database_path=str(dbs[name]))
            else:
                config["project_db"] = str(dbs[name])
            configs[name] = prefix / "config" / (name + "-service.json")
            write_json(configs[name], config)
        core_config = prefix / "config/core-hook.json"
        write_json(core_config, {"schema": "mneme.codex-core.config.v1",
                                "global_service": str(configs["user"]),
                                "global_database": str(dbs["user"]),
                                "projects": [{"root": str(root), "service_config": str(configs["project"])}]})
        receipt["stores"] = {key: str(value) for key, value in dbs.items()}
        receipt["ports"] = ports
        cold = hook(runner, core_config, outside, pace=False)
        require(not any(identifier in cold["context"] for identifier in ids.values()), "cold hook returned unavailable memory")
        require(cold["elapsed_ms"] < 2500, "cold passive hook exceeded short fail-open deadline")
        require(host_inventory(runner) == [], "passive core hook started a host")
        require(not any((out / (name + "-service-state")).exists() for name in configs),
                "passive hook wrote service state or attempted lifecycle locking")
        receipt["steps"].append({"name": "cold_hook_passive_fail_open", **cold})
        # Launchers, not hooks, own startup. Two actual front doors race one store.
        runner.hosts_may_be_live = True
        for index in range(2):
            clients.append(StdioClient(runner, configs["user"], "user-launcher-%d" % index))
        barrier = threading.Barrier(2)
        def cold_initialize(client):
            barrier.wait(timeout=3)
            client.initialize()
            state = json.loads((out / "user-service-state/mneme-codex-service.json").read_text())
            return state["pid"]
        with ThreadPoolExecutor(max_workers=2) as pool:
            pids = list(pool.map(cold_initialize, clients))
        require(pids[0] == pids[1], "parallel launchers did not share one PID")
        hosts = host_inventory(runner)
        require(len(hosts) == 1 and hosts[0]["pid"] == pids[0], "global cold start created extra hosts")
        receipt["parallel_cold_start"] = {"pids": pids, "single_host": True, "inventory": hosts}
        for client in clients:
            assert_catalog(client.tool("databases", {}), "user", dbs["user"])
            assert_core(client.tool("core", {"db": "user"}), ids["user_core"])
            catalog = client.request("tools/list")
            names = {row["name"] for row in catalog["tools"]}
            require({"core", "capture", "query", "get", "databases"} <= names, "operator catalog missing required tools")
        user = clients[0]
        refused = user.tool("core", {"db": "project"}, refused=True)
        require("unknown database" in refused, "wrong-store refusal was masked by another admission error")
        user.tool("query", {"db": "user", "text": "synthetic preference", "k": 0}, refused=True)
        query = user.tool("query", {"db": "user", "text": FIXTURE["user_core"]["summary"], "k": 1})
        require(ids["user_core"] in canonical(query), "bounded global query did not recall fixture")
        project = StdioClient(runner, configs["project"], "project-launcher")
        clients.append(project)
        project.initialize()
        assert_catalog(project.tool("databases", {}), "project", dbs["project"])
        assert_core(project.tool("core", {"db": "project"}), ids["project_core"])
        # The bridge derives db from user config, never from capture content.
        bridge = [sys.executable, prefix / "lib/memory.py", "--service-config", configs["user"], "--no-start"]
        runner.pace_cold("user bridge core")
        bridge_core, _ = runner.json(bridge + ["core"])
        assert_core(bridge_core, ids["user_core"])
        captured, _ = runner.json(bridge + ["capture", "--input", "-"], payload=canonical(FIXTURE["user_capture"]))
        require(captured.get("ok") is True and captured.get("readback_status") == "verified", "user capture readback failed")
        require(captured.get("db") == "user" and bridge_core.get("db") == "user",
                "bridge did not label config-derived user scope")
        ids["user_capture"] = captured["id"]
        replayed, _ = runner.json(bridge + ["capture", "--input", "-"], payload=canonical(FIXTURE["user_capture"]))
        require(replayed.get("id") == captured["id"] and replayed.get("replayed") is True, "capture retry did not replay user ID")
        captured_node = user.tool("get", {"db": "user", "id": ids["user_capture"], "body": True})
        require(captured_node.get("summary") == FIXTURE["user_capture"]["summary"]
                and captured_node.get("origin_commit") is None, "user native readback mismatch or project origin leakage")
        require(project.tool("status", {"db": "project"})["nodes"] == receipt["seed_counts"]["project"]["nodes"],
                "user bridge capture changed project count")
        receipt["bridge"] = {"core": bridge_core, "capture": captured, "replay": replayed,
                             "native_user_readback": captured_node}
        receipt["before_reads"] = {"user": user.tool("status", {"db": "user"}),
                                   "project": project.tool("status", {"db": "project"})}
        before_hosts = host_inventory(runner)
        assert_core(user.tool("core", {"db": "user"}), ids["user_core"])
        rate_limited = hook(runner, core_config, outside, pace=False)
        require(ids["user_core"] not in rate_limited["context"]
                and '"outcome":"unavailable"' in rate_limited["context"],
                "immediate hook did not report the native 1-second cold-work refusal")
        receipt["steps"].append({"name": "native_cold_rate_limit_fail_open", **rate_limited})
        for source in ("startup", "resume", "clear", "compact"):
            row = hook(runner, core_config, project_cwd, source=source)
            assert_context(row, ids, project=True)
            receipt["steps"].append({"name": "project_subdirectory_" + source, **row})
        for cwd in (outside, root.with_name(root.name + "-lookalike")):
            row = hook(runner, core_config, cwd)
            assert_context(row, ids, project=False)
            receipt["steps"].append({"name": "outside_project_global_only", **row})
        ignored = hook(runner, core_config, project_cwd, extra={"agent_type": "subagent"})
        require(ignored["output"] == {}, "subagent event was not ignored")
        receipt["steps"].append({"name": "subagent_ignored", **ignored})
        assert_core(user.tool("core", {"db": "user"}), ids["user_core"])
        assert_core(project.tool("core", {"db": "project"}), ids["project_core"])
        receipt["after_reads"] = {"user": user.tool("status", {"db": "user"}),
                                  "project": project.tool("status", {"db": "project"})}
        require(receipt["before_reads"] == receipt["after_reads"], "read paths changed native store counts")
        require(host_inventory(runner) == before_hosts, "passive read paths changed host PIDs")
        receipt["read_paths_counts_unchanged"] = True
        receipt["read_paths_host_pids_unchanged"] = True
        receipt["ids"] = ids
    except Exception as error:
        failure = "%s: %s" % (type(error).__name__, error)
        receipt["error"] = failure
    finally:
        for client in clients:
            try:
                client.close()
                receipt["cleanup"].append({"launcher": client.name, "closed": True})
            except Exception as error:
                receipt["cleanup"].append({"launcher": client.name, "error": str(error)})
                failure = failure or "launcher cleanup failed"
        # Stop only hosts with our exact config/process identity. Never SIGKILL a
        # host or delete a lease file to make a failed smoke appear green.
        stopped = True
        for name, config in configs.items():
            try:
                value, _ = runner.json([sys.executable, prefix / "lib/service.py", "--config", config, "stop"],
                                       timeout=45, cleanup=True)
                require(value.get("state") == "stopped", "service did not stop")
                receipt["cleanup"].append({"service": name, "result": value})
            except Exception as error:
                stopped = False
                receipt["cleanup"].append({"service": name, "error": str(error)})
                failure = failure or "service cleanup failed"
        # A failed launcher startup can leave a detached native host before its
        # state is published. A stopped service receipt alone cannot exclude
        # that orphan; inspect the exact private binary even on failure, with
        # an independent cleanup deadline, and report rather than blindly kill.
        try:
            receipt["final_host_inventory"] = host_inventory(runner, cleanup=True)
            if receipt["final_host_inventory"]:
                stopped = False
                failure = failure or "private native hosts survived cleanup; inspect final_host_inventory"
        except Exception as error:
            stopped = False
            failure = failure or "final host inventory failed: %s" % error
        runner.hosts_may_be_live = not stopped
        if stopped and not failure:
            try:
                row = hook(runner, core_config, project_cwd, pace=False)
                require(not any(identifier in row["context"] for identifier in ids.values()), "stopped hook emitted core IDs")
                require(row["elapsed_ms"] < 2500, "stopped hook exceeded short fail-open deadline")
                require(host_inventory(runner) == [], "stopped passive hook restarted a host")
                receipt["steps"].append({"name": "stopped_hosts_passive_fail_open", **row})
                receipt["offline_counts"] = {name: runner.cli(name, db, "status") for name, db in dbs.items()}
                for name, db in dbs.items():
                    offline = runner.cli(name, db, "core")
                    # The existing CLI core format is an array, whereas MCP
                    # uses a bounded native envelope; compare the shared IDs.
                    require(isinstance(offline, list)
                            and [node.get("id") for node in offline] == [ids[name + "_core"]],
                            "CLI core readback differs from native MCP core")
                require(receipt["offline_counts"] == receipt["after_reads"], "offline reopen counts mismatch")
                receipt["offline_user_capture"] = runner.cli("user", dbs["user"], "get", ids["user_capture"], "--body")
                require(receipt["offline_user_capture"].get("summary") == FIXTURE["user_capture"]["summary"],
                        "offline user capture readback mismatch")
                receipt["clean_stop_and_cli_reopen"] = True
                receipt["matched_pair_persistent_reopen"] = True
                receipt["result"] = "passed"
            except Exception as error:
                failure = "%s: %s" % (type(error).__name__, error)
                receipt["error"] = failure
        if failure:
            receipt["error"] = failure
        receipt["commands"] = runner.commands
        receipt["explicit_cold_work_pacing"] = runner.pacing
        write_json(out / "receipt.json", receipt)
        write_json(out / "inventory.json", inventory(out))
    print(canonical({"result": receipt["result"], "receipt": str(out / "receipt.json"),
                     "inventory": str(out / "inventory.json"), "error": failure}))
    return 0 if receipt["result"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())

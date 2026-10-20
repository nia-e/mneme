#!/usr/bin/env python3
"""Finite installed-Codex stable-hook proof, entirely inside a disposable home.

No real credentials, provider, plugins, MCP, services or stores are used. The
single prompt hook blocks every turn before inference. Runtime publications are
fixture files only. Native trust is granted exactly once, before thread creation.
"""
from __future__ import annotations

import argparse
import contextlib
import datetime
import hashlib
import http.server
import importlib.util
import json
import os
from pathlib import Path
import re
import select
import shlex
import signal
import subprocess
import sys
import tempfile
import threading
import time


RUNTIME = '''import argparse,json
from pathlib import Path
import sys
parser=argparse.ArgumentParser()
parser.add_argument("--config",required=True)
args=parser.parse_args()
config=json.loads(Path(args.config).read_text())
event=json.load(sys.stdin)
with Path(config["events"]).open("a") as stream:
 stream.write(json.dumps({{"runtime":{marker!r},"config":config["tag"],
  "event":event.get("hook_event_name"),"turn_id":event.get("turn_id")}})+"\\n")
print(json.dumps({{"decision":"block","reason":"Stable-hook no-provider fixture"}}))
'''


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


class DenyProvider(http.server.BaseHTTPRequestHandler):
    requests = 0

    def do_POST(self):
        type(self).requests += 1
        self.send_error(503, "Fixture forbids inference")

    do_GET = do_POST

    def log_message(self, *_args):
        pass


class DenyServer(http.server.ThreadingHTTPServer):
    daemon_threads = True
    block_on_close = False
    connections = 0

    def get_request(self):
        connection, address = super().get_request()
        self.connections += 1
        connection.settimeout(2)
        return connection, address


class Rpc:
    """Sequential, allowlisted, byte/frame/time-bounded disposable RPC."""

    METHODS = frozenset(("initialize", "hooks/list", "config/read", "config/batchWrite",
                         "thread/start", "turn/start"))

    def __init__(self, process):
        self.process = process
        self.buffer = b""
        self.frames = []
        self.calls = []
        self.ident = self.total = 0

    def receive(self, deadline):
        while b"\n" not in self.buffer:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([self.process.stdout], [], [], remaining)[0]:
                return None
            block = os.read(self.process.stdout.fileno(), 65536)
            if not block:
                raise RuntimeError("isolated app-server closed")
            self.total += len(block)
            self.buffer += block
            if len(self.buffer) > 2 * 1024 * 1024 or self.total > 16 * 1024 * 1024:
                raise RuntimeError("response byte bound exceeded")
        raw, self.buffer = self.buffer.split(b"\n", 1)
        value = json.loads(raw)
        self.frames.append(value)
        if len(self.frames) > 768:
            raise RuntimeError("response frame bound exceeded")
        if "id" in value and "method" in value:
            raise RuntimeError("server requested an action; no approval granted")
        return value

    def request(self, method, params):
        if method not in self.METHODS:
            raise ValueError("RPC outside fixture allowlist")
        self.ident += 1
        self.calls.append({"method": method, "params": params})
        raw = json.dumps({"id": self.ident, "method": method, "params": params}).encode() + b"\n"
        if len(raw) > 65536:
            raise RuntimeError("request byte bound exceeded")
        self.process.stdin.write(raw)
        deadline = time.monotonic() + 20
        while True:
            value = self.receive(deadline)
            if value is None:
                raise TimeoutError(method)
            if value.get("id") == self.ident:
                if "error" in value:
                    raise RuntimeError(value["error"])
                return value["result"]

    def blocked_turn(self, thread, sequence):
        start = len(self.frames)
        result = self.request("turn/start", {"threadId": thread,
            "input": [{"type": "text", "text": f"blocked fixture turn {sequence}"}]})
        turn = result["turn"]["id"]
        deadline = time.monotonic() + 15
        while not any(f.get("method") == "turn/completed"
                      and f["params"]["turn"]["id"] == turn for f in self.frames[start:]):
            if self.receive(deadline) is None:
                raise TimeoutError("blocked turn completion")
        blocked = [f for f in self.frames[start:] if f.get("method") == "hook/completed"
                   and f["params"]["run"]["status"] == "blocked"]
        if len(blocked) != 1:
            raise AssertionError(f"expected one blocked hook, found {len(blocked)}")
        return {"thread_id": thread, "turn_id": turn, "blocked_hooks": len(blocked)}


def load_helper(path):
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location("stable_hook_fixture_helper", path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    sys.path.insert(0, str(path.parent))
    try:
        spec.loader.exec_module(module)
    finally:
        sys.path.pop(0)
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", required=True, type=Path)
    parser.add_argument("--helper", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--selector-kind", choices=("text", "symlink"), default="symlink")
    args = parser.parse_args()
    codex = args.codex.resolve(strict=True)
    helper_path = args.helper.resolve(strict=True)
    helper = load_helper(helper_path)
    args.out.mkdir(parents=True, exist_ok=False)
    receipt = {"schema": "mneme.native-stable-hook-proof.v1", "passed": False,
               "at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
               "codex": str(codex), "codex_sha256": digest(codex.read_bytes()),
               "helper": str(helper_path), "helper_sha256": digest(helper_path.read_bytes()),
               "probe_sha256": digest(Path(__file__).read_bytes()),
               "selector_kind": args.selector_kind,
               "phases": [], "processes": []}
    DenyProvider.requests = 0
    provider = DenyServer(("127.0.0.1", 0), DenyProvider)
    threading.Thread(target=provider.serve_forever, daemon=True).start()
    all_calls = []
    fixture_base = None
    completed = False
    try:
        with tempfile.TemporaryDirectory(prefix="mneme-stable-hook-proof-") as tmp:
            base = Path(tmp).resolve()
            fixture_base = str(base)
            home, codex_home, cwd = base / "home", base / "codex-home", base / "cwd"
            for path in (home, codex_home, cwd, base / "tmp"):
                path.mkdir(mode=0o700)
            env = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
                   "HOME": str(home), "CODEX_HOME": str(codex_home),
                   "TMPDIR": str(base / "tmp"),
                   "GIT_CEILING_DIRECTORIES": str(base),
                   "LANG": "en_US.UTF-8"}
            for name in ("CONFIG", "DATA", "CACHE", "STATE", "RUNTIME"):
                path = base / ("xdg-" + name.lower())
                path.mkdir(mode=0o700)
                env["XDG_" + name + "_HOME" if name != "RUNTIME" else "XDG_RUNTIME_DIR"] = str(path)
            receipt["environment_keys"] = sorted(env)
            receipt["version"] = subprocess.run([str(codex), "--version"], cwd=cwd, env=env,
                check=True, text=True, capture_output=True, timeout=10).stdout.strip()
            selector, shim = base / "current", base / "stable-hook"
            runtime_config, events = base / "runtime-config.json", base / "events.jsonl"
            runtime_config.write_text(json.dumps({"tag": "config-a", "events": str(events)}))
            programs, wrappers = {}, {}
            for name in ("a", "b"):
                directory = base / ("runtime-" + name)
                directory.mkdir(mode=0o700)
                program, wrapper = directory / "hook_launcher.py", directory / "invoke"
                program.write_text(RUNTIME.format(marker=name + "1"))
                wrapper.write_bytes(helper.render_runtime(Path(sys.executable), program, runtime_config))
                wrapper.chmod(0o700)
                programs[name], wrappers[name] = program, wrapper
            shim.write_bytes(helper.render_entrypoint(selector))
            shim.chmod(0o700)

            def select_wrapper(wrapper):
                temporary = base / "current.next"
                if args.selector_kind == "symlink":
                    temporary.symlink_to(wrapper)
                else:
                    temporary.write_text(str(wrapper) + "\n")
                os.replace(temporary, selector)

            def selector_snapshot():
                raw = (os.readlink(selector).encode() if selector.is_symlink()
                       else selector.read_bytes())
                return {"kind": args.selector_kind, "target": raw.decode().rstrip("\n"),
                        "sha256": digest(raw)}

            select_wrapper(wrappers["a"])
            baseline_shim = shim.read_bytes()
            command = shlex.join(["/bin/sh", str(shim)])
            config = codex_home / "config.toml"
            config.write_text(
                'model="fixture-no-provider"\nmodel_provider="fixture"\nproject_doc_max_bytes=0\n'
                'cli_auth_credentials_store="file"\n'
                '[model_providers.fixture]\nname="No-provider stable-hook fixture"\n'
                f'base_url="http://127.0.0.1:{provider.server_port}/v1"\n'
                'wire_api="responses"\nrequires_openai_auth=false\n'
                '[features]\nhooks=true\nplugins=false\napps=false\nmemories=false\n'
                '[analytics]\nenabled=false\n[hooks]\n'
                'UserPromptSubmit=[{hooks=[{type="command",command='
                + json.dumps(command) + ',timeout=5}]}]\n')
            receipt["recipe"] = {"event": "UserPromptSubmit", "command": command,
                                 "timeout": 5, "async": False}
            receipt["entrypoint"] = {"sha256": digest(baseline_shim),
                                      "text": baseline_shim.decode()}

            @contextlib.contextmanager
            def server():
                index = len(receipt["processes"])
                with open(os.devnull, "wb") as stderr:
                    process = subprocess.Popen([str(codex), "app-server", "--listen", "stdio://"],
                        cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=stderr, bufsize=0, start_new_session=True)
                    rpc = Rpc(process)
                    process_record = {"index": index, "pid": process.pid}
                    receipt["processes"].append(process_record)
                    try:
                        rpc.request("initialize", {
                            "clientInfo": {"name": "mneme_stable_hook_fixture", "version": "1"},
                            "capabilities": {"experimentalApi": True, "requestAttestation": False}})
                        process.stdin.write(b'{"method":"initialized"}\n')
                        effective = rpc.request("config/read", {
                            "includeLayers": True, "cwd": str(cwd)})["config"]
                        # Verify effective authority before creating any thread,
                        # not merely the isolated user file's intended settings.
                        assert effective.get("model_provider") == "fixture", "provider overridden"
                        assert effective.get("cli_auth_credentials_store") == "file", "auth storage overridden"
                        assert not effective.get("mcp_servers"), "unexpected effective MCP servers"
                        features = effective.get("features", {})
                        for feature in ("plugins", "apps", "memories"):
                            assert features.get(feature) is False, f"{feature} isolation overridden"
                        effective_provider = effective.get("model_providers", {}).get("fixture", {})
                        assert effective_provider.get("base_url") == (
                            f"http://127.0.0.1:{provider.server_port}/v1"), "provider URL overridden"
                        assert effective_provider.get("requires_openai_auth") is False, "provider auth overridden"
                        process_record["effective_isolation_verified"] = True
                        yield rpc
                    finally:
                        all_calls.extend(rpc.calls)
                        process_record["calls"] = [call["method"] for call in rpc.calls]
                        process_record["received_frames"] = len(rpc.frames)
                        process_record["received_bytes"] = rpc.total
                        process_record["hook_completed"] = [
                            f["params"] for f in rpc.frames if f.get("method") == "hook/completed"]
                        try:
                            os.killpg(process.pid, signal.SIGTERM)
                        except ProcessLookupError:
                            pass
                        try:
                            process.wait(timeout=3)
                        except subprocess.TimeoutExpired:
                            try:
                                os.killpg(process.pid, signal.SIGKILL)
                            except ProcessLookupError:
                                pass
                            process.wait(timeout=3)
                        # The leader may exit before its descendants. Always
                        # retire the owned group independently of leader state.
                        try:
                            os.killpg(process.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        process.stdin.close()
                        process.stdout.close()
                        process_record["stderr_discarded"] = True
                        process_record["process_group_retired"] = True

            def catalog(rpc):
                result = rpc.request("hooks/list", {"cwds": [str(cwd)]})["data"]
                assert len(result) == 1 and not result[0].get("errors"), result
                rows = result[0]["hooks"]
                assert len(rows) == 1, rows
                row = rows[0]
                assert row["command"] in (command, command + " --control"), row
                return row

            def new_thread(rpc):
                return rpc.request("thread/start", {"model": "fixture-no-provider", "cwd": str(cwd),
                    "approvalPolicy": "never", "sandbox": "read-only",
                    "baseInstructions": "Isolated blocked-turn diagnostic only.",
                    "ephemeral": True})["thread"]["id"]

            baseline = None
            baseline_native_config = None

            def observe(rpc, name, *, thread=None, expected=None, stable=True):
                row = catalog(rpc)
                phase = {"name": name, "catalog": row,
                         "shim_sha256": digest(shim.read_bytes()),
                         "selector": selector_snapshot(),
                         "runtime_config_sha256": digest(runtime_config.read_bytes()),
                         "runtime_sha256": {k: digest(v.read_bytes()) for k, v in programs.items()}}
                receipt["phases"].append(phase)
                if stable:
                    assert (row["key"], row["currentHash"], row["trustStatus"], row["enabled"]) == (
                        baseline["key"], baseline["currentHash"], "trusted", True), phase
                if thread is not None:
                    phase["dispatch"] = rpc.blocked_turn(thread, name)
                    rows = [json.loads(line) for line in events.read_text().splitlines()]
                    phase["event"] = rows[-1]
                    assert (rows[-1]["runtime"], rows[-1]["config"]) == expected, phase
                    assert rows[-1]["turn_id"] == phase["dispatch"]["turn_id"], phase
                assert DenyProvider.requests == 0, "unexpected local inference request"
                return phase

            with server() as rpc:
                before = catalog(rpc)
                assert before["trustStatus"] == "untrusted" and before["enabled"], before
                receipt["before_trust"] = before
                rpc.request("config/batchWrite", {"edits": [{
                    "keyPath": "hooks.state." + json.dumps(before["key"]) + ".trusted_hash",
                    "value": before["currentHash"], "mergeStrategy": "replace"}],
                    "filePath": str(config), "reloadUserConfig": True})
                baseline = catalog(rpc)
                assert baseline["trustStatus"] == "trusted", baseline
                baseline_native_config = config.read_bytes()
                thread = new_thread(rpc)
                observe(rpc, "baseline", thread=thread, expected=("a1", "config-a"))
                programs["a"].write_text(RUNTIME.format(marker="a2"))
                observe(rpc, "runtime_bytes_changed", thread=thread, expected=("a2", "config-a"))
                select_wrapper(wrappers["b"])
                observe(rpc, "selector_target_changed", thread=thread, expected=("b1", "config-a"))
                runtime_config.write_text(json.dumps({"tag": "config-b", "events": str(events)}))
                observe(rpc, "runtime_config_changed", thread=thread, expected=("b1", "config-b"))
                assert config.read_bytes() == baseline_native_config, "native config changed during proof"
                assert shim.read_bytes() == baseline_shim, "shim changed during stable proof"
            with server() as rpc:
                observe(rpc, "fresh_server", thread=new_thread(rpc), expected=("b1", "config-b"))
            # Controls happen after the stable proof, never retrust or reload.
            shim.write_bytes(baseline_shim + b"\n# native hash sensitivity control\n")
            with server() as rpc:
                observe(rpc, "control_shim_bytes", stable=False)
            shim.write_bytes(baseline_shim)
            changed = baseline_native_config.decode().replace(command, command + " --control")
            assert changed != baseline_native_config.decode()
            config.write_text(changed)
            with server() as rpc:
                phase = observe(rpc, "control_command_argument", stable=False)
                assert phase["catalog"]["currentHash"] != baseline["currentHash"], phase
                assert phase["catalog"]["trustStatus"] != "trusted", phase
            changed, count = re.subn(r"timeout\s*=\s*5\b", "timeout=6", baseline_native_config.decode())
            assert count == 1, count
            config.write_text(changed)
            with server() as rpc:
                phase = observe(rpc, "control_timeout_definition", stable=False)
                assert phase["catalog"]["currentHash"] != baseline["currentHash"], phase
                assert phase["catalog"]["trustStatus"] != "trusted", phase
            config.write_bytes(baseline_native_config)
            with server() as rpc:
                observe(rpc, "restored_definition")
            writes = [call for call in all_calls if call["method"] == "config/batchWrite"]
            assert len(writes) == 1, writes
            receipt["native_config_writes"] = writes
            receipt["events"] = [json.loads(line) for line in events.read_text().splitlines()]
            assert len(receipt["events"]) == 5, receipt["events"]
            completed = True
    except BaseException as exc:
        receipt["error"] = {"type": type(exc).__name__, "message": str(exc)}
        raise
    finally:
        provider.shutdown()
        provider.server_close()
        receipt["provider_requests"] = DenyProvider.requests
        receipt["provider_connections"] = provider.connections
        # shutdown() joins the accept loop. Zero accepted connections also
        # excludes a late handler increment after this final observation.
        receipt["passed"] = completed and DenyProvider.requests == 0 and provider.connections == 0
        raw = json.dumps(receipt, indent=2)
        if fixture_base is not None:
            raw = raw.replace(fixture_base, "<fixture>")
        raw = raw.replace(f"http://127.0.0.1:{provider.server_port}", "http://127.0.0.1:<port>")
        (args.out / "receipt.json").write_text(raw + "\n")
    assert receipt["passed"], "unexpected late local inference request"
    print(json.dumps({"passed": True, "provider_requests": DenyProvider.requests,
                      "receipt": str(args.out / "receipt.json")}))


if __name__ == "__main__":
    main()

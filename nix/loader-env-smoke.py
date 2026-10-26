#!/usr/bin/env python3
"""Installed Linux loader isolation, including disposable native init/lifecycle.

No enrolled configuration, credentials, models or stores are inherited. The only
owner created belongs to this temporary fixture; failed graceful cleanup retains
the directory rather than deleting a possibly live database or forcing a kill.
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib


LIMIT = 1024 * 1024


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def run(command, env, cwd, *, timeout=30, success=True):
    """Bound both output streams while the finite child is running."""
    command = list(map(str, command))
    output = {"stdout": bytearray(), "stderr": bytearray()}
    with subprocess.Popen(command, env=env, cwd=cwd, stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE) as child:
        deadline = time.monotonic() + timeout
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(child.stdout, selectors.EVENT_READ, "stdout")
                selector.register(child.stderr, selectors.EVENT_READ, "stderr")
                while selector.get_map():
                    remaining = deadline - time.monotonic()
                    require(remaining > 0, f"command timed out: {command!r}")
                    for key, _ in selector.select(remaining):
                        chunk = os.read(key.fileobj.fileno(), 65536)
                        if not chunk:
                            selector.unregister(key.fileobj)
                            continue
                        output[key.data].extend(chunk)
                        require(len(output[key.data]) <= LIMIT,
                                f"command output exceeded limit: {command!r}")
            status = child.wait(timeout=max(0.1, deadline - time.monotonic()))
        except BaseException:
            # This is the foreground command, never a recorded fixture owner.
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)
            raise
    result = subprocess.CompletedProcess(command, status,
        output["stdout"].decode(errors="replace"), output["stderr"].decode(errors="replace"))
    if success is not None:
        require((status == 0) == success,
                f"exit {status}: {command!r}\n{result.stdout[-8192:]}\n{result.stderr[-8192:]}")
    return result


def metadata(helper, prefix, binaries, features):
    value = helper.installed_metadata(prefix, binaries, features)
    require(value.get("loaderEnvironment") == {
        "unset": ["LD_LIBRARY_PATH"], "inheritedByChildren": True,
        "nativeBinaryDirectory": "libexec/mneme"}, "wrong loader isolation metadata")
    expected = {f"libexec/mneme/{name}" for name in binaries}
    seen = set()
    for line in (prefix / "share/mneme/native-binary-sha256.txt").read_text().splitlines():
        digest, name = line.split()
        require(name in expected and name not in seen, "unexpected native hash inventory")
        require(re.fullmatch(r"[0-9a-f]{64}", digest)
                and helper.digest(prefix / name) == digest, f"native hash mismatch: {name}")
        with (prefix / name).open("rb") as binary:
            require(binary.read(4) == b"\x7fELF", "native payload is not ELF")
        require(helper.digest(prefix / "bin" / Path(name).name) != digest,
                "public executable is still the unwrapped payload")
        seen.add(name)
    require(seen == expected, "incomplete native hash inventory")
    require({p.name for p in (prefix / "libexec/mneme").iterdir()} == set(binaries),
            "unexpected native payload inventory")
    return value


def configured_stdio(helper, command, env, cwd, version, *, db_id=None):
    with helper.Stdio(list(map(str, command)), env, cwd) as rpc:
        helper.initialize(rpc, version, "configured-databases")
        listed = rpc.request("tools/list", {})
        names = {tool["name"] for tool in listed["tools"]}
        require({"databases", "config_select"} <= names, "configured metadata tools absent")
        selected = helper.tool(rpc, "config_select", {})
        require(selected.get("digest"), "configuration selection has no digest")
        if db_id is None:
            require(selected["active"] == [], "empty fixture unexpectedly selected an owner")
        else:
            require([row["store"]["id"] for row in selected["active"]] == [db_id],
                    "project launcher selected a different owner")
        rpc.finish()


def python_probe(root):
    executable = root / "python-probe"
    log = root / "python-environment.jsonl"
    executable.write_text(f"#!{sys.executable}\n" +
        "import json, os, sys\n" +
        f"with open({str(log)!r}, 'a') as stream:\n" +
        "    stream.write(json.dumps({key: os.environ.get(key) for key in "
        "('LD_LIBRARY_PATH', 'LD_PRELOAD', 'PATH', 'MNEME_LOADER_SMOKE_SENTINEL')}) + '\\n')\n" +
        f"os.execv({sys.executable!r}, [{sys.executable!r}, *sys.argv[1:]])\n")
    executable.chmod(0o700)
    return executable, log


def service_action(project, action, env):
    prefix = project / ".mneme/codex-integration"
    result = run([sys.executable, "-B", prefix / "lib/service.py", "--config",
                  prefix / "config/service.json", action], env, project, timeout=50)
    return json.loads(result.stdout)


def process_running(pid):
    # Independent of service.py's argv witness: a broken wrapper argv0 can make
    # that helper report stale while its actual child still holds the database.
    try:
        state = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0]
        return state != "Z"
    except FileNotFoundError:
        return False


def init_and_lifecycle(helper, args, root, clean, foreign, version):
    project = root / "project"
    project.mkdir(mode=0o700)
    probe, probe_log = python_probe(root)
    prefix = project / ".mneme/codex-integration"
    cli_prefix = args.fixture_cli.parent.parent
    mcp = args.replica / "bin/mneme-mcp"
    command = [args.fixture_cli, "--json", "init", "--root", project,
               "--no-register", "--python", probe, "--mcp-binary", mcp]
    cleanup_verified = False
    owner_pids = set()
    try:
        first = json.loads(run(command, foreign, project, timeout=210).stdout)
        require(first["status"] == "configured" and first["integration"] == "native",
                f"native init did not complete: {first!r}")
        require(first["published"] is False, "fixture init published discovery")
        db_id = first["db_id"]
        observed = [json.loads(line) for line in probe_log.read_text().splitlines()]
        require(observed, "explicit Python probe was not executed")
        for entry in observed:
            require(entry == {"LD_LIBRARY_PATH": None, "LD_PRELOAD": "",
                              "PATH": foreign["PATH"],
                              "MNEME_LOADER_SMOKE_SENTINEL": "retained"},
                    f"wrapper changed other environment or leaked loader path: {entry!r}")
        plan = json.loads((prefix / "receipt.json").read_text())["plan"]
        sources = {row["destination"]: row["source"] for row in plan["files"]}
        require(sources["bin/mnemed"] == str(cli_prefix / "libexec/mneme/mnemed"),
                "init current_exe is not the packaged native payload")
        require(sources["bin/mneme-mcp"] == str(mcp), "MCP install source lost wrapper")
        require(plan["native_command"] == "mnemed" and plan["mcp_command"] == str(mcp),
                "artifact source leaked into runtime command selectors")
        service = json.loads((prefix / "config/service.json").read_text())
        hooks = json.loads((prefix / "config/hooks.json").read_text())
        require(service["binary"] == str(mcp) and hooks["native_cli"] == "mnemed",
                "persistent runtime selectors bypass wrappers")
        initial = service_action(project, "status", foreign)
        if "pid" in initial:
            owner_pids.add(initial["pid"])
        require(initial["state"] == "ready", f"wrapper argv0 broke service identity: {initial!r}")
        second = json.loads(run(command, foreign, project, timeout=210).stdout)
        resumed = service_action(project, "status", foreign)
        require(second["status"] == "configured" and second["db_id"] == db_id
                and resumed["state"] == "ready" and resumed["pid"] == initial["pid"],
                "rerun changed project identity or owner process")
        require(service_action(project, "stop", foreign)["state"] == "stopped",
                "installed service helper did not stop its owner")
        require(service_action(project, "status", foreign)["state"] == "stopped",
                "fixture owner still recorded after stop")
        require(not process_running(initial["pid"]), "stopped owner process is still running")
        config = tomllib.loads((project / ".codex/config.toml").read_text())
        server = config["mcp_servers"][plan["native_managed"]["mcp"]["name"]]
        # Execute the actually installed entrypoint, not a hand-built equivalent.
        configured_stdio(helper, [server["command"], *server["args"]], foreign,
                         project, version, db_id=db_id)
        restarted = service_action(project, "status", foreign)
        if "pid" in restarted:
            owner_pids.add(restarted["pid"])
        require(restarted["state"] == "ready", "installed project launcher did not restart owner")
        require(restarted["pid"] != initial["pid"], "stopped owner unexpectedly reused its PID")
        print("native init: retained identity/PID on retry; installed launcher restarted owner", flush=True)
    finally:
        if (prefix / "config/service.json").is_file() and (prefix / "lib/service.py").is_file():
            try:
                state_path = prefix / "run/mneme-codex-service.json"
                if state_path.is_file():
                    owner_pids.add(json.loads(state_path.read_text())["pid"])
                stopped = service_action(project, "stop", clean)
                status = service_action(project, "status", clean)
                cleanup_verified = (stopped["state"] == status["state"] == "stopped"
                                    and not any(process_running(pid) for pid in owner_pids))
            except Exception as error:
                print(f"fixture graceful cleanup failed: {error}", file=sys.stderr)
        if not cleanup_verified:
            # main must not remove a database while an unverified owner may exist.
            (root / "KEEP-FIXTURE").write_text("Owner cleanup not verified; retain data and inspect service state.\n")
            print(f"retained fixture; no owner force-kill or database deletion: {root}", file=sys.stderr)
        require(cleanup_verified, f"fixture owner cleanup could not be verified: {root}")


def smoke(helper, args, root):
    client = metadata(helper, args.client, ["mnemed", "mneme-mcp"], [])
    replica = metadata(helper, args.replica, ["mneme-mcp"], ["cozo", "fastembed", "http"])
    fixture = metadata(helper, args.fixture_cli.parent.parent, ["mnemed"], ["cozo", "fastembed"])
    require(client["version"] == replica["version"] == fixture["version"], "package versions differ")
    clean = helper.isolated_env(root)
    ps = shutil.which("ps")
    require(ps, "ps is required for exact fixture owner identity checks")
    tool_bin = root / "tool-bin"
    tool_bin.mkdir(mode=0o700)
    codex_called = root / "UNEXPECTED-CODEX-CALL"
    codex = tool_bin / "codex"
    codex.write_text(f"#!{sys.executable}\nfrom pathlib import Path\n"
                     f"Path({str(codex_called)!r}).touch()\nraise SystemExit(97)\n")
    codex.chmod(0o700)
    clean["PATH"] = os.pathsep.join(dict.fromkeys(map(str, [tool_bin, args.fixture_cli.parent,
        args.replica / "bin", Path(sys.executable).parent, Path(ps).parent])))
    clean.update(CODEX_HOME=str(root / "codex"), LD_PRELOAD="", MNEME_LOADER_SMOKE_SENTINEL="retained")
    (root / "codex").mkdir(mode=0o700)
    memory = Path(clean["XDG_CONFIG_HOME"]) / "mneme/memory.toml"
    memory.parent.mkdir(mode=0o700)
    memory.write_text('schema = "mneme.config.v1"\n')
    foreign = {**clean, "LD_LIBRARY_PATH": str(args.foreign_library_dir)}
    for env in (clean, foreign):
        for cli in (args.client / "bin/mnemed", args.fixture_cli):
            require(run([cli, "--version"], env, root).stdout.strip() == f"mnemed {client['version']}",
                    "wrapped CLI version failed")
        for mcp in (args.client / "bin/mneme-mcp", args.replica / "bin/mneme-mcp"):
            configured_stdio(helper, [mcp, "--memory-config", memory, "--cwd", root],
                             env, root, client["version"])
    raw = args.fixture_cli.parent.parent / "libexec/mneme/mnemed"
    run([raw, "--version"], clean, root)
    counterfactual = run([raw, "--version"], foreign, root, success=None)
    if counterfactual.returncode:
        require("GLIBCXX_" in counterfactual.stderr and "not found" in counterfactual.stderr,
                f"raw counterfactual failed for another reason: {counterfactual.stderr[-8192:]}")
        versions = sorted(set(re.findall(r"GLIBCXX_[0-9.]+", counterfactual.stderr)))
        print("raw counterfactual: incompatible foreign runtime reproduced " + ", ".join(versions), flush=True)
    else:
        print("raw counterfactual: provided foreign runtime is compatible; no negative ABI reproduction", flush=True)
    init_and_lifecycle(helper, args, root, clean, foreign, fixture["version"])
    require(not codex_called.exists(), "native setup unexpectedly invoked Codex")
    require(not list((root / "models").rglob("*")), "smoke unexpectedly populated model cache")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("client", "replica", "fixture-cli", "foreign-library-dir", "smoke-helper"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    for name, value in vars(args).items():
        setattr(args, name, value.resolve(strict=True))
    require(sys.platform.startswith("linux"), "loader smoke requires Linux")
    require((args.foreign_library_dir / "libstdc++.so.6").is_file(), "foreign C++ runtime missing")
    spec = importlib.util.spec_from_file_location("mneme_replica_smoke", args.smoke_helper)
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    root = Path(tempfile.mkdtemp(prefix="mneme-loader-smoke-")).resolve()
    try:
        smoke(helper, args, root)
    finally:
        if not (root / "KEEP-FIXTURE").exists():
            shutil.rmtree(root)
    print("loader environment smoke passed: four entrypoints, manifests, child environment and native lifecycle")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Finite isolated install/producer/CLI/stdio smoke for session activity.

Uses supplied binaries and the repository installer, never builds, starts an
owner, initializes a graph, trusts hooks, or changes the caller's configuration.
The synthetic install, HOME/XDG paths and diagnostic data are disposable. The
optional receipt retains identities and check names, not subprocess payloads.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
from mcp_stdio_smoke import read_response, send, tool_json  # noqa: E402


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def artifact(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return {"name": path.name, "bytes": path.stat().st_size,
            "sha256": digest.hexdigest()}


def inventory(root):
    """Fixture-only inventory. Never follow links or inspect an enrolled store."""
    result = []
    for path in sorted(root.rglob("*")):
        name = str(path.relative_to(root))
        if path.is_symlink():
            result.append((name, "link", os.readlink(path)))
        elif path.is_file():
            result.append((name, "file", artifact(path)["sha256"], path.stat().st_mode))
        else:
            result.append((name, "directory", path.stat().st_mode))
    return result


class Smoke:
    def __init__(self, options, root):
        self.options, self.root = options, root
        self.deadline = time.monotonic() + options.timeout
        self.checks = []
        self.children = []
        self.installation = None
        self.protocol = None
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("MNEME_", "XDG_", "HF_", "HUGGINGFACE_"))}
        self.env.update({"HOME": str(root / "home"),
                         "XDG_CONFIG_HOME": str(root / "config"),
                         "XDG_DATA_HOME": str(root / "data"),
                         "XDG_CACHE_HOME": str(root / "cache"),
                         "XDG_STATE_HOME": str(root / "state"),
                         "PYTHONDONTWRITEBYTECODE": "1", "HF_HUB_OFFLINE": "1",
                         "TRANSFORMERS_OFFLINE": "1", "MNEME_RERANK": "0",
                         "HF_ENDPOINT": "http://127.0.0.1:9"})
        self.project = root / "project"
        self.cwd = self.project / "nested"
        self.prefix = root / "runtime"
        self.activity = root / "activity"
        self.manifest = root / "config/mneme/activity.json"
        for path in (self.cwd, root / "home", self.manifest.parent):
            path.mkdir(parents=True, mode=0o700, exist_ok=True)

    def remaining(self):
        remaining = self.deadline - time.monotonic()
        require(remaining > 0, "activity smoke exceeded its deadline")
        return remaining

    def run(self, argv, *, expect=0, input=None):
        result = subprocess.run([str(arg) for arg in argv], cwd=self.cwd,
                                env=self.env, input=input, capture_output=True,
                                text=True, timeout=self.remaining())
        require(len(result.stdout.encode()) <= 512 * 1024
                and len(result.stderr.encode()) <= 512 * 1024,
                "unexpected smoke subprocess output volume")
        require(result.returncode == expect if expect is not None else result.returncode != 0,
                f"unexpected exit for {Path(argv[0]).name}: {result.returncode}")
        return result

    def install(self):
        # Importing the installer is read-only. The explicit plan/apply below is
        # scoped wholly to this fixture; no native trust/service commands run.
        source = Path(__file__).resolve().parents[1] / "integrations/codex"
        program = (
            "import json,sys; from pathlib import Path; "
            "sys.path.insert(0,sys.argv[1]); import install; "
            "p=install.prepare(Path(sys.argv[2]),Path(sys.argv[3]),"
            "Path(sys.argv[4]),Path(sys.argv[5]),18765,"
            "program_root=Path(sys.argv[1]),activity_root=Path(sys.argv[6]),"
            "activity_config=Path(sys.argv[7]),activity_device='smoke-device'); "
            "install.apply(p); print(json.dumps({'config_revision':p['config_revision']}))"
        )
        installed = self.run([sys.executable, "-c", program, source, self.project, self.prefix,
                              self.options.cli, self.options.mcp, self.activity, self.manifest])
        self.cli = self.prefix / "bin/mnemed"
        self.mcp = self.prefix / "bin/mneme-mcp"
        require(artifact(self.cli) == artifact(self.options.cli), "CLI copy identity mismatch")
        require(artifact(self.mcp) == artifact(self.options.mcp), "MCP copy identity mismatch")
        require((self.prefix / "lib/session_activity.py").is_file(), "missing installed producer")
        helpers = [artifact(path) for path in sorted((self.prefix / "lib").glob("*.py"))]
        self.installation = {**json.loads(installed.stdout), "helper_count": len(helpers),
                             "helpers_sha256": hashlib.sha256(json.dumps(
                                 helpers, sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
                             "producer": artifact(self.prefix / "lib/session_activity.py")}
        self.installation["cli_version"] = self.run([self.cli, "--version"]).stdout.strip()
        help_text = self.run([self.cli, "activity", "--help"]).stdout
        require("--session" in help_text and "--activity-config" in help_text,
                "installed activity help disagrees with its command")
        self.installation["activity_help_sha256"] = hashlib.sha256(help_text.encode()).hexdigest()
        self.checks.append("explicit isolated install and copied artifact identities")

    def produce(self):
        # Exercise the installed helper, not the repository import. Event bodies
        # are synthetic and deliberately contain fields the writer must ignore.
        program = r"""
import json, sys, time
sys.path.insert(0, sys.argv[1])
import session_activity as activity
now = int(time.time())
event = {'session_id':'smoke-session-a17c9e-full', 'cwd':sys.argv[2],
         'turn_id':'turn-1', 'prompt':'NEVER_RETAIN_THIS_PROMPT',
         'transcript_path':'NEVER_RETAIN_THIS_TRANSCRIPT_PATH'}
assert activity.emit(event, 'lifecycle', 'startup', key='startup-1', at=now)
assert activity.emit(event, 'recall_admission', 'skipped', reason='disabled_mode',
                     key='skip-1', at=now)
assert activity.emit(event, 'recording', 'maintenance_verified', count=0,
                     key='maintenance-1', at=now)
print('produced')
"""
        self.run([sys.executable, "-c", program, self.prefix / "lib", self.cwd])
        require(not any(b"NEVER_RETAIN" in path.read_bytes()
                        for path in self.activity.glob("*.json")),
                "producer retained non-metadata input")
        self.checks.append("installed producer emits body-free receipts")

    def reports(self):
        # If the route accidentally invokes ordinary owner discovery, these
        # deliberately invalid inputs prevent it from looking like a success.
        (self.manifest.parent / "memory.toml").write_text("not valid [owner configuration\n")
        before = inventory(self.root)
        command = [self.cli, "--json", "activity", "--activity-config", self.manifest]
        listing = json.loads(self.run(command).stdout)
        require(listing.get("sessions"), "CLI did not list the synthetic session")
        args = {"session": "codex:smoke-session-a17c9e-full", "details": True}
        detail = json.loads(self.run([*command, "--session", args["session"], "--details"]).stdout)
        human = self.run([self.cli, "activity", "--activity-config", self.manifest,
                          "--session", "smoke-session", "--details"]).stdout
        require("smoke-session" in human, "human report omitted recognizable session identity")
        require("NEVER_RETAIN" not in json.dumps(detail), "report leaked ignored input")
        self.run([self.cli, "--db", self.root / "must-not-open.db", *command[1:]], expect=None)
        require(inventory(self.root) == before, "CLI reporting/refusal changed its fixture")
        self.checks.append("CLI chooser exact/prefix human/JSON and early owner-selector refusal")

        process = subprocess.Popen([str(self.mcp), "--activity-config", str(self.manifest),
                                    "--cwd", str(self.cwd)], cwd=self.cwd, env=self.env,
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True)
        self.children.append(process)

        def rpc(number, method, params):
            send(process, {"jsonrpc": "2.0", "id": number, "method": method, "params": params})
            return read_response(process, number, self.remaining())

        initialized = rpc(1, "initialize", {"protocolVersion": "2025-11-25",
                           "capabilities": {}, "clientInfo": {"name": "activity-smoke", "version": "1"}})
        require(initialized["result"]["serverInfo"]["name"] == "mneme-mcp-session-activity",
                "wrong activity server identity")
        self.protocol = {"version": initialized["result"]["protocolVersion"],
                         "server": initialized["result"]["serverInfo"]}
        send(process, {"jsonrpc": "2.0", "method": "notifications/initialized"})
        catalog = rpc(2, "tools/list", {})["result"]["tools"]
        require([row["name"] for row in catalog] == ["session_activity"], "activity catalog is not closed")
        result = tool_json(rpc(3, "tools/call", {"name": "session_activity", "arguments": args}))
        require(result == detail, "CLI/MCP report representations disagree")
        for number, name, arguments in ((4, "save", {}),
                                        (5, "session_activity", {"path": str(self.root)}),
                                        (6, "session_activity", {"details": "true"})):
            refused = rpc(number, "tools/call", {"name": name, "arguments": arguments})
            require(refused["result"].get("isError") is True, "MCP accepted unauthorized/invalid arguments")
        process.stdin.close()
        require(process.wait(timeout=self.remaining()) == 0, "MCP did not shut down cleanly at EOF")
        require(inventory(self.root) == before, "MCP reporting/refusal changed its fixture")
        self.run([self.mcp, "--activity-config", self.manifest, "--http", "127.0.0.1:0"], expect=None)
        require(inventory(self.root) == before, "transport refusal changed its fixture")
        self.checks.append("stdio initialize closed catalog exact parity refusals and finite shutdown")
        self.checks.append("fixture inventory unchanged after report and refusal paths")

    def close(self):
        for process in self.children:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            for stream in (process.stdin, process.stdout, process.stderr):
                if stream is not None:
                    stream.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", required=True, type=Path)
    parser.add_argument("--mcp", required=True, type=Path)
    parser.add_argument("--receipt", type=Path)
    parser.add_argument("--timeout", type=float, default=90)
    parser.add_argument("--features", default="unspecified", help="Build feature label for the receipt")
    options = parser.parse_args()
    require(0 < options.timeout <= 600, "timeout must be in (0, 600]")
    options.cli, options.mcp = options.cli.resolve(strict=True), options.mcp.resolve(strict=True)
    receipt = {"schema": "mneme.session-activity.smoke.v1", "features": options.features,
               "cli": artifact(options.cli), "mcp": artifact(options.mcp), "checks": []}
    try:
        with tempfile.TemporaryDirectory(prefix="mneme-session-activity-smoke-") as temporary:
            smoke = Smoke(options, Path(temporary).resolve())
            try:
                smoke.install()
                smoke.produce()
                smoke.reports()
                receipt["checks"] = smoke.checks
                receipt["installation"] = smoke.installation
                receipt["protocol"] = smoke.protocol
            finally:
                smoke.close()
        receipt["status"] = "passed"
    except Exception as error:
        # These inputs are synthetic; still avoid copying arbitrary child output.
        receipt.update(status="failed", reason=str(error))
    if options.receipt:
        options.receipt.parent.mkdir(parents=True, exist_ok=True)
        options.receipt.write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))
    return 0 if receipt["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())

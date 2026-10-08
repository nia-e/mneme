#!/usr/bin/env python3
"""Finite disposable smoke of one explicit workshop Python runtime.

Exercises real ledger/queue/bridge/tool modules without signal-cli, network,
models or live state. A bounded child owns private synthetic fixtures. Genuine
prior runtime fixtures establish the format-upgrade/old-writer checks; no schema
is manually forged. This is not the full failure-injection or component suite.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import hashlib
import importlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time

sys.dont_write_bytecode = True
REQUIRED = ("signal_store.py", "signal_bridge.py", "signal_tools.py", "signal_jsonrpc.py",
            "wake.py", "heartbeat.py", "inbox_hook.py")
PRIOR = {"signal_store_v2.py": "0825937e341e8585038f00a91939b7ba0afa1fa07b801419b744446116e74268",
         "wake_v3.py": "df38ecd86e624499955bcdfdd6921f2fc98d1c3d1351794052e5960a153e825b"}
OWNER = "22222222-2222-4222-8222-222222222222"
ACCOUNT = "11111111-1111-4111-8111-111111111111"


def need(condition, message):
    if not condition:
        raise RuntimeError(message)


def artifact(path):
    path = Path(path)
    return {"path": str(path), "bytes": path.stat().st_size,
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}


def digest(path):
    return artifact(path)["sha256"]


def private_json(path, value):
    path.write_text(json.dumps(value, sort_keys=True) + "\n", encoding="utf-8")
    path.chmod(0o600)


def snapshot(paths):
    # Call only with no live writer. SQLite journals would invalidate a database-
    # file-only comparison, so don't silently treat those as an unchanged store.
    for path in paths:
        for suffix in ("-journal", "-wal"):
            sidecar = Path(str(path) + suffix)
            need(not sidecar.exists() or sidecar.stat().st_size == 0, "canonical hash has a nonempty SQLite sidecar")
    return {path.name: digest(path) for path in paths}


def refused(operation, error_type, explanation):
    try:
        operation()
    except error_type as error:
        return str(error)[:600]
    raise RuntimeError(explanation)


def load_prior(runtime, name):
    path = runtime / "fixtures" / name
    need(path.is_file() and digest(path) == PRIOR[name], f"missing or changed genuine prior fixture {name}")
    spec = importlib.util.spec_from_file_location("smoke_prior_" + path.stem, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class NoExternalRpc:
    def __getattr__(self, name):
        raise AssertionError(f"smoke tried an external RPC: {name}")


class Fixture:
    def __init__(self, base, modules, name, *, initialize=True):
        self.base = base / name
        self.base.mkdir(mode=0o700)
        self.workshop = self.base / "w"
        self.workshop.mkdir(mode=0o700)
        self.data = self.base / "data"
        self.data.mkdir(mode=0o700)
        self.assets = self.base / "assets"
        self.assets.mkdir(mode=0o700)
        self.portrait = self.assets / "portrait.png"
        self.portrait.write_bytes(b"\x89PNG\r\n\x1a\nsmoke-only")
        cli = self.base / "never-signal-cli"
        cli.write_text("#!/bin/sh\nexit 97\n", encoding="utf-8")
        cli.chmod(0o700)
        self.state = self.base / "s"
        self.ledger = self.state / "ledger.sqlite"
        self.clock = [0.0]
        self.modules = modules
        rpc_config = self.base / "rpc.json"
        private_json(rpc_config, modules["signal_jsonrpc"].MINIMAL_CONFIG)
        config_path = self.base / "bridge.json"
        private_json(config_path, {"schema": modules["signal_bridge"].CONFIG_SCHEMA,
            "workshop_root": str(self.workshop), "state_dir": str(self.state), "store_path": str(self.ledger),
            "signal_cli": str(cli), "signal_data_dir": str(self.data), "signal_rpc_config": str(rpc_config),
            "account": ACCOUNT, "owner_id": OWNER,
            "agent_tools": {"asset_roots": [str(self.assets)], "portrait_path": str(self.portrait)}})
        self.config = modules["signal_bridge"].Config(config_path)
        if initialize:
            modules["signal_bridge"].init(self.config)
        else:
            self.state.mkdir(mode=0o700)
            (self.state / "spool").mkdir(mode=0o700)

    @contextmanager
    def opened(self):
        bridge = self.modules["signal_bridge"]
        with bridge._lock(self.state):
            with self.modules["signal_store"].Store(self.ledger, owner_id=OWNER) as store:
                actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                      clock=lambda: self.clock[0])
                yield store, actor

    def history_tool(self, actor, limit):
        """Real stdio MCP parser -> private socket -> bridge callback, no fake RPC.

        The SQLite owner stays on this thread. Only the MCP client runs on the
        helper thread, matching the bridge's normal single-thread ownership.
        """
        tools = self.modules["signal_tools"]
        socket_path = self.state / "tools.sock"
        frames = [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "live-read-smoke", "version": "1"}}},
            {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}},
            {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
                "name": tools.HISTORY_TOOL, "arguments": {"limit": limit}}},
        ]
        source = io.BytesIO(b"".join((json.dumps(frame) + "\n").encode() for frame in frames))
        sink, errors = io.BytesIO(), []
        def client():
            try:
                tools.run_stream(socket_path, source, sink)
            except BaseException as error:
                errors.append(error)
        with tools.SignalToolsServer(socket_path, asset_roots=(self.assets,), portrait_path=self.portrait) as server:
            worker = threading.Thread(target=client, daemon=True)
            worker.start()
            deadline = time.monotonic() + 5
            while worker.is_alive() and time.monotonic() < deadline:
                server.poll(NoExternalRpc(), read_history=actor.read_history)
            worker.join(timeout=0.2)
            need(not worker.is_alive(), "stdio/socket history operation exceeded five seconds")
            need(not errors, f"stdio/socket client failed: {errors!r}")
        need(not socket_path.exists(), "tool shutdown retained its owned socket")
        need(len(sink.getvalue()) <= 32 * 1024, "MCP smoke output unexpectedly large")
        rows = [json.loads(line) for line in sink.getvalue().splitlines()]
        need([row.get("id") for row in rows] == [1, 2, 3], "MCP response identity mismatch")
        need(rows[0]["result"]["serverInfo"]["name"] == "mneme-signal-tools", "wrong runtime tool identity")
        catalog = next(tool for tool in rows[1]["result"]["tools"] if tool["name"] == tools.HISTORY_TOOL)
        need(catalog.get("annotations", {}).get("readOnlyHint") is False,
             "history catalog still promises no durable effects")
        result = rows[2]["result"]
        need(not result.get("isError"), "history MCP call refused")
        value = result["structuredContent"]
        need(value.get("outcome") not in ("failed", "unknown"), f"history tool did not complete: {value!r}")
        need(json.loads(result["content"][0]["text"]) == value, "MCP text and structured history disagree")
        value = {key: item for key, item in value.items() if key != "request_id"}
        need(len(json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()) <= tools.MAX_HISTORY_BYTES,
             "history result exceeds 4096-byte contract")
        return value


class Smoke:
    def __init__(self, runtime, base, receipt):
        self.runtime, self.base, self.receipt = runtime, base, receipt
        sys.path.insert(0, str(runtime))
        self.modules = {Path(name).stem: importlib.import_module(Path(name).stem) for name in REQUIRED}
        for name, module in self.modules.items():
            need(Path(module.__file__).resolve() == runtime / (name + ".py"), f"import escaped selected runtime: {name}")
        self.store = self.modules["signal_store"]
        self.bridge = self.modules["signal_bridge"]
        self.wake = self.modules["wake"]
        need(self.store.SCHEMA == "mneme.signal.ledger.v3" and self.wake.SCHEMA == "mneme.workshop.events.v4",
             "runtime does not implement the selected ledger v3 / queue v4 contract")
        receipt["schemas"] = {"ledger": self.store.SCHEMA, "queue": self.wake.SCHEMA,
                              "tools": self.modules["signal_tools"].SCHEMA}

    @contextmanager
    def check(self, name):
        check = {"name": name, "status": "running"}
        self.receipt["checks"].append(check)
        start = time.monotonic()
        try:
            yield check
        except Exception as error:
            check.update(status="failed", error=str(error)[-2000:])
            raise
        else:
            check["status"] = "passed"
        finally:
            check["duration_ms"] = round((time.monotonic() - start) * 1000)

    def selected_history(self):
        fixture = Fixture(self.base, self.modules, "selection")
        with self.check("stdio_socket_exact_selection_consumption_retry_and_held_unseen_preservation") as evidence:
            with fixture.opened() as (store, actor):
                store.ingest("a", 1000, "PRIVATE_A_NOT_FOR_HOOK", 0)
                fixture.clock[0] = 10
                actor.tick(NoExternalRpc())
                held = self.wake.claim(fixture.workshop, "synthetic-held-run")["events"]
                need(len(held) == 1, "fixture did not establish one run-owned event")
                store.ingest("b", 2000, "PRIVATE_B_NOT_FOR_HOOK", 11)
                fixture.clock[0] = 21
                actor.tick(NoExternalRpc())
                pending = next(record for record in self.wake.active_records(fixture.workshop) if record["status"] == "pending")
                need(len(self.wake.active_records(fixture.workshop)) == 2, "overlapping burst fixture missing")
                with self.wake._workshop_locked(fixture.workshop):
                    newest = fixture.history_tool(actor, 1)
                    need([item["id"] for item in newest["entries"]] == ["b"], "bounded history selected wrong incoming ID")
                    need(store.status()["seen"] == 1 and store.status()["unseen"] == 1,
                         "history consumed more than its exact selected incoming IDs")
                    need(len(self.wake.active_records(fixture.workshop)) == 2, "partial overlap falsely consumed a wake")
                    whole = fixture.history_tool(actor, 2)
                need([item["id"] for item in whole["entries"]] == ["a", "b"], "full history did not retain original order")
                need(self.wake.active_records(fixture.workshop) == held, "read changed run-owned work or retained fully-seen pending wake")
                proof = self.wake.lookup(fixture.workshop, self.bridge.SOURCE, pending["event"]["event_id"])
                need(proof["terminal"] == {"status": "consumed", "reply": None}, "consumption fabricated a reply/completion receipt")
                retry = self.wake.consume_signal_events(fixture.workshop, [pending["event"]])
                need(retry["already_consumed_events"] == 1 and retry["consumed_events"] == 0,
                     "exact consumed-event retry was not idempotent")
                store.ingest("c", 500, "PRIVATE_OLDER_TIMESTAMP_UNSEEN", 22)
                fixture.clock[0] = 32
                actor.tick(NoExternalRpc())
                repeated = fixture.history_tool(actor, 2)
                need(repeated["entries"] == whole["entries"] and repeated["truncated"] is True,
                     "history retry silently selected an omitted old-timestamp message")
                records = self.wake.active_records(fixture.workshop)
                need(sum(record["status"] == "held" for record in records) == 1
                     and sum(record["status"] == "pending" for record in records) == 1,
                     "history retry erased unseen or held work")
                status = store.status()
                need((status["seen"], status["unseen"], status["unanswered"]) == (2, 1, 3)
                     and status["outbox"] == status["direct_outbox"] == 0,
                     "seen was confused with answered or a send was manufactured")
                evidence.update(seen=status["seen"], unseen=status["unseen"], unanswered=status["unanswered"],
                                consumed_events=retry["already_consumed_events"], held=1, pending=1)
        with self.check("metadata_hook_remains_read_only_and_excludes_consumed_held"):
            paths = [fixture.ledger, fixture.workshop / "events.json"]
            before = snapshot(paths)
            counts = self.wake.inbox_summary(fixture.workshop)
            need(counts == {"pending_signal_bursts": 1, "pending_mailbox_messages": 0,
                            "unfinished_signal_bursts": 0, "unfinished_mailbox_messages": 0},
                 "passive inbox summary counted consumed/held work as pending")
            cue = self.modules["inbox_hook"].handle_event({"hook_event_name": "SessionStart", "source": "compact"}, fixture.workshop)
            text = cue["hookSpecificOutput"]["additionalContext"]
            need("pending Signal bursts: 1" in text and "PRIVATE_" not in text, "hook leaked content or miscounted pending bursts")
            need(snapshot(paths) == before, "passive hook modified ledger/queue state")

    def quiet_and_bounds(self):
        with self.check("before_quiet_and_prebuilt_seen_bursts_do_not_reappear"):
            for prebuilt in (False, True):
                fixture = Fixture(self.base, self.modules, "quiet-" + str(int(prebuilt)))
                with fixture.opened() as (store, actor):
                    store.ingest("early", 1, "READ_BEFORE_QUIET", 0)
                    if prebuilt:
                        need(store.prepare_burst(10) is not None, "fixture did not prepare an unpublished burst")
                    result = actor.read_history({"limit": 12})
                    need([item["id"] for item in result["entries"]] == ["early"], "early history missing")
                    fixture.clock[0] = 10
                    actor.tick(NoExternalRpc())
                    need(self.wake.active_records(fixture.workshop) == [], "seen-before-quiet message generated a stale wake")
                    store.ingest("later", 2, "NEW_UNSEEN", 10)
                    fixture.clock[0] = 20
                    actor.tick(NoExternalRpc())
                    records = self.wake.active_records(fixture.workshop)
                    need(len(records) == 1 and "NEW_UNSEEN" in records[0]["event"]["body"]
                         and "READ_BEFORE_QUIET" not in records[0]["event"]["body"], "later burst replayed seen backlog")
        with self.check("unreturned_oversized_message_stays_unseen"):
            fixture = Fixture(self.base, self.modules, "oversized")
            with fixture.opened() as (store, actor):
                store.ingest("oversized", 1, "🪨" * 1100, 0)
                result = fixture.history_tool(actor, 20)
                need(result["entries"] == [] and result["truncated"] is True, "oversized history item was partially exposed")
                need(store.status()["seen"] == 0 and store.status()["unseen"] == 1,
                     "a body omitted by the byte budget was marked seen")

    def prior_upgrade(self):
        with self.check("genuine_prior_pair_explicit_upgrade_replay_and_old_writer_fence") as evidence:
            prior_store = load_prior(self.runtime, "signal_store_v2.py")
            prior_wake = load_prior(self.runtime, "wake_v3.py")
            fixture = Fixture(self.base, self.modules, "upgrade", initialize=False)
            with self.bridge._lock(fixture.state, create=True):
                with prior_store.Store(fixture.ledger, owner_id=OWNER, create=True) as old:
                    old.ingest("prior", 1, "Preserved prior conversation", 0)
                    burst = old.prepare_burst(10)
                    old.mark_enqueued(burst["event_id"])
                    history = old.read_history()
            event = self.bridge._event_for_burst(burst)
            prior_wake.enqueue(fixture.workshop, event)
            original_records = prior_wake.active_records(fixture.workshop)
            paths = [fixture.ledger, fixture.workshop / "events.json"]
            before = snapshot(paths)
            refused(lambda: self.store.Store(fixture.ledger, owner_id=OWNER), self.store.StoreError,
                    "ordinary current ledger open silently migrated a predecessor")
            refused(lambda: self.bridge.upgrade(fixture.config), self.bridge.Refusal,
                    "paired upgrade did not require an explicitly paused workshop")
            need(snapshot(paths) == before, "failed pre-upgrade admission changed canonical source state")
            (fixture.workshop / "PAUSED").touch(mode=0o600)
            upgraded = self.bridge.upgrade(fixture.config)
            need(upgraded == {"status": "upgraded", "ledger_schema": self.store.SCHEMA,
                              "queue_schema": self.wake.SCHEMA}, "paired upgrade reported unexpected identities")
            after = snapshot(paths)
            need(self.bridge.upgrade(fixture.config)["status"] == "already_current",
                 "paired upgrade retry did not recognize the exact current pair")
            need(snapshot(paths) == after, "exact upgrade retry changed canonical bytes")
            with fixture.opened() as (store, _actor):
                need(store.read_history() == history and store.status()["seen"] == 0
                     and store.status()["unseen"] == 1, "upgrade lost history or invented a prior read")
            need(self.wake.active_records(fixture.workshop) == original_records,
                 "queue upgrade changed canonical events or claims")
            quiescent = snapshot(paths)
            old_errors = {
                "ledger_open": refused(lambda: prior_store.Store(fixture.ledger, owner_id=OWNER), prior_store.StoreError,
                    "prior ledger writer admitted current schema"),
                "queue_read": refused(lambda: prior_wake.active_records(fixture.workshop), prior_wake.Refusal,
                    "prior queue reader ignored the consumed-aware schema"),
                "queue_write": refused(lambda: prior_wake.enqueue(fixture.workshop, event), prior_wake.Refusal,
                    "prior queue writer ignored the consumed-aware schema"),
            }
            need(snapshot(paths) == quiescent, "refused prior writer modified current canonical state")
            with fixture.opened() as (store, actor):
                actor.read_history({"limit": 12})
                need(store.status()["seen"] == 1 and store.status()["unanswered"] == 1,
                     "upgraded runtime cannot record seen without answering")
            need(self.wake.active_records(fixture.workshop) == [], "upgraded pending event did not consume")
            evidence.update(prior_fixtures={name: artifact(self.runtime / "fixtures" / name) for name in PRIOR},
                            upgraded=upgraded, old_refusals=old_errors)

    def run(self):
        self.selected_history()
        self.quiet_and_bounds()
        self.prior_upgrade()


def worker(runtime, base):
    receipt = {"schema": "mneme.workshop.live-read-smoke.v1", "status": "running",
               "scope": "private disposable runtime composition; no live state, signal-cli, network or models",
               "harness": artifact(Path(__file__).resolve()), "runtime": str(runtime),
               "runtime_files": {name: artifact(runtime / name) for name in REQUIRED}, "checks": [],
               "python": {"executable": sys.executable, "version": sys.version}}
    start = time.monotonic()
    try:
        need(base.is_dir() and not any(base.iterdir()) and base.name.startswith("mneme-live-read-smoke-"),
             "worker needs its own empty temporary root")
        Smoke(runtime, base, receipt).run()
        receipt["status"] = "passed"
    except Exception as error:
        receipt.update(status="failed", error=f"{type(error).__name__}: {error}"[-4000:])
    finally:
        receipt["duration_ms"] = round((time.monotonic() - start) * 1000)
        after = {name: artifact(runtime / name) for name in REQUIRED}
        receipt["runtime_unchanged"] = after == receipt["runtime_files"]
        if not receipt["runtime_unchanged"]:
            receipt.update(status="failed", error="runtime source changed during the smoke")
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime", type=Path, required=True, help="exact workshop runtime directory, not live state")
    parser.add_argument("--output", type=Path, help="write the finite smoke receipt")
    parser.add_argument("--timeout", type=int, default=90, help="whole disposable child timeout, 5..300 seconds")
    parser.add_argument("--worker-root", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    runtime = args.runtime.resolve(strict=True)
    need(runtime.is_dir() and all((runtime / name).is_file() for name in REQUIRED), "runtime is incomplete")
    need(5 <= args.timeout <= 300, "timeout must be 5..300 seconds")
    os.umask(0o077)
    if args.worker_root is not None:
        result = worker(runtime, args.worker_root.resolve(strict=True))
        print(json.dumps(result, ensure_ascii=False, sort_keys=True))
        return 0 if result["status"] == "passed" else 1
    need(args.output is not None, "--output is required")
    output = args.output.resolve()
    need(not output.is_relative_to(runtime), "receipt must not modify the runtime")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="mneme-live-read-smoke-", dir="/tmp") as dirname:
        base = Path(dirname).resolve()
        command = [sys.executable, "-B", str(Path(__file__).resolve()), "--runtime", str(runtime),
                   "--worker-root", str(base), "--timeout", str(args.timeout)]
        env = os.environ.copy()
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        try:
            completed = subprocess.run(command, text=True, capture_output=True, env=env, timeout=args.timeout)
            need(len(completed.stdout.encode()) <= 128 * 1024, "child smoke receipt exceeds bound")
            result = json.loads(completed.stdout)
            need(isinstance(result, dict) and result.get("status") in ("passed", "failed"), "child returned an invalid receipt")
            need((completed.returncode == 0) == (result["status"] == "passed"), "child receipt and exit code disagree")
            if completed.stderr:
                result["stderr"] = completed.stderr[-4000:]
        except Exception as error:
            result = {"schema": "mneme.workshop.live-read-smoke.v1", "status": "failed", "runtime": str(runtime),
                      "error": f"{type(error).__name__}: {error}"[-4000:], "checks": []}
    private_json(output, result)
    print(json.dumps({"receipt": str(output), "status": result["status"],
                      "passed": sum(check["status"] == "passed" for check in result["checks"])}))
    return 0 if result["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())

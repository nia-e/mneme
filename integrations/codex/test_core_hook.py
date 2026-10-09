import copy
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

import core_hook

ID1 = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
ID2 = "01ARZ3NDEKTSV4RRFFQ69G5FAW"
ID3 = "01ARZ3NDEKTSV4RRFFQ69G5FAX"


def node(identifier=ID1, summary="Name: Owner", body="My name is Owner."):
    return {"id": identifier, "summary": summary, "summary_truncated": False,
            "status": "active", "tags": ["core", "identity"],
            "body": body, "body_truncated": False,
            "provenance": {"type": "conversation", "session": "fixture", "turn": 1}}


def native(nodes=None, **changes):
    nodes = [node()] if nodes is None else nodes
    result = {"nodes": nodes, "total": len(nodes), "truncated": False,
              "nodes_truncated": False, "bodies_truncated": False,
              "body_bytes_limit": 131072}
    result.update(changes)
    return result


class StubMcp(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def handle(self):
        try:
            super().handle()
        except ConnectionResetError:
            # Deadline tests intentionally kill readers with HTTP keep-alive
            # sockets; a reset while waiting for their next request is expected.
            pass

    def reply(self, status, body=b"", session=False):
        self.send_response(status)
        if body:
            self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        if session:
            self.send_header("mcp-session-id", "core-fixture-session")
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        method = request["method"]
        self.server.calls.append((self.server.db, method, request.get("params", {})))
        session = False
        if method == "initialize":
            if self.server.not_ready:
                self.server.not_ready -= 1
                self.reply(503)
                return
            result = {"protocolVersion": "2025-11-25",
                      "capabilities": {"tools": {}},
                      "serverInfo": {"name": "mneme-mcp"}}
            session = True
        elif method == "notifications/initialized":
            self.reply(202)
            return
        elif method == "tools/list":
            result = {"tools": [{"name": name,
                                 "inputSchema": {"type": "object"}}
                                for name in ("databases", "core")]}
        elif method == "tools/call":
            name = request["params"]["name"]
            if name == "databases":
                value = self.server.catalog
            elif name == "core":
                if self.server.core_delay:
                    time.sleep(self.server.core_delay)
                value = self.server.core
            else:
                raise AssertionError("hook called non-core tool " + name)
            result = {"content": [{"type": "text", "text": json.dumps(value)}],
                      "isError": self.server.refuse_core and name == "core"}
        else:
            raise AssertionError("unexpected method " + method)
        self.reply(200, json.dumps({"jsonrpc": "2.0", "id": request["id"],
                                    "result": result}).encode(), session=session)

    def do_DELETE(self):
        self.server.calls.append((self.server.db, "DELETE", {}))
        self.reply(204)


class CoreHookTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.project = self.root / "project"
        self.project.mkdir()
        self.cwd = self.project / "src" / "deep"
        self.cwd.mkdir(parents=True)
        self.calls = []
        self.global_path = self.root / "user.db"
        self.project_path = self.project / ".mneme" / "codex-memory.db"
        self.global_service = self.service("user", self.global_path, 18763)
        self.project_service = self.service("project", self.project_path, 18764)
        self.config_path = self.root / "core-hook.json"
        self.config_value = {"schema": core_hook.CONFIG_SCHEMA,
                             "global_service": str(self.global_service),
                             "global_database": str(self.global_path),
                             "projects": [{"root": str(self.project),
                                           "service_config": str(self.project_service)}]}
        self.write_config()
        self.event = {"hook_event_name": "SessionStart", "source": "startup",
                      "cwd": str(self.cwd), "session_id": "core-test"}

    def service(self, name, store, port, filename=None, legacy=False):
        path = self.root / (filename or (name + "-service.json"))
        value = {"binary": str(self.root / "missing-binary-no-launch"),
                 "port": port, "state_dir": str(self.root / (name + "-state")),
                 "working_directory": str(self.root)}
        if legacy:
            value["project_db"] = str(store)
        else:
            value.update(database_name=name, database_path=str(store))
        path.write_text(json.dumps(value))
        return path

    def write_config(self):
        self.config_path.write_text(json.dumps(self.config_value))
        return core_hook._config(self.config_path)

    def server(self, db, core=None, **attributes):
        self.require_native_client()
        server = ThreadingHTTPServer(("127.0.0.1", 0), StubMcp)
        server.daemon_threads = True
        server.db = db
        server.calls = self.calls
        path = self.global_path if db == "user" else self.project_path
        server.catalog = [{"db": db, "name": db, "state": "open", "configured_path": str(path)}]
        server.core = native() if core is None else core
        server.not_ready = 0
        server.refuse_core = False
        server.core_delay = 0
        for key, value in attributes.items():
            setattr(server, key, value)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        service = self.global_service if db == "user" else self.project_service
        data = json.loads(service.read_text())
        data["port"] = server.server_port
        service.write_text(json.dumps(data))
        return server

    def require_native_client(self):
        # The integration exercises the *current* Rust bridge, not an unrelated
        # mnemed that happens to be installed on the developer's PATH.
        binary = os.environ.get("MNEME_CLIENT_BINARY")
        if not binary or not Path(binary).is_file():
            self.skipTest("set MNEME_CLIENT_BINARY to a built compatible mnemed")

    def connect_global(self, server, database_path="/home/user/.local/share/mneme/memory.db"):
        # This is an opaque server identity, not a file on the test machine.
        self.global_service.write_text(json.dumps({
            "mode": "connect", "url": "http://127.0.0.1:%d/" % server.server_port,
            "database_name": "user", "database_path": database_path,
        }))
        self.config_value["global_database"] = database_path
        server.catalog[0]["configured_path"] = database_path

    def context(self, event=None, timeout=2):
        result = core_hook.handle_event(event or self.event, self.write_config(), timeout)
        self.assertEqual(result["hookSpecificOutput"]["hookEventName"], "SessionStart")
        text = result["hookSpecificOutput"]["additionalContext"]
        self.assertLessEqual(len(text.encode()), core_hook.MAX_CONTEXT_BYTES)
        return text, [json.loads(line) for line in text.splitlines() if line.startswith("{")]

    def cli(self, event, *extra):
        return subprocess.run([sys.executable, "-B", str(Path(core_hook.__file__).resolve()),
                               "--config", str(self.config_path), *extra],
                              input=json.dumps(event).encode(), capture_output=True,
                              timeout=5, check=False)

    def test_exact_config_optional_projects_and_no_unknown_fields(self):
        value = core_hook._config(self.config_path)
        self.assertEqual(value["global_service"], self.global_service)
        del self.config_value["projects"]
        self.assertEqual(self.write_config()["projects"], [])
        for change in ({"schema": "mneme.codex-core.config.v2"}, {"extra": True},
                       {"global_service": "relative"}, {"global_service": 7},
                       {"global_database": "relative"},
                       {"projects": {}}, {"projects": [{"root": str(self.root)}]}):
            original = copy.deepcopy(self.config_value)
            self.config_value.update(change)
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.write_config()
            self.config_value = original
        self.config_path.write_text('{"schema":"mneme.codex-core.config.v1",'
                                    '"global_service":"/a","global_service":"/b"}')
        with self.assertRaisesRegex(ValueError, "duplicate"):
            core_hook._config(self.config_path)
        self.config_path.write_bytes(b" " * (core_hook.MAX_CONFIG + 1))
        with self.assertRaisesRegex(ValueError, "size"):
            core_hook._config(self.config_path)

    def test_closest_resolved_allowlisted_root_not_nearest_discovered_store(self):
        nested = self.project / "src"
        nested_config = self.root / "nested-service.json"
        self.config_value["projects"].append({"root": str(nested), "service_config": str(nested_config)})
        config = self.write_config()
        self.assertEqual(core_hook._targets(config, str(self.cwd))[1]["service_config"], str(nested_config))
        alias = self.root / "alias"
        alias.symlink_to(self.cwd, target_is_directory=True)
        self.assertEqual(core_hook._targets(config, str(alias))[1]["root"], str(nested))
        for cwd in (str(self.root), str(self.root / "project-lookalike")):
            with self.subTest(cwd=cwd):
                self.assertEqual([x["db"] for x in core_hook._targets(config, cwd)], ["user"])
        for cwd in (None, "relative"):
            with self.subTest(cwd=cwd), self.assertRaisesRegex(ValueError, "working directory"):
                core_hook._targets(config, cwd)
        self.config_value["projects"].append({"root": str(alias / ".." / ".."),
                                             "service_config": str(nested_config)})
        with self.assertRaisesRegex(ValueError, "duplicate"):
            self.write_config()

    def test_supported_sources_and_other_events_subagents_noop_before_config_read(self):
        section = core_hook._unavailable({"scope": "global", "db": "user"})
        for source in core_hook.SOURCES:
            with self.subTest(source=source), patch("core_hook.collect_core", return_value=[section]) as collect:
                result = core_hook.handle_event({**self.event, "source": source}, self.write_config())
                self.assertIn("hookSpecificOutput", result)
                collect.assert_called_once()
        self.config_path.unlink()
        for event in ({**self.event, "source": "other"}, {**self.event, "hook_event_name": "Stop"},
                      {**self.event, "agent_id": "child"}, {**self.event, "agent_type": "subagent"},
                      {"hook_event_name": "UserPromptSubmit", "prompt": "secret"}, None):
            with self.subTest(event=event):
                completed = self.cli(event)
                self.assertEqual(completed.returncode, 0)
                self.assertEqual(json.loads(completed.stdout), {})
                self.assertEqual(completed.stderr, b"")

    def test_real_http_global_before_project_with_native_body_and_no_files(self):
        self.server("user")
        self.server("project", native([node(ID2, "Project identity", "This project is Mneme.")]))
        event = {**self.event, "prompt": "never forward this prompt", "transcript_path": "/never/read/this"}
        text, sections = self.context(event)
        self.assertEqual([s["db"] for s in sections], ["user", "project"])
        self.assertEqual([s["outcome"] for s in sections], ["ok", "ok"])
        self.assertEqual(sections[0]["cards"][0]["summary"], "Name: Owner")
        self.assertEqual(sections[0]["cards"][0]["body"], "My name is Owner.")
        self.assertEqual(sections[0]["cards"][0]["provenance"], node()["provenance"])
        self.assertEqual(sections[0]["cards"][0]["status"], "active")
        self.assertLess(text.index(ID1), text.index(ID2))
        self.assertNotIn("never forward", text)
        tool_calls = [(db, data["name"], data["arguments"]) for db, method, data in self.calls if method == "tools/call"]
        self.assertEqual(tool_calls, [("user", "databases", {}),
                                      ("user", "core", {"db": "user", "max_body_bytes": 8192}),
                                      ("project", "databases", {}),
                                      ("project", "core", {"db": "project", "max_body_bytes": 8192})])
        for db in ("user", "project"):
            methods = [method for scope, method, _ in self.calls if scope == db]
            self.assertEqual(methods[:5], ["initialize", "notifications/initialized",
                                           "tools/list", "tools/call", "tools/call"])
        self.assertEqual([method for _, method, _ in self.calls].count("DELETE"), 2)
        self.assertFalse(self.global_path.exists())
        self.assertFalse(self.project_path.parent.exists())
        self.assertFalse((self.root / "user-state").exists())
        self.assertFalse((self.root / "project-state").exists())

    def test_outside_scope_queries_only_global_and_no_task_recall(self):
        self.server("user")
        text, sections = self.context({**self.event, "cwd": str(self.root)})
        self.assertEqual([section["db"] for section in sections], ["user"])
        self.assertNotIn("db=project", text)
        self.assertEqual({db for db, _, _ in self.calls}, {"user"})

    def test_connect_global_core_precedes_local_project_without_local_service_files(self):
        remote = self.server("user", native([node(ID1, "Agent identity", "My name is Example Agent.")]))
        self.connect_global(remote)
        self.server("project", native([node(ID2, "Project identity", "This project is Mneme.")]))
        before = set(self.root.rglob("*"))
        text, sections = self.context(timeout=core_hook.WALL_TIMEOUT)
        self.assertEqual([s["scope"] for s in sections], ["global", "project"])
        self.assertEqual([s["outcome"] for s in sections], ["ok", "ok"])
        self.assertLess(text.index(ID1), text.index(ID2))
        self.assertEqual(sections[0]["cards"][0]["body"], "My name is Example Agent.")
        self.assertEqual(sections[0]["cards"][0]["provenance"], node()["provenance"])
        tools = [(db, data["name"]) for db, method, data in self.calls if method == "tools/call"]
        self.assertEqual(tools, [("user", "databases"), ("user", "core"),
                                 ("project", "databases"), ("project", "core")])
        self.assertEqual(set(self.root.rglob("*")), before)
        self.assertFalse((self.root / "user-state").exists())
        self.assertFalse(self.project_path.exists())

    def test_connect_hook_never_resolves_or_stats_server_database_path(self):
        remote = self.server("user")
        self.connect_global(remote)
        target = core_hook._targets(self.write_config(), str(self.cwd))[0]
        original_resolve, original_stat = Path.resolve, Path.stat

        def resolve(path, *args, **kwargs):
            self.assertFalse(str(path).startswith("/home/user"), "remote path was locally resolved")
            return original_resolve(path, *args, **kwargs)

        def stat(path, *args, **kwargs):
            self.assertFalse(str(path).startswith("/home/user"), "remote path was locally inspected")
            return original_stat(path, *args, **kwargs)

        with patch.object(Path, "resolve", resolve), patch.object(Path, "stat", stat):
            section = core_hook._collect_scope(target, time.monotonic() + 1)
        self.assertEqual(section["outcome"], "ok")
        self.assertEqual(section["cards"][0]["id"], ID1)

    def test_connect_global_exact_pin_refuses_before_network(self):
        remote = self.server("user")
        self.connect_global(remote)
        self.config_value["global_database"] = "/home/user/.local/share/mneme/other.db"
        target = core_hook._targets(self.write_config(), str(self.cwd))[0]
        with patch("core_hook.McpClient") as factory:
            with self.assertRaisesRegex(ValueError, "global store"):
                core_hook._collect_scope(target, time.monotonic() + 1)
            factory.assert_not_called()
        self.assertEqual(self.calls, [])

    def test_connect_wrong_catalog_keeps_local_project_core_and_does_not_retry(self):
        remote = self.server("user")
        self.connect_global(remote)
        remote.catalog[0]["configured_path"] += "-wrong"
        self.server("project", native([node(ID2, "Project identity", "Project core still available.")]))
        text, sections = self.context()
        self.assertEqual([s["outcome"] for s in sections], ["unavailable", "ok"])
        self.assertNotIn(ID1, text)
        self.assertIn(ID2, text)
        tools = [(db, data["name"]) for db, method, data in self.calls if method == "tools/call"]
        self.assertEqual(tools, [("user", "databases"),
                                 ("project", "databases"), ("project", "core")])

    def test_connect_stalled_global_leaves_time_for_local_project_core(self):
        text, sections, elapsed = self.scripted_deadline_context("stalled", timeout=1)
        self.assertLess(elapsed, 1.5)
        self.assertEqual([s["outcome"] for s in sections], ["unavailable", "ok"])
        self.assertIn(ID2, text)
        self.assertIn("not evidence of absence", text)
        self.assertNotIn(ID1, text)
        self.assertFalse((self.root / "user-state").exists())

    def test_connect_unready_global_uses_only_its_fair_deadline(self):
        _, sections, elapsed = self.scripted_deadline_context("unready", timeout=1)
        self.assertLess(elapsed, 1.5)
        self.assertEqual([s["outcome"] for s in sections], ["unavailable", "ok"])
        self.assertGreaterEqual(sum(db == "user" and method == "initialize"
                                    for db, method, _ in self.calls), 2)
        self.assertEqual([data["name"] for db, method, data in self.calls
                          if db == "user" and method == "tools/call"], [])

    def scripted_deadline_context(self, global_mode, *, timeout):
        """Exercise real scope scheduling without measuring native cold-start.

        Short watchdog fixtures need deterministic transport timing. Separate
        native tests exercise ordinary two-owner collection and the real parent
        watchdog that kills a stalled HTTP collector.
        """
        targets = core_hook._targets(self.write_config(), str(self.cwd))
        clock = [10.0]
        lines = []
        owner = self

        class Client:
            def __init__(inner, url, *, token, timeout):
                inner.db = "user" if "18763" in url else "project"
                inner.timeout = timeout

            def connect(inner):
                owner.calls.append((inner.db, "initialize", {}))
                clock[0] += min(.01, inner.timeout)
                if inner.db == "user" and global_mode == "unready":
                    raise core_hook.McpTransportError("scripted not ready")

            def call_tool(inner, name, arguments):
                owner.calls.append((inner.db, "tools/call", {"name": name}))
                if inner.db == "user" and name == "core" and global_mode == "stalled":
                    clock[0] += inner.timeout
                    raise core_hook.McpTransportError("scripted stalled core")
                clock[0] += min(.01, inner.timeout)
                if name == "databases":
                    path = owner.global_path if inner.db == "user" else owner.project_path
                    return [{"db": inner.db, "name": inner.db, "state": "open", "configured_path": str(path)}]
                return native([node(ID1 if inner.db == "user" else ID2)])

            def close(inner):
                pass

        request = core_hook._json({"targets": targets, "timeout": timeout}).encode()
        with patch("core_hook.sys.stdin") as stdin, \
             patch("builtins.print", side_effect=lambda line, **kwargs: lines.append(line)), \
             patch("core_hook.time.monotonic", side_effect=lambda: clock[0]), \
             patch("core_hook.time.sleep", side_effect=lambda delay: clock.__setitem__(0, clock[0] + delay)), \
             patch("core_hook.McpClient", Client):
            stdin.buffer.read.return_value = request
            self.assertEqual(core_hook._child_main(), 0)
        sections = [json.loads(line) for line in lines]
        text = core_hook._context(sections)["hookSpecificOutput"]["additionalContext"]
        return text, sections, clock[0] - 10.0

    def test_native_connect_over_250ms_uses_remaining_scope_deadline(self):
        target = core_hook._targets(self.write_config(), str(self.cwd))[0]
        clock = [10.0]
        exchanges = []
        catalog = [{"db": "user", "name": "user", "state": "open",
                    "configured_path": str(self.global_path)}]

        class Client:
            def __init__(inner, _url, *, token, timeout):
                inner.timeout = timeout

            def advance(inner, operation, duration):
                exchanges.append((operation, inner.timeout))
                if duration > inner.timeout:
                    clock[0] += inner.timeout
                    raise core_hook.McpTransportError("fixture deadline")
                clock[0] += duration

            def connect(inner):
                inner.advance("connect", .27)

            def __enter__(inner):
                inner.connect()
                return inner

            def __exit__(inner, *_):
                inner.close()

            def call_tool(inner, name, arguments):
                inner.advance(name, .1)
                return catalog if name == "databases" else native()

            def close(inner):
                inner.advance("close", .01)

        with patch("core_hook.time.monotonic", side_effect=lambda: clock[0]), \
             patch("core_hook.time.sleep", side_effect=lambda duration: clock.__setitem__(0, clock[0] + duration)), \
             patch("core_hook.McpClient", Client):
            section = core_hook._collect_scope(target, 12.0)
        self.assertEqual(section["outcome"], "ok")
        self.assertEqual(section["cards"][0]["id"], ID1)
        self.assertEqual([name for name, _ in exchanges], ["connect", "databases", "core", "close"])
        for (_, actual), expected in zip(exchanges, (2.0, 1.73, 1.63, 1.53)):
            self.assertAlmostEqual(actual, expected)
        self.assertLess(clock[0], 12.0)

    def test_catalog_exhaustion_does_not_mint_core_or_cleanup_allowance(self):
        target = core_hook._targets(self.write_config(), str(self.cwd))[0]
        clock = [10.0]
        exchanges = []
        catalog = [{"db": "user", "name": "user", "state": "open",
                    "configured_path": str(self.global_path)}]

        class Client:
            def __init__(inner, _url, *, token, timeout):
                inner.timeout = timeout

            def connect(inner):
                exchanges.append(("connect", inner.timeout))
                clock[0] += .27

            def __enter__(inner):
                inner.connect()
                return inner

            def __exit__(inner, *_):
                inner.close()

            def call_tool(inner, name, arguments):
                exchanges.append((name, inner.timeout))
                self.assertEqual(name, "databases")
                clock[0] += inner.timeout
                return catalog

            def close(inner):
                exchanges.append(("close", inner.timeout))

        with patch("core_hook.time.monotonic", side_effect=lambda: clock[0]), \
             patch("core_hook.McpClient", Client):
            section = core_hook._collect_scope(target, 10.5)
        self.assertEqual(section["outcome"], "unavailable")
        self.assertEqual([name for name, _ in exchanges], ["connect", "databases", "close"])
        self.assertAlmostEqual(exchanges[0][1], .5)
        self.assertAlmostEqual(exchanges[1][1], .23)
        self.assertEqual(exchanges[2][1], .001)
        self.assertAlmostEqual(clock[0], 10.5)

    def test_default_watchdog_gives_user_first_fair_native_startup_room(self):
        targets = core_hook._targets(self.write_config(), str(self.cwd))
        clock = [10.0]
        calls = []

        def collect(target, deadline):
            calls.append((target["db"], deadline))
            clock[0] += .27
            return core_hook._pack(target, native())

        request = core_hook._json({"targets": targets, "timeout": core_hook.WALL_TIMEOUT}).encode()
        with patch("core_hook.sys.stdin") as stdin, patch("builtins.print") as output, \
             patch("core_hook.time.monotonic", side_effect=lambda: clock[0]), \
             patch("core_hook._collect_scope", side_effect=collect):
            stdin.buffer.read.return_value = request
            self.assertEqual(core_hook._child_main(), 0)
        self.assertEqual(core_hook.WALL_TIMEOUT, 12)
        self.assertEqual([name for name, _ in calls], ["user", "project"])
        self.assertAlmostEqual(calls[0][1], 16.0)
        self.assertAlmostEqual(calls[1][1], 22.0)
        self.assertEqual(output.call_count, 2)

    def test_legacy_project_service_config_still_accepted(self):
        self.service("project", self.project_path, 18764, legacy=True)
        self.server("user")
        self.server("project", native([]))
        _, sections = self.context()
        self.assertEqual([section["outcome"] for section in sections], ["ok", "empty"])

    def test_bad_catalog_is_not_retried_and_prevents_core(self):
        server = self.server("user")
        good = copy.deepcopy(server.catalog)
        self.config_value["projects"] = []
        for catalog in (good + good, [], [{**good[0], "db": "project"}],
                        [{**good[0], "name": "project"}], [{**good[0], "state": "maintenance"}],
                        [{**good[0], "configured_path": str(self.global_path) + "-wrong"}], {}):
            self.calls.clear()
            server.catalog = catalog
            with self.subTest(catalog=catalog):
                text, sections = self.context()
                self.assertEqual(sections[0]["outcome"], "unavailable")
                tools = [data["name"] for _, method, data in self.calls if method == "tools/call"]
                self.assertEqual(tools, ["databases"])
                self.assertIn("not evidence of absence", text)

    def test_wrong_service_scope_and_project_path_refuse_before_network(self):
        targets = core_hook._targets(self.write_config(), str(self.cwd))
        global_config = json.loads(self.global_service.read_text())
        global_config["database_name"] = "project"
        self.global_service.write_text(json.dumps(global_config))
        project_config = json.loads(self.project_service.read_text())
        project_config["database_path"] = str(self.project_path) + "-wrong"
        self.project_service.write_text(json.dumps(project_config))
        for target in targets:
            with self.subTest(target=target), patch("core_hook.McpClient") as client:
                with self.assertRaises(ValueError):
                    core_hook._collect_scope(target, time.monotonic() + 1)
                client.assert_not_called()

    def test_global_database_pin_refuses_wrong_physical_store_before_network(self):
        target = core_hook._targets(self.write_config(), str(self.cwd))[0]
        config = json.loads(self.global_service.read_text())
        config["database_path"] = str(self.project_path)
        self.global_service.write_text(json.dumps(config))
        with patch("core_hook.McpClient") as client:
            with self.assertRaisesRegex(ValueError, "global store"):
                core_hook._collect_scope(target, time.monotonic() + 1)
            client.assert_not_called()

    def test_missing_or_oversized_provenance_is_omitted_not_invented(self):
        target = {"scope": "global", "db": "user"}
        for provenance in (None, {}, "not an object", {"type": "web", "url": "x" * 9000}):
            with self.subTest(provenance=str(provenance)[:80]):
                section = core_hook._pack(target, native([{**node(), "provenance": provenance}]))
                self.assertEqual(section["outcome"], "partial")
                self.assertEqual(section["cards"], [])

    def test_empty_native_core_distinguished_from_unavailable_and_malformed(self):
        server = self.server("user", native([]))
        self.config_value["projects"] = []
        text, sections = self.context()
        self.assertEqual(sections[0], {"scope": "global", "db": "user", "outcome": "empty", "cards": [], "total": 0})
        self.assertIn("not that all memory is empty", text)
        for value in ({}, {"nodes": [], "total": 0, "truncated": False},
                      native([node()], total=0), native([node(body="bad")], truncated=True)):
            server.core = value
            with self.subTest(value=value):
                text, sections = self.context()
                self.assertEqual(sections[0]["outcome"], "unavailable")
                self.assertEqual(sections[0]["cards"], [])
                self.assertIn("core with db=user first", text)
        server.refuse_core = True
        server.core = "secret refusal diagnostic"
        text, sections = self.context()
        self.assertNotIn("secret refusal", text)
        self.assertEqual(sections[0]["outcome"], "unavailable")

    def test_native_truncated_and_unavailable_body_are_honestly_partial(self):
        truncated = node(body="native prefix")
        truncated["body_truncated"] = True
        server = self.server("user", native([truncated], truncated=True, bodies_truncated=True))
        self.config_value["projects"] = []
        text, sections = self.context()
        self.assertEqual(sections[0]["outcome"], "partial")
        self.assertTrue(sections[0]["cards"][0]["body_truncated"])
        self.assertIn("not evidence of absence", text)
        missing = node(body=None)
        del missing["body_truncated"]
        server.core = native([missing])
        _, sections = self.context()
        self.assertEqual(sections[0]["outcome"], "partial")
        self.assertIsNone(sections[0]["cards"][0]["body"])
        summary = node()
        summary["summary_truncated"] = True
        server.core = native([summary])
        _, sections = self.context()
        self.assertEqual(sections[0]["outcome"], "partial")

    def test_utf8_and_json_escape_budgets_preserve_complete_fit_cards(self):
        # One giant card must not prevent smaller complete cards later in the
        # native core order. Quotes/backslashes charge their serialized cost.
        large = node(ID1, "large", "é" * 4000)
        escaped = node(ID2, "escaped", '\\"\n' * 300)
        small = node(ID3, "small", "🦋" * 100)
        for target in ({"scope": "global", "db": "user"}, {"scope": "project", "db": "project"}):
            section = core_hook._pack(target, native([large, escaped, small]))
            self.assertEqual(section["outcome"], "partial")
            self.assertEqual([card["id"] for card in section["cards"]], [ID2, ID3])
            self.assertEqual(section["cards"][0]["body"], escaped["body"])
            encoded = core_hook._json(section).encode()
            self.assertLessEqual(len(encoded), 8192)
            self.assertEqual(json.loads(encoded), section)
        sections = [core_hook._pack({"scope": scope, "db": db}, native([node(body="é" * 3600)]))
                    for scope, db in (("global", "user"), ("project", "project"))]
        result = core_hook._context(sections)
        text = result["hookSpecificOutput"]["additionalContext"]
        self.assertLessEqual(len(text.encode()), 18 * 1024)
        self.assertGreater(len(text.encode()), 14 * 1024)
        self.assertEqual(len([json.loads(line) for line in text.splitlines() if line.startswith("{")]), 2)

    def test_passive_short_readiness_retries_global_and_project(self):
        self.server("user", not_ready=2)
        self.server("project", not_ready=1)
        _, sections = self.context(timeout=3)
        self.assertEqual([s["outcome"] for s in sections], ["ok", "ok"])
        self.assertEqual([db for db, method, _ in self.calls if method == "initialize"],
                         ["user", "user", "user", "project", "project"])

    def test_parent_real_wall_deadline_kills_stalled_http_child(self):
        self.require_native_client()
        class Slow(socketserver.BaseRequestHandler):
            def handle(self):
                self.request.recv(4096)
                time.sleep(1)

        class Server(socketserver.ThreadingTCPServer):
            allow_reuse_address = True
            daemon_threads = True

        with Server(("127.0.0.1", 0), Slow) as server:
            threading.Thread(target=server.serve_forever, daemon=True).start()
            data = json.loads(self.global_service.read_text())
            data["port"] = server.server_address[1]
            self.global_service.write_text(json.dumps(data))
            self.config_value["projects"] = []
            started = time.monotonic()
            text, sections = self.context(timeout=0.2)
            elapsed = time.monotonic() - started
            server.shutdown()
        self.assertLess(elapsed, 0.65)
        self.assertEqual(sections[0]["outcome"], "unavailable")
        self.assertIn("core with db=user first", text)
        self.assertFalse((self.root / "user-state").exists())

    def test_good_global_survives_parent_timeout_during_project(self):
        targets = core_hook._targets(self.write_config(), str(self.cwd))
        flushed = []

        def collect(target, deadline):
            if target["db"] == "project":
                # The first complete section must be flushed before attempting
                # project work; model the parent's kill at this boundary.
                self.assertEqual(len(flushed), 1)
                raise subprocess.TimeoutExpired("scripted collector", .4,
                                                output=(flushed[0] + "\n").encode())
            return core_hook._pack(target, native())

        def child(*args, **kwargs):
            with patch("core_hook.sys.stdin") as stdin, patch("core_hook._collect_scope", side_effect=collect), \
                 patch("builtins.print", side_effect=lambda line, **kw: (self.assertTrue(kw.get("flush")), flushed.append(line))):
                stdin.buffer.read.return_value = kwargs["input"]
                core_hook._child_main()

        with patch("core_hook.subprocess.run", side_effect=child):
            sections = core_hook.collect_core(targets, timeout=.4)
        text = core_hook._context(sections)["hookSpecificOutput"]["additionalContext"]
        self.assertEqual([section["outcome"] for section in sections], ["ok", "unavailable"])
        self.assertIn(ID1, text)
        self.assertIn("then core with db=project", text)

    def test_parent_timeout_preserves_only_complete_and_correct_scope_records(self):
        targets = core_hook._targets(self.write_config(), str(self.cwd))
        section = core_hook._pack(targets[0], native())
        raw = (core_hook._json(section) + "\n" + '{"incomplete":').encode()
        error = subprocess.TimeoutExpired(["child"], 0.2, output=raw)
        with patch("core_hook.subprocess.run", side_effect=error):
            sections = core_hook.collect_core(targets, 0.2)
        self.assertEqual([s["outcome"] for s in sections], ["ok", "unavailable"])
        for raw in (b"not json\n", b"{}\n", b"x" * (core_hook.MAX_CHILD_OUTPUT + 1),
                    (core_hook._json({**section, "db": "project"}) + "\n").encode()):
            with self.subTest(raw=raw[:80]):
                self.assertEqual(core_hook._read_sections(raw, targets), [])

    def test_collector_input_contains_only_selected_configs_not_event_or_prompt(self):
        event = {**self.event, "prompt": "never-forward-this-prompt", "transcript_path": "/never-read-this-transcript"}
        completed = subprocess.CompletedProcess([], 0, stdout=b"", stderr=b"")
        with patch("core_hook.subprocess.run", return_value=completed) as run:
            core_hook.handle_event(event, self.write_config())
        call = run.call_args
        self.assertEqual(call.args[0][-1], "--collect")
        self.assertEqual(call.kwargs["timeout"], 12)
        payload = json.loads(call.kwargs["input"])
        self.assertEqual(set(payload), {"targets", "timeout"})
        self.assertNotIn("never-forward-this-prompt", core_hook._json(payload))
        self.assertNotIn("never-read-this-transcript", core_hook._json(payload))

    def test_invalid_config_and_timeout_cli_fail_open_without_raw_error(self):
        self.config_path.write_text('{"secret-config-error": true}')
        result = self.cli(self.event)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stderr, b"")
        self.assertIn("unavailable", result.stdout.decode())
        self.assertNotIn("secret-config-error", result.stdout.decode())
        self.write_config()
        for timeout in ("nan", "inf", "0", "-1", "13"):
            with self.subTest(timeout=timeout):
                result = self.cli(self.event, "--timeout", timeout)
                self.assertEqual(result.returncode, 0)
                self.assertIn("unavailable", result.stdout.decode())


if __name__ == "__main__":
    unittest.main()

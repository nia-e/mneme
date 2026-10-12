import io
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import tempfile
import time
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import core_hook
import store_registry


ID = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
OTHER_ID = "01ARZ3NDEKTSV4RRFFQ69G5FAW"


def core_value(body="Selected core"):
    return {"nodes": [{"id": ID, "summary": "Identity", "summary_truncated": False,
                       "body": body, "body_truncated": False, "tags": ["core"],
                       "status": "active", "provenance": {"type": "conversation", "session": "fixture"}}],
            "total": 1, "truncated": False, "nodes_truncated": False,
            "bodies_truncated": False, "body_bytes_limit": 131072}


class CoreRegistryTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.project = self.root / "project"
        self.project.mkdir()
        self.registry = self.root / "stores.json"
        self.service = self.root / "owner.json"
        self.service.write_text(json.dumps({"schema": 1, "mode": "remote",
            "url": "ssh://owner@fixture.invalid", "database_name": "thoughts",
            "database_path": "/srv/mneme/thoughts.db"}), encoding="utf-8")
        self.entry = {"name": "personal", "service_config": str(self.service),
                      "db_alias": "thoughts", "database_path": "/srv/mneme/thoughts.db",
                      "db_id": ID, "purpose": "Authored continuity", "read": {"enabled": True, "core": True}}
        self.write_registry([self.entry])
        self.config_path = self.root / "core.json"
        self.config_path.write_text(json.dumps({"schema": core_hook.REGISTRY_CONFIG_SCHEMA,
                                               "store_registry": str(self.registry)}), encoding="utf-8")

    def write_registry(self, entries):
        self.registry.write_text(json.dumps({"schema": store_registry.SCHEMA, "stores": entries}), encoding="utf-8")

    def selection(self):
        return core_hook._registry_selection(core_hook._config(self.config_path), str(self.project))

    def test_v2_has_no_implicit_global_or_library_fallback(self):
        self.write_registry([])
        with patch("core_hook.load_config") as legacy, patch("core_hook.collect_registry_core") as collect:
            result = core_hook.handle_event({"hook_event_name": "SessionStart", "source": "startup",
                                             "cwd": str(self.project)}, core_hook._config(self.config_path))
        legacy.assert_not_called()
        collect.assert_not_called()
        self.assertIn("No core stores are selected", result["hookSpecificOutput"]["additionalContext"])

    def test_named_profile_cannot_resurrect_legacy_ambient_global(self):
        metadata = self.project / ".mneme"
        metadata.mkdir()
        (metadata / "profile.json").write_text(json.dumps({"schema": "mneme.profile.v2", "mode": "default",
                                                          "stores": ["personal"]}), encoding="utf-8")
        legacy = {"global_service": self.root / "never-read.json", "global_database": "/never/read.db", "projects": []}
        self.assertEqual(core_hook._targets(legacy, str(self.project)), [])

    def test_explicit_project_precedes_named_extras(self):
        service = self.root / "project.json"
        service.write_text(json.dumps({"binary": str(self.root / "never-start"), "port": 18766,
            "state_dir": str(self.root / "never-create"), "working_directory": str(self.project),
            "database_name": "project", "database_path": str(self.project / ".mneme/codex-memory.db")}), encoding="utf-8")
        config = {"store_registry": self.registry, "projects": [{"root": self.project,
                   "service_config": service, "db_id": OTHER_ID}]}
        selection = core_hook._registry_selection(config, str(self.project))
        self.assertEqual([store.name for store in selection.core], ["project", "personal"])
        self.assertEqual(selection.core[0].db_id, OTHER_ID)

    def test_excluded_owner_never_loads_service_or_connects(self):
        self.write_registry([{**self.entry, "service_config": str(self.root / "missing-service"),
                              "when": {"excluded_roots": [str(self.project)]}}])
        with patch("store_registry.load_config") as load, patch("core_hook.client_for_service") as client:
            selected = self.selection()
        self.assertEqual(selected.core, ())
        self.assertEqual(len(selected.omissions), 1)
        load.assert_not_called()
        client.assert_not_called()

    def test_profile_and_registry_drift_refuse_before_contact(self):
        selection = self.selection()
        self.write_registry([{**self.entry, "purpose": "changed purpose"}])
        with patch("core_hook.client_for_service") as client, self.assertRaisesRegex(ValueError, "changed"):
            core_hook._collect_registry_scope(selection, selection.core[0], time.monotonic() + 1, 8192)
        client.assert_not_called()

    def test_core_configuration_drift_refuses_before_contact(self):
        config = core_hook._config(self.config_path)
        selection = core_hook._registry_selection(config, str(self.project))
        self.config_path.write_text(json.dumps({"schema": core_hook.REGISTRY_CONFIG_SCHEMA,
            "store_registry": str(self.root / "replacement.json")}), encoding="utf-8")
        with patch("core_hook.client_for_service") as client, self.assertRaisesRegex(ValueError, "configuration changed"):
            core_hook._collect_registry_scope(selection, selection.core[0], time.monotonic() + 1, 8192,
                                               config_guard=config["_config_guard"])
        client.assert_not_called()

    def test_revocation_during_core_call_does_not_deliver_memory(self):
        selection = self.selection()
        client = SimpleNamespace(connect=lambda: None, close=lambda: None)

        def call(name, args):
            if name == "databases":
                return [{"db": "thoughts", "name": "thoughts", "state": "open",
                         "configured_path": "/srv/mneme/thoughts.db", "db_id": ID}]
            self.write_registry([])
            return core_value()

        client.call_tool = call
        with patch("core_hook.client_for_service", return_value=client), self.assertRaisesRegex(ValueError, "changed"):
            core_hook._collect_registry_scope(selection, selection.core[0], time.monotonic() + 1, 8192)

    def test_parent_revalidates_before_exposing_completed_sections(self):
        selection = self.selection()
        section = core_hook._pack(core_hook._registry_target(selection.core[0]), core_value())

        def collect(*_args, **_kwargs):
            self.write_registry([])
            return [section]

        with patch("core_hook.collect_registry_core", side_effect=collect), self.assertRaisesRegex(ValueError, "changed"):
            core_hook.handle_selection({"hook_event_name": "SessionStart", "source": "startup",
                                        "cwd": str(self.project)}, selection)

    def test_passive_remote_core_identity_pin_and_bounded_body(self):
        selection = self.selection()
        client = SimpleNamespace(connect=lambda: None, close=lambda: None)
        calls = []

        def call(name, args):
            calls.append((name, args))
            if name == "databases":
                return [{"db": "thoughts", "name": "thoughts", "state": "open",
                         "configured_path": "/srv/mneme/thoughts.db", "db_id": ID}]
            return core_value()

        client.call_tool = call
        with patch("core_hook.client_for_service", return_value=client) as factory, patch("service.ensure_ready") as start:
            section = core_hook._collect_registry_scope(selection, selection.core[0], time.monotonic() + 1, 2048)
        start.assert_not_called()
        self.assertEqual(factory.call_args.args[0].url, "ssh://owner@fixture.invalid")
        self.assertEqual(section["scope"], "personal")
        self.assertEqual(section["outcome"], "ok")
        self.assertEqual(calls[-1], ("core", {"db": "thoughts", "expected_db_id": ID, "max_body_bytes": 2048}))

    def test_wrong_native_identity_refuses_before_core(self):
        selection = self.selection()
        client = SimpleNamespace(connect=lambda: None, close=lambda: None)
        calls = []

        def call(name, args):
            calls.append(name)
            return [{"db": "thoughts", "name": "thoughts", "state": "open",
                     "configured_path": "/srv/mneme/thoughts.db", "db_id": OTHER_ID}]

        client.call_tool = call
        with patch("core_hook.client_for_service", return_value=client), self.assertRaisesRegex(ValueError, "owner catalog"):
            core_hook._collect_registry_scope(selection, selection.core[0], time.monotonic() + 1, 8192)
        self.assertEqual(calls, ["databases"])

    def test_arbitrary_stores_share_one_time_and_byte_budget(self):
        stores = tuple(SimpleNamespace(name="owner-%d" % i, db_alias="memory") for i in range(80))
        selection = SimpleNamespace(core=stores, omissions=())
        request = json.dumps({"selection": {}, "timeout": 12}).encode()
        output = []
        clock = [0.0]
        deadlines = []

        def collect(_selection, store, deadline, limit, **_):
            deadlines.append(deadline)
            clock[0] += .01
            return core_hook._pack(core_hook._registry_target(store), core_value("x" * 7000), limit)

        with patch("store_registry.selection_from_dict", return_value=selection), \
             patch("core_hook.sys.stdin", SimpleNamespace(buffer=io.BytesIO(request))), \
             patch("builtins.print", side_effect=lambda value, **_: output.append(value)), \
             patch("core_hook.time.monotonic", side_effect=lambda: clock[0]), \
             patch("core_hook._collect_registry_scope", side_effect=collect):
            self.assertEqual(core_hook._registry_child_main(), 0)
        encoded = "\n".join(output) + "\n"
        self.assertLessEqual(len(encoded.encode()), core_hook.MAX_REGISTRY_OUTPUT)
        self.assertLess(len(output), len(stores))
        self.assertLessEqual(max(deadlines), 12)
        self.assertAlmostEqual(deadlines[0], 12 / 80)
        context = core_hook._registry_context(selection, [json.loads(line) for line in output])
        text = context["hookSpecificOutput"]["additionalContext"]
        self.assertIn("omitted by the shared context budget", text)
        self.assertLessEqual(len(text.encode()), 18 * 1024)

    def test_parent_watchdog_preserves_complete_records_and_marks_rest_unavailable(self):
        selection = self.selection()
        section = core_hook._pack(core_hook._registry_target(selection.core[0]), core_value())
        import subprocess
        with patch("core_hook.subprocess.run", side_effect=subprocess.TimeoutExpired(
                "collector", 1, output=(core_hook._json(section) + "\n").encode())) as run:
            self.assertEqual(core_hook.collect_registry_core(selection, 1), [section])
        self.assertEqual(run.call_args.kwargs["timeout"], 1)
        self.assertIn("--collect-registry", run.call_args.args[0])

    def test_metadata_is_charged_to_the_shared_remaining_budget(self):
        target = {"scope": "named", "db": "memory"}
        value = core_value()
        value["total"] = int("9" * 1000)
        section = core_hook._pack(target, value, 128)
        self.assertLessEqual(len(core_hook._json(section).encode()), 128)
        self.assertEqual(section["outcome"], "unavailable")

    def test_v2_rejects_mixed_legacy_keys_and_bad_project_pin(self):
        for change in ({"global_service": str(self.service)},
                       {"projects": [{"root": str(self.project), "service_config": str(self.service), "db_id": "bad"}]}):
            self.config_path.write_text(json.dumps({"schema": core_hook.REGISTRY_CONFIG_SCHEMA,
                "store_registry": str(self.registry), **change}), encoding="utf-8")
            with self.subTest(change=change), self.assertRaises(ValueError):
                core_hook._config(self.config_path)

    def test_v12_hook_config_uses_same_selection_without_actor_or_state_creation(self):
        project_service = self.root / "project-owner.json"
        project_service.write_text(json.dumps({"schema": 1, "mode": "remote", "database_name": "project",
            "url": "ssh://project@fixture.invalid", "database_path": "/srv/project.db"}), encoding="utf-8")
        hook_path = self.root / "hook.json"
        hook_path.write_text(json.dumps({"schema": "mneme.codex-hooks.config.v12", "project_root": str(self.project),
            "service_config": str(project_service), "store_registry": str(self.registry),
            "state_dir": str(self.root / "never-created"), "memory_mode": "async", "recording_mode": "off",
            "reader_model": "gpt-6.1-sol", "librarian_effort": "low", "reader_codex": str(self.root / "never-start"),
            "reader_codex_sha256": "a" * 64}), encoding="utf-8")
        event = {"hook_event_name": "SessionStart", "source": "startup", "cwd": str(self.project)}
        before = set(self.root.rglob("*"))
        config = core_hook._hook_config(hook_path, event)
        with patch("core_hook.collect_registry_core", return_value=[]) as collect, patch("service.ensure_ready") as ready:
            core_hook.handle_hook_event(event, config)
        selected = collect.call_args.args[0]
        self.assertEqual([store.name for store in selected.core], ["project", "personal"])
        self.assertEqual(selected.recordable, ())
        self.assertEqual(collect.call_args.kwargs["config_guard"]["path"], str(hook_path))
        ready.assert_not_called()
        self.assertEqual(set(self.root.rglob("*")), before)

    def test_hook_cli_noop_never_reads_configuration(self):
        with patch("core_hook.sys.stdin", SimpleNamespace(buffer=io.BytesIO(b'{"hook_event_name":"Stop"}'))), \
             patch("builtins.print") as output, patch("core_hook._hook_config") as load:
            self.assertEqual(core_hook.main(["--hook-config", str(self.root / "missing")]), 0)
        load.assert_not_called()
        output.assert_called_once_with("{}")

    def test_native_three_owner_subprocess_is_project_first_and_passive(self):
        binary = os.environ.get("MNEME_CLIENT_BINARY")
        if not binary or not Path(binary).is_file():
            self.skipTest("set MNEME_CLIENT_BINARY to a built compatible mnemed")
        calls = []

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def reply(self, status, value=None, session=False):
                data = json.dumps(value).encode() if value is not None else b""
                self.send_response(status)
                if data:
                    self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                if session:
                    self.send_header("mcp-session-id", "registry-fixture")
                self.end_headers()
                self.wfile.write(data)

            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                method = request["method"]
                if method == "notifications/initialized":
                    self.reply(202)
                    return
                if method == "initialize":
                    result = {"protocolVersion": "2025-11-25", "capabilities": {"tools": {}},
                              "serverInfo": {"name": "mneme-mcp"}}
                elif method == "tools/list":
                    result = {"tools": [{"name": name, "inputSchema": {"type": "object", "properties":
                                        {"expected_db_id": {"type": "string", "minLength": 26, "maxLength": 26,
                                        "pattern": "^[0-7][0-9A-HJKMNP-TV-Z]{25}$"}} if name == "core" else {}}}
                                        for name in ("databases", "core")]}
                else:
                    name = request["params"]["name"]
                    calls.append((self.server.alias, name, request["params"]["arguments"]))
                    value = self.server.catalog if name == "databases" else core_value(self.server.alias)
                    result = {"content": [{"type": "text", "text": json.dumps(value)}], "isError": False}
                self.reply(200, {"jsonrpc": "2.0", "id": request["id"], "result": result}, method == "initialize")

            def do_DELETE(self):
                self.reply(204)

        entries = []
        for index, (alias, identifier) in enumerate((("project", OTHER_ID), ("thoughts", ID),
                                                    ("research", "01ARZ3NDEKTSV4RRFFQ69G5FAX"))):
            path = str(self.project / ".mneme/codex-memory.db") if index == 0 else "/srv/%s.db" % alias
            server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
            server.daemon_threads = True
            server.alias = alias
            server.catalog = [{"db": alias, "name": alias, "db_id": identifier,
                               "state": "open", "configured_path": path}]
            threading.Thread(target=server.serve_forever, daemon=True).start()
            self.addCleanup(server.server_close)
            self.addCleanup(server.shutdown)
            service = self.root / (alias + ".json")
            if index == 0:
                # The project is a native local owner even though its HTTP
                # process is this finite peer. Core is passive: these launch
                # fields must not create state or start an owner. Remote extra
                # identities remain opaque POSIX paths on Windows too.
                owner = {"binary": str(Path(binary).resolve()), "database_name": alias,
                         "database_path": path, "port": server.server_port,
                         "state_dir": str(self.root / "never-create-project-state"),
                         "working_directory": str(self.project)}
            else:
                owner = {"schema": 1, "mode": "remote", "database_name": alias,
                         "database_path": path, "url": "http://127.0.0.1:%d/" % server.server_port}
            service.write_text(json.dumps(owner), encoding="utf-8")
            if index:
                entries.append({**self.entry, "name": alias, "db_alias": alias, "db_id": identifier,
                                "database_path": path, "service_config": str(service)})
        self.write_registry(entries)
        config = {"store_registry": self.registry, "projects": [{"root": self.project,
                  "service_config": self.root / "project.json", "db_id": OTHER_ID}]}
        before = set(self.root.rglob("*"))
        selection = core_hook._registry_selection(config, str(self.project))
        sections = core_hook.collect_registry_core(selection)
        self.assertEqual([section["db"] for section in sections], ["project", "thoughts", "research"])
        self.assertEqual([section["outcome"] for section in sections], ["ok", "ok", "ok"])
        self.assertEqual([alias for alias, name, _ in calls if name == "core"], ["project", "thoughts", "research"])
        self.assertTrue(all("expected_db_id" in args for _, name, args in calls if name == "core"))
        self.assertEqual(set(self.root.rglob("*")), before)


if __name__ == "__main__":
    unittest.main()

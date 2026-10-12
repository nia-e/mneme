import io
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import launcher
import store_registry


ID = "01ARZ3NDEKTSV4RRFFQ69G5FAV"


class LauncherRegistryTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.service = self.root / "service.json"
        self.service.write_text(json.dumps({"schema": 1, "mode": "remote",
            "url": "ssh://owner@fixture.invalid", "database_name": "notes",
            "database_path": "/srv/notes.db"}), encoding="utf-8")
        self.registry = self.root / "stores.json"
        self.entry = {"name": "research", "service_config": str(self.service),
                      "db_alias": "notes", "database_path": "/srv/notes.db", "db_id": ID,
                      "purpose": "Research continuity", "read": {"enabled": True},
                      "record": {"enabled": False}}
        self.write_registry()
        self.selection = store_registry.resolve(self.root, registry_path=self.registry, mode="read")
        self.calls = []
        self.ready = []
        self.client = SimpleNamespace(connect=lambda: self.calls.append("connect"),
            close=lambda: self.calls.append("close"),
            connect_result={"protocolVersion": "2025-11-25", "serverInfo": {"name": "mneme-mcp"}},
            call_tool=self.call_tool, list_tools=self.list_tools, raw_rpc=self.raw_rpc)

    def write_registry(self):
        self.registry.write_text(json.dumps({"schema": store_registry.SCHEMA, "stores": [self.entry]}), encoding="utf-8")

    def call_tool(self, name, arguments):
        self.calls.append((name, arguments))
        return [{"db": "notes", "name": "notes", "state": "open", "configured_path": "/srv/notes.db", "db_id": ID}]

    def list_tools(self):
        self.calls.append("list_tools")
        return [{"name": "capture", "inputSchema": {"properties": {"db": {}, "expected_db_id": {}}}},
                {"name": "databases", "inputSchema": {"properties": {}}}]

    def raw_rpc(self, method, arguments):
        self.calls.append((method, arguments))
        return {"content": [], "isError": False}

    def relay(self):
        return launcher.Relay(self.selection.named("research").service(), ensure=self.ready.append,
                              client_factory=lambda *_args, **_kwargs: self.client,
                              selection=self.selection, store_name="research")

    def initialize(self, relay):
        return json.loads(relay.handle({"jsonrpc": "2.0", "method": "initialize", "id": 1,
                                       "params": {"protocolVersion": "2025-11-25"}}))

    def test_read_exposure_does_not_turn_auto_record_denial_into_manual_write_denial(self):
        relay = self.relay()
        self.assertIn("result", self.initialize(relay))
        request = {"name": "capture", "arguments": {"summary": "Explicit user request"}}
        result = json.loads(relay.handle({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": request}))
        self.assertIn("result", result)
        self.assertEqual(self.calls[-1], ("tools/call", {"name": "capture", "arguments": {
            "summary": "Explicit user request", "db": "notes", "expected_db_id": ID}}))
        self.assertNotIn("expected_db_id", request["arguments"])

    def test_conflicting_alias_and_identity_pin_refused_without_dispatch(self):
        for arguments in ({"db": "other"}, {"expected_db_id": "01ARZ3NDEKTSV4RRFFQ69G5FAW"}):
            relay = self.relay()
            self.assertIn("result", self.initialize(relay))
            before = len(self.calls)
            result = json.loads(relay.handle({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "capture", "arguments": arguments}}))
            self.assertIn("selected store", result["error"]["message"])
            self.assertFalse(any(isinstance(item, tuple) and item[0] == "tools/call" for item in self.calls[before:]))

    def test_metadata_tools_not_given_database_or_pin_arguments(self):
        relay = self.relay()
        self.initialize(relay)
        relay.handle({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                      "params": {"name": "databases", "arguments": {}}})
        self.assertEqual(self.calls[-1], ("tools/call", {"name": "databases", "arguments": {}}))

    def test_union_branch_guard_is_not_silently_dropped(self):
        self.client.list_tools = lambda: [{"name": "list", "inputSchema": {"oneOf": [
            {"properties": {"expected_db_id": {}, "kind": {"const": "nodes"}}},
            {"properties": {"expected_db_id": {}, "kind": {"const": "touchstones"}}}]}}]
        relay = self.relay()
        self.initialize(relay)
        relay.handle({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                      "params": {"name": "list", "arguments": {"kind": "nodes"}}})
        self.assertEqual(self.calls[-1][1]["arguments"]["expected_db_id"], ID)

    def test_legacy_owner_schema_does_not_silently_downgrade_identity_pin(self):
        self.client.list_tools = lambda: [{"name": "capture", "inputSchema": {"properties": {"db": {}}}}]
        relay = self.relay()
        self.initialize(relay)
        relay.handle({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                      "params": {"name": "capture", "arguments": {"summary": "explicit request"}}})
        # Native refuses unsupported guards; the relay must not omit the guard
        # and accidentally downgrade this to an old unpinned owner operation.
        self.assertEqual(self.calls[-1][1]["arguments"]["expected_db_id"], ID)

    def test_rejected_native_identity_never_exposes_catalog(self):
        self.client.call_tool = lambda *_: [{"db": "notes", "name": "notes", "state": "open",
            "configured_path": "/srv/notes.db", "db_id": "01ARZ3NDEKTSV4RRFFQ69G5FAW"}]
        result = self.initialize(self.relay())
        self.assertIn("unexpected database catalog", result["error"]["message"])
        self.assertNotIn("list_tools", self.calls)

    def test_changed_registry_refuses_before_readiness(self):
        relay = self.relay()
        self.entry["read"]["enabled"] = False
        self.write_registry()
        result = self.initialize(relay)
        self.assertIn("error", result)
        self.assertEqual(self.ready, [])
        self.assertEqual(self.calls, [])

    def test_mid_session_change_refuses_without_replay(self):
        relay = self.relay()
        self.initialize(relay)
        self.entry["purpose"] = "new selection"
        self.write_registry()
        result = json.loads(relay.handle({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}))
        self.assertIn("not retried", result["error"]["message"])
        self.assertEqual(len(self.ready), 1)
        self.assertNotIn(("tools/list", {}), self.calls)

    def test_reconnect_catalog_revocation_prevents_following_raw_write(self):
        relay = self.relay()
        self.initialize(relay)
        relay._reset()

        def tools():
            self.entry["read"]["enabled"] = False
            self.write_registry()
            return self.list_tools()

        self.client.list_tools = tools
        result = json.loads(relay.handle({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "capture", "arguments": {"summary": "must not send"}}}))
        self.assertIn("error", result)
        self.assertFalse(any(isinstance(item, tuple) and item[0] == "tools/call" for item in self.calls))

    def test_response_after_revocation_is_not_exposed_or_retried(self):
        relay = self.relay()
        self.initialize(relay)

        def response(method, params):
            self.entry["read"]["enabled"] = False
            self.write_registry()
            return self.raw_rpc(method, params)

        self.client.raw_rpc = response
        result = json.loads(relay.handle({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "capture", "arguments": {"summary": "sent once"}}}))
        self.assertIn("not retried", result["error"]["message"])
        self.assertEqual(sum(isinstance(item, tuple) and item[0] == "tools/call" for item in self.calls), 1)

    def test_default_client_factory_uses_explicit_remote_service_adapter(self):
        with patch("launcher.client_for_service", return_value=self.client) as factory:
            relay = launcher.Relay(self.selection.named("research").service(), ensure=self.ready.append,
                                   selection=self.selection, store_name="research")
            self.assertIn("result", self.initialize(relay))
        self.assertEqual(factory.call_args.args[0].url, "ssh://owner@fixture.invalid")

    def test_named_launcher_routes_without_legacy_selection_or_enrollment(self):
        with patch("launcher.Path.cwd", return_value=self.root), \
             patch("launcher.run_stream") as run, patch("launcher.load_config") as load, \
             patch("launcher._enroll_selected_project") as enroll:
            launcher.main(["--store-registry", str(self.registry), "--store", "research"])
        self.assertEqual(run.call_args.kwargs["store_name"], "research")
        self.assertEqual(run.call_args.args[0].database_name, "notes")
        load.assert_not_called()
        enroll.assert_not_called()

    def test_missing_or_mixed_cli_selection_is_rejected(self):
        for args in (["--store-registry", str(self.registry)], ["--service-config", str(self.service), "--store", "research"],
                     ["--service-config", str(self.service), "--store-registry", str(self.registry), "--store", "research"]):
            with self.subTest(args=args), patch("sys.stderr", io.StringIO()), self.assertRaises(SystemExit):
                launcher.main(args)


if __name__ == "__main__":
    unittest.main()

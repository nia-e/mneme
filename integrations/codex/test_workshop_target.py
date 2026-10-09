"""Deterministic explicit-user-store routing; no native/provider/live calls."""
import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch, Mock

import hook_recall
import hooks
import recording_contract
import recording_jobs
import reader_worker
from target_policy import workshop_policy, policy_for, target_kwargs
import workshop_config

DB = "1" * 26
NODE = "2" * 26
OTHER = "3" * 26


class WorkshopTargetTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.store = self.root / "personal" / "memory.db"
        self.store.parent.mkdir()
        self.store.write_bytes(b"fixture only")
        self.reader = self.root / "codex"
        self.reader.write_text("#!/bin/sh\nexit 0\n")
        self.reader.chmod(0o700)
        self.service = self.root / "service.json"
        self.service.write_text(json.dumps({"binary": str(self.reader), "database_name": "user",
            "database_path": str(self.store), "working_directory": str(self.root),
            "state_dir": str(self.root / "service-state"), "port": 18766}))
        self.prepared = workshop_config.prepare(workspace_root=str(self.root), state_dir=str(self.root / "state"),
            service_config=str(self.service), database_path=str(self.store), db_id=DB, reader_codex=str(self.reader))
        self.hook = self.root / "hooks.json"
        self.hook.write_text(json.dumps(self.prepared["hook_config"]))
        self.config = hooks._config(self.hook)
        self.policy = workshop_policy(self.config)
        self.catalog = [{"db": "user", "name": "user", "state": "open",
                         "configured_path": str(self.store), "db_id": DB}]
        self.node = {"id": NODE, "status": "active", "summary": "Prior shared choice",
            "summary_truncated": False, "memory_kind": {"kind": "semantic"},
            "provenance": {"type": "conversation", "session": "fixture", "turn": 1}}
        from test_touchstone_librarian import coverage
        from test_hook_recall import discovery_metadata
        self.context = {"schema": "mneme.context.v7", "core": [], "primary": [{"id": NODE}],
                        "expansions": [], "episodes": [], "touchstone_retrieval": coverage(),
                        "observation": {"schema": 1, "learning": "disabled", "cards": [
                            {"node_id": NODE, "card_sha256": "a" * 64, "lane": "primary"}]},
                        **discovery_metadata()}

    def client(self, *, drift=None):
        owner = self
        class Client:
            timeout = 1
            def __init__(self): self.calls = []
            def __enter__(self): return self
            def __exit__(self, *_): pass
            def call_tool(self, name, args):
                self.calls.append((name, args))
                if name == "databases": return owner.catalog
                if name == "recall_context":
                    if drift: drift()
                    return owner.context
                if name == "get": return owner.node
                raise AssertionError(name)
        return Client()

    def test_prepare_is_explicit_reviewed_and_v8_remains_project_only(self):
        self.assertEqual(self.prepared["installation"], "not performed")
        self.assertEqual(self.prepared["service"], "not contacted")
        self.assertEqual(self.config["schema"], hooks.CONFIG_SCHEMA_V9)
        self.assertTrue(recording_jobs.enabled(self.config))
        self.assertFalse((self.root / "state").exists())
        old = dict(self.prepared["hook_config"], schema=hooks.CONFIG_SCHEMA_V8)
        old.pop("store_target")
        self.hook.write_text(json.dumps(old))
        with self.assertRaisesRegex(hooks.HookError, "reminder"):
            hooks._config(self.hook)
        with self.assertRaises(ValueError):
            policy_for(self.service, self.root)  # User alias is never a project fallback.

    def test_successful_user_read_and_overlap_have_singleton_identity_guards(self):
        client = self.client()
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall.collect_reader(self.service, "Recall our earlier shared decision", self.root,
                                               **target_kwargs(self.config))
        self.assertEqual(result["outcome"], "ok", result)
        self.assertEqual(result["db_id"], DB)
        self.assertEqual(result["cards"][0]["id"], NODE)
        self.assertEqual([name for name, _ in client.calls], ["databases", "recall_context", "get", "databases"])
        self.assertTrue(all(args["db"] == "user" for name, args in client.calls if name != "databases"))
        client = self.client()
        with patch("hook_recall.McpClient", return_value=client):
            overlap = hook_recall.collect_overlap(self.service, "Recall our shared decision", self.root,
                expected_db_id=DB, **target_kwargs(self.config))
        self.assertEqual(overlap["outcome"], "ok", overlap)
        self.assertEqual(overlap["db_id"], DB)
        self.assertTrue(all(args["db"] == "user" for name, args in client.calls if name != "databases"))

    def test_catalog_wrong_alias_path_id_and_multistore_refused_before_read(self):
        original = copy.deepcopy(self.catalog)
        for change in ({"db": "project"}, {"name": "project"}, {"configured_path": str(self.root / "fake.db")},
                       {"db_id": OTHER}):
            with self.subTest(change=change):
                self.catalog = [{**original[0], **change}]
                client = self.client()
                with patch("hook_recall.McpClient", return_value=client):
                    result = hook_recall.collect_reader(self.service, "Recall our shared decision", self.root,
                                                       store_target=self.policy)
                self.assertEqual(result["outcome"], "unavailable")
                self.assertEqual([name for name, _ in client.calls], ["databases"])
        with self.assertRaises(ValueError):
            self.policy.catalog_identity(original * 2)

    def test_config_drift_discards_user_pool_and_fences_old_jobs(self):
        before = recording_jobs._config_digest(self.config)
        client = self.client(drift=lambda: self.service.write_text(self.service.read_text() + "\n"))
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall.collect_reader(self.service, "Recall our shared decision", self.root,
                                               store_target=self.policy)
        self.assertEqual(result["outcome"], "unavailable")
        self.assertEqual(result["cards"], [])
        self.assertNotEqual(before, recording_jobs._config_digest(self.config))
        changed = copy.deepcopy(self.config)
        changed["store_target"]["db_id"] = OTHER
        self.assertNotEqual(recording_jobs._config_digest(self.config), recording_jobs._config_digest(changed))

    def test_isolated_and_nested_projects_denied_before_client_or_worker(self):
        nested = self.root / "nested"
        nested.mkdir(); (nested / ".git").mkdir()
        event = {"hook_event_name": "UserPromptSubmit", "session_id": "fixture", "turn_id": "turn",
                 "prompt": "Investigate this private project", "cwd": str(nested)}
        with patch("hooks._reader_worker") as worker:
            self.assertEqual(hooks.handle_event(event, self.config), {})
            worker.assert_not_called()
        (self.root / ".mneme").mkdir()
        (self.root / ".mneme/profile.json").write_text('{"schema":"mneme.profile.v1","mode":"isolated"}')
        with patch("hook_recall.McpClient") as client:
            result = hook_recall.collect_reader(self.service, "Recall our shared decision", self.root,
                                               store_target=self.policy)
            self.assertEqual(result["outcome"], "unavailable")
            client.assert_not_called()

    def test_user_verified_save_and_maintenance_keep_target_and_receipt_alias(self):
        job = {"config_sha256": recording_jobs._config_digest(self.config), "db_id": DB,
               "db_alias": "user", "store_target": self.policy.canonical(), "proposal_kind": "lesson",
               "payload": {"summary": "A shared preference", "body": "Attributed choice",
                           "source": {"namespace": "fixture", "key": "1"}}}
        client = Mock()
        client.call_tool.return_value = self.catalog
        client.save_verified.return_value = {"db": "user", "db_id": DB, "id": NODE, "readback_status": "verified"}
        client.concern_checked.return_value = {"db": "user", "db_id": DB, "action": "observe", "outcome": {"status": "applied"}}
        with patch("mcp_client.McpClient", return_value=client):
            receipt = recording_jobs._native_write(self.config, job, 2)
            self.assertEqual(receipt["db"], "user")
            client.save_verified.assert_called_once_with("user", {**job["payload"], "kind": "note"}, expected_db_id=DB)
            recording_jobs._native_maintenance(self.config, job, {"payload": {"action": "observe"}}, 2)
            client.concern_checked.assert_called_once_with("user", {"action": "observe"}, expected_db_id=DB)
        job["db_id"] = OTHER
        client.reset_mock()
        with patch("mcp_client.McpClient", return_value=client), self.assertRaises(ValueError):
            recording_jobs._native_write(self.config, job, 2)
        client.save_verified.assert_not_called()
        job["db_id"] = DB
        job["db_alias"] = "project"
        with patch("mcp_client.McpClient") as factory, self.assertRaisesRegex(ValueError, "frozen_store_target_changed"):
            recording_jobs._native_write(self.config, job, 2)
        factory.assert_not_called()

    def test_workshop_prompt_scope_and_foreign_delivery_are_not_export_permission(self):
        from test_recording_contract import observation, statement, memory_marker
        observed = observation([statement("We chose a shared routine")])
        prompt, context = recording_contract.prepare(observed, recording_scope="workshop", expected_db_id=DB)
        self.assertIsInstance(prompt, str, context)
        self.assertEqual(context.recording_scope, "workshop")
        self.assertIn("personal_shared_continuity_and_agent_practice", prompt)
        self.assertIn("Never extract private project work", recording_contract.instructions_for(context))
        marker = memory_marker()
        prompt, reason = recording_contract.prepare(observation([statement("A choice"), marker]),
                                                    recording_scope="workshop", expected_db_id=DB)
        self.assertIsNone(prompt)
        self.assertEqual(reason, "foreign_memory_delivery")
        import test_routing_memory as routing_fixture
        source, _, _ = routing_fixture.RoutingRecordingTests().prepared(conditional=True)
        prompt, context = recording_contract.prepare(source, recording_scope="workshop",
                                                     expected_db_id=routing_fixture.DB)
        self.assertIsInstance(prompt, str, context)
        self.assertTrue(context.routing_enabled)

    def test_native_child_startup_has_write_allowance_not_catalog_timeout(self):
        job = {"config_sha256": recording_jobs._config_digest(self.config), "db_id": DB,
               "db_alias": "user", "store_target": self.policy.canonical(), "proposal_kind": "lesson",
               "payload": {"summary": "Fixture only", "body": "Not personal memory",
                           "source": {"namespace": "fixture", "key": "timeout"}}}
        for operation in ("save", "maintenance"):
            with self.subTest(operation=operation):
                client = Mock()
                phases = []
                client.connect.side_effect = lambda: phases.append(("startup", client.timeout))
                client.call_tool.side_effect = lambda *_: phases.append(("catalog", client.timeout)) or self.catalog
                client.save_verified.side_effect = lambda *_args, **_kwargs: phases.append(("write", client.timeout)) or {"verified": True}
                client.concern_checked.side_effect = client.save_verified.side_effect
                def construct(*_args, **kwargs):
                    client.timeout = kwargs["timeout"]
                    return client
                # Preparation, connect and catalog consume the same allowance;
                # no wall-clock sleeps or native/provider commands are needed.
                with patch("mcp_client.McpClient", side_effect=construct) as factory, patch(
                        "recording_jobs.time.monotonic", side_effect=[100, 101, 102, 103, 104]):
                    if operation == "save":
                        recording_jobs._native_write(self.config, job, 10)
                    else:
                        recording_jobs._native_maintenance(self.config, job, {"payload": {}}, 10)
                self.assertEqual(factory.call_args.kwargs["timeout"], 9)
                self.assertEqual(phases, [("startup", 9), ("catalog", 2), ("write", 7)])
                client.close.assert_called_once()
        with patch("mcp_client.McpClient") as factory, patch(
                "recording_jobs.time.monotonic", side_effect=[100, 111]), self.assertRaisesRegex(
                    TimeoutError, "native_deadline"):
            recording_jobs._native_write(self.config, job, 10)
        factory.assert_not_called()

    def test_remote_service_uses_exact_owner_path_not_local_workshop_project(self):
        remote = "/home/user/.local/share/mneme/memory.db"
        self.service.write_text(json.dumps({"mode": "connect", "url": "http://127.0.0.1:18766/",
            "database_name": "user", "database_path": remote}))
        config = {**self.config, "store_target": {"db_alias": "user", "database_path": remote, "db_id": DB}}
        policy = workshop_policy(config)
        self.assertEqual(policy.database_path, remote)
        with self.assertRaises(ValueError):
            workshop_policy({**config, "store_target": {**config["store_target"], "database_path": str(self.store)}})

    def test_target_rebind_does_not_replenish_paid_reader_accounting(self):
        event = {"session_id": "fixture", "turn_id": "turn", "prompt": "Recall our shared decision"}
        reader_worker.notice(self.config, event, True)
        ok, _ = reader_worker._state(self.config, "fixture", lambda data: (data.update(
            attempts=3, input_tokens=11, output_tokens=2, reservation={"paid": "identity"}), True))
        self.assertTrue(ok)
        path, _ = reader_worker._paths(self.config, "fixture")
        self.config["store_target"]["db_id"] = OTHER
        reader_worker.notice(self.config, {**event, "turn_id": "later"}, True)
        data = json.loads(path.read_text())
        self.assertEqual((data["attempts"], data["input_tokens"], data["output_tokens"]), (3, 11, 2))
        self.assertEqual(data["reservation"], {"paid": "identity"})

    def test_unsupported_session_cannot_gain_new_reader_or_recording_ledger(self):
        # Known project/workshop v1 promotion and untouched paid ledgers are
        # covered by test_hooks.LegacyHookStateUpgradeTests; unsupported state
        # must still fail before this route can create fresh accounting.
        state = self.config["state_dir"]
        state.mkdir()
        path = hooks._state_path(state, "legacy")
        event = {"session_id": "legacy", "hook_event_name": "SessionStart", "source": "resume",
                 "cwd": str(self.root)}
        for raw in ('{"schema":"mneme.codex-hooks.state.future","turns":{}}',
                    '{"schema":"mneme.codex-hooks.state.v1","turns":{"broken":{"at":1}}}'):
            with self.subTest(raw=raw):
                path.write_text(raw)
                before = path.read_bytes()
                with patch("hooks._reader_worker") as reader, patch("hooks._recording_jobs") as recorder:
                    self.assertIn("systemMessage", hooks.handle_event(event, self.config))
                    self.assertEqual(hooks.handle_event(event, self.config, reader_background=True), {})
                    reader.assert_not_called(); recorder.assert_not_called()
                self.assertEqual(path.read_bytes(), before)
                self.assertFalse((state / "reader").exists())
                self.assertFalse((state / "recording").exists())


if __name__ == "__main__":
    unittest.main()

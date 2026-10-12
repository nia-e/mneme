"""One session actor and honest owner-qualified delivery for registry hooks."""
import copy
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import hook_launcher
import hooks
import registry_config
import registry_runtime
from host_platform import executable_name
from turn_observer import validate_delivery_concerns, validate_delivery_packet, DELIVERY_SCHEMA


class RegistryLifecycleTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.project = self.base / "project"
        self.project.mkdir()
        (self.project / ".mneme").mkdir()
        self.workspace = self.base / "ordinary"
        self.workspace.mkdir()
        self.reader = self.base / executable_name("reader")
        self.reader.write_text("#!/bin/sh\nexit 0\n")
        self.reader.chmod(0o700)
        self.service = self.owner("project-owner", str(self.project / ".mneme/codex-memory.db"))
        self.registry = self.base / "stores.json"
        self.entries = [self.entry("personal", "1" * 26, core=True),
                        self.entry("company", "2" * 26), self.entry("reference", "3" * 26, record=False)]
        self.write_registry()
        self.path = self.base / "hooks.json"
        self.value = {"schema": hooks.CONFIG_SCHEMA_V12, "project_root": str(self.project),
                      "service_config": str(self.service), "store_registry": str(self.registry),
                      "state_dir": str(self.base / "state"), "memory_mode": "async",
                      "reader_model": "gpt-6.1-sol", "librarian_effort": "medium",
                      "reader_codex": str(self.reader), "reader_codex_sha256": hashlib.sha256(self.reader.read_bytes()).hexdigest(),
                      "recording_mode": "automatic", "tag_stewardship": True}
        self.write_hooks()

    def owner(self, name, path):
        service = self.base / (name + ".json")
        value = ({"binary": str(self.reader), "project_db": path, "port": 18763,
                  "state_dir": str(self.base / "owner-state"), "working_directory": str(self.project)}
                 if name == "project-owner" else
                 {"mode": "connect", "url": "http://127.0.0.1:18763/",
                  "database_name": "project", "database_path": path})
        service.write_text(json.dumps(value))
        return service

    def entry(self, name, identifier, *, core=False, record=True):
        database = "/remote/" + name + ".db"
        service = self.owner(name, database)
        return {"name": name, "service_config": str(service), "db_alias": "project",
                "database_path": database, "db_id": identifier, "purpose": "Purpose of " + name,
                "read": {"enabled": True, "core": core},
                "record": {"enabled": record, "kinds": ["lesson"], "evidence_kinds": ["user_statement"]},
                "maintenance": {"enabled": record}}

    def write_registry(self):
        self.registry.write_text(json.dumps({"schema": "mneme.stores.v1", "stores": self.entries}))

    def write_hooks(self):
        self.path.write_text(json.dumps(self.value))

    def config(self, **kwargs):
        self.write_hooks()
        return hooks._config(self.path, **kwargs)

    def device(self):
        self.value.pop("project_root")
        self.value.pop("service_config")
        self.value["memory_scope"] = "registry"
        self.write_hooks()
        return {"hook_event_name": "SessionStart", "source": "startup", "cwd": str(self.workspace), "session_id": "session"}

    def test_project_plus_three_named_owners_are_one_frozen_selection(self):
        with patch("service.ensure_ready") as start, patch("service.client_for_service") as contact:
            config = self.config()
            selected = registry_runtime.selection(config)
            start.assert_not_called()
            contact.assert_not_called()
        self.assertEqual([s.name for s in selected.stores], ["project", "personal", "company", "reference"])
        self.assertEqual([s.name for s in selected.core], ["project", "personal"])
        self.assertEqual([s.name for s in selected.recordable], ["project", "personal", "company"])
        self.assertEqual(config["selection_cwd"], self.project)
        self.assertFalse((self.base / "state").exists())

    def test_recording_off_revokes_all_writes_and_upkeep_not_reads(self):
        self.value["recording_mode"] = "off"
        selected = registry_runtime.selection(self.config())
        self.assertEqual(len(selected.readable), 4)
        self.assertEqual(selected.recordable, ())
        self.assertEqual(selected.maintainable, ())

    def test_empty_registry_is_project_only_but_missing_is_error(self):
        self.entries = []
        self.write_registry()
        self.assertEqual([s.name for s in registry_runtime.selection(self.config()).stores], ["project"])
        self.registry.unlink()
        with self.assertRaises(hooks.HookError):
            self.config()

    def test_legacy_fields_cannot_union_with_registry(self):
        for field, value in (("global_preferences", {}), ("store_target", {}), ("excluded_roots", [])):
            with self.subTest(field=field):
                candidate = {**self.value, field: value}
                with self.assertRaises(ValueError):
                    registry_runtime.validate_static(candidate)

    def test_nested_profile_denies_before_loading_owner(self):
        nested = self.project / "nested"
        (nested / ".mneme").mkdir(parents=True)
        (nested / ".mneme/profile.json").write_text('{"schema":"mneme.profile.v1","mode":"isolated"}')
        with patch("service.load_config") as contact, self.assertRaises(hooks.HookError):
            self.config(cwd=nested)
        contact.assert_not_called()

    def test_isolated_root_keeps_project_but_never_loads_denied_services(self):
        (self.project / ".mneme/profile.json").write_text('{"schema":"mneme.profile.v1","mode":"isolated"}')
        for entry in self.entries:
            Path(entry["service_config"]).unlink()
        selected = registry_runtime.selection(self.config())
        self.assertEqual([s.name for s in selected.stores], ["project"])
        self.assertEqual(len(selected.omissions), 3)

    def test_config_change_revokes_snapshot_without_changing_accounting(self):
        config = self.config()
        self.path.write_text(self.path.read_text() + "\n")
        with self.assertRaisesRegex(ValueError, "hook policy changed"):
            registry_runtime.selection(config)
        self.assertFalse((self.base / "state").exists())

    def test_project_focus_change_is_fenced_after_store_projection(self):
        purpose = self.project / ".mneme/hippocampus.md"
        purpose.write_text("Keep project implementation experience.")
        config = self.config()
        selected = registry_runtime.selection(config)
        projection = registry_runtime.config_for_store(config, selected.named("company"))
        self.assertEqual(projection["memory_scope"], "registry")
        self.assertEqual(projection["selected_store"], "company")
        self.assertEqual(projection["project_root"], self.project)
        purpose.write_text("Changed capture boundary.")
        with self.assertRaisesRegex(ValueError, "project purpose changed"):
            registry_runtime.selection(projection)

    def test_global_dispatch_has_no_primary_owner_and_one_session_actor(self):
        event = self.device()
        with patch("hooks._handle_async", return_value={"same": "pipeline"}) as lifecycle:
            self.assertEqual(hook_launcher.handle_event(event, self.path), {"same": "pipeline"})
        config = lifecycle.call_args.args[1]
        self.assertNotIn("service_config", config)
        self.assertEqual(config["memory_scope"], "registry")
        self.assertEqual(config["workspace_binding"]["workspace_root"], str(self.workspace))
        state = json.loads(hooks._state_path(self.base / "state", "session").read_bytes())
        self.assertIn("registry_binding", state)
        self.assertEqual(len(list((self.base / "state").glob("*.json"))), 1)

    def test_global_dispatch_declines_legacy_project_even_without_profile(self):
        event = self.device()
        for marker in (".mneme", ".codex"):
            boundary = self.workspace / marker
            boundary.mkdir()
            if marker == ".codex":
                (boundary / "config.toml").write_text('[mcp_servers.mneme_project]\ncommand="python"\nargs=["/old/hooks.py"]\n')
            with patch("hooks._config") as load:
                self.assertEqual(hook_launcher.handle_event(event, self.path), {})
                load.assert_not_called()
            if marker == ".mneme":
                boundary.rmdir()
        self.assertFalse((self.base / "state").exists())

    def test_existing_session_cannot_reset_accounting_on_registry_upgrade(self):
        config = self.config()
        state = self.base / "state"
        state.mkdir()
        path = hooks._state_path(state, "session")
        old = {"schema": hooks.STATE_SCHEMA, "turns": {"old": {"at": 0}}, "context_epoch": 4,
               "seen_ids": [], "seen_fingerprints": []}
        raw = json.dumps(old)
        path.write_text(raw)
        event = {"hook_event_name": "SessionStart", "source": "startup", "session_id": "session", "cwd": str(self.project)}
        with patch("hooks._handle_async") as lifecycle:
            self.assertEqual(hooks.handle_event(event, config), {"systemMessage": "Automatic memory temporarily disabled."})
            lifecycle.assert_not_called()
        self.assertEqual(path.read_text(), raw)

    def test_session_selection_change_refuses_and_keeps_previous_binding(self):
        config = self.config()
        event = {"hook_event_name": "SessionStart", "source": "startup", "session_id": "session", "cwd": str(self.project)}
        with patch("hooks._handle_async", return_value={}):
            hooks.handle_event(event, config)
        path = hooks._state_path(self.base / "state", "session")
        raw = path.read_bytes()
        self.entries[1]["purpose"] = "Revised company scope"
        self.write_registry()
        replacement = self.config()
        with patch("hooks._handle_async") as lifecycle:
            self.assertIn("systemMessage", hooks.handle_event(event, replacement))
            lifecycle.assert_not_called()
        self.assertEqual(path.read_bytes(), raw)

    def test_prepare_device_is_review_only_and_recording_is_off_by_default(self):
        prepared = registry_config.prepare(store_registry=self.registry, state_dir=self.base / "device-state",
            reader_codex=self.reader, hooks_program=Path(hook_launcher.__file__), hook_config_path=self.base / "device.json")
        self.assertEqual(prepared["hook_config"]["recording_mode"], "off")
        self.assertNotIn("service_config", prepared["hook_config"])
        self.assertNotIn("project_root", prepared["hook_config"])
        self.assertFalse((self.base / "device-state").exists())
        self.assertFalse((self.base / "device.json").exists())
        self.assertIn("PostToolUse", prepared["global_hooks"]["hooks"])
        self.assertEqual(prepared["installation"], "not performed")

    def test_global_binding_does_not_infer_recording_or_core_authority(self):
        binding = {"service_config": str(self.service), "database_path": "/remote/user.db", "db_id": "1" * 26}
        entry = registry_config.default_registry(global_preferences=binding)["stores"][0]
        self.assertFalse(entry["record"]["enabled"])
        self.assertFalse(entry["read"]["core"])
        reviewed = {"schema": "mneme.codex-hooks.config.v10", "memory_mode": "async",
                    "recording_mode": "automatic", "global_preferences": binding}
        entry = registry_config.default_registry(global_preferences=reviewed, global_core=True)["stores"][0]
        self.assertTrue(entry["record"]["enabled"])
        self.assertTrue(entry["read"]["core"])
        self.assertEqual(entry["record"]["evidence_kinds"], ["user_statement"])
        self.assertFalse(entry["record"]["allow_associations"])

    def test_quiescence_never_creates_state_and_refuses_active_worker(self):
        import os
        import host_files
        absent = self.base / "unused-state"
        self.assertTrue(registry_config.quiescence(absent)["quiescent_snapshot"])
        self.assertFalse(absent.exists())
        directory = self.base / "state/reader"
        directory.mkdir(parents=True)
        lease = directory / "fixture.worker.lock"
        lease.write_bytes(b"immutable lock contents")
        ledger = directory / "fixture.json"
        from reader_worker import _new
        value = {**_new(), "worker_pid": None, "attempts": 17, "unknown_usage": True}
        raw = json.dumps(value).encode()
        ledger.write_bytes(raw)
        fd = host_files.open_regular(lease, os.O_RDWR)
        try:
            host_files.lock_file(fd, blocking=False)
            report = registry_config.quiescence(directory.parent)
            self.assertFalse(report["quiescent_snapshot"])
        finally:
            os.close(fd)
        self.assertTrue(registry_config.quiescence(directory.parent)["quiescent_snapshot"])
        self.assertEqual(ledger.read_bytes(), raw)
        self.assertEqual(lease.read_bytes(), b"immutable lock contents")

    def test_quiescence_unrecognized_or_torn_state_is_not_empty(self):
        directory = self.base / "state/reader"
        directory.mkdir(parents=True)
        for text in ("{bad", '{"schema":"unknown"}', '[]', '{"schema":"mneme.codex-reader.future"}',
                     '{"schema":"mneme.codex-reader.state.v2"}'):

            with self.subTest(text=text):
                (directory / "fixture.json").write_text(text)
                self.assertFalse(registry_config.quiescence(directory.parent)["quiescent_snapshot"])

    def test_quiescence_malformed_identity_is_not_proof_of_pid_reuse(self):
        import os
        from reader_worker import _new
        directory = self.base / "state/reader"
        directory.mkdir(parents=True)
        value = {**_new(), "worker_pid": os.getpid(), "worker_identity": {}}
        (directory / "fixture.json").write_text(json.dumps(value))
        self.assertFalse(registry_config.quiescence(directory.parent)["quiescent_snapshot"])

    def test_core_dispatch_declines_project_and_never_starts_actor(self):
        event = self.device()
        with patch("core_hook.handle_hook_event", return_value={"core": "only"}, create=True) as core, \
                patch("hooks.handle_event") as lifecycle:
            self.assertEqual(hook_launcher.handle_event(event, self.path, core=True), {"core": "only"})
            lifecycle.assert_not_called()
            self.assertEqual(core.call_count, 1)
            (self.workspace / ".mneme").mkdir()
            self.assertEqual(hook_launcher.handle_event(event, self.path, core=True), {})
            self.assertEqual(core.call_count, 1)
        self.assertFalse((self.base / "state").exists())

    def test_empty_default_recipe_never_guesses_personal_or_misc_paths(self):
        self.assertEqual(registry_config.default_registry(), {"schema": "mneme.stores.v1", "stores": []})


class RegistryMigrationTests(unittest.TestCase):
    def test_receipt_detach_new_prefix_same_owner_and_rollback_preserve_ledgers(self):
        import install
        from install_test_support import InstallFixture
        fixture = InstallFixture()
        self.addCleanup(fixture.temp.cleanup)
        old_plan = fixture.async_plan(recording_mode="automatic")
        old = install.apply(old_plan)
        old_prefix = fixture.prefix
        configuration = {name: (fixture.project / ".codex" / name).read_bytes()
                         for name in install.CONFIG_NAMES}
        state = fixture.project / ".mneme/codex-hook-state"
        (state / "reader").mkdir(parents=True)
        ledger = state / "reader/fixture.json"
        from reader_worker import _new
        raw = json.dumps({**_new(), "worker_pid": None,
                          "attempts": 29, "unknown_usage": True, "inflight": {"retained": "unknown outcome"}}).encode()
        ledger.write_bytes(raw)
        self.assertTrue(registry_config.quiescence(state)["quiescent_snapshot"])
        registry = fixture.base / "stores.json"
        registry.write_text('{"schema":"mneme.stores.v1","stores":[]}')
        with patch("service.ensure_ready") as owner:
            install.uninstall(old["receipt"])
            fixture.prefix = fixture.base / "registry-runtime"
            plan = install.prepare(fixture.project, fixture.prefix, fixture.native("mnemed"),
                    old_prefix / install.binary_destination(old_plan, "mneme-mcp"), 18765,
                    program_root=fixture.sources, recall_mode="async", reader_model="gpt-6.1-sol",
                    reader_codex=fixture.native("codex"), recording_mode="automatic", store_registry=registry,
                    existing_service_config=old_prefix / "config/service.json")
            replacement = install.apply(plan)
            service, hook = install.runtime_configs(plan)
            self.assertEqual(service, install.runtime_configs(old_plan)[0])
            self.assertEqual(hook["state_dir"], str(state))
            self.assertEqual(hook["schema"], hooks.CONFIG_SCHEMA_V12)
            self.assertEqual(ledger.read_bytes(), raw)
            self.assertTrue(Path(old["receipt"]).exists())
            # Supported rollback removes only new owned outputs. The operator
            # restores the saved exact old pair, never downgrades queued ledgers.
            install.uninstall(replacement["receipt"])
            for name, contents in configuration.items():
                install.private_write(fixture.project / ".codex" / name, contents)
            install.uninstall(old["receipt"])  # Exact old receipt still verifies.
            owner.assert_not_called()
        self.assertEqual(ledger.read_bytes(), raw)
        self.assertTrue(old_prefix.exists())
        self.assertTrue(fixture.prefix.exists())


class NamedDeliveryTests(unittest.TestCase):
    def card(self, *, db="1" * 26, node="2" * 26, name="company"):
        return {"id": node, "summary": "Useful authored context", "status": "active", "source": "fixture",
                "kind": "semantic", "fingerprint": "a" * 64, "scope": "named", "db_id": db,
                "store_name": name, "store_purpose": "Company knowledge, not personal identity."}

    def test_same_node_id_different_owners_remain_distinct_visible_cards(self):
        cards = [self.card(), self.card(db="3" * 26, name="other")]
        packed = hooks._pack_async_delivery(cards, "session", "turn")
        self.assertEqual(len(packed["cards"]), 2)
        self.assertEqual(len(packed["displayed"]), 2)
        self.assertIn('"store_name":"company"', packed["context"])
        self.assertIn('"db_id":"' + "3" * 26 + '"', packed["context"])

    def test_owner_metadata_is_atomic_and_cannot_be_omitted_or_forged(self):
        card = self.card()
        for key in ("db_id", "store_name", "store_purpose"):
            damaged = {k: v for k, v in card.items() if k != key}
            self.assertFalse(hooks._valid_card(damaged))
        self.assertFalse(hooks._valid_card({**card, "scope": "project"}))
        packed = hooks._pack_async_delivery([card], "session", "turn")
        bound = copy.deepcopy(packed["displayed"])
        bound[0]["db_id"] = "3" * 26
        packet = {"schema": DELIVERY_SCHEMA, "session_id": "session", "turn_id": "turn",
                  "rendered_text": packed["context"], "rendered_sha256": hashlib.sha256(packed["context"].encode()).hexdigest(),
                  "displayed": bound, "concerns": []}
        validate_delivery_packet({**packet, "displayed": packed["displayed"]})
        with self.assertRaisesRegex(ValueError, "invalid_delivery_view"):
            validate_delivery_packet(packet)

    def test_owner_qualified_concern_survives_cross_owner_node_collision(self):
        cards = [self.card(), self.card(node="4" * 26), self.card(db="3" * 26, name="other")]
        case = {"db_id": "1" * 26, "shown_text": "These memories disagree.",
                "displayed_endpoint_ids": ["2" * 26, "4" * 26], "expected_row": None}
        packed = hooks._pack_async_delivery(cards, "session", "turn", concerns=[case])
        self.assertEqual(packed["concerns"], [case])
        self.assertIn("Advisory concern (" + "1" * 26 + ":", packed["context"])
        with self.assertRaises(ValueError):
            validate_delivery_concerns([{k: v for k, v in case.items() if k != "db_id"}], packed["displayed"])

    def test_partial_owner_coverage_is_visible_without_inventing_empty_stores(self):
        coverage = {"selected": 4, "attempted": 3, "unavailable": 1, "budget_omitted": 1, "complete": False}
        packed = hooks._pack_async_delivery([], "session", "turn", read_coverage=coverage)
        self.assertEqual(packed["read_coverage"], coverage)
        self.assertIn("3/4 owners attempted", packed["context"])
        self.assertIn("not evidence", packed["context"])
        with self.assertRaises(ValueError):
            hooks._pack_async_delivery([], "session", "turn", read_coverage={**coverage, "complete": True})


if __name__ == "__main__":
    unittest.main()

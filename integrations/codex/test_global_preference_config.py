"""Explicit preference-owner admission and copied runtime; no live stores."""
import copy
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import hooks
import install
from target_policy import (global_preferences_policy, policy_for, target_kwargs,
                           target_service_config, validate_global_preferences)

DB = "1" * 26
OTHER = "2" * 26


class GlobalPreferenceConfigTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.base = Path(temp.name).resolve()
        self.root = self.base / "project"
        self.root.mkdir()
        self.reader = self.base / "codex"
        self.reader.write_text("#!/bin/sh\nexit 0\n")
        self.reader.chmod(0o700)
        self.project_service = self.base / "project-service.json"
        self.project_service.write_text(json.dumps({"mode": "connect", "url": "http://127.0.0.1:18765/",
            "database_name": "project", "database_path": "/owner/project.db"}))
        self.global_service = self.base / "global-service.json"
        self.global_service.write_text(json.dumps({"mode": "connect", "url": "http://127.0.0.1:18766/",
            "database_name": "user", "database_path": "/owner/personal.db"}))
        self.target = {"service_config": str(self.global_service), "database_path": "/owner/personal.db",
                       "db_id": DB}
        self.value = {"schema": hooks.CONFIG_SCHEMA_V10, "project_root": str(self.root),
            "state_dir": str(self.base / "hook-state"), "service_config": str(self.project_service),
            "memory_mode": "async", "reader_model": "gpt-6.1-sol", "librarian_effort": "medium",
            "recording_mode": "automatic", "reader_codex": str(self.reader),
            "reader_codex_sha256": hashlib.sha256(self.reader.read_bytes()).hexdigest(),
            "global_preferences": self.target}
        self.hook = self.base / "hook.json"

    def load(self, value=None):
        self.hook.write_text(json.dumps(self.value if value is None else value))
        return hooks._config(self.hook)

    def test_v10_is_explicit_and_project_default_never_selects_user(self):
        config = self.load()
        policy = global_preferences_policy(config)
        self.assertEqual((policy.scope, policy.db_alias, policy.db_id), ("global_preference", "user", DB))
        self.assertEqual(target_kwargs(config), {})
        self.assertEqual(target_service_config(config), self.project_service)
        self.assertEqual(target_kwargs(config, destination="global_preference"), {"store_target": policy})
        self.assertEqual(target_service_config(config, "global_preference"), self.global_service)
        with self.assertRaisesRegex(ValueError, "project store boundary"):
            policy_for(self.global_service, self.root)
        old = {k: v for k, v in self.value.items() if k != "global_preferences"}
        old["schema"] = hooks.CONFIG_SCHEMA_V8
        self.assertIsNone(global_preferences_policy(self.load(old)))
        with self.assertRaisesRegex(ValueError, "not enabled"):
            target_kwargs(old, destination="global_preference")
        self.assertFalse((self.base / "hook-state").exists())

    def test_old_schemas_cannot_gain_lane_and_v10_cannot_omit_target(self):
        for schema in (hooks.CONFIG_SCHEMA_V8, hooks.CONFIG_SCHEMA_V9):
            with self.subTest(schema=schema), self.assertRaises(hooks.HookError):
                self.load({**self.value, "schema": schema})
        with self.assertRaises(hooks.HookError):
            self.load({k: v for k, v in self.value.items() if k != "global_preferences"})
        with self.assertRaises(hooks.HookError):
            self.load({**self.value, "memory_scope": "workshop"})

    def test_bad_binding_and_wrong_service_are_rejected_without_network(self):
        for change in ({"service_config": "relative"}, {"database_path": "/owner/../private.db"},
                       {"db_id": "invalid"}, {"extra": True}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                validate_global_preferences({**self.target, **change})
        for change in ({"database_name": "project"}, {"database_path": "/owner/other.db"}):
            service = json.loads(self.global_service.read_text())
            self.global_service.write_text(json.dumps({**service, **change}))
            with self.subTest(change=change), self.assertRaises(hooks.HookError):
                self.load()
            self.global_service.write_text(json.dumps(service))

    def test_exact_catalog_alias_path_id_and_singleton(self):
        policy = global_preferences_policy(self.load())
        catalog = [{"db": "user", "name": "user", "state": "open",
                    "configured_path": "/owner/personal.db", "db_id": DB}]
        self.assertEqual(policy.catalog_identity(catalog), DB)
        for change in ({"db": "project"}, {"configured_path": "/owner/other.db"}, {"db_id": OTHER}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                policy.catalog_identity([{**catalog[0], **change}])
        with self.assertRaises(ValueError):
            policy.catalog_identity(catalog * 2)

    def test_isolated_and_selected_library_block_ambient_owner(self):
        (self.root / ".mneme").mkdir()
        profile = self.root / ".mneme/profile.json"
        profile.write_text('{"schema":"mneme.profile.v1","mode":"isolated"}')
        with self.assertRaisesRegex(hooks.HookError, "isolated"):
            self.load()
        library = self.base / "library.json"
        library.write_text('{"schema":"mneme.library.config.v1"}')
        profile.write_text(json.dumps({"schema": "mneme.profile.v1", "mode": "default",
                                       "library_config": str(library)}))
        with self.assertRaisesRegex(hooks.HookError, "library"):
            self.load()
        library.write_text(json.dumps({"schema": "mneme.library.config.v1", "core": {
            "global_service": str(self.global_service), "global_database": "/owner/personal.db"}}))
        self.assertIsNotNone(global_preferences_policy(self.load()))

    def test_nested_profile_is_denied_before_worker_and_owner(self):
        config = self.load()
        nested = self.root / "nested"
        (nested / ".mneme").mkdir(parents=True)
        (nested / ".mneme/profile.json").write_text('{"schema":"mneme.profile.v1","mode":"default"}')
        event = {"hook_event_name": "UserPromptSubmit", "session_id": "fixture", "turn_id": "turn",
                 "prompt": "Investigate the selected project", "cwd": str(nested)}
        with patch("hooks._reader_worker") as worker:
            self.assertEqual(hooks.handle_event(event, config), {})
            worker.assert_not_called()

    def install_plan(self):
        return install.prepare(self.root, self.base / "runtime", self.reader, self.reader, 18765,
            recall_mode="async", reader_model="gpt-6.1-sol", reader_codex=self.reader,
            recording_mode="automatic", global_preferences=self.target)

    def test_installer_pins_opt_in_and_refuses_owner_drift_before_apply(self):
        plan = self.install_plan()
        self.assertEqual(plan["config_revision"], install.CONFIG_REVISION)
        self.assertEqual(plan["global_preferences"], self.target)
        self.assertEqual(plan["global_preferences_service_sha256"],
                         hashlib.sha256(self.global_service.read_bytes()).hexdigest())
        install.validate_plan(plan)
        old = copy.deepcopy(plan)
        old["config_revision"] = 18
        old["plan_sha256"] = install.plan_hash(old)
        with self.assertRaisesRegex(ValueError, "new async"):
            install.validate_plan(old)
        self.global_service.write_text(self.global_service.read_text() + "\n")
        with self.assertRaisesRegex(ValueError, "service changed"):
            install.apply(plan)
        self.assertFalse((self.base / "runtime").exists())
        self.assertFalse((self.root / ".codex").exists())
        # Recovery/uninstall can validate the immutable receipt even if the owner
        # endpoint is unavailable now; no live endpoint is a rollback prerequisite.
        self.global_service.unlink()
        install.validate_plan(plan)

    def test_copied_runtime_parses_v10_without_touching_either_store(self):
        plan = self.install_plan()
        receipt = install.apply(plan)
        prefix = Path(plan["prefix"])
        config = prefix / "config/hooks.json"
        raw = json.loads(config.read_text())
        self.assertEqual(raw["schema"], hooks.CONFIG_SCHEMA_V10)
        self.assertEqual(raw["global_preferences"], self.target)
        code = ("import hooks,target_policy; from pathlib import Path; "
                f"c=hooks._config(Path({str(config)!r})); "
                "p=target_policy.global_preferences_policy(c); "
                "assert p.scope=='global_preference' and p.db_alias=='user'; "
                "assert target_policy.target_kwargs(c)=={}; print('copied-v10-config: ok')")
        result = subprocess.run([sys.executable, "-B", "-c", code], cwd=prefix / "lib",
                                capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("copied-v10-config: ok", result.stdout)
        self.assertFalse((self.root / ".mneme/codex-memory.db").exists())
        self.assertFalse((self.base / "hook-state").exists())
        install.uninstall(Path(receipt["receipt"]))


if __name__ == "__main__":
    unittest.main()

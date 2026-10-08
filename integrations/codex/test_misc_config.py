"""Static misc preparation and immutable owner routing; no live owners."""
import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import hook_recall
import misc_config
from misc_binding import choose_workspace
from target_policy import (misc_policy, policy_for, target_kwargs, alias_for,
                           target_service_config, TargetPolicy)

DB = "1" * 26
OTHER = "2" * 26


class MiscConfigTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.root = self.base / "workspace"
        self.root.mkdir()
        self.reader = self.base / "reader"
        self.reader.write_text("#!/bin/sh\nexit 0\n")
        self.reader.chmod(0o700)
        self.service = self.base / "service.json"
        self.service.write_text(json.dumps({"mode": "connect", "url": "http://127.0.0.1:18765/",
            "database_name": "project", "database_path": "/pi/shared/misc.db"}))
        self.arguments = dict(state_dir=str(self.base / "state"), service_config=str(self.service),
            database_path="/pi/shared/misc.db", db_id=DB, reader_codex=str(self.reader),
            hooks_program=str(Path(misc_config.__file__).with_name("hooks.py")),
            hook_config_path=str(self.base / "device-hooks.json"))
        self.prepared = misc_config.prepare(**self.arguments)
        self.static = self.prepared["hook_config"]
        self.config = {**self.static, "project_root": self.root, "workspace_binding": choose_workspace(self.root)}

    def test_prepare_is_static_reviewable_and_does_not_write_or_contact(self):
        self.assertNotIn("project_root", self.static)
        self.assertNotIn("workspace_binding", self.static)
        self.assertEqual(self.static["memory_scope"], "misc")
        self.assertEqual(self.prepared["installation"], "not performed")
        self.assertEqual(self.prepared["service"], "not contacted")
        self.assertFalse((self.base / "state").exists())
        self.assertFalse((self.base / "device-hooks.json").exists())
        events = self.prepared["global_hooks"]["hooks"]
        self.assertEqual(set(events), {"SessionStart", "UserPromptSubmit", "Stop", "PostToolUse", "Interrupt", "SessionEnd"})
        self.assertTrue(events["Stop"][1]["hooks"][0]["async"])
        self.assertEqual(len(events["SessionEnd"]), 1)

    def test_static_shape_rejects_old_schemas_root_and_missing_authority(self):
        for mutation in ({"schema": "mneme.codex-hooks.config.v8"}, {"project_root": str(self.root)},
                         {"workspace_binding": choose_workspace(self.root)}, {"recording_mode": "unknown"},
                         {"excluded_roots": "not-array"}):
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                misc_config.validate_config({**self.static, **mutation})
        for mutation in ({"db_alias": "user"}, {"db_id": None}, {"database_path": "/pi/../shared/misc.db"}):
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                misc_config.validate_config({**self.static, "store_target": {**self.static["store_target"], **mutation}})
        exclusions = ["/excluded/" + str(index) + "x" * 180 for index in range(48)]
        with self.assertRaisesRegex(ValueError, "byte limit"):
            misc_config.validate_config({**self.static, "excluded_roots": exclusions})

    def test_frozen_policy_pins_shared_owner_and_catalog_identity(self):
        policy = misc_policy(self.config)
        self.assertEqual((policy.scope, policy.db_alias, policy.db_id), ("misc", "project", DB))
        self.assertEqual(target_kwargs(self.config), {"store_target": policy})
        self.assertEqual(alias_for(self.config), "project")
        self.assertEqual(target_service_config(self.config), self.service)
        self.assertEqual(policy_for(self.service, self.root, policy)[0], policy)
        catalog = [{"db": "project", "name": "project", "state": "open",
                    "configured_path": "/pi/shared/misc.db", "db_id": DB}]
        self.assertEqual(policy.catalog_identity(catalog), DB)
        for mutation in ({"db_id": OTHER}, {"configured_path": "/pi/shared/other.db"}, {"db": "user"}):
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                policy.catalog_identity([{**catalog[0], **mutation}])
        with self.assertRaises(ValueError):
            policy.catalog_identity(catalog * 2)
        serialized = policy.canonical()
        serialized["excluded_roots"] = tuple(serialized["excluded_roots"])
        self.assertEqual(TargetPolicy(**serialized), policy)

    def test_policy_accepts_alias_then_refuses_lexical_privacy_drift(self):
        alias_parent = self.base / "alias-parent"
        alias_parent.mkdir()
        alias = alias_parent / "workspace"
        alias.symlink_to(self.root, target_is_directory=True)
        config = {**self.config, "workspace_binding": choose_workspace(alias)}
        policy = misc_policy(config)
        policy.validate_workspace(str(alias))
        (alias_parent / ".mneme").mkdir()
        with self.assertRaisesRegex(ValueError, "configured or excluded"):
            policy.validate_workspace(str(alias))

    def test_configured_or_excluded_workspace_is_refused_before_contact(self):
        policy = misc_policy(self.config)
        (self.root / ".mneme").mkdir()
        with patch("hook_recall.McpClient") as client:
            result = hook_recall.collect_reader(self.service, "Recall an earlier decision", self.root,
                                               store_target=policy)
            self.assertEqual(result["outcome"], "unavailable")
            client.assert_not_called()
        with patch("target_policy.load_config") as load, self.assertRaises(ValueError):
            policy_for(self.service, self.root, policy)
        load.assert_not_called()
        (self.root / ".mneme").rmdir()
        with self.assertRaises(ValueError):
            misc_policy({**self.config, "excluded_roots": [str(self.base)]})

    def test_unavailable_owner_never_falls_back_to_user_or_local_store(self):
        policy = misc_policy(self.config)
        with patch("hook_recall.McpClient", side_effect=OSError("owner unavailable")) as client:
            result = hook_recall.collect_reader(self.service, "Recall prior work", self.root, store_target=policy)
        self.assertEqual(result["outcome"], "unavailable")
        self.assertEqual(client.call_count, 1)
        self.assertFalse((self.root / ".mneme").exists())

    def test_service_wrong_alias_or_path_refused_offline(self):
        value = json.loads(self.service.read_text())
        for mutation in ({"database_name": "user"}, {"database_path": "/pi/other.db"}):
            self.service.write_text(json.dumps({**value, **mutation}))
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                misc_config.prepare(**self.arguments)
            with self.assertRaises(ValueError):
                misc_policy(self.config)

    def test_cli_emits_only_reviewable_json(self):
        args = [sys.executable, misc_config.__file__]
        for name, value in self.arguments.items():
            args.extend(["--" + name.replace("_", "-"), value])
        result = subprocess.run(args, text=True, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["hook_config"], self.static)
        self.assertFalse((self.base / "device-hooks.json").exists())

    def test_recording_off_retains_reader_lifecycle_without_recorder_background(self):
        prepared = misc_config.prepare(**self.arguments, recording_mode="off")
        self.assertEqual(misc_config.validate_config(prepared["hook_config"])["recording_mode"], "off")
        events = prepared["global_hooks"]["hooks"]
        self.assertEqual(len(events["UserPromptSubmit"]), 2)
        self.assertEqual(len(events["Stop"]), 1)
        self.assertEqual(len(events["SessionEnd"]), 1)


if __name__ == "__main__":
    unittest.main()

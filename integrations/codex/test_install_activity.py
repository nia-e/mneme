"""Disposable explicit diagnostic setup and copied-helper evidence, never owners."""
import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

import install
import registry_config
from host_files import atomic_write, ensure_private_directory
from install_test_support import InstallFixture


@unittest.skipUnless(os.name == "posix", "activity v1 setup is Unix only")
class ActivityInstallTests(unittest.TestCase):
    def setUp(self):
        self.fixture = InstallFixture()
        self.addCleanup(self.fixture.temp.cleanup)
        self.config = self.fixture.base / "configuration" / "activity.json"
        self.root = self.fixture.base / "diagnostics"

    def plan(self, **options):
        return self.fixture.prepare(activity_root=self.root, activity_config=self.config, **options)

    def write_manifest(self, value=None, raw=None):
        ensure_private_directory(self.config.parent)
        if value is None and raw is None:
            value = self.plan()["activity"]["manifest"]
        atomic_write(self.config, raw if raw is not None else install.json_bytes(value))

    def test_prepare_is_read_only_and_activity_is_explicit(self):
        before = sorted(str(p) for p in self.fixture.base.rglob("*"))
        plan = self.plan(activity_device="fixture-device")
        install.validate_plan(plan)
        self.assertEqual(sorted(str(p) for p in self.fixture.base.rglob("*")), before)
        self.assertEqual(plan["activity"]["manifest"], {
            "schema": registry_config.ACTIVITY_SCHEMA, "root": str(self.root),
            "workspaces": [{"path": str(self.fixture.project), "source": "codex"}],
            "device": "fixture-device"})
        self.assertIsNone(plan["activity"]["expected_sha256"])
        self.assertIn("explicit shared activity", plan["authority"])
        self.assertNotIn("activity", self.fixture.prepare())
        for options in ({"activity_config": self.config}, {"activity_device": "device"}):
            with self.subTest(options=options), self.assertRaisesRegex(ValueError, "explicit --activity-root"):
                self.fixture.prepare(**options)

    def test_default_manifest_matches_fixed_platform_path_not_hook_env_override(self):
        with patch.dict(os.environ, {"XDG_CONFIG_HOME": str(self.fixture.base / "xdg"),
                                    "MNEME_ACTIVITY_CONFIG": str(self.fixture.base / "ignored.json")}):
            plan = self.fixture.prepare(activity_root=self.root)
        self.assertEqual(plan["activity"]["config_path"], str(self.fixture.base / "xdg/mneme/activity.json"))
        self.assertFalse(self.root.exists())

    def test_apply_creates_private_binding_separate_server_and_preserves_uninstall(self):
        plan = self.plan(program_root=Path(install.__file__).parent)
        with patch("service.ensure_ready", side_effect=AssertionError("owner contact")), \
                patch("store_registry.native_select", side_effect=AssertionError("discovery")):
            result = install.apply(plan)
        self.assertEqual(self.root.stat().st_mode & 0o777, 0o700)
        self.assertEqual(self.config.stat().st_mode & 0o777, 0o600)
        self.assertEqual(json.loads(self.config.read_bytes()), plan["activity"]["manifest"])
        import tomllib
        servers = tomllib.loads((self.fixture.project / ".codex/config.toml").read_text())["mcp_servers"]
        self.assertEqual(set(servers), {"mneme_project", "mneme_session_activity"})
        activity = servers["mneme_session_activity"]
        self.assertEqual(activity["command"], str(self.fixture.prefix / "bin/mneme-mcp"))
        self.assertEqual(activity["args"], ["--activity-config", str(self.config), "--cwd", str(self.fixture.project)])
        self.assertEqual(activity["enabled_tools"], ["session_activity"])
        self.assertNotIn("session_activity", servers["mneme_project"]["enabled_tools"])
        self.assertNotIn("activity", install.runtime_configs(plan)[1])
        before = self.config.read_bytes()
        (self.root / "unrelated-evidence").write_bytes(b"preserve")
        removed = install.uninstall(result["receipt"])
        self.assertEqual(self.config.read_bytes(), before)
        self.assertEqual((self.root / "unrelated-evidence").read_bytes(), b"preserve")
        self.assertIn("shared activity bindings and diagnostic data", removed["preserved"])
        self.assertFalse((self.fixture.project / ".mneme").exists())

    def test_merge_keeps_other_workspaces_and_existing_device(self):
        other = self.fixture.base / "other-project"
        other.mkdir()
        value = self.plan()["activity"]["manifest"]
        value.update(workspaces=[{"path": str(other), "source": "codex"}], device="original-device")
        self.write_manifest(value)
        before = self.config.read_bytes()
        plan = self.plan()
        self.assertEqual(plan["activity"]["manifest"]["workspaces"], [
            {"path": str(other), "source": "codex"}, {"path": str(self.fixture.project), "source": "codex"}])
        self.assertEqual(plan["activity"]["manifest"]["device"], "original-device")
        self.assertEqual(plan["activity"]["expected_sha256"], install.digest(before))
        self.assertEqual(self.config.read_bytes(), before)
        install.apply(plan)
        self.assertEqual(json.loads(self.config.read_bytes()), plan["activity"]["manifest"])

    def test_merge_preserves_missing_unrelated_workspace_without_rebinding_it(self):
        retained = str(self.fixture.base / "deleted-other-workspace")
        manifest = self.plan()["activity"]["manifest"]
        manifest["workspaces"] = [{"path": retained, "source": "codex"}]
        self.write_manifest(manifest)
        plan = self.plan()
        self.assertEqual(plan["activity"]["manifest"]["workspaces"], [
            {"path": retained, "source": "codex"},
            {"path": str(self.fixture.project), "source": "codex"}])
        install.apply(plan)
        self.assertEqual(json.loads(self.config.read_bytes())["workspaces"],
                         plan["activity"]["manifest"]["workspaces"])
        self.assertFalse(Path(retained).exists())

    def test_foreign_malformed_oversized_and_duplicate_bindings_refuse_without_writes(self):
        good = self.plan()["activity"]["manifest"]
        invalid = []
        foreign = copy.deepcopy(good)
        foreign["root"] = str(self.fixture.base / "different-root")
        invalid.append(install.json_bytes(foreign))
        duplicate = copy.deepcopy(good)
        duplicate["workspaces"] *= 2
        invalid.append(install.json_bytes(duplicate))
        unknown = copy.deepcopy(good)
        unknown["owner_url"] = "do-not-contact"
        invalid.append(install.json_bytes(unknown))
        unsupported = copy.deepcopy(good)
        unsupported["schema"] = "mneme.session-activity.config.v999"
        invalid.extend([install.json_bytes(unsupported), b"{}", b"{" + b" " * (64 * 1024),
                        b'{"schema":"a","schema":"b"}', b"[" * 20 + b"]" * 20])
        for raw in invalid:
            with self.subTest(raw=raw[:40]):
                self.write_manifest(raw=raw)
                with self.assertRaises(ValueError):
                    self.plan()
                self.assertEqual(self.config.read_bytes(), raw)
                self.assertFalse(self.root.exists())
                self.assertFalse(self.fixture.prefix.exists())

    def test_symlink_or_public_root_refused_without_repair(self):
        self.root.mkdir(mode=0o755)
        with self.assertRaises(PermissionError):
            self.plan()
        self.assertEqual(self.root.stat().st_mode & 0o777, 0o755)
        self.root.rmdir()
        real = self.fixture.base / "real-root"
        real.mkdir(mode=0o700)
        self.root.symlink_to(real, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.plan()
        self.assertEqual(list(real.iterdir()), [])

    def test_manifest_drift_refused_before_install_or_root_creation(self):
        plan = self.plan()
        changed = copy.deepcopy(plan["activity"]["manifest"])
        changed["device"] = "new-device"
        self.write_manifest(changed)
        before = self.config.read_bytes()
        with self.assertRaisesRegex(ValueError, "changed since prepare"):
            install.apply(plan)
        self.assertEqual(self.config.read_bytes(), before)
        self.assertFalse(self.fixture.prefix.exists())
        self.assertFalse(self.root.exists())
        self.assertFalse((self.fixture.project / ".codex").exists())

    def test_partial_project_failure_keeps_shared_binding_and_recovery_never_erases_it(self):
        plan = self.plan()
        original_write = install.private_write
        def fail_project(path, data):
            if path == self.fixture.project / ".codex/config.toml":
                raise OSError("synthetic project publication failure")
            return original_write(path, data)
        with patch("install.private_write", side_effect=fail_project), self.assertRaises(OSError):
            install.apply(plan)
        before = self.config.read_bytes()
        self.assertTrue(self.root.is_dir())
        result = install.recover(self.fixture.prefix / "pending.json")
        self.assertEqual(result["status"], "aborted")
        self.assertEqual(self.config.read_bytes(), before)
        registry_config.publish_activity(plan["activity"])
        self.assertEqual(self.config.read_bytes(), before)

    def test_copied_helper_emits_using_only_explicit_prepared_manifest(self):
        plan = self.plan(program_root=Path(install.__file__).parent)
        install.apply(plan)
        script = '''
import json, os, pathlib, sys
root = pathlib.Path(sys.argv[1]); sys.path.insert(0, str(root))
import session_activity, registry_config
assert pathlib.Path(session_activity.__file__) == root / 'session_activity.py'
os.environ['MNEME_ACTIVITY_CONFIG'] = sys.argv[2]
assert session_activity.emit({'session_id':'packaged-fixture-session','cwd':sys.argv[3]},
                             'lifecycle', 'startup', key='fixture-start', at=1792770000)
print('copied-activity-ok')
'''
        result = subprocess.run([sys.executable, "-I", "-S", "-B", "-c", script,
                str(self.fixture.prefix / "lib"), str(self.config), str(self.fixture.project)],
                cwd=self.fixture.base, capture_output=True, text=True, timeout=10, check=True)
        self.assertEqual(result.stdout.strip(), "copied-activity-ok")
        record = next(self.root.glob("codex-*.json"))
        self.assertEqual(json.loads(record.read_bytes())["session_id"], "packaged-fixture-session")
        row = next(row for row in plan["files"] if row["destination"] == "lib/session_activity.py")
        self.assertEqual(install.digest((self.fixture.prefix / row["destination"]).read_bytes()), row["sha256"])

    def test_native_preparation_keeps_manifest_outside_strict_v13(self):
        from types import SimpleNamespace
        memory = self.fixture.base / "memory.toml"
        memory.write_text('schema = "mneme.config.v1"\n')
        args = dict(memory_config=memory, native_cli=self.fixture.native("mnemed"),
            cwd=self.fixture.project, state_dir=self.fixture.base / "device-state",
            reader_codex=self.fixture.native("codex"),
            hooks_program=Path(install.__file__).with_name("hook_launcher.py"),
            hook_config_path=self.fixture.base / "device-hooks.json")
        with patch("store_registry.native_select", return_value=SimpleNamespace(snapshot={})) as selected:
            plain = registry_config.prepare_native(**args)
            prepared = registry_config.prepare_native(**args, activity_root=self.root,
                activity_config=self.config, activity_mcp=self.fixture.native("mneme-mcp"))
            self.assertEqual(selected.call_count, 2)
            with self.assertRaisesRegex(ValueError, "requires an explicit --activity-mcp"):
                registry_config.prepare_native(**args, activity_root=self.root)
            self.assertEqual(selected.call_count, 2)
        self.assertEqual(prepared["hook_config"], plain["hook_config"])
        self.assertEqual(prepared["global_hooks"], plain["global_hooks"])
        self.assertEqual(prepared["hook_config"]["schema"], "mneme.codex-hooks.config.v13")
        self.assertEqual(prepared["activity"]["manifest"], self.plan()["activity"]["manifest"])
        self.assertFalse(self.config.exists())
        self.assertFalse(self.root.exists())
        self.assertFalse((self.fixture.base / "device-state").exists())

    def test_publication_lock_contention_and_changed_shared_binding_are_not_overwritten(self):
        from host_files import lock_file, open_regular
        plan = self.plan()
        ensure_private_directory(self.config.parent)
        lock = open_regular(self.config.with_name(".activity.json.setup.lock"), os.O_RDWR | os.O_CREAT)
        try:
            lock_file(lock, blocking=False)
            with self.assertRaises(BlockingIOError):
                registry_config.publish_activity(plan["activity"])
            self.assertFalse(self.config.exists())
            self.assertFalse(self.root.exists())
        finally:
            os.close(lock)
        registry_config.publish_activity(plan["activity"])
        changed = json.loads(self.config.read_bytes())
        changed["device"] = "changed"
        self.write_manifest(changed)
        before = self.config.read_bytes()
        with self.assertRaisesRegex(ValueError, "changed since prepare"):
            registry_config.publish_activity(plan["activity"])
        self.assertEqual(self.config.read_bytes(), before)

    def test_existing_config_parent_mode_is_preserved_without_weakening_root_privacy(self):
        self.config.parent.mkdir(mode=0o755)
        plan = self.plan()
        install.apply(plan)
        self.assertEqual(self.config.parent.stat().st_mode & 0o777, 0o755)
        self.assertEqual(self.config.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.root.stat().st_mode & 0o777, 0o700)

    def test_exact_retry_reestablishes_durability_after_rename_sync_failure(self):
        import host_files
        plan = self.plan()
        real_sync = host_files.sync_directory
        failed = False
        def fail_after_rename(path):
            nonlocal failed
            if Path(path) == self.config.parent and self.config.exists() and not failed:
                failed = True
                raise OSError("synthetic post-rename sync failure")
            return real_sync(path)
        with patch("host_files.sync_directory", side_effect=fail_after_rename), self.assertRaises(OSError):
            registry_config.publish_activity(plan["activity"])
        self.assertTrue(failed)
        self.assertEqual(json.loads(self.config.read_bytes()), plan["activity"]["manifest"])
        with patch("host_files.sync_file", wraps=host_files.sync_file) as sync_file, \
                patch("host_files.sync_directory", wraps=real_sync) as sync_directory:
            registry_config.publish_activity(plan["activity"])
        sync_file.assert_called_once_with(self.config)
        sync_directory.assert_any_call(self.config.parent)

    def test_device_and_path_bounds_match_producer(self):
        for device in ("", "a" * 129, "🦊" * 33, "bad\x80label", "bad\nlabel"):
            with self.subTest(device=device), self.assertRaises(ValueError):
                self.plan(activity_device=device)
        invalid = self.fixture.base / "bad\nroot"
        with self.assertRaises(ValueError):
            self.fixture.prepare(activity_root=invalid, activity_config=self.config)
        self.assertFalse(invalid.exists())

    def test_revision34_and35_inventories_stay_exact_without_activity(self):
        service = self.fixture.base / "old-service.json"
        service.write_bytes(install.json_bytes({"binary": str(self.fixture.native("mneme-mcp")),
            "project_db": str(self.fixture.project / ".mneme/codex-memory.db"),
            "working_directory": str(self.fixture.project), "port": 18765,
            "state_dir": str(self.fixture.base / "old-owner-state")}))
        for revision, options in ((34, {}), (35, {"existing_service_config": service})):
            with self.subTest(revision=revision):
                old = self.fixture.prepare(**options)
                old["config_revision"] = revision
                old["files"] = old["files"][:len(install.V34_DESTINATIONS)]
                old["plan_sha256"] = install.plan_hash(old)
                install.validate_plan(old)
                self.assertNotIn("lib/session_activity.py", [row["destination"] for row in old["files"]])
                old["activity"] = self.plan()["activity"]
                old["plan_sha256"] = install.plan_hash(old)
                with self.assertRaisesRegex(ValueError, "old install plan cannot contain activity"):
                    install.validate_plan(old)


if __name__ == "__main__":
    unittest.main()

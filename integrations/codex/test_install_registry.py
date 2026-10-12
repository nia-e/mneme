"""Reviewed named-store installation with disposable files, never live owners."""
import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

import install
from install_test_support import InstallFixture


class RegistryInstallTests(unittest.TestCase):
    def setUp(self):
        self.fixture = InstallFixture()
        self.addCleanup(self.fixture.temp.cleanup)
        self.registry = self.fixture.base / "stores.json"
        self.registry.write_bytes(install.json_bytes({"schema": "mneme.stores.v1", "stores": []}))

    def plan(self, **options):
        return self.fixture.async_plan(store_registry=self.registry, **options)

    def test_plan_pins_registry_bytes_and_runtime_has_exact_registry_route(self):
        with patch("service.load_config") as load, patch("service.ensure_ready") as ready:
            plan = self.plan()
            install.validate_plan(plan)
            load.assert_not_called()
            ready.assert_not_called()
        self.assertEqual(plan["store_registry"], str(self.registry))
        self.assertEqual(plan["store_registry_sha256"], install.digest(self.registry.read_bytes()))
        service, hook = install.runtime_configs(plan)
        self.assertEqual(hook["schema"], "mneme.codex-hooks.config.v12")
        self.assertEqual(hook["store_registry"], str(self.registry))
        self.assertEqual(hook["project_root"], str(self.fixture.project))
        self.assertEqual(service["project_db"], str(self.fixture.project / ".mneme/codex-memory.db"))
        self.assertNotIn("global_preferences", hook)
        self.assertFalse(self.fixture.prefix.exists())

    def test_named_startup_has_separate_read_only_core_with_exact_argv_and_bounds(self):
        from host_platform import hook_command
        for recording in ("off", "automatic"):
            with self.subTest(recording=recording):
                plan = self.plan(recording_mode=recording)
                legacy = self.fixture.async_plan(recording_mode=recording)
                events = install.planned_hook_handlers(plan)
                legacy_events = install.planned_hook_handlers(legacy)
                self.assertEqual(len(legacy_events["SessionStart"]), 1)
                self.assertEqual(events["SessionStart"][:-1], legacy_events["SessionStart"])
                argv = [plan["python"], str(self.fixture.prefix / "lib/core_hook.py"),
                        "--hook-config", str(self.fixture.prefix / "config/hooks.json")]
                core = events["SessionStart"][-1]
                self.assertEqual(core, {"hooks": [{"type": "command",
                    "command": hook_command(argv, windows=install.plan_platform(plan) == "windows"),
                    "timeout": 15, "additionalContextLimit": 0}]})
                self.assertNotIn("async", core["hooks"][0])
                self.assertNotIn("--reader-background", core["hooks"][0]["command"])
                self.assertEqual({key: value for key, value in events.items() if key != "SessionStart"},
                                 {key: value for key, value in legacy_events.items() if key != "SessionStart"})
                self.assertEqual(sum(len(group["hooks"]) for groups in events.values() for group in groups),
                                 9 if recording == "automatic" else 8)
                outputs = install.config_outputs(plan, {"config.toml": None, "hooks.json": None})
                self.assertEqual(json.loads(outputs["hooks.json"])["hooks"], events)

    def test_windows_core_startup_encodes_complete_argv_and_legacy_has_no_core(self):
        from host_platform import hook_command
        plan = self.plan()
        plan["host_platform"] = "windows"
        plan["python"] = str(self.fixture.base / "a %!& ' Python.exe")
        argv = [plan["python"], str(self.fixture.prefix / "lib/core_hook.py"),
                "--hook-config", str(self.fixture.prefix / "config/hooks.json")]
        core = install.planned_hook_handlers(plan)["SessionStart"][-1]["hooks"][0]
        self.assertEqual(core["command"], hook_command(argv, windows=True))
        self.assertTrue(core["command"].startswith("powershell.exe -NoProfile"))
        for revision in (22, 23):
            plan["config_revision"] = revision
            self.assertEqual(len(install.planned_hook_handlers(plan)["SessionStart"]), 1)
        plan["config_revision"] = 25
        plan.pop("store_registry")
        self.assertEqual(len(install.planned_hook_handlers(plan)["SessionStart"]), 1)

    def test_install_copies_all_registry_helpers_and_uninstall_needs_no_registry(self):
        plan = self.plan()
        result = install.apply(plan)
        for name in ("store_registry.py", "registry_config.py", "registry_runtime.py", "registry_reader.py", "registry_recording.py", "core_hook.py"):
            self.assertIn(name, install.PROGRAMS)
            row = next(row for row in plan["files"] if row["destination"] == "lib/" + name)
            self.assertEqual(install.digest((self.fixture.prefix / row["destination"]).read_bytes()), row["sha256"])
        actual = json.loads((self.fixture.prefix / "config/hooks.json").read_bytes())
        self.assertEqual(actual, install.runtime_configs(plan)[1])
        self.registry.unlink()
        self.assertEqual(install.uninstall(result["receipt"])["status"], "configuration removed")

    def test_copied_registry_runtime_loads_config_without_source_tree_or_owner_start(self):
        import hook_trust
        plan = self.plan(program_root=Path(install.__file__).parent)
        result = install.apply(plan)
        reviewed = hook_trust.reviewed_definitions(self.fixture.project,
                                                   self.fixture.prefix / "config/service.json")
        self.assertEqual(len(reviewed.signatures), 8)
        script = """
import json, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import hooks, store_registry, registry_runtime, registry_reader, registry_recording, registry_config, core_hook
import service

def forbidden(*args, **kwargs):
    raise AssertionError("installed config load must not start or contact owners")
service.ensure_ready = forbidden
service.subprocess.Popen = forbidden
config = hooks._config(Path(sys.argv[2]))
selected = registry_runtime.selection(config)
assert config["schema"] == "mneme.codex-hooks.config.v12"
assert [store.name for store in selected.stores] == ["project"]
for module in (hooks, store_registry, registry_runtime, registry_reader, registry_recording, registry_config, core_hook):
    assert Path(module.__file__).parent == Path(sys.argv[1])
print(json.dumps({"schema": config["schema"], "selected": len(selected.stores)}))
"""
        process = subprocess.run([sys.executable, "-I", "-c", script,
                                  str(self.fixture.prefix / "lib"),
                                  str(self.fixture.prefix / "config/hooks.json")],
                                 capture_output=True, text=True, timeout=10, check=True,
                                 cwd=self.fixture.base)
        self.assertEqual(json.loads(process.stdout), {"schema": "mneme.codex-hooks.config.v12", "selected": 1})
        self.assertFalse((self.fixture.project / ".mneme/codex-memory.db").exists())
        self.registry.unlink()
        self.assertEqual(install.uninstall(result["receipt"])["status"], "configuration removed")

    def test_even_semantically_equivalent_registry_edit_refuses_before_writes(self):
        plan = self.plan()
        self.registry.write_bytes(self.registry.read_bytes() + b"\n")
        with self.assertRaisesRegex(ValueError, "registry changed"):
            install.apply(plan)
        self.assertFalse(self.fixture.prefix.exists())
        self.assertFalse((self.fixture.project / ".codex").exists())

    def test_registry_is_explicit_and_cannot_union_legacy_preferences(self):
        legacy = self.fixture.async_plan()
        self.assertNotIn("store_registry", legacy)
        self.assertEqual(install.runtime_configs(legacy)[1]["schema"], "mneme.codex-hooks.config.v8")
        with self.assertRaisesRegex(ValueError, "cannot be combined"):
            self.plan(global_preferences={})
        with self.assertRaisesRegex(ValueError, "requires.*async"):
            self.fixture.prepare(store_registry=self.registry)
        legacy["store_registry_sha256"] = "0" * 64
        legacy["plan_sha256"] = install.plan_hash(legacy)
        with self.assertRaisesRegex(ValueError, "no target"):
            install.validate_plan(legacy)

    def test_invalid_registry_never_reaches_installation(self):
        for raw in (b'{"schema":"mneme.stores.v1","stores":[],"stores":[]}',
                    b'{"schema":"mneme.stores.v1","stores":[],"unknown":true}',
                    b" " * (64 * 1024 + 1)):
            with self.subTest(raw=raw[:80]):
                self.registry.write_bytes(raw)
                with self.assertRaises(ValueError):
                    self.plan()
                self.assertFalse(self.fixture.prefix.exists())
        with self.assertRaisesRegex(ValueError, "canonical absolute"):
            self.fixture.async_plan(store_registry=Path("stores.json"))

    def test_old_receipt_cannot_smuggle_in_registry_authority(self):
        plan = self.plan()
        plan["config_revision"] = 22
        plan["files"] = plan["files"][:len(install.V22_DESTINATIONS)]
        plan["plan_sha256"] = install.plan_hash(plan)
        with self.assertRaisesRegex(ValueError, "new async install plan"):
            install.validate_plan(plan)

    def test_apply_requires_a_well_formed_registry_pin(self):
        plan = self.plan()
        for path, digest in (("relative", "0" * 64), (str(self.registry), "bad"),
                             (str(self.registry), None)):
            broken = copy.deepcopy(plan)
            broken.update(store_registry=path, store_registry_sha256=digest)
            broken["plan_sha256"] = install.plan_hash(broken)
            with self.assertRaisesRegex(ValueError, "invalid store registry pin"):
                install.apply(broken)
            self.assertFalse(self.fixture.prefix.exists())

    def test_installer_help_exposes_reviewed_registry_route(self):
        result = subprocess.run([sys.executable, install.__file__, "prepare", "--help"],
                                capture_output=True, text=True, check=True, timeout=10)
        self.assertIn("--store-registry", result.stdout)


class RegistryMigrationCliTests(unittest.TestCase):
    """The operator guide's commands, with copied helpers and no owner launch.

    Native binaries are deliberately inert admission fixtures. This covers
    configuration lifecycle on the host, not native database/process behavior.
    """

    def test_documented_detach_prepare_apply_rollback_retains_owner_and_accounting(self):
        fixture = InstallFixture()
        self.addCleanup(fixture.temp.cleanup)
        old_prefix = fixture.prefix
        old_plan = fixture.async_plan(program_root=Path(install.__file__).parent,
                                     recording_mode="automatic", tag_stewardship=False)
        old = install.apply(old_plan)
        dot_codex = fixture.project / ".codex"
        saved_configs = {name: (dot_codex / name).read_bytes() for name in install.CONFIG_NAMES}
        old_hook = json.loads((old_prefix / "config/hooks.json").read_bytes())
        owner = json.loads((old_prefix / "config/service.json").read_bytes())
        state = Path(old_hook["state_dir"])
        (state / "reader").mkdir(parents=True)
        from reader_worker import _new
        ledger = state / "reader/retained.json"
        ledger.write_bytes(install.json_bytes({**_new(), "worker_pid": None, "attempts": 29,
            "unknown_usage": True, "inflight": {"retained": "unknown outcome"}}))
        # Opaque existing database/body and owner-state fixtures must never be
        # opened, rewritten or deleted during a config-only migration.
        database = Path(owner["project_db"])
        database.write_bytes(b"existing database fixture: never opened")
        body = database.with_suffix(".body")
        body.write_bytes(b"paired body fixture")
        owner_state = Path(owner["state_dir"]) / "mneme-codex-service.json"
        owner_state.write_bytes(b"existing owner state: never probed")
        retained = {path: path.read_bytes() for path in (
            ledger, database, body, owner_state, Path(old["receipt"]),
            old_prefix / "config/service.json", Path(owner["binary"]))}

        def cli(script, *args, ok=True):
            result = subprocess.run([sys.executable, str(script), *map(str, args)],
                capture_output=True, text=True, timeout=15, cwd=fixture.base,
                env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})
            self.assertEqual(result.returncode, 0 if ok else 1, result.stderr)
            return json.loads(result.stdout if ok else result.stderr)

        installer = Path(install.__file__)
        config_tool = installer.with_name("registry_config.py")
        registry = fixture.base / "stores.json"
        registry.write_bytes(install.json_bytes(cli(config_tool, "defaults")))
        self.assertEqual(cli(installer, "uninstall", "--receipt", old["receipt"])["status"],
                         "configuration removed")
        quiet = cli(config_tool, "quiescence", "--state-dir", state)
        self.assertTrue(quiet["quiescent_snapshot"])
        self.assertEqual(quiet["ledgers_checked"], 1)
        new_prefix = fixture.base / "registry-runtime"
        output = fixture.base / "registry-plan.json"
        options = ["--project-root", fixture.project, "--output", output,
            "--mnemed", fixture.native("mnemed"), "--mcp", owner["binary"],
            "--port", owner["port"], "--existing-service-config", old_prefix / "config/service.json",
            "--store-registry", registry, "--recall-mode", "async",
            "--reader-model", old_hook["reader_model"], "--reader-codex", old_hook["reader_codex"],
            "--librarian-effort", old_hook["librarian_effort"],
            "--recording-mode", old_hook["recording_mode"], "--no-tag-stewardship"]
        refusal = cli(installer, "prepare", "--prefix", old_prefix, *options, ok=False)
        self.assertIn("prefix must be absent", refusal["error"])
        self.assertFalse(output.exists())
        cli(installer, "prepare", "--prefix", new_prefix, *options)
        self.assertFalse(new_prefix.exists())
        self.assertTrue(all(not (dot_codex / name).exists() for name in install.CONFIG_NAMES))
        plan = json.loads(output.read_bytes())
        install.validate_plan(plan)
        planned_owner, planned_hook = install.runtime_configs(plan)
        self.assertEqual(planned_owner, owner)
        self.assertEqual(planned_hook["state_dir"], str(state))
        self.assertFalse(planned_hook["tag_stewardship"])
        replacement = cli(installer, "apply", "--plan", output)
        copied_tool = new_prefix / "lib/registry_config.py"
        self.assertTrue(cli(copied_tool, "quiescence", "--state-dir", state)["quiescent_snapshot"])
        self.assertEqual(json.loads((new_prefix / "config/service.json").read_bytes()), owner)

        # Changed outputs cannot be forcibly overwritten during rollback.
        hooks_path = dot_codex / "hooks.json"
        current_hooks = hooks_path.read_bytes()
        hooks_path.write_bytes(current_hooks + b"\n")
        refusal = cli(installer, "uninstall", "--receipt", replacement["receipt"], ok=False)
        self.assertIn("changed", refusal["error"])
        self.assertEqual(hooks_path.read_bytes(), current_hooks + b"\n")
        hooks_path.write_bytes(current_hooks)
        cli(installer, "uninstall", "--receipt", replacement["receipt"])
        self.assertTrue(cli(copied_tool, "quiescence", "--state-dir", state)["quiescent_snapshot"])
        for name, raw in saved_configs.items():
            install.private_write(dot_codex / name, raw)
        self.assertEqual({name: (dot_codex / name).read_bytes() for name in install.CONFIG_NAMES}, saved_configs)
        for path, raw in retained.items():
            self.assertEqual(path.read_bytes(), raw, str(path))
        self.assertTrue(Path(replacement["receipt"]).exists())
        self.assertEqual(list((new_prefix / "run").iterdir()), [])
        # Restoring exactly the old config pair restores receipt verification,
        # not worker/session continuation or permission to downgrade paid state.
        cli(installer, "uninstall", "--receipt", old["receipt"])


if __name__ == "__main__":
    unittest.main()

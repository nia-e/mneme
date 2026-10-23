"""Offline native-config adapter preparation and fenced disposable installation."""
import copy
from contextlib import redirect_stdout
import io
import json
import os
import shlex
import subprocess
import sys
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import hook_launcher
import install
from install_test_support import InstallFixture
import registry_config


class NativeInstallTests(unittest.TestCase):
    def setUp(self):
        self.fixture = InstallFixture()
        self.addCleanup(self.fixture.temp.cleanup)
        self.memory = self.fixture.base / "memory.toml"
        self.memory.write_text('schema = "mneme.config.v1"\n')
        self.snapshot = {"schema": "mneme.config-selection.v1", "digest": "a" * 64,
            "sources": [{"role": "user", "path": str(self.memory), "sha256": install.digest(self.memory.read_bytes()),
                         "bytes": len(self.memory.read_bytes())}],
            "config": {"background": {"enabled": False, "model": "gpt-6.1-sol", "effort": "medium"}},
            "stores": [], "active": [], "read_default": None, "cwd": str(self.fixture.project), "project": {"configured": False}}
        self.select = patch("store_registry.native_select", return_value=SimpleNamespace(snapshot=self.snapshot))
        self.select.start()
        self.addCleanup(self.select.stop)

    def plan(self, **options):
        return self.fixture.prepare(memory_config=self.memory, reader_codex=self.fixture.native("codex"), **options)

    def discovery_plan(self):
        self.snapshot["discovery"] = {
            "binding": {"path": str(self.fixture.base / "discovery-receipt.json"), "sha256": "b" * 64},
            "boundary_sha256": "c" * 64,
        }
        return self.plan()

    def test_discovery_prepare_pins_digest_without_receipt_and_revalidates_frozen(self):
        with patch("store_registry.native_select", return_value=SimpleNamespace(snapshot=self.snapshot)) as select:
            plan = self.discovery_plan()
        select.assert_called_once_with(str(self.memory), str(self.fixture.native("mnemed")), str(self.fixture.project))
        pin = plan["memory_config"]
        self.assertEqual(pin, {"path": str(self.memory), "digest": self.snapshot["digest"],
                               "sources": self.snapshot["sources"]})
        install.validate_plan(plan)
        with patch("store_registry.native_select", return_value=SimpleNamespace(snapshot=self.snapshot)) as select:
            install.revalidate_memory_config(plan)
        select.assert_called_once_with(str(self.memory), str(self.fixture.native("mnemed")),
                                       str(self.fixture.project), expected_selection_digest=pin["digest"])

    def test_discovery_apply_preserves_pin_and_rollback_needs_no_receipt_replay(self):
        plan = self.discovery_plan()
        with patch("store_registry.native_select", return_value=SimpleNamespace(snapshot=self.snapshot)) as select:
            result = install.apply(plan)
        select.assert_called_once_with(str(self.memory), str(self.fixture.native("mnemed")),
                                       str(self.fixture.project), expected_selection_digest=plan["memory_config"]["digest"])
        receipt = json.loads(Path(result["receipt"]).read_bytes())
        self.assertEqual(receipt["plan"]["memory_config"], plan["memory_config"])
        self.memory.unlink()
        with patch("store_registry.native_select", side_effect=AssertionError("rollback reacquired discovery")) as select:
            self.assertEqual(install.uninstall(result["receipt"])["status"], "configuration removed")
            select.assert_not_called()

    def test_discovery_refusal_stops_reviewed_apply_before_writes_without_fallback(self):
        plan = json.loads(json.dumps(self.discovery_plan()))
        for cause in ("missing", "stale", "malformed"):
            refusal = ValueError(f"native configuration refused: {cause} discovery receipt")
            with self.subTest(cause=cause), \
                 patch("store_registry.native_select", side_effect=[refusal, SimpleNamespace(snapshot=self.snapshot)]) as select, \
                 patch("install.ensure_private_directory") as mkdir, \
                 patch("install.private_write") as private_write, \
                 patch("install.atomic_write") as atomic_write:
                with self.assertRaises(ValueError) as caught:
                    install.apply(plan)
                self.assertIs(caught.exception, refusal)
                select.assert_called_once_with(str(self.memory), str(self.fixture.native("mnemed")),
                                               str(self.fixture.project), expected_selection_digest=plan["memory_config"]["digest"])
                mkdir.assert_not_called()
                private_write.assert_not_called()
                atomic_write.assert_not_called()
                self.assertFalse(self.fixture.prefix.exists())
                self.assertFalse((self.fixture.project / ".codex").exists())

    def test_discovery_revalidation_rejects_changed_digest_or_sources(self):
        plan = self.discovery_plan()
        for change in ("digest", "sources"):
            value = copy.deepcopy(self.snapshot)
            if change == "digest":
                value["digest"] = "d" * 64
            else:
                value["sources"][0]["sha256"] = "d" * 64
            with self.subTest(change=change), \
                 patch("store_registry.native_select", return_value=SimpleNamespace(snapshot=value)) as select:
                with self.assertRaisesRegex(ValueError, "memory config changed"):
                    install.revalidate_memory_config(plan)
                select.assert_called_once_with(str(self.memory), str(self.fixture.native("mnemed")),
                                               str(self.fixture.project), expected_selection_digest=plan["memory_config"]["digest"])

    def test_local_only_reviewed_plan_also_revalidates_offline(self):
        plan = self.plan()
        self.assertEqual(set(plan["memory_config"]), {"path", "digest", "sources"})
        with patch("store_registry.native_select", return_value=SimpleNamespace(snapshot=self.snapshot)) as select:
            install.revalidate_memory_config(plan)
        select.assert_called_once_with(str(self.memory), str(self.fixture.native("mnemed")),
                                       str(self.fixture.project), expected_selection_digest=plan["memory_config"]["digest"])

    def test_local_only_reviewed_plan_cannot_acquire_new_discovery_after_refusal(self):
        plan = self.plan()
        refusal = ValueError("native config changed to enable discovery; reprepare")
        with patch("store_registry.native_select", side_effect=[refusal, SimpleNamespace(snapshot=self.snapshot)]) as select, \
             patch("install.ensure_private_directory") as mkdir:
            with self.assertRaises(ValueError) as caught:
                install.apply(plan)
            self.assertIs(caught.exception, refusal)
            select.assert_called_once_with(str(self.memory), str(self.fixture.native("mnemed")),
                                           str(self.fixture.project), expected_selection_digest=plan["memory_config"]["digest"])
            mkdir.assert_not_called()
        self.assertFalse(self.fixture.prefix.exists())

    def test_discovery_plan_rejects_unused_replay_markers_and_receipt_fields(self):
        plan = self.discovery_plan()
        for marker in (None, False, True, 0, 1, "true", {}, []):
            candidate = copy.deepcopy(plan)
            candidate["memory_config"]["discovery"] = marker
            candidate["plan_sha256"] = install.plan_hash(candidate)
            with self.subTest(marker=marker), patch("store_registry.native_select") as select:
                with self.assertRaisesRegex(ValueError, "invalid native memory config pin"):
                    install.validate_plan(candidate)
                select.assert_not_called()
        candidate = copy.deepcopy(plan)
        candidate["memory_config"]["discovery_receipt"] = self.snapshot["discovery"]
        candidate["plan_sha256"] = install.plan_hash(candidate)
        with self.assertRaisesRegex(ValueError, "invalid native memory config pin"):
            install.validate_plan(candidate)

    def test_one_policy_native_mcp_no_project_actor_no_start(self):
        with patch("service.ensure_ready") as start:
            plan = self.plan()
            install.validate_plan(plan)
            service, hook = install.runtime_configs(plan)
            self.assertEqual(hook["schema"], "mneme.codex-hooks.config.v13")
            self.assertEqual(set(hook), {"schema", "memory_config", "native_cli", "state_dir", "reader_codex", "reader_codex_sha256"})
            self.assertEqual(install.planned_hook_handlers(plan), {})
            outputs = install.config_outputs(plan, {"config.toml": None, "hooks.json": None})
            import tomllib
            server = tomllib.loads(outputs["config.toml"].decode())["mcp_servers"][install.SERVER]
            self.assertEqual(server["command"], str(self.fixture.prefix / install.binary_destination(plan, "mneme-mcp")))
            self.assertEqual(server["args"], ["--memory-config", str(self.memory), "--cwd", str(self.fixture.project)])
            self.assertEqual(json.loads(outputs["hooks.json"])["hooks"], {})
            self.assertNotIn("permissions", hook)
            start.assert_not_called()
        self.assertFalse(self.fixture.prefix.exists())

    def test_config_drift_fails_before_any_publication_and_rollback_needs_no_config(self):
        plan = self.plan()
        self.snapshot["digest"] = "b" * 64
        with self.assertRaisesRegex(ValueError, "memory config changed"):
            install.apply(plan)
        self.assertFalse(self.fixture.prefix.exists())
        plan = self.plan()
        result = install.apply(plan)
        self.memory.unlink()
        self.select.stop()
        self.assertEqual(install.uninstall(result["receipt"])["status"], "configuration removed")

    def test_native_source_byte_witness_is_preserved_and_bounded(self):
        plan = self.plan()
        self.assertEqual(plan["memory_config"]["sources"], self.snapshot["sources"])
        self.assertEqual(plan["memory_config"]["sources"][0]["bytes"], len(self.memory.read_bytes()))
        for invalid in (None, True, -1, 1.5, "25", install.MAX_NATIVE_SOURCE_BYTES + 1):
            candidate = copy.deepcopy(plan)
            candidate["memory_config"]["sources"][0]["bytes"] = invalid
            candidate["plan_sha256"] = install.plan_hash(candidate)
            with self.subTest(bytes=invalid), self.assertRaisesRegex(ValueError, "invalid native memory config pin"):
                install.validate_plan(candidate)
        for change in ("missing", "unknown"):
            candidate = copy.deepcopy(plan)
            source = candidate["memory_config"]["sources"][0]
            if change == "missing":
                del source["bytes"]
            else:
                source["unknown"] = True
            candidate["plan_sha256"] = install.plan_hash(candidate)
            with self.subTest(change=change), self.assertRaisesRegex(ValueError, "invalid native memory config pin"):
                install.validate_plan(candidate)

    def test_source_inventory_uses_byte_envelope_not_eight_ancestor_quota(self):
        plan = self.plan()
        source = plan["memory_config"]["sources"][0]
        plan["memory_config"]["sources"] = [
            {**source, "path": str(self.fixture.base / f"ancestor-{index}.toml")} for index in range(9)]
        plan["plan_sha256"] = install.plan_hash(plan)
        install.validate_plan(plan)
        for kind in ("aggregate-input", "wire-envelope"):
            candidate = copy.deepcopy(plan)
            for row in candidate["memory_config"]["sources"]:
                if kind == "aggregate-input":
                    row["bytes"] = install.MAX_NATIVE_SOURCE_BYTES
                else:
                    row["path"] += "a" * (64 * 1024)
            candidate["plan_sha256"] = install.plan_hash(candidate)
            with self.subTest(kind=kind), self.assertRaisesRegex(ValueError, "exceeds byte bounds"):
                install.validate_plan(candidate)

    def test_symlink_path_is_preserved_and_target_only_drift_refuses_install(self):
        original = self.memory
        self.memory = self.fixture.base / "managed-memory.toml"
        self.memory.symlink_to(original)
        replacement = self.fixture.base / "generation-two.toml"
        replacement.write_bytes(original.read_bytes())

        def select(path, *_, **options):
            self.assertEqual(path, str(self.memory))
            value = copy.deepcopy(self.snapshot)
            # Native selection supplies the canonical source witness. Even if a
            # caller returned an unchanged digest, target-only drift is fenced.
            value["sources"][0]["path"] = str(Path(path).resolve(strict=True))
            return SimpleNamespace(snapshot=value)

        with patch("store_registry.native_select", side_effect=select):
            plan = self.plan()
            self.assertEqual(plan["memory_config"]["path"], str(self.memory))
            self.assertEqual(plan["memory_config"]["sources"][0]["path"], str(original))
            self.assertEqual(install.runtime_configs(plan)[1]["memory_config"], str(self.memory))
            outputs = install.config_outputs(plan, {"config.toml": None, "hooks.json": None})
            import tomllib
            args = tomllib.loads(outputs["config.toml"].decode())["mcp_servers"][install.SERVER]["args"]
            self.assertEqual(args[args.index("--memory-config") + 1], str(self.memory))
            self.memory.unlink()
            self.memory.symlink_to(replacement)
            with self.assertRaisesRegex(ValueError, "memory config changed"):
                install.apply(plan)
            self.assertFalse(self.fixture.prefix.exists())

    def test_native_rejection_of_config_target_stops_prepare_without_publication(self):
        self.memory.unlink()
        self.memory.symlink_to(self.fixture.base / "missing.toml")
        with patch("store_registry.native_select", side_effect=ValueError("native configuration refused")) as select:
            with self.assertRaisesRegex(ValueError, "native configuration refused"):
                self.plan()
        self.assertEqual(select.call_args.args[0], str(self.memory))
        self.assertFalse(self.fixture.prefix.exists())

    def test_native_source_drift_refuses_without_running_changed_selector(self):
        plan = self.plan()
        native = self.fixture.native("mnemed")
        native.write_text("changed executable")
        with patch("store_registry.native_select") as select:
            with self.assertRaisesRegex(ValueError, "installation source changed"):
                install.apply(plan)
            select.assert_not_called()
        self.assertFalse(self.fixture.prefix.exists())

    def test_cannot_add_old_independent_policy_or_reenrollment(self):
        for extra in ({"recording_mode": "automatic"}, {"reader_model": "gpt-6.1-sol"},
                      {"librarian_effort": "medium"}, {"tag_stewardship": False},
                      {"store_registry": self.memory}, {"global_preferences": {}},
                      {"library_helper": self.memory}):
            with self.subTest(extra=extra), self.assertRaisesRegex(ValueError, "independent hook policy"):
                self.plan(**extra)

    def test_retained_owner_binary_does_not_become_staged_configured_server(self):
        owner = self.fixture.base / "old-mneme-mcp"
        owner.write_text("retained owner binary fixture")
        owner.chmod(0o700)
        state = self.fixture.base / "owner-state"
        state.mkdir()
        service = {"binary": str(owner), "project_db": str(self.fixture.project / ".mneme/codex-memory.db"),
                   "working_directory": str(self.fixture.project), "port": 18765, "state_dir": str(state)}
        path = self.fixture.base / "retained-service.json"
        path.write_bytes(install.json_bytes(service))
        plan = self.plan(existing_service_config=path)
        install.validate_plan(plan)
        self.assertEqual(install.runtime_configs(plan)[0], service)
        self.assertEqual(plan["existing_owner_binary"]["source"], str(owner))
        owner.write_text("drifted owner")
        with self.assertRaisesRegex(ValueError, "existing service binary changed"):
            install.apply(plan)
        self.assertFalse(self.fixture.prefix.exists())

    def test_old_revision_cannot_smuggle_native_policy_or_new_helper_inventory(self):
        plan = self.plan()
        plan["config_revision"] = 24
        plan["files"] = plan["files"][:len(install.V24_DESTINATIONS)]
        plan["plan_sha256"] = install.plan_hash(plan)
        with self.assertRaisesRegex(ValueError, "invalid native memory config pin"):
            install.validate_plan(plan)
        legacy = self.fixture.async_plan()
        legacy["config_revision"] = 24
        legacy["files"] = legacy["files"][:len(install.V24_DESTINATIONS)]
        legacy["plan_sha256"] = install.plan_hash(legacy)
        install.validate_plan(legacy)

    def test_native_install_default_keeps_symbolic_reader_without_lookup(self):
        with patch('shutil.which', side_effect=AssertionError('reader resolved during installation')):
            plan = self.fixture.prepare(memory_config=self.memory)
            self.assertEqual(plan['reader_codex'], 'codex')
            _, hook = install.runtime_configs(plan)
            self.assertEqual(hook['reader_codex'], 'codex')
            self.assertNotIn('reader_codex_sha256', hook)
            result = install.apply(plan)
        self.assertEqual(json.loads((self.fixture.prefix / 'config/hooks.json').read_text()), hook)
        self.assertEqual(install.uninstall(result['receipt'])['status'], 'configuration removed')

    def test_native_prepare_cli_defaults_to_symbolic_reader(self):
        args = self.device_prepare_args()
        args.pop('reader_codex')
        argv = ['prepare-native']
        for name, value in args.items():
            argv.extend(['--' + name.replace('_', '-'), str(value)])
        output = io.StringIO()
        with redirect_stdout(output), patch('shutil.which', side_effect=AssertionError('reader resolved during preparation')):
            registry_config.main(argv)
        config = json.loads(output.getvalue())['hook_config']
        self.assertEqual(config['reader_codex'], 'codex')
        self.assertNotIn('reader_codex_sha256', config)
        self.assertFalse(Path(args['hook_config_path']).exists())

    def test_native_device_and_workspace_adapter_preserve_path_choices(self):
        args = self.device_prepare_args()
        args.pop('reader_codex')
        nested = self.fixture.project / 'nested'
        nested.mkdir()
        args['workspace_adapters'] = [
            {'root': str(self.fixture.project), 'state_dir': str(self.fixture.base / 'path-state'),
             'reader_codex': 'codex'},
            {'root': str(nested), 'state_dir': str(self.fixture.base / 'pinned-state'),
             'reader_codex': str(self.fixture.native('codex'))}]
        with patch('shutil.which', side_effect=AssertionError('reader resolved during preparation')):
            prepared = registry_config.prepare_native(**args)
        config = prepared['hook_config']
        self.assertEqual(config['reader_codex'], 'codex')
        self.assertNotIn('reader_codex_sha256', config)
        symbolic, pinned = config['workspace_adapters']
        self.assertEqual(symbolic['reader_codex'], 'codex')
        self.assertNotIn('reader_codex_sha256', symbolic)
        self.assertEqual(pinned['reader_codex'], str(self.fixture.native('codex')))
        self.assertEqual(pinned['reader_codex_sha256'], install.digest(self.fixture.native('codex').read_bytes()))
        self.assertNotIn('recording_mode', config)
        self.assertFalse(Path(args['state_dir']).exists())
        self.assertFalse(Path(args['hook_config_path']).exists())

    def test_device_plan_keeps_core_and_async_independent_and_adapter_is_machine_only(self):
        adapters = [{"root": str(self.fixture.project), "state_dir": str(self.fixture.project / ".mneme/retained"),
                     "reader_codex": str(self.fixture.native("codex"))}]
        destination = self.fixture.base / "device-hooks.json"
        logical = self.fixture.base / "managed-memory.toml"
        logical.symlink_to(self.memory)
        args = dict(memory_config=logical, native_cli=self.fixture.native("mnemed"), cwd=self.fixture.project,
                    state_dir=self.fixture.base / "device-state", reader_codex=self.fixture.native("codex"),
                    hooks_program=Path(hook_launcher.__file__), hook_config_path=destination,
                    workspace_adapters=adapters)
        prepared = registry_config.prepare_native(**args)
        config = prepared["hook_config"]
        self.assertEqual(config["memory_config"], str(logical))
        self.assertEqual(config["workspace_adapters"][0]["state_dir"], adapters[0]["state_dir"])
        self.assertNotIn("recording_mode", config)
        start = prepared["global_hooks"]["hooks"]["SessionStart"]
        self.assertIn("--core", start[0]["hooks"][0]["command"])
        self.assertFalse(start[0]["hooks"][0].get("async", False))
        self.assertTrue(prepared["global_hooks"]["hooks"]["Stop"][-1]["hooks"][0]["async"])
        self.assertFalse(destination.exists())
        self.assertFalse((self.fixture.base / "device-state").exists())
        adapters[0]["permissions"] = {"write": True}
        with self.assertRaisesRegex(ValueError, "select only"):
            registry_config.prepare_native(**args)

    def device_prepare_args(self):
        return dict(memory_config=self.memory, native_cli=self.fixture.native("mnemed"),
                    cwd=self.fixture.project, state_dir=self.fixture.base / "device-state",
                    reader_codex=self.fixture.native("codex"), hooks_program=Path(hook_launcher.__file__),
                    hook_config_path=self.fixture.base / "device-hooks.json")

    @unittest.skipUnless(os.name == "posix", "stable hook bootstrap is explicitly POSIX-only")
    def test_stable_native_recipe_is_independent_of_runtime_and_configuration_paths(self):
        args = self.device_prepare_args()
        root = self.fixture.base / "stable café's hooks"
        first = registry_config.prepare_native(**args, stable_root=root)
        release = self.fixture.base / "new release"
        release.mkdir()
        dispatcher = release / "hook_launcher.py"
        dispatcher.write_bytes(Path(hook_launcher.__file__).read_bytes())
        interpreter = self.fixture.native("python3-new")
        interpreter.write_text("#!/bin/sh\nexit 0\n")
        interpreter.chmod(0o700)
        second = registry_config.prepare_native(
            **{**args, "python": interpreter, "hooks_program": dispatcher,
               "hook_config_path": self.fixture.base / "other-config.json"},
            stable_root=root)
        self.assertEqual(first["global_hooks"], second["global_hooks"])
        self.assertNotEqual(first["stable_runtime"], second["stable_runtime"])
        self.assertEqual(second["stable_runtime"], {
            "stable_root": str(root), "python": str(interpreter),
            "hooks_program": str(dispatcher), "hook_config_path": str(self.fixture.base / "other-config.json")})
        for groups in first["global_hooks"]["hooks"].values():
            for group in groups:
                for handler in group["hooks"]:
                    argv = shlex.split(handler["command"])
                    self.assertEqual(argv[:2], ["/bin/sh", str(root / "launch")])
                    self.assertIn(argv[2:], ([], ["--core"], ["--reader-background"]))
        self.assertFalse(root.exists())
        self.assertFalse(args["hook_config_path"].exists())
        self.assertFalse(args["state_dir"].exists())
        # Opt-in changes only launch indirection, not selected memory authority.
        direct = registry_config.prepare_native(**args)
        self.assertEqual(direct["hook_config"], first["hook_config"])
        self.assertEqual(direct["selection"], first["selection"])
        self.assertNotIn("stable_runtime", direct)
        self.assertNotIn("trust_policy", direct)

    @unittest.skipUnless(os.name == "posix", "stable hook bootstrap is explicitly POSIX-only")
    def test_stable_native_definition_contract_does_not_follow_package_revisions(self):
        root = self.fixture.base / "stable"
        prepared = registry_config.prepare_native(**self.device_prepare_args(), stable_root=root)
        command = shlex.join(["/bin/sh", str(root / "launch")])
        def group(extra="", **fields):
            return {"hooks": [{"type": "command", "command": command + extra, **fields}]}
        # This is the once-trusted native ABI, not a snapshot to regenerate when
        # installer revisions or ordinary runtime behaviour change.
        self.assertEqual(prepared["global_hooks"], {"hooks": {
            "SessionStart": [
                group(" --core", timeout=15, additionalContextLimit=0),
                group(timeout=5, additionalContextLimit=1600)],
            "UserPromptSubmit": [
                group(timeout=5, additionalContextLimit=0),
                group(" --reader-background", timeout=5, **{"async": True})],
            "Stop": [group(timeout=5), group(" --reader-background", timeout=5, **{"async": True})],
            "PostToolUse": [group(timeout=5, additionalContextLimit=0)],
            "Interrupt": [group(timeout=3)],
            "SessionEnd": [group(timeout=3)],
        }})

    def test_invalid_stable_root_refuses_before_native_selection(self):
        args = self.device_prepare_args()
        linked = self.fixture.base / "linked-stable"
        linked.symlink_to(self.fixture.base, target_is_directory=True)
        for root in (Path("relative"), linked, self.fixture.base / "line\nbreak"):
            with self.subTest(root=root), patch("store_registry.native_select") as select:
                with self.assertRaises(ValueError):
                    registry_config.prepare_native(**args, stable_root=root)
                select.assert_not_called()

    def test_copied_native_adapter_uses_its_bundle_without_source_tree(self):
        logical = self.fixture.base / "managed-memory.toml"
        logical.symlink_to(self.memory)
        self.memory = logical
        native = self.fixture.native("mnemed")
        native.write_text("#!" + sys.executable + "\nimport json\nprint(json.dumps(" + repr(self.snapshot) + "))\n")
        plan = self.plan(program_root=Path(install.__file__).parent)
        result = install.apply(plan)
        script = """
import json, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import hooks, store_registry, registry_runtime, guidance, registry_config
config = hooks._config(Path(sys.argv[2]), cwd=sys.argv[3])
assert config["schema"] == "mneme.codex-hooks.config.v13"
assert config["recording_mode"] == "off"
assert str(config["memory_config"]) == sys.argv[4]
assert config["memory_config"].is_symlink()
assert registry_runtime.selection(config).stores == ()
for module in (hooks, store_registry, registry_runtime, guidance, registry_config):
    assert Path(module.__file__).parent == Path(sys.argv[1])
print(json.dumps({"schema": config["schema"], "selected": 0}))
"""
        process = subprocess.run([sys.executable, "-I", "-c", script,
            str(self.fixture.prefix / "lib"), str(self.fixture.prefix / "config/hooks.json"),
            str(self.fixture.project), str(self.memory)], capture_output=True, text=True, timeout=10, check=True,
            cwd=self.fixture.base)
        self.assertEqual(json.loads(process.stdout), {"schema": "mneme.codex-hooks.config.v13", "selected": 0})
        self.assertFalse((self.fixture.project / ".mneme/codex-hook-state").exists())
        self.assertEqual(install.uninstall(result["receipt"])["status"], "configuration removed")

    def test_native_prepare_help_and_dispatch_agree(self):
        for path, command, fields in ((install.__file__, "prepare", ("--memory-config",)),
                                     (registry_config.__file__, "prepare-native", ("--memory-config", "--native-cli", "--workspace-adapters", "--stable-root"))):
            process = subprocess.run([sys.executable, path, command, "--help"],
                capture_output=True, text=True, timeout=10, check=True)
            for field in fields:
                self.assertIn(field, process.stdout)

    def test_native_dispatch_does_not_decline_enrolled_workspace_in_python(self):
        event = {"hook_event_name": "SessionStart", "source": "startup", "session_id": "session",
                 "cwd": str(self.fixture.project)}
        with patch("hooks._bounded_json", return_value={"schema": "mneme.codex-hooks.config.v13"}), \
             patch("hooks._config", return_value={"native": "checked"}) as configure, \
             patch("hooks.handle_event", return_value={"handled": True}), \
             patch("hook_launcher.choose_workspace") as discover:
            self.assertEqual(hook_launcher.handle_event(event, self.memory), {"handled": True})
            configure.assert_called_once_with(self.memory, cwd=str(self.fixture.project))
            discover.assert_not_called()


if __name__ == "__main__":
    unittest.main()

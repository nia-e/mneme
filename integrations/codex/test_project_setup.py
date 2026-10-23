"""Disposable setup tests; never run inference, services or touch enrolled stores."""
import argparse
from contextlib import redirect_stdout
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import project_setup as setup

DB_ID = "01ARZ3NDEKTSV4RRFFQ69G5FAV"

class ProjectSetupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.root = self.base / "project"
        self.root.mkdir()
        self.bin = self.base / "bin"
        self.bin.mkdir()
        for name in ("mnemed", "mneme-mcp", "codex"):
            (self.bin / name).write_text("#!/bin/sh\nexit 1\n")
            (self.bin / name).chmod(0o700)
        self.args = argparse.Namespace(root=self.root, mnemed=self.bin / "mnemed",
            mcp_binary=self.bin / "mneme-mcp", codex_binary=self.bin / "codex",
            library_helper=Path(__file__).resolve().parent.parent / "library/library.py",
            library_config=None, port=19871, no_recording=False, no_trust_hooks=False, json=True)
        self.addCleanup(patch.stopall)
        patch.dict(os.environ, {"HOME":str(self.base / "home"), "XDG_DATA_HOME":str(self.base / "data"),
                                "XDG_CONFIG_HOME":str(self.base / "config"), "PATH":str(self.bin)}).start()
        self.native = patch.object(setup, "native", side_effect=self.native_call).start()
        self.trust = patch.object(setup, "trust_project_hooks", return_value={"status":"trusted", "trusted":8}).start()
        self.ready = patch.object(setup, "ensure_ready", return_value={"state":"ready"}).start()
        patch.object(setup, "catalog", return_value=DB_ID).start()

    def native_call(self, mnemed, root, *args):
        if args[-1] == "init":
            Path(args[1]).write_bytes(b"native current database")
        return {"status":"initialized", "db_id":DB_ID}

    def run_setup(self):
        result = {"status":"incomplete", "stage":"preflight"}
        setup.setup(self.args, result)
        return result

    def test_complete_default_and_idempotent_without_publication(self):
        result = self.run_setup()
        self.assertEqual(result["recording"], "automatic")
        self.assertEqual(result["hippocampus"], "async")
        self.assertFalse(result["published"])
        self.assertTrue(result["local_registration"])
        owner = json.loads((self.root / ".mneme/cli.json").read_text())
        self.assertEqual(owner["db_id"], DB_ID)
        config = Path(result["library_config"])
        before = config.read_bytes()
        self.native.reset_mock()
        again = self.run_setup()
        self.native.assert_not_called()  # no offline opener beside the existing owner
        self.assertEqual(again["db_id"], DB_ID)
        self.assertEqual(config.read_bytes(), before)
        self.assertFalse(config.with_name(config.name + ".publisher.json").exists())
        receipt = json.loads((self.root / ".mneme/codex-integration/receipt.json").read_text())
        self.assertEqual(receipt["plan"]["library_enrollment"], "none")
        self.assertNotIn("library_helper", result)

    def registry_config(self, path=None):
        from store_registry import default_registry_path
        path = path or default_registry_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('{"schema":"mneme.stores.v1","stores":[]}')
        return path

    def test_new_setup_discovers_existing_default_store_registry(self):
        path = self.registry_config()
        result = self.run_setup()
        self.assertEqual(result["store_registry"], str(path))
        hook_path = self.root / ".mneme/codex-integration/config/hooks.json"
        hook = json.loads(hook_path.read_bytes())
        self.assertEqual(hook["schema"], "mneme.codex-hooks.config.v12")
        self.assertEqual(hook["store_registry"], str(path))
        self.assertNotIn("global_preferences", hook)
        self.assertIsNone(setup.load_profile(self.root)["library_config"])
        self.assertEqual(self.run_setup()["store_registry"], str(path))

    def test_explicit_store_registry_wins_over_default_for_new_setup(self):
        default = self.registry_config()
        default.write_text('{"invalid":true}')
        self.args.store_registry = self.registry_config(self.base / "explicit/stores.json")
        self.assertEqual(self.run_setup()["store_registry"], str(self.args.store_registry))

    def test_invalid_default_store_registry_refuses_before_native_or_config_writes(self):
        path = self.registry_config()
        path.write_text('{"schema":"mneme.stores.v1","stores":[],"unknown":true}')
        with self.assertRaisesRegex(ValueError, "store registry"):
            self.run_setup()
        self.native.assert_not_called()
        self.ready.assert_not_called()
        self.assertFalse((self.root / ".mneme").exists())
        self.assertFalse((self.root / ".codex").exists())

    def test_existing_legacy_setup_never_silently_adopts_new_default_registry(self):
        self.assertNotIn("store_registry", self.run_setup())
        path = self.registry_config()
        path.write_text('{"invalid":true}')
        self.assertNotIn("store_registry", self.run_setup())
        path.write_text('{"schema":"mneme.stores.v1","stores":[]}')
        self.args.store_registry = path
        self.ready.reset_mock()
        with self.assertRaisesRegex(ValueError, "conflicts with existing integration"):
            self.run_setup()
        self.ready.assert_not_called()

    def test_existing_named_setup_preserves_route_and_refuses_explicit_reroute(self):
        original = self.registry_config()
        self.run_setup()
        self.args.store_registry = self.registry_config(self.base / "other/stores.json")
        self.ready.reset_mock()
        with self.assertRaisesRegex(ValueError, "conflicts with existing integration"):
            self.run_setup()
        self.ready.assert_not_called()
        self.args.store_registry = original
        self.assertEqual(self.run_setup()["store_registry"], str(original))

    def test_adopts_existing_database_and_bodies_unchanged(self):
        dot = self.root / ".mneme"
        dot.mkdir()
        (dot / "codex-memory.db").write_bytes(b"retained store")
        (dot / "bodies").mkdir()
        (dot / "bodies/note").write_bytes(b"retained body")
        result = self.run_setup()
        self.assertEqual(result["store"], "adopted")
        self.assertEqual((dot / "codex-memory.db").read_bytes(), b"retained store")
        self.assertEqual((dot / "bodies/note").read_bytes(), b"retained body")
        self.assertEqual(self.native.call_args.args[-1], "inspect")

    def test_recording_off_still_sets_up_recall_owner(self):
        self.args.no_recording = True
        result = self.run_setup()
        self.assertEqual(result["recording"], "off")
        self.assertEqual(result["hippocampus"], "async")
        self.args.no_recording = False
        self.assertEqual(self.run_setup()["recording"], "off")

    def test_explicit_recording_off_can_disable_existing_automatic(self):
        self.run_setup()
        self.args.no_recording = True
        self.assertEqual(self.run_setup()["recording"], "off")

    def test_other_store_and_activation_refused_before_writes(self):
        for name in ("memory.db", "current", "generations"):
            with self.subTest(name=name):
                dot = self.root / ".mneme"
                dot.mkdir(exist_ok=True)
                path = dot / name
                path.write_bytes(b"existing")
                with self.assertRaisesRegex(ValueError, "ambiguous"):
                    self.run_setup()
                self.assertFalse((dot / "codex-memory.db").exists())
                self.assertFalse((self.root / ".codex").exists())
                path.unlink()
        self.native.assert_not_called()

    def run_cli(self, codex, python=None):
        output = io.StringIO()
        with redirect_stdout(output):
            status = setup.main(['--root', str(self.root), '--mnemed', str(self.args.mnemed),
                '--library-helper', str(self.args.library_helper), '--mcp-binary', str(self.args.mcp_binary),
                '--port', str(self.args.port), '--no-trust-hooks', '--json', '--codex-binary', codex,
                *(['--python', python] if python is not None else [])])
        return status, json.loads(output.getvalue())

    def test_cli_rejects_relative_reader_spellings_without_normalizing_to_path_command(self):
        for reader in ('./codex', 'codex/'):
            with self.subTest(reader=reader):
                status, result = self.run_cli(reader)
                self.assertEqual(status, 1)
                self.assertIn("executable must be 'codex' or an absolute path", result['error'])
                self.assertFalse((self.root / '.mneme').exists())
                self.assertFalse((self.root / '.codex').exists())
        self.native.assert_not_called()
        self.ready.assert_not_called()
        self.trust.assert_not_called()

    def test_dynamic_launcher_envelope_is_exact_and_legacy_direct_launcher_still_loads(self):
        self.run_setup()
        path = self.root / '.codex/config.toml'
        raw = path.read_text()
        service, _, _ = setup.configured_integration(self.root)
        prefix = service.parent.parent
        launcher_args = [str(prefix / 'lib/launcher.py'), '--service-config', str(service)]
        expected = setup.install.python_hook_argv('python3', launcher_args)
        import tomllib
        server = tomllib.loads(raw)['mcp_servers'][setup.install.SERVER]
        self.assertEqual([server['command'], *server['args']], expected)
        if len(expected) > 4:
            # The shell body is data with one exact approved shape, not a parser
            # invitation to adopt any command with a familiar-looking suffix.
            changed = raw.replace('for python in', 'echo unexpected; for python in', 1)
            self.assertNotEqual(changed, raw)
            path.write_text(changed)
            with self.assertRaisesRegex(ValueError, 'not the shipped local launcher'):
                setup.configured_integration(self.root)
        path.write_text('[mcp_servers.' + setup.install.SERVER + ']\ncommand = "python3"\nargs = ' +
                        json.dumps(launcher_args) + '\n')
        self.assertEqual(setup.configured_integration(self.root)[0], service)

    def test_explicit_runtime_python_reaches_plan_without_canonicalizing_symlink(self):
        import sys
        selected = self.bin / 'selected-python'
        selected.symlink_to(sys.executable)
        status, result = self.run_cli(str(self.bin / "codex"), python=str(selected))
        self.assertEqual(status, 0, result)
        receipt = json.loads((self.root / '.mneme/codex-integration/receipt.json').read_text())
        self.assertEqual(receipt['plan']['python'], str(selected))
        definitions = setup.install.planned_hook_handlers(receipt['plan'])
        import shlex
        self.assertEqual(shlex.split(definitions['SessionStart'][0]['hooks'][0]['command'])[0], str(selected))

    def test_explicit_mcp_runtime_selector_retains_symlink_spelling(self):
        selected = self.bin / 'selected-mcp'
        selected.symlink_to(self.bin / 'mneme-mcp')
        self.args.mcp_binary = selected
        self.run_setup()
        prefix = self.root / '.mneme/codex-integration'
        receipt = json.loads((prefix / 'receipt.json').read_text())
        service = json.loads((prefix / 'config/service.json').read_text())
        self.assertEqual(receipt['plan']['mcp_command'], str(selected))
        self.assertEqual(service['binary'], str(selected))
        self.assertEqual(receipt['plan']['native_command'], 'mnemed')

    def test_default_mcp_selector_uses_path_without_persisting_discovered_path(self):
        self.args.mcp_binary = None
        self.run_setup()
        prefix = self.root / '.mneme/codex-integration'
        self.assertEqual(json.loads((prefix / 'config/service.json').read_text())['binary'], 'mneme-mcp')

    def test_missing_runtime_mcp_refuses_before_storage_even_with_sibling_artifact(self):
        self.args.mcp_binary = None
        with patch('host_platform.shutil.which', return_value=None), \
             self.assertRaisesRegex(ValueError, 'executable unavailable: mneme-mcp'):
            self.run_setup()
        self.assertFalse((self.root / '.mneme').exists())
        self.assertFalse((self.root / '.codex').exists())
        self.native.assert_not_called()
        self.ready.assert_not_called()

    def test_cli_literal_codex_remains_symbolic(self):
        status, result = self.run_cli('codex')
        self.assertEqual(status, 0, result)
        hook = json.loads((self.root / '.mneme/codex-integration/config/hooks.json').read_text())
        self.assertEqual(hook['reader_codex'], 'codex')
        self.assertNotIn('reader_codex_sha256', hook)

    def test_cli_absolute_reader_remains_lexical_and_unpinned(self):
        reader = self.bin / 'selected-codex'
        reader.symlink_to(self.bin / 'codex')
        status, result = self.run_cli(str(reader))
        self.assertEqual(status, 0, result)
        hook = json.loads((self.root / '.mneme/codex-integration/config/hooks.json').read_text())
        self.assertEqual(hook['reader_codex'], str(reader))
        self.assertNotIn('reader_codex_sha256', hook)

    def test_default_reader_stays_symbolic_while_trust_resolves_current_path(self):
        self.args.codex_binary = None
        for name in ('codex', 'codex-updated'):
            binary = self.bin / name
            if not binary.exists():
                binary.write_text('#!/bin/sh\nexit 1\n')
                binary.chmod(0o700)
            self.trust.reset_mock()
            with patch.object(setup.shutil, 'which', return_value=str(binary)) as which:
                result = self.run_setup()
            which.assert_called_once_with(setup.executable_name('codex'))
            self.assertEqual(self.trust.call_args.args[0], binary)
            self.assertEqual(result['hook_trust']['status'], 'trusted')
            hook = json.loads((self.root / '.mneme/codex-integration/config/hooks.json').read_text())
            self.assertEqual(hook['reader_codex'], 'codex')
            self.assertNotIn('reader_codex_sha256', hook)

    def test_default_missing_codex_leaves_setup_configured_and_trust_pending(self):
        self.args.codex_binary = None
        with patch.object(setup.shutil, 'which', return_value=None):
            result = self.run_setup()
        self.assertEqual(result['status'], 'configured')
        self.assertEqual(result['hook_trust'], {'status': 'pending', 'trusted': 0, 'reason': 'codex_unavailable'})
        self.trust.assert_not_called()
        hook = json.loads((self.root / '.mneme/codex-integration/config/hooks.json').read_text())
        self.assertEqual(hook['reader_codex'], 'codex')
        self.assertNotIn('reader_codex_sha256', hook)

    def test_default_no_trust_needs_no_codex_lookup(self):
        self.args.codex_binary = None
        self.args.no_trust_hooks = True
        with patch.object(setup.shutil, 'which', side_effect=AssertionError('unneeded Codex lookup')):
            result = self.run_setup()
        self.assertEqual(result['status'], 'configured')
        self.trust.assert_not_called()

    def test_explicit_missing_codex_has_no_store_or_configuration_residue(self):
        self.args.codex_binary = self.bin / "absent"
        with self.assertRaisesRegex(ValueError, "executable unavailable"):
            self.run_setup()
        self.assertFalse((self.root / ".mneme").exists())
        self.assertFalse((self.root / ".codex").exists())

    def test_private_isolated_and_disabled_choices_preserved(self):
        dot = self.root / ".mneme"
        dot.mkdir()
        for mode in ("private", "isolated"):
            (dot / "profile.json").write_text(json.dumps({"schema":"mneme.profile.v1", "mode":mode}))
            with self.assertRaisesRegex(ValueError, "explicitly"):
                self.run_setup()
        (dot / "profile.json").unlink()
        (self.root / ".codex").mkdir()
        (self.root / ".codex/hooks.json").write_text('{"enabled":false,"hooks":{}}')
        with self.assertRaisesRegex(ValueError, "disabled"):
            self.run_setup()
        self.native.assert_not_called()
        self.assertFalse((dot / "codex-memory.db").exists())

    def test_conflicting_owner_refused_without_alternate_start(self):
        self.run_setup()
        owner_path = self.root / ".mneme/cli.json"
        owner = json.loads(owner_path.read_text())
        owner["url"] = "http://127.0.0.1:19872/"
        owner_path.write_text(json.dumps(owner))
        self.ready.reset_mock()
        with self.assertRaisesRegex(ValueError, "route conflicts"):
            self.run_setup()
        self.ready.assert_not_called()
        self.assertEqual(json.loads(owner_path.read_text())["url"], owner["url"])

    def test_service_failure_retains_truthful_recoverable_install(self):
        self.ready.side_effect = RuntimeError("owner unavailable")
        result = {"status":"incomplete", "stage":"preflight"}
        with self.assertRaisesRegex(RuntimeError, "unavailable"):
            setup.setup(self.args, result)
        self.assertEqual(result["stage"], "owner_start")
        self.assertTrue((self.root / ".mneme/codex-memory.db").is_file())
        self.assertTrue((self.root / ".mneme/codex-integration/receipt.json").is_file())
        self.assertFalse((self.root / ".mneme/cli.json").exists())
        self.ready.side_effect = None
        self.native.reset_mock()
        self.assertEqual(self.run_setup()["status"], "configured")
        self.native.assert_not_called()

    def test_project_toml_hook_off_switch_preserved(self):
        (self.root / ".codex").mkdir()
        config = self.root / ".codex/config.toml"
        config.write_text('[hooks]\nenabled=false\n')
        with self.assertRaisesRegex(ValueError, "TOML hooks.*disabled"):
            self.run_setup()
        self.assertEqual(config.read_text(), '[hooks]\nenabled=false\n')
        self.native.assert_not_called()
        self.assertFalse((self.root / ".mneme").exists())

    def test_hook_trust_optout_never_starts_trust_client(self):
        self.args.no_trust_hooks = True
        result = self.run_setup()
        self.assertEqual(result["hook_trust"]["status"], "not_requested")
        self.trust.assert_not_called()
        self.assertEqual(result["status"], "configured")

    def test_trust_pending_does_not_rollback_completed_setup(self):
        self.trust.return_value = {"status":"pending", "trusted":0, "reason":"native_rpc_refused"}
        result = self.run_setup()
        self.assertEqual(result["hook_trust"]["status"], "pending")
        self.assertEqual(result["status"], "configured")
        self.assertTrue((self.root / ".mneme/cli.json").exists())
        self.assertTrue((self.root / ".mneme/codex-memory.db").exists())
        self.assertEqual(self.run_setup()["hook_trust"]["status"], "pending")

    def test_custom_database_or_other_named_integration_refused(self):
        dot = self.root / ".mneme"
        dot.mkdir()
        (dot / "custom.db").write_bytes(b"memory")
        with self.assertRaisesRegex(ValueError, "ambiguous"):
            self.run_setup()
        (dot / "custom.db").unlink()
        (self.root / ".codex").mkdir()
        (self.root / ".codex/config.toml").write_text('[mcp_servers.alternate_memory]\ncommand="/custom/mneme-mcp"\n')
        with self.assertRaisesRegex(ValueError, "existing Mneme"):
            self.run_setup()
        self.native.assert_not_called()
        self.assertFalse((dot / "codex-memory.db").exists())

    def test_inherited_private_profile_refused(self):
        (self.base / ".mneme").mkdir()
        (self.base / ".mneme/profile.json").write_text('{"schema":"mneme.profile.v1","mode":"private"}')
        with self.assertRaisesRegex(ValueError, "private"):
            self.run_setup()
        self.native.assert_not_called()
        self.assertFalse((self.root / ".mneme").exists())

    def test_explicit_registry_selection_persisted(self):
        self.args.library_config = self.base / "chosen/library.json"
        result = self.run_setup()
        profile = json.loads((self.root / ".mneme/profile.json").read_text())
        self.assertEqual(profile["library_config"], result["library_config"])
        self.assertEqual(self.run_setup()["library_config"], result["library_config"])

    def test_adoption_identity_change_refuses_cli_binding(self):
        dot = self.root / ".mneme"
        dot.mkdir()
        (dot / "codex-memory.db").write_bytes(b"retained")
        with patch.object(setup, "catalog", return_value="01ARZ3NDEKTSV4RRFFQ69G5FAW"):
            with self.assertRaisesRegex(ValueError, "identity changed"):
                self.run_setup()
        self.assertFalse((dot / "cli.json").exists())

    def test_linked_store_refused(self):
        dot = self.root / ".mneme"
        dot.mkdir()
        origin = self.base / "database"
        origin.write_bytes(b"store")
        os.link(origin, dot / "codex-memory.db")
        with self.assertRaisesRegex(ValueError, "single-link"):
            self.run_setup()
        self.native.assert_not_called()

if __name__ == "__main__":
    unittest.main()

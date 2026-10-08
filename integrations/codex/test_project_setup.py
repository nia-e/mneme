"""Disposable setup tests; never run inference, services or touch enrolled stores."""
import argparse
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
        patch.dict(os.environ, {"HOME":str(self.base / "home"), "XDG_DATA_HOME":str(self.base / "data")}).start()
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

    def test_missing_codex_has_no_store_or_configuration_residue(self):
        self.args.codex_binary = self.bin / "absent"
        with self.assertRaises(FileNotFoundError):
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

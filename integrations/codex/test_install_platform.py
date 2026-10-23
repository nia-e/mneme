"""Portable installer fixtures; native Windows assertions never claim Unix emulation is Windows."""
import base64
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import patch

import host_files
import install


class PortableInstallTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name).resolve() / "café 🦋 & %NAME% ! (workspace)"
        self.base.mkdir()
        self.root = self.base / "project"
        self.root.mkdir()
        self.prefix = self.base / "installed runtime"
        self.sources = self.base / "sources"
        self.sources.mkdir()
        for name in install.PROGRAMS:
            shutil.copyfile(Path(install.__file__).with_name(name), self.sources / name)

    def prepare(self, **options):
        # These executables are pinned/copied, never invoked by the installer.
        return install.prepare(self.root, self.prefix, sys.executable, sys.executable, 18765,
            program_root=self.sources, recall_mode="async", reader_model=install.READER_MODELS[0],
            reader_codex=Path(sys.executable), recording_mode="automatic", **options)

    def test_every_runtime_helper_is_in_inventory_hashes_and_installed_copy(self):
        plan = self.prepare()
        install.validate_plan(plan)
        self.assertTrue({"host_files.py", "host_process.py", "host_platform.py"} <= set(install.PROGRAMS))
        result = install.apply(plan)
        self.assertEqual(result["status"], "configured")
        for row in plan["files"]:
            self.assertEqual(install.digest((self.prefix / row["destination"]).read_bytes()), row["sha256"])
        for name in ("host_files.py", "host_process.py", "host_platform.py"):
            host_files.check_private(self.prefix / "lib" / name)
        host_files.check_private_directory(self.prefix)
        self.assertEqual(install.uninstall(result["receipt"])["status"], "configuration removed")
        self.assertFalse((self.root / ".codex/config.toml").exists())

    def test_changed_helper_refuses_before_installation(self):
        plan = self.prepare()
        (self.sources / "host_files.py").write_bytes(b"changed")
        with self.assertRaisesRegex(ValueError, "source changed"):
            install.apply(plan)
        self.assertFalse(self.prefix.exists())
        self.assertFalse((self.root / ".codex").exists())

    def test_utf8_non_bmp_project_and_runtime_paths_are_valid_toml(self):
        plan = self.prepare()
        outputs = install.config_outputs(plan, {"config.toml": None, "hooks.json": None})
        server = tomllib.loads(outputs["config.toml"].decode("utf-8"))["mcp_servers"][install.SERVER]
        expected = install.python_hook_argv(plan["python"], [str(self.prefix / "lib/launcher.py"),
            "--service-config", str(self.prefix / "config/service.json")], windows=install.plan_platform(plan) == "windows")
        self.assertEqual([server["command"], *server["args"]], expected)

    def test_windows_plan_binds_exe_inventory_and_separately_encoded_background(self):
        python = self.base / "Python fixture.exe"
        shutil.copyfile(sys.executable, python)
        python.chmod(0o700)
        with patch.object(install, "HOST_PLATFORM", "windows"), patch.object(sys, "executable", str(python)):
            plan = self.prepare()
        install.validate_plan(plan)
        self.assertEqual([row["destination"] for row in plan["files"][:2]],
                         ["bin/mnemed.exe", "bin/mneme-mcp.exe"])
        service, _ = install.runtime_configs(plan)
        self.assertEqual(service["binary"], "mneme-mcp")
        outputs = install.config_outputs(plan, {"config.toml": None, "hooks.json": None})
        hooks = json.loads(outputs["hooks.json"])["hooks"]["UserPromptSubmit"]
        commands = [group["hooks"][0]["command"] for group in hooks]
        self.assertEqual(len(commands), 2)
        for command in commands:
            self.assertTrue(command.startswith("powershell.exe -NoProfile -NonInteractive -OutputFormat Text -EncodedCommand "))
            self.assertNotIn(str(self.prefix), command)
            # A suffix outside the encoded program would be an argument to
            # powershell.exe, not the intended Python hook.
            self.assertEqual(len(command.split()), 7)
        self.assertNotEqual(commands[0], commands[1])
        decoded = [base64.b64decode(command.split()[-1], validate=True).decode("utf-16-le") for command in commands]
        self.assertNotIn("--reader-background", decoded[0])
        self.assertIn("--reader-background", decoded[1])
        if os.name != "nt":
            with self.assertRaisesRegex(ValueError, "different host platform"):
                install.apply(plan)
            self.assertFalse(self.prefix.exists())

    @unittest.skipIf(os.name == "nt", "historical receipts target POSIX paths, not Windows installations")
    def test_historical_receipts_keep_frozen_outputs_and_inventory(self):
        fixture = json.loads(Path(__file__).with_name("fixtures").joinpath("install-revision-22-23.json").read_text(encoding="utf-8"))
        self.assertEqual(len(fixture["source_sha256"]), 64)
        for entry in fixture["entries"]:
            with self.subTest(revision=entry["plan"]["config_revision"]):
                before = {key: value.encode() if value is not None else None for key, value in entry["before"].items()}
                expected = {key: value.encode() for key, value in entry["outputs"].items()}
                self.assertEqual(install.config_outputs(entry["plan"], before), expected)
                self.assertEqual([list(row) for row in install.V22_DESTINATIONS], entry["destinations"])
                self.assertEqual(install.plan_platform(entry["plan"]), "posix")

    def test_legacy_revision_cannot_gain_windows_filenames(self):
        plan = self.prepare()
        plan["config_revision"] = 22
        plan["python"] = sys.executable
        plan["files"] = plan["files"][:len(install.V22_DESTINATIONS)]
        plan["files"][0]["destination"] = "bin/mnemed.exe"
        plan["plan_sha256"] = install.plan_hash(plan)
        with self.assertRaisesRegex(ValueError, "files are inconsistent"):
            install.validate_plan(plan)


if __name__ == "__main__":
    unittest.main()

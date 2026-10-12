import base64
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import host_platform


class HostPlatformTests(unittest.TestCase):
    def test_native_executable_name(self):
        self.assertEqual(host_platform.executable_name("mnemed", windows=True), "mnemed.exe")
        self.assertEqual(host_platform.executable_name("mnemed.exe", windows=True), "mnemed.exe")
        self.assertEqual(host_platform.executable_name("mnemed", windows=False), "mnemed")
        for invalid in ("../mnemed", "bin\\mnemed", "bad\nname", ""):
            with self.assertRaises(ValueError):
                host_platform.executable_name(invalid)

    def test_unix_hook_argv_roundtrips(self):
        argv = ["/a weird/python", "script's.py", "--config", "hi & bye.json", ""]
        self.assertEqual(shlex.split(host_platform.hook_command(argv, windows=False)), argv)

    def test_windows_hook_shell_sees_only_fixed_tokens_and_base64(self):
        argv = [r"C:\Program Files\Python\python.exe", r"C:\Nia's !%& café\hooks.py", "", 'a"b']
        command = host_platform.hook_command(argv, windows=True)
        envelope, encoded = command.rsplit(" ", 1)
        self.assertEqual(envelope, "powershell.exe -NoProfile -NonInteractive -OutputFormat Text -EncodedCommand")
        script = base64.b64decode(encoded).decode("utf-16-le")
        self.assertIn("$p.UseShellExecute=$false", script)
        self.assertIn("$p.EnvironmentVariables['PYTHONUTF8']='1'", script)
        self.assertIn("Nia''s !%& café", script)
        self.assertIn("exit $c.ExitCode", script)
        self.assertNotIn("café", command)
        self.assertNotIn("%", command)

    def test_windows_hook_over_cmd_limit_refused_before_install(self):
        with self.assertRaisesRegex(ValueError, "command length"):
            host_platform.hook_command(["python.exe", "x" * 9000], windows=True)

    def test_windows_hook_rejects_shell_trampoline(self):
        with self.assertRaises(ValueError):
            host_platform.hook_command(["python.cmd", "script.py"], windows=True)

    def test_windows_executable_check_rejects_cmd(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "python.cmd"
            path.write_bytes(b"anything")
            self.assertFalse(host_platform.is_executable(path, windows=True))
            native = path.with_suffix(".exe")
            native.write_bytes(b"anything")
            self.assertTrue(host_platform.is_executable(native, windows=True))

    def test_config_and_data_defaults_respect_explicit_paths(self):
        with tempfile.TemporaryDirectory() as temp, patch.dict(os.environ,
                {"XDG_CONFIG_HOME": temp, "XDG_DATA_HOME": temp, "APPDATA": temp, "LOCALAPPDATA": temp}):
            for windows in (False, True):
                with patch.object(host_platform, "WINDOWS", windows):
                    self.assertEqual(host_platform.default_config_dir(), Path(temp) / "mneme")
                    self.assertEqual(host_platform.default_data_dir(), Path(temp) / "mneme")

    def test_explicit_xdg_precedes_windows_appdata(self):
        with tempfile.TemporaryDirectory() as temp, patch.object(host_platform, "WINDOWS", True), patch.dict(os.environ,
                {"XDG_CONFIG_HOME": temp, "XDG_DATA_HOME": temp, "APPDATA": str(Path(temp) / "other"), "LOCALAPPDATA": str(Path(temp) / "other")}):
            self.assertEqual(host_platform.default_config_dir(), Path(temp) / "mneme")
            self.assertEqual(host_platform.default_data_dir(), Path(temp) / "mneme")

    def test_relative_environment_default_is_refused(self):
        with patch.object(host_platform, "WINDOWS", False), patch.dict(os.environ, {"XDG_CONFIG_HOME": "relative"}):
            with self.assertRaises(ValueError):
                host_platform.default_config_dir()

    def test_checkpoint_prefix_keeps_fixed_json_argv_and_checks_suffix(self):
        argv = [r"C:\Python\python.exe", "hooks.py", "--workspace-binding", '{"a":"quote\\\" and café"}']
        command = host_platform.checkpoint_command(argv, windows=True)
        self.assertTrue(command.startswith("& {"))
        self.assertIn("$p.Arguments=", command)
        self.assertIn("-cnotmatch", command)
        self.assertIn("--outcome", command)
        self.assertIn("{1,8}", command)
        self.assertEqual(shlex.split(host_platform.checkpoint_command(argv, windows=False)), argv)

    @unittest.skipUnless(os.name == "nt", "Native Windows checkpoint execution")
    def test_windows_checkpoint_suffix_and_json_roundtrip(self):
        with tempfile.TemporaryDirectory(prefix="mneme café '") as temp:
            script = Path(temp) / "args.py"
            script.write_text("import json,sys\nprint(json.dumps(sys.argv[1:],ensure_ascii=False))\n", encoding="utf-8")
            binding = json.dumps({"root": r"C:\User %!& café\workspace", "quote": 'a"b'}, ensure_ascii=False)
            argv = [sys.executable, str(script), "--workspace-binding", binding]
            command = host_platform.checkpoint_command(argv)
            for tail in ("--outcome none", "--outcome deferred", "--outcome captured --id 01ARZ3NDEKTSV4RRFFQ69G5FAV"):
                result = subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", command + " " + tail],
                                        capture_output=True, timeout=15)
                self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
                self.assertEqual(json.loads(result.stdout), argv[2:] + tail.split())
            result = subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", command + " --outcome none --config other"],
                                    capture_output=True, timeout=15)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, b"")

    @unittest.skipUnless(os.name == "nt", "Native Windows hook shell execution")
    def test_windows_cmd_and_powershell_inherit_bytes_and_exact_argv(self):
        with tempfile.TemporaryDirectory(prefix="mneme %!& café '") as temp:
            script = Path(temp) / "inspect args.py"
            script.write_text("import json,sys\n"
                              "raw=sys.stdin.buffer.read()\n"
                              "print(json.dumps(sys.argv[1:],ensure_ascii=False),flush=True)\n"
                              "sys.stdout.buffer.write(raw)\n"
                              "sys.stderr.buffer.write(b'stderr-marker')\n"
                              "sys.exit(7)\n", encoding="utf-8")
            args = ["", 'quoted "value"', "a\\", "space path\\", "!%&|<>", "café λ", "apostrophe'"]
            command = host_platform.hook_command([sys.executable, str(script), *args])
            payload = "stdin café λ\n".encode()
            for outer in (["cmd.exe", "/d", "/s", "/c", command],
                          ["powershell.exe", "-NoProfile", "-NonInteractive", "-OutputFormat", "Text", "-Command", command]):
                result = subprocess.run(outer, input=payload, capture_output=True, timeout=15)
                self.assertEqual(result.returncode, 7, result.stderr.decode(errors="replace"))
                first, rest = result.stdout.split(b"\n", 1)
                self.assertEqual(json.loads(first), args)
                self.assertEqual(rest, payload)
                self.assertEqual(result.stderr, b"stderr-marker")


if __name__ == "__main__":
    unittest.main()

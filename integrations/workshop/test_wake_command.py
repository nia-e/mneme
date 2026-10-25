"""Copied wake launcher resolves SSH at invocation, preserving message data."""

import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest


@unittest.skipUnless(os.name == "posix", "POSIX copied-launcher smoke")
class WakeCommandTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        source = Path(__file__).with_name("wake-command")
        self.launcher = self.root / "wake"
        shutil.copyfile(source, self.launcher)
        self.assertEqual(hashlib.sha256(source.read_bytes()).digest(),
                         hashlib.sha256(self.launcher.read_bytes()).digest())
        self.capture = self.root / "invocation.json"

    def fake_ssh(self, name, status=0):
        directory = self.root / name
        directory.mkdir()
        executable = directory / "ssh"
        executable.write_text("#!" + sys.executable + "\n"
            "import json, os, sys\nfrom pathlib import Path\n"
            "Path(os.environ['WAKE_TEST_CAPTURE']).write_text(json.dumps({"
            "'executable': sys.argv[0], 'arguments': sys.argv[1:]}))\n"
            f"raise SystemExit({status})\n")
        executable.chmod(0o700)
        return directory

    def run_launcher(self, path, *arguments):
        return subprocess.run([sys.executable, str(self.launcher), *arguments],
            env={"PATH": str(path), "WAKE_TEST_CAPTURE": str(self.capture)},
            capture_output=True, text=True, timeout=5)

    def test_current_path_is_used_and_remote_message_stays_data(self):
        message = "it's $(not-a-command); $HOME\nnext line"
        first = self.fake_ssh("first profile")
        second = self.fake_ssh("second profile")
        for directory in (first, second):
            with self.subTest(path=directory):
                result = self.run_launcher(directory, "--ssh", "user@example.invalid",
                    "--root", "/workshop with spaces", "--source", "shell-test",
                    "--reference", "a; reference", "--id", "retry-1", "--", message)
                self.assertEqual(result.returncode, 0, result.stderr)
                capture = json.loads(self.capture.read_text())
                self.assertEqual(capture["executable"], str(directory / "ssh"))
                arguments = capture["arguments"]
                self.assertEqual(arguments[:-1], ["-T", "-o", "BatchMode=yes", "-o",
                    "ConnectTimeout=10", "user@example.invalid"])
                self.assertEqual(shlex.split(arguments[-1]), [".local/bin/wake",
                    "--source", "shell-test", "--reference", "a; reference",
                    "--id", "retry-1", "--root", "/workshop with spaces", "--", message])

    def test_ssh_exit_status_is_preserved(self):
        result = self.run_launcher(self.fake_ssh("bin", 23),
                                   "--ssh", "user@example.invalid")
        self.assertEqual(result.returncode, 23, result.stderr)

    def test_missing_ssh_is_an_actionable_failure(self):
        empty = self.root / "empty"
        empty.mkdir()
        result = self.run_launcher(empty, "--ssh", "user@example.invalid")
        self.assertEqual(result.returncode, 1)
        self.assertIn("Cannot invoke wake delivery:", result.stderr)
        self.assertIn("ssh", result.stderr)
        self.assertFalse(self.capture.exists())

    def test_ssh_destination_cannot_be_an_option_or_multiple_words(self):
        directory = self.fake_ssh("bin")
        for destination in ("-oProxyCommand=bad", "user@host another", "user@host\n"):
            with self.subTest(destination=destination):
                result = self.run_launcher(directory, "--ssh=" + destination)
                self.assertEqual(result.returncode, 2)
                self.assertIn("--ssh must be one SSH destination", result.stderr)
                self.assertFalse(self.capture.exists())

    def test_local_route_keeps_its_current_interpreter(self):
        (self.root / "wake.py").write_text(
            "import json, os, sys\nfrom pathlib import Path\n"
            "Path(os.environ['WAKE_TEST_CAPTURE']).write_text(json.dumps({"
            "'executable': sys.executable, 'arguments': sys.argv[1:]}))\n")
        result = self.run_launcher(self.root, "--root", str(self.root), "--id", "local-1", "hello")
        self.assertEqual(result.returncode, 0, result.stderr)
        capture = json.loads(self.capture.read_text())
        self.assertEqual(capture["executable"], sys.executable)
        self.assertEqual(capture["arguments"], ["--root", str(self.root), "enqueue",
            "--source", "local-shell", "--reference", "", "--id", "local-1",
            "--summary", "hello"])


if __name__ == "__main__":
    unittest.main()

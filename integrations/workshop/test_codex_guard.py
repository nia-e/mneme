"""Bounded local subprocess tests; no live Codex or remote sessions are started."""

import json
import os
from pathlib import Path
import pty
import signal
import subprocess
import sys
import tempfile
import time
import unittest

import codex_guard


FAKE_CODEX = r'''
import json, os, pathlib, sys, time
root = pathlib.Path(os.environ["GUARD_TEST_ROOT"])
lock = root / ".pi-codex-activity.lock"
fds = []
if lock.exists():
    info = lock.stat()
    for fd in range(3, 128):
        try:
            held = os.fstat(fd)
            if (held.st_dev, held.st_ino) == (info.st_dev, info.st_ino):
                fds.append(fd)
        except OSError:
            pass
value = {"args": sys.argv[1:], "pid": os.getpid(), "fds": fds,
         "tty": [os.isatty(fd) for fd in range(3)]}
(root / "ready.json").write_text(json.dumps(value))
if os.environ.get("GUARD_READ_STDIN"):
    print(sys.stdin.readline().rstrip("\n"), flush=True)
until = time.monotonic() + 15
while os.environ.get("GUARD_HOLD") and not (root / "release").exists() and time.monotonic() < until:
    time.sleep(0.01)
sys.exit(int(os.environ.get("GUARD_EXIT", "0")))
'''


class ActivityGateTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        self.path = self.root / codex_guard.LOCK_NAME

    def test_contention_release_keeps_permanent_inode_and_contents(self):
        self.path.write_bytes(b"retained operator evidence")
        inode = self.path.stat().st_ino
        gate = codex_guard.try_activity_gate(self.path)
        self.assertIsNotNone(gate)
        try:
            self.assertIsNone(codex_guard.try_activity_gate(self.path))
            self.assertFalse(os.get_inheritable(gate.fileno()))
        finally:
            gate.close()
        with codex_guard.try_activity_gate(self.path):
            self.assertEqual(self.path.stat().st_ino, inode)
            self.assertEqual(self.path.read_bytes(), b"retained operator evidence")

    def test_parent_alias_contends_on_same_inode(self):
        actual = self.root / "actual"
        actual.mkdir()
        alias = self.root / "alias"
        alias.symlink_to(actual, target_is_directory=True)
        with codex_guard.try_activity_gate(actual / codex_guard.LOCK_NAME):
            self.assertIsNone(codex_guard.try_activity_gate(alias / codex_guard.LOCK_NAME))

    def test_busy_status_does_not_create_absent_lock_or_change_contents(self):
        self.assertFalse(codex_guard.activity_busy(self.path))
        self.assertFalse(self.path.exists())
        self.path.write_bytes(b"keep")
        before = self.path.stat()
        self.assertFalse(codex_guard.activity_busy(self.path))
        with codex_guard.try_activity_gate(self.path):
            self.assertTrue(codex_guard.activity_busy(self.path))
        after = self.path.stat()
        self.assertEqual((before.st_ino, before.st_mtime_ns), (after.st_ino, after.st_mtime_ns))
        self.assertEqual(self.path.read_bytes(), b"keep")

    def test_relative_missing_parent_and_nonregular_refused(self):
        with self.assertRaises(codex_guard.Refusal):
            codex_guard.try_activity_gate(Path("relative"))
        with self.assertRaises(FileNotFoundError):
            codex_guard.try_activity_gate(self.root / "absent" / "lock")
        self.assertFalse((self.root / "absent").exists())
        self.path.mkdir()
        with self.assertRaises(OSError):
            codex_guard.try_activity_gate(self.path)
        self.path.rmdir()
        os.mkfifo(self.path)
        with self.assertRaises(codex_guard.Refusal):
            codex_guard.try_activity_gate(self.path)

    def test_symlink_and_hardlink_refused_without_mutation(self):
        target = self.root / "target"
        target.write_bytes(b"keep")
        self.path.symlink_to(target)
        with self.assertRaises(OSError):
            codex_guard.try_activity_gate(self.path)
        self.path.unlink()
        os.link(target, self.path)
        with self.assertRaises(codex_guard.Refusal):
            codex_guard.try_activity_gate(self.path)
        self.assertEqual(target.read_bytes(), b"keep")
        with self.assertRaises(codex_guard.Refusal):
            codex_guard.activity_busy(self.path)

    def test_child_inherits_gate_if_launcher_is_killed(self):
        # Models the runner's pass_fds contract, not arbitrary daemon behaviour.
        helper = r'''
import pathlib, subprocess, sys, time
import codex_guard
root = pathlib.Path(sys.argv[1])
gate = codex_guard.try_activity_gate(root / codex_guard.LOCK_NAME)
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(15)"],
                         pass_fds=(gate.fileno(),))
(root / "child.pid").write_text(str(child.pid))
time.sleep(15)
'''
        process = subprocess.Popen([sys.executable, "-c", helper, str(self.root)],
                                   cwd=Path(codex_guard.__file__).parent,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        child_pid = None
        try:
            _wait_for(self.root / "child.pid")
            child_pid = int((self.root / "child.pid").read_text())
            process.kill()
            process.wait(timeout=5)
            self.assertIsNone(codex_guard.try_activity_gate(self.path))
            os.kill(child_pid, signal.SIGKILL)
            child_pid = None
            self._wait_released()
        finally:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=5)
            if child_pid is not None:
                os.kill(child_pid, signal.SIGKILL)

    def _wait_released(self):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            gate = codex_guard.try_activity_gate(self.path)
            if gate is not None:
                gate.close()
                return
            time.sleep(0.01)
        self.fail("activity lock was not released")


def _wait_for(path):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if path.exists() and path.stat().st_size:
            return
        time.sleep(0.01)
    raise AssertionError(f"subprocess did not publish {path.name}")


class GuardCliTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        self.fake = self.root / "fake codex"
        self.fake.write_text(f"#!{sys.executable}\n" + FAKE_CODEX)
        self.fake.chmod(0o700)
        self.env = dict(os.environ, GUARD_TEST_ROOT=str(self.root))
        self.command = [sys.executable, str(Path(codex_guard.__file__).resolve()),
                        "--root", str(self.root), "--codex", str(self.fake), "--"]

    def run_cli(self, arguments=(), **kwargs):
        return subprocess.run([*self.command, *arguments], env=self.env,
                              text=True, capture_output=True, timeout=5, **kwargs)

    def read_ready(self):
        _wait_for(self.root / "ready.json")
        return json.loads((self.root / "ready.json").read_text())

    def test_argument_transparency_inherited_fd_stdio_and_exit(self):
        self.env.update(GUARD_READ_STDIN="1", GUARD_EXIT="23")
        original = ["exec", "-c", 'key="two words"', "--", "literal ; $nothing", "--no-daemon"]
        result = self.run_cli(original, input="unchanged stdin\n")
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertEqual(result.stdout, "unchanged stdin\n")
        ready = self.read_ready()
        self.assertEqual(ready["args"], ["--no-daemon", *original])
        self.assertEqual(len(ready["fds"]), 1)
        with codex_guard.try_activity_gate(self.root / codex_guard.LOCK_NAME):
            pass

    def test_no_daemon_is_not_duplicated(self):
        self.assertEqual(self.run_cli(["--no-daemon", "exec", "hello"]).returncode, 0)
        self.assertEqual(self.read_ready()["args"], ["--no-daemon", "exec", "hello"])

    def test_busy_does_not_start_child_and_returns_temporary_failure(self):
        with codex_guard.try_activity_gate(self.root / codex_guard.LOCK_NAME):
            result = self.run_cli(["exec", "hello"])
            self.assertEqual(result.returncode, codex_guard.BUSY_EXIT)
            self.assertIn("another Pi session", result.stderr)
            self.assertFalse((self.root / "ready.json").exists())

    def test_direct_metadata_and_auth_do_not_take_gate(self):
        with codex_guard.try_activity_gate(self.root / codex_guard.LOCK_NAME):
            for arguments in (["--version"], ["-V"], ["--help"], ["-h"],
                              ["help", "exec"], ["login", "status"], ["logout"]):
                with self.subTest(arguments=arguments):
                    result = self.run_cli(arguments)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(self.read_ready()["args"], arguments)
                    self.assertEqual(self.read_ready()["fds"], [])

    def test_metadata_with_global_options_is_conservatively_gated(self):
        with codex_guard.try_activity_gate(self.root / codex_guard.LOCK_NAME):
            result = self.run_cli(["-c", "x=1", "login", "status"])
            self.assertEqual(result.returncode, codex_guard.BUSY_EXIT)

    def test_bare_arguments_start_foreground_interactive_session(self):
        self.assertEqual(self.run_cli().returncode, 0)
        self.assertEqual(self.read_ready()["args"], ["--no-daemon"])

    def test_exec_replaces_wrapper_and_signals_release_gate(self):
        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGKILL):
            with self.subTest(signal=sig):
                (self.root / "ready.json").unlink(missing_ok=True)
                process = subprocess.Popen(self.command, env=dict(self.env, GUARD_HOLD="1"),
                                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                try:
                    self.assertEqual(self.read_ready()["pid"], process.pid)
                    self.assertIsNone(codex_guard.try_activity_gate(self.root / codex_guard.LOCK_NAME))
                    process.send_signal(sig)
                    self.assertEqual(process.wait(timeout=5), -sig)
                    with codex_guard.try_activity_gate(self.root / codex_guard.LOCK_NAME):
                        pass
                finally:
                    if process.poll() is None:
                        process.kill()
                    process.wait(timeout=5)

    def test_tty_descriptors_are_not_piped_or_replaced(self):
        master, slave = pty.openpty()
        try:
            result = subprocess.run(self.command, env=self.env, stdin=slave, stdout=slave,
                                    stderr=slave, timeout=5)
            self.assertEqual(result.returncode, 0)
            self.assertEqual(self.read_ready()["tty"], [True, True, True])
        finally:
            os.close(master)
            os.close(slave)

    def test_bad_cli_and_exec_failure_release_gate(self):
        for command in (self.command[:-1],
                        [*self.command[:2], "--root", "relative", "--codex", str(self.fake), "--"],
                        [*self.command[:2], "--root", str(self.root), "--codex", "relative", "--"]):
            result = subprocess.run(command, env=self.env, capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 2)
            self.assertFalse((self.root / codex_guard.LOCK_NAME).exists())
        self.fake.write_text("not an executable format\n")
        result = self.run_cli()
        self.assertEqual(result.returncode, 1)
        self.assertIn("launch refused", result.stderr)
        with codex_guard.try_activity_gate(self.root / codex_guard.LOCK_NAME):
            pass


if __name__ == "__main__":
    unittest.main()

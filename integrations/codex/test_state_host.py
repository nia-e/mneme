"""Portable state/lease contracts; disposable files, no provider or live owner.

These same tests exercise native Win32 handles/DACLs when run on Windows. A Unix
pass is not evidence that those native branches ran.
"""
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

import hooks
import host_files
import reader_worker as reader
import recording_jobs as recording
import stewardship


class StateHostTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.config = {"state_dir": self.root / "state", "project_root": self.root,
                       "service_config": self.root / "service.json",
                       "recording_mode": "automatic", "memory_mode": "async", "memory_scope": "project",
                       "reader_model": "gpt-6.1-sol", "librarian_effort": "medium"}
        self.config["service_config"].write_text("{}")

    def test_reader_first_then_hook_state_stays_private(self):
        self.assertEqual(reader.notice(self.config, {"session_id": "s", "turn_id": "t",
                         "prompt": "Investigate the local parser"}, True)["outcome"], "queued")
        path, lock = reader._paths(self.config, "s")
        for directory in (self.config["state_dir"], path.parent):
            host_files.check_private_directory(directory)
        for file in (path, lock):
            host_files.check_private(file)
        with hooks._locked_state(self.config["state_dir"], "s") as hook_path:
            hooks._atomic_write(hook_path, {"schema": hooks.STATE_SCHEMA, "turns": {}})
        host_files.check_private(hook_path)
        hooks._admit_async_state(self.config, "s")  # Current state is not a migration.

    @unittest.skipIf(host_files.WINDOWS, "Unix predecessor modes; Windows requires a private DACL")
    def test_retained_unix_0755_state_directories_are_not_repaired_or_refused(self):
        # The prior reader/recording mkdir(parents=True) itself produced 0755
        # ancestors. Portability must not turn these genuine retained states
        # into a permission migration, and must never silently chmod them.
        state = self.config["state_dir"]
        directories = [state, state / "reader", state / "recording", self.root / "journal"]
        for directory in directories:
            directory.mkdir()
            directory.chmod(0o755)
        with hooks._locked_state(state, "s") as path:
            hooks._atomic_write(path, {"schema": hooks.STATE_SCHEMA, "turns": {}})
        self.assertEqual(reader.notice(self.config, {"session_id": "s", "turn_id": "t",
                         "prompt": "Investigate parsing"}, True)["outcome"], "queued")
        self.assertTrue(recording._transaction(self.config, "s", lambda data: (None, True), create=True)[0])
        with stewardship.Journal("01ARZ3NDEKTSV4RRFFQ69G5FAV", root=directories[-1]):
            pass
        self.assertTrue(all(stat.S_IMODE(path.stat().st_mode) == 0o755 for path in directories))
        for file in (path, reader._paths(self.config, "s")[0], recording._path(self.config, "s")):
            host_files.check_private(file)

    def test_hook_lease_contends_across_processes_then_releases(self):
        code = """
import sys
from pathlib import Path
import hooks
try:
    with hooks._locked_state(Path(sys.argv[1]), 's', blocking=False):
        outcome = 'acquired'
except hooks.HookError as error:
    if str(error) != 'hook state busy':
        raise
    outcome = 'busy'
print(outcome)
"""
        command = [sys.executable, "-c", code, str(self.config["state_dir"])]
        def probe():
            return subprocess.run(command, cwd=Path(__file__).parent, text=True,
                                  capture_output=True, check=True, timeout=10).stdout.strip()
        with hooks._locked_state(self.config["state_dir"], "s", blocking=False):
            self.assertEqual(probe(), "busy")
        self.assertEqual(probe(), "acquired")

    def test_hardlinked_hook_lock_refuses_without_touching_payload(self):
        host_files.ensure_private_directory(self.config["state_dir"])
        path = hooks._state_path(self.config["state_dir"], "s").with_suffix(".lock")
        host_files.atomic_write(path, b"preserve")
        os.link(path, self.root / "alias")
        with self.assertRaises(ValueError):
            with hooks._locked_state(self.config["state_dir"], "s"):
                self.fail("multiply-linked lease admitted")
        self.assertEqual(path.read_bytes(), b"preserve")

    def test_reader_paid_ledger_survives_publication_failure(self):
        event = {"session_id": "s", "turn_id": "t", "prompt": "Investigate parsing"}
        reader.notice(self.config, event, True)
        ok, _ = reader._state(self.config, "s", lambda data: (
            data.update(attempts=3, input_tokens=19, output_tokens=5,
                        reservation={"recording": "paid"}, unknown_usage=True), True))
        self.assertTrue(ok)
        path, _ = reader._paths(self.config, "s")
        before = path.read_bytes()
        with patch.object(reader.host_files, "atomic_write", side_effect=OSError("publication unavailable")):
            ok, _ = reader._state(self.config, "s", lambda data: (data.update(prior="optional"), True))
        self.assertFalse(ok)
        self.assertEqual(path.read_bytes(), before)

    def test_unknown_or_reused_worker_pid_is_not_false_death_evidence(self):
        identity = {"pid": 42, "created": "first", "executable": "python"}
        with patch.object(reader.host_process, "process_alive", return_value=None), \
             patch.object(reader.host_process, "process_identity", return_value=None):
            self.assertTrue(reader._alive(42, identity))
        with patch.object(reader.host_process, "process_alive", return_value=True), \
             patch.object(reader.host_process, "process_identity", return_value={**identity, "created": "second"}):
            self.assertFalse(reader._alive(42, identity))
        with patch.object(reader.host_process, "process_alive", return_value=False):
            self.assertFalse(reader._alive(42, identity))

    def test_detached_worker_records_exact_spawned_identity(self):
        self.config["_config_path"] = self.root / "hook.json"
        self.config["_config_path"].write_text("{}")
        event = {"session_id": "s", "turn_id": "t", "prompt": "Investigate parsing"}
        reader.notice(self.config, event, True)
        child = Mock(pid=42)
        identity = {"pid": 42, "created": "exact", "executable": "python"}
        with patch.object(reader.host_process, "spawn", return_value=child) as spawn, \
             patch.object(reader.host_process, "spawned_identity", return_value=identity) as identify:
            self.assertEqual(reader.background(self.config, event)["outcome"], "launched")
        self.assertTrue(spawn.call_args.kwargs["detached"])
        identify.assert_called_once_with(child)
        self.assertEqual(json.loads(reader._paths(self.config, "s")[0].read_bytes())["worker_identity"], identity)

    def test_recording_and_reader_slot_leases_release(self):
        ok, _ = recording._transaction(self.config, "s", lambda data: (None, True), create=True)
        self.assertTrue(ok)
        host_files.check_private(recording._path(self.config, "s"))
        with os.fdopen(host_files.open_regular(recording._directory(self.config) / ".lock", os.O_RDWR), "r+b") as lock:
            host_files.lock_file(lock, blocking=False)
            self.assertFalse(recording._transaction(self.config, "s", lambda data: (None, False))[0])
        self.assertTrue(recording._transaction(self.config, "s", lambda data: (None, False))[0])
        slots = []
        try:
            for _ in range(reader.MAX_WORKERS):
                slots.append(reader._slot(self.config))
                self.assertIsNotNone(slots[-1])
            self.assertIsNone(reader._slot(self.config))
        finally:
            for descriptor in slots:
                if descriptor is not None:
                    os.close(descriptor)
        descriptor = reader._slot(self.config)
        self.assertIsNotNone(descriptor)
        os.close(descriptor)

    def test_windows_predecessor_refuses_before_sidecars_or_rewrite(self):
        host_files.ensure_private_directory(self.config["state_dir"])
        path = hooks._state_path(self.config["state_dir"], "s")
        raw = b'{"schema":"mneme.codex-hooks.state.v1","turns":{}}'
        host_files.atomic_write(path, raw)
        # Select only the admission policy. Real file operations still use this
        # host's implementation; do not fake a successful Win32 call on Unix.
        platform = Mock(wraps=host_files)
        platform.WINDOWS = True
        with patch.object(hooks, "host_files", platform):
            with self.assertRaisesRegex(hooks.HookError, "supported Unix host"):
                hooks._admit_async_state(self.config, "s")
        self.assertEqual(path.read_bytes(), raw)
        self.assertEqual(list(path.parent.iterdir()), [path])

    def test_windows_missing_boot_witness_never_renews_closed_job(self):
        platform = Mock(wraps=host_files)
        platform.WINDOWS = True
        with patch.object(recording, "host_files", platform), \
             patch.object(recording.host_process, "boot_identity", return_value=None):
            self.assertIsNone(recording._boot_id())
            self.assertEqual(recording._remaining({"boot_id": "previous"}), 0)

    def test_stewardship_private_reopen_preserves_progress(self):
        owner = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
        root = self.root / "journal"
        with stewardship.Journal(owner, root=root) as journal:
            journal.put("cursor", "continuation")
            path = journal.path
            host_files.check_private(path)
            host_files.check_private_directory(root)
            with self.assertRaises(BlockingIOError):
                stewardship.Journal(owner, root=root)
        before = path.read_bytes()
        self.assertEqual(stewardship.inspect(owner, root=root)["outcome"], "available")
        self.assertEqual(path.read_bytes(), before)
        with stewardship.Journal(owner, root=root) as journal:
            self.assertEqual(journal.get("cursor"), "continuation")


if __name__ == "__main__":
    unittest.main()

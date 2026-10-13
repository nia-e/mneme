"""Host file contracts; the same tests run on native Windows in CI."""
import ctypes
import contextlib
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import Mock, patch

import host_files


class CheckedFiles(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_bounded_lock_acquires_after_real_contention(self):
        path = self.root / "bounded.lock"
        first = host_files.open_regular(path, os.O_CREAT | os.O_RDWR)
        second = host_files.open_regular(path, os.O_RDWR)
        try:
            host_files.lock_file(first, blocking=False)
            release = threading.Timer(.04, host_files.unlock_file, args=(first,))
            release.start()
            try:
                host_files.lock_file_bounded(second, timeout=.5)
            finally:
                release.join(1)
            with self.assertRaises(BlockingIOError):
                host_files.lock_file(first, blocking=False)
        finally:
            os.close(second)
            os.close(first)

    def test_bounded_lock_expires_without_releasing_holder(self):
        path = self.root / "held.lock"
        first = host_files.open_regular(path, os.O_CREAT | os.O_RDWR)
        second = host_files.open_regular(path, os.O_RDWR)
        try:
            host_files.lock_file(first, blocking=False)
            started = time.monotonic()
            with self.assertRaises(BlockingIOError):
                host_files.lock_file_bounded(second, timeout=.04)
            self.assertLess(time.monotonic() - started, .4)
            with self.assertRaises(BlockingIOError):
                host_files.lock_file(second, blocking=False)
        finally:
            os.close(second)
            os.close(first)

    def test_bounded_lock_only_retries_contention(self):
        with patch.object(host_files, "lock_file", side_effect=PermissionError("denied")) as lock, \
                patch.object(host_files, "_lock_sleep") as sleep:
            with self.assertRaises(PermissionError):
                host_files.lock_file_bounded(123, timeout=.5, exclusive=False)
            lock.assert_called_once_with(123, exclusive=False, blocking=False)
            sleep.assert_not_called()
        with patch.object(host_files, "lock_file", side_effect=BlockingIOError) as lock:
            with self.assertRaises(BlockingIOError):
                host_files.lock_file_bounded(123, timeout=0)
            self.assertEqual(lock.call_count, 1)
        for invalid in (-1, True, None, float("nan"), float("inf")):
            with self.subTest(timeout=invalid), self.assertRaises(ValueError):
                host_files.lock_file_bounded(123, timeout=invalid)

    def test_bounded_lock_does_not_acquire_after_suspension_past_deadline(self):
        with patch.object(host_files, "_lock_monotonic", side_effect=[1, 1.01, 2]), \
                patch.object(host_files, "_lock_sleep") as sleep, \
                patch.object(host_files, "lock_file", side_effect=[BlockingIOError(), None]) as lock:
            with self.assertRaises(BlockingIOError):
                host_files.lock_file_bounded(123, timeout=.5)
            self.assertEqual(lock.call_count, 1)
            sleep.assert_called_once_with(.01)

    def test_regular_round_trip_and_directory_rejected(self):
        path = self.root / "file"
        fd = host_files.open_regular(path, os.O_CREAT | os.O_EXCL | os.O_RDWR)
        os.write(fd, b"bytes")
        os.close(fd)
        fd = host_files.open_regular(path)
        self.assertEqual(os.read(fd, 10), b"bytes")
        os.close(fd)
        with self.assertRaises((ValueError, OSError)):
            host_files.open_regular(self.root)

    def test_hardlink_refused_before_truncation(self):
        source, alias = self.root / "source", self.root / "alias"
        source.write_bytes(b"must survive")
        os.link(source, alias)
        with self.assertRaises(ValueError):
            host_files.open_regular(source, os.O_WRONLY | os.O_TRUNC)
        self.assertEqual(source.read_bytes(), b"must survive")
        with self.assertRaises(ValueError):
            host_files.atomic_write(alias, b"replacement")
        self.assertEqual(source.read_bytes(), b"must survive")

    def test_symlink_refused_before_truncation(self):
        source, alias = self.root / "source", self.root / "alias"
        source.write_bytes(b"must survive")
        try:
            alias.symlink_to(source)
        except OSError as error:
            if os.name == "nt" and error.winerror == 1314:
                self.skipTest("Windows symlink privilege unavailable; junction test covers reparse")
            raise
        with self.assertRaises((ValueError, OSError)):
            host_files.open_regular(alias, os.O_WRONLY | os.O_TRUNC)
        self.assertEqual(source.read_bytes(), b"must survive")

    def test_private_directory_idempotent(self):
        directory = self.root / "private"
        host_files.ensure_private_directory(directory)
        host_files.ensure_private_directory(directory)
        host_files.check_private_directory(directory)
        fd = host_files.open_directory(directory)
        self.assertTrue(stat.S_ISDIR(os.fstat(fd).st_mode))
        os.close(fd)

    def test_private_write_is_complete_and_reopenable(self):
        directory = self.root / "private"
        host_files.ensure_private_directory(directory)
        path = directory / "state.json"
        host_files.atomic_write(path, b'{"first": true}')
        host_files.check_private(path)
        host_files.atomic_write(path, b'{"second": true}')
        self.assertEqual(path.read_bytes(), b'{"second": true}')
        host_files.check_private(path)
        self.assertEqual([p.name for p in directory.iterdir()], ["state.json"])

    def test_atomic_write_failure_preserves_previous_and_cleans_stage(self):
        path = self.root / "state"
        host_files.atomic_write(path, b"old")
        with patch.object(host_files.os, "fsync", side_effect=OSError("disk failure")):
            with self.assertRaises(OSError):
                host_files.atomic_write(path, b"new")
        self.assertEqual(path.read_bytes(), b"old")
        self.assertEqual([p.name for p in self.root.iterdir()], ["state"])

    @unittest.skipIf(os.name == "nt", "retained Unix directory modes")
    def test_state_directory_preserves_retained_0755_without_chmod(self):
        directory = self.root / "retained"
        directory.mkdir(mode=0o755)
        directory.chmod(0o755)
        with patch.object(host_files.os, "fchmod", side_effect=AssertionError("must not chmod retained state")):
            host_files.ensure_state_directory(directory)
        self.assertEqual(stat.S_IMODE(directory.stat().st_mode), 0o755)
        with self.assertRaises(FileExistsError):
            host_files.ensure_state_directory(directory, exist_ok=False)

    def test_fresh_state_directory_is_private(self):
        directory = self.root / "state" / "reader"
        host_files.ensure_state_directory(directory)
        host_files.check_private_directory(directory)
        host_files.check_private_directory(directory.parent)

    @unittest.skipIf(os.name == "nt", "Unix directory write permissions")
    def test_state_directory_does_not_admit_untrusted_write(self):
        directory = self.root / "untrusted"
        directory.mkdir(mode=0o777)
        directory.chmod(0o777)
        with self.assertRaises(PermissionError):
            host_files.ensure_state_directory(directory)
        self.assertEqual(stat.S_IMODE(directory.stat().st_mode), 0o777)

    def test_private_directory_missing_parents_are_private_too(self):
        directory = self.root / "first" / "second" / "third"
        host_files.ensure_private_directory(directory)
        for path in (directory, directory.parent, directory.parent.parent):
            host_files.check_private_directory(path)
        with self.assertRaises(FileExistsError):
            host_files.ensure_private_directory(directory, exist_ok=False)

    def test_handle_private_check_is_identity_bound(self):
        path = self.root / "private"
        host_files.atomic_write(path, b"private")
        fd = host_files.open_regular(path)
        try:
            host_files.check_private_fd(fd)
        finally:
            os.close(fd)

    def test_replace_existing_regular_stage_and_flush_immutable_bytes(self):
        target, source = self.root / "target", self.root / "source"
        host_files.atomic_write(target, b"old")
        host_files.atomic_write(source, b"new")
        identity = target.stat().st_ino
        host_files.sync_file(target)
        self.assertEqual(target.stat().st_ino, identity)
        self.assertEqual(target.read_bytes(), b"old")
        host_files.replace_file(source, target)
        self.assertFalse(source.exists())
        self.assertEqual(target.read_bytes(), b"new")
        host_files.check_private(target)

    def test_rename_no_replace_refuses_even_empty_directory(self):
        source, target = self.root / "source", self.root / "target"
        host_files.ensure_private_directory(source)
        host_files.ensure_private_directory(target)
        with self.assertRaises(FileExistsError):
            host_files.rename_no_replace(source, target)
        self.assertTrue(source.is_dir())
        self.assertTrue(target.is_dir())
        target.rmdir()
        host_files.atomic_write(source / "bytes", b"payload")
        host_files.rename_no_replace(source, target)
        self.assertFalse(source.exists())
        self.assertEqual((target / "bytes").read_bytes(), b"payload")

    def test_windows_stage_collision_never_deletes_unowned_file(self):
        import secrets
        collision = self.root / ".mneme-publish-fixed"
        collision.write_bytes(b"unrelated evidence")
        api = Mock()
        api.parents.side_effect = lambda path: contextlib.nullcontext(path)
        with patch.object(host_files, "WINDOWS", True), patch.object(host_files, "_windows", return_value=api), patch.object(secrets, "token_hex", return_value="fixed"), patch.object(host_files, "open_regular", side_effect=[FileNotFoundError(), FileExistsError()]):
            with self.assertRaises(FileExistsError):
                host_files.atomic_write(self.root / "state", b"private")
        self.assertEqual(collision.read_bytes(), b"unrelated evidence")

    def test_windows_create_verifies_resulting_acl_before_exposing_fd(self):
        api = Mock()
        api.parents.side_effect = lambda path: contextlib.nullcontext(path)
        api.open_handle.return_value = (123, Mock(links=1))
        api.check_private.side_effect = PermissionError("filesystem ignores ACL")
        msvcrt = Mock()
        with patch.object(host_files, "WINDOWS", True), patch.object(host_files, "_windows", return_value=api), patch.dict(sys.modules, {"msvcrt": msvcrt}):
            with self.assertRaises(PermissionError):
                host_files.open_regular(self.root / "private", os.O_WRONLY | os.O_CREAT | os.O_EXCL)
        api.check_private.assert_called_once_with(123)
        msvcrt.open_osfhandle.assert_not_called()
        api.kernel.CloseHandle.assert_called_once_with(123)

    def test_windows_security_descriptor_explicitly_sets_user_owner(self):
        api = object.__new__(host_files._WindowsFiles)
        api.user_sid = "S-1-5-21-123-456-789-1000"
        api.kernel, api.security = Mock(), Mock()
        def convert(sddl, revision, output, size):
            output._obj.value = 123
            return True
        api.security.ConvertStringSecurityDescriptorToSecurityDescriptorW.side_effect = convert
        with api.private_security(True):
            pass
        sddl = api.security.ConvertStringSecurityDescriptorToSecurityDescriptorW.call_args.args[0]
        self.assertTrue(sddl.startswith("O:S-1-5-21-123-456-789-1000D:P"))
        self.assertIn("(A;OICI;FA;;;S-1-5-21-123-456-789-1000)", sddl)
        api.kernel.LocalFree.assert_called_once()

    def test_exclusive_lock_contends_and_releases(self):
        path = self.root / "lock"
        first = host_files.open_regular(path, os.O_CREAT | os.O_RDWR)
        second = host_files.open_regular(path, os.O_RDWR)
        try:
            host_files.lock_file(first, blocking=False)
            with self.assertRaises(BlockingIOError):
                host_files.lock_file(second, blocking=False)
            host_files.unlock_file(first)
            host_files.lock_file(second, blocking=False)
            host_files.unlock_file(second)
        finally:
            os.close(first)
            os.close(second)

    def test_shared_lock_allows_reader_not_writer(self):
        path = self.root / "lock"
        first = host_files.open_regular(path, os.O_CREAT | os.O_RDWR)
        second = host_files.open_regular(path, os.O_RDWR)
        third = host_files.open_regular(path, os.O_RDWR)
        try:
            host_files.lock_file(first, exclusive=False)
            host_files.lock_file(second, exclusive=False, blocking=False)
            with self.assertRaises(BlockingIOError):
                host_files.lock_file(third, blocking=False)
            host_files.unlock_file(first)
            host_files.unlock_file(second)
            host_files.lock_file(third, blocking=False)
        finally:
            for fd in (first, second, third):
                os.close(fd)

    def test_lock_contention_between_processes(self):
        path = self.root / "lock"
        fd = host_files.open_regular(path, os.O_CREAT | os.O_RDWR)
        try:
            host_files.lock_file(fd)
            script = """import host_files, os, sys
fd=host_files.open_regular(sys.argv[1], os.O_RDWR)
try:
    host_files.lock_file(fd, blocking=False)
except BlockingIOError:
    sys.exit(23)
finally:
    os.close(fd)
"""
            result = subprocess.run([sys.executable, "-c", script, str(path)],
                cwd=Path(__file__).parent, timeout=5, capture_output=True)
            self.assertEqual(result.returncode, 23, result.stderr.decode(errors="replace"))
        finally:
            os.close(fd)

    @unittest.skipIf(os.name == "nt", "Unix mode contract")
    def test_existing_unix_public_directory_is_refused_not_chmodded(self):
        directory = self.root / "public"
        directory.mkdir(mode=0o755)
        directory.chmod(0o755)
        with self.assertRaises(PermissionError):
            host_files.ensure_private_directory(directory)
        self.assertEqual(stat.S_IMODE(directory.stat().st_mode), 0o755)
        host_files.set_private(directory, directory=True)
        host_files.check_private_directory(directory)

    @unittest.skipIf(os.name == "nt", "Unix FIFO boundary")
    def test_fifo_is_refused_without_blocking(self):
        fifo = self.root / "fifo"
        os.mkfifo(fifo)
        with self.assertRaises(ValueError):
            host_files.open_regular(fifo)

    def test_windows_directory_sync_is_not_silent_success(self):
        with patch.object(host_files, "WINDOWS", True):
            with self.assertRaises(NotImplementedError):
                host_files.sync_directory(self.root)

    @unittest.skipUnless(os.name == "nt", "Native Windows ACL contract")
    def test_windows_broad_dacl_rejected_then_explicitly_narrowed(self):
        path = self.root / "state"
        host_files.atomic_write(path, b"test")
        # Preserve explicit TokenUser ownership even in an elevated CI process,
        # while deliberately broadening its DACL for the refusal assertion.
        api = host_files._windows()
        descriptor = ctypes.c_void_p()
        sddl = f"D:P(A;;FA;;;{api.user_sid})(A;;FR;;;WD)"
        self.assertTrue(api.security.ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl, 1, ctypes.byref(descriptor), None))
        try:
            present, defaulted, dacl = ctypes.c_int(), ctypes.c_int(), ctypes.c_void_p()
            self.assertTrue(api.security.GetSecurityDescriptorDacl(descriptor,
                ctypes.byref(present), ctypes.byref(dacl), ctypes.byref(defaulted)))
            handle, _ = api.open_handle(path, access=0x60000 | 0x80)
            try:
                self.assertEqual(api.security.SetSecurityInfo(handle, 1, 0x80000004,
                    None, None, dacl, None), 0)
            finally:
                api.kernel.CloseHandle(handle)
        finally:
            api.kernel.LocalFree(descriptor)
        with self.assertRaises(PermissionError):
            host_files.check_private(path)
        host_files.set_private(path)
        host_files.check_private(path)

    @unittest.skipUnless(os.name == "nt", "Native Windows junction contract")
    def test_windows_junction_component_refused(self):
        target, alias = self.root / "target", self.root / "alias"
        target.mkdir()
        (target / "state").write_bytes(b"untouched")
        result = subprocess.run(["cmd.exe", "/d", "/c", "mklink", "/J", str(alias), str(target)],
                                timeout=5, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        try:
            with self.assertRaises(ValueError):
                host_files.open_regular(alias / "state", os.O_WRONLY | os.O_TRUNC)
            self.assertEqual((target / "state").read_bytes(), b"untouched")
        finally:
            os.rmdir(alias)


if __name__ == "__main__":
    unittest.main()

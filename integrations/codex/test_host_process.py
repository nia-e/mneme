"""Portable component tests plus native-Windows-only process contracts.

Run: python -m unittest discover -s integrations/codex -p test_host_process.py -v
The Windows cases exercise real kernel32 calls on Windows; mocked cases are not
substitutes for those CI checks.
"""

import ctypes
import os
import signal
import subprocess
import sys
import time
import unittest
from unittest.mock import Mock, patch

import host_process as host


class PipeTests(unittest.TestCase):
    def child(self, source, **options):
        process = host.spawn([sys.executable, "-u", "-c", source],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL, bufsize=0, **options)
        self.addCleanup(self.cleanup_child, process)
        return process

    @staticmethod
    def cleanup_child(process):
        host.terminate_tree(process)
        for stream in (process.stdin, process.stdout, process.stderr):
            if stream is not None:
                stream.close()

    def test_binary_echo_and_eof(self):
        process = self.child("import sys; data=sys.stdin.buffer.read(5); sys.stdout.buffer.write(data)")
        pipe = host.BoundedPipe(process, max_read_bytes=32)
        self.addCleanup(pipe.close)
        pipe.write(b"\0\xffabc", time.monotonic() + 5)
        value = bytearray()
        while len(value) < 5:
            value.extend(pipe.read(5 - len(value), time.monotonic() + 5))
        self.assertEqual(value, b"\0\xffabc")
        self.assertEqual(pipe.read(32, time.monotonic() + 5), b"")
        pipe.close()
        pipe.close()
        with self.assertRaises(ValueError):
            pipe.read(1, time.monotonic() + 1)

    def test_large_binary_transfer_exceeds_pipe_capacity_without_losing_bytes(self):
        payload = bytes(range(256)) * 1024
        process = self.child(
            "import sys; data=sys.stdin.buffer.read(%d); sys.stdout.buffer.write(data)" % len(payload))
        pipe = host.BoundedPipe(process)
        deadline = time.monotonic() + 10
        pipe.write(payload, deadline)
        received = bytearray()
        while len(received) < len(payload):
            chunk = pipe.read(min(8192, len(payload) - len(received)), deadline)
            self.assertTrue(chunk, "child exited before completing its response")
            received.extend(chunk)
        self.assertEqual(received, payload)

    def test_quiet_reader_deadline(self):
        process = self.child("import time; time.sleep(30)")
        pipe = host.BoundedPipe(process)
        started = time.monotonic()
        with self.assertRaises(TimeoutError):
            pipe.read(8192, started + 0.12)
        self.assertLess(time.monotonic() - started, 2)

    def test_full_input_pipe_deadline(self):
        process = self.child("import time; time.sleep(30)")
        pipe = host.BoundedPipe(process)
        payload = b"x" * (8 * 1024 * 1024)
        started = time.monotonic()
        with self.assertRaises(TimeoutError):
            pipe.write(payload, started + 0.15)
        self.assertLess(time.monotonic() - started, 2)

    def test_per_read_bound_checked_before_allocating_or_reading(self):
        process = self.child("import sys; sys.stdout.write('hello')")
        pipe = host.BoundedPipe(process, max_read_bytes=4)
        for size in (0, -1, True, 5, 1 << 40):
            with self.subTest(size=size), self.assertRaises(ValueError):
                pipe.read(size, time.monotonic() + 5)
        self.assertEqual(pipe.read(4, time.monotonic() + 5), b"hell")
        self.assertEqual(pipe.read(4, time.monotonic() + 5), b"o")

    def test_deadlines_cannot_be_unbounded(self):
        process = self.child("import time; time.sleep(30)")
        pipe = host.BoundedPipe(process)
        for deadline in (float("inf"), float("nan"), None):
            with self.subTest(deadline=deadline), self.assertRaises(ValueError):
                pipe.read(1, deadline)
        with self.assertRaises(TimeoutError):
            pipe.write(b"x", time.monotonic() - 1)

    def test_tree_termination_closes_descendant_stdout_after_leader_exit(self):
        process = self.child(
            "import subprocess, sys; "
            "subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)']); "
            "sys.stdout.write('ready'); sys.stdout.flush()", tree=True)
        pipe = host.BoundedPipe(process)
        self.assertEqual(pipe.read(5, time.monotonic() + 5), b"ready")
        process.wait(timeout=5)
        host.terminate_tree(process)
        self.assertEqual(pipe.read(1, time.monotonic() + 5), b"")

    def test_invalid_pipe_construction(self):
        with self.assertRaises(ValueError):
            host.BoundedPipe(Mock(stdin=None, stdout=None))
        for bound in (0, -1, True):
            with self.assertRaises(ValueError):
                host.BoundedPipe(Mock(), max_read_bytes=bound)
        with self.assertRaises(ValueError):
            host.BoundedPipe(Mock(text_mode=True))


class UnixProcessTests(unittest.TestCase):
    @unittest.skipIf(host.IS_WINDOWS, "Unix session contract")
    def test_tree_gets_own_session(self):
        process = host.spawn([sys.executable, "-c", "import time; time.sleep(30)"], tree=True)
        try:
            self.assertEqual(os.getsid(process.pid), process.pid)
            self.assertTrue(host.process_alive(process.pid))
            identity = host.process_identity(process.pid)
            self.assertEqual(identity["pid"], process.pid)
        finally:
            host.terminate_tree(process)
        self.assertFalse(host.process_alive(process.pid))

    @unittest.skipIf(host.IS_WINDOWS, "Unix signal contract")
    def test_unix_permission_error_is_unknown_not_absent(self):
        with patch.object(host.os, "kill", side_effect=PermissionError):
            self.assertIsNone(host.process_alive(123))
        with patch.object(host.os, "kill", side_effect=ProcessLookupError):
            self.assertFalse(host.process_alive(123))

    @unittest.skipIf(host.IS_WINDOWS, "Darwin exited-group EPERM contract")
    def test_exited_group_permission_error_reaps_then_retries(self):
        process = Mock(pid=123)
        process.poll.return_value = 0
        with patch.object(host.os, "killpg", side_effect=[PermissionError(), ProcessLookupError()]) as killpg:
            host._signal_owned_group(process, signal.SIGTERM)
        self.assertEqual(killpg.call_count, 2)
        process.poll.return_value = None
        with patch.object(host.os, "killpg", side_effect=PermissionError()) as killpg:
            with self.assertRaises(PermissionError):
                host._signal_owned_group(process, signal.SIGTERM)
        self.assertEqual(killpg.call_count, 1)
        process.poll.return_value = 0
        with patch.object(host.os, "killpg", side_effect=PermissionError()):
            with self.assertRaises(PermissionError):
                host._signal_owned_group(process, signal.SIGTERM)

    @unittest.skipIf(host.IS_WINDOWS, "Darwin exited-descendant EPERM contract")
    def test_exited_descendant_reap_race_retries_but_preserves_live_permissions(self):
        process = Mock(pid=123)
        process.poll.return_value = 0  # Leader already reaped; descendant is exiting.
        with patch.object(host.os, "killpg", side_effect=[PermissionError(), PermissionError(), ProcessLookupError()]) as killpg, \
                patch.object(host.time, "sleep"):
            host._signal_owned_group(process, signal.SIGKILL)
        self.assertEqual(killpg.call_count, 3)
        with patch.object(host.os, "killpg", side_effect=PermissionError()) as killpg, \
                patch.object(host.time, "monotonic", side_effect=[0, 0, 0.04, 0.08, 0.12]), \
                patch.object(host.time, "sleep"):
            with self.assertRaises(PermissionError):
                host._signal_owned_group(process, signal.SIGKILL)
        self.assertEqual(killpg.call_count, 4)

    def test_invalid_pids_never_signal(self):
        with patch.object(host.os, "kill") as kill:
            for pid in (0, -1, True, "123", 0x100000000):
                self.assertFalse(host.process_alive(pid))
                self.assertIsNone(host.process_identity(pid))
            kill.assert_not_called()

    def test_shell_is_not_an_escape_hatch(self):
        with self.assertRaises(ValueError):
            host.spawn("echo hello", shell=True)


class MockedWindowsContracts(unittest.TestCase):
    def setUp(self):
        self.api = Mock()
        self.windows = patch.object(host, "IS_WINDOWS", True)
        self.windows.start()
        self.addCleanup(self.windows.stop)
        self.api_patch = patch.object(host, "_win", return_value=self.api)
        self.api_patch.start()
        self.addCleanup(self.api_patch.stop)

    def pipe(self):
        process = Mock(text_mode=False)
        process.stdin.fileno.return_value = 1
        process.stdout.fileno.return_value = 2
        self.api.pipe_handle.side_effect = lambda fd: fd + 100
        return host.BoundedPipe(process, max_read_bytes=1024)

    def test_windows_io_uses_native_nonblocking_never_selectors(self):
        with patch.object(host.selectors, "DefaultSelector", side_effect=AssertionError("not pipes")):
            pipe = self.pipe()
            self.api.nonblocking.assert_any_call(102)
            self.api.nonblocking.assert_any_call(101)
            self.api.read_pipe.side_effect = [None, b"ok"]
            self.assertEqual(pipe.read(32, time.monotonic() + 1), b"ok")
            self.api.write_pipe.side_effect = [0, 1, 2]
            pipe.write(b"abc", time.monotonic() + 1)
            chunks = [bytes(call.args[1]) for call in self.api.write_pipe.call_args_list]
            self.assertEqual(chunks, [b"abc", b"a", b"bc"])

    def test_windows_io_never_falls_back_to_blocking_on_mode_error(self):
        self.api.nonblocking.side_effect = OSError("unsupported pipe")
        with self.assertRaises(OSError):
            self.pipe()
        self.api.read_pipe.assert_not_called()
        self.api.write_pipe.assert_not_called()

    def test_zero_progress_windows_pipe_is_bounded(self):
        pipe = self.pipe()
        self.api.write_pipe.return_value = 0
        with self.assertRaises(TimeoutError):
            pipe.write(b"x", time.monotonic() + 0.03)
        self.assertLess(self.api.write_pipe.call_count, 10)
        self.api.read_pipe.return_value = None
        with self.assertRaises(TimeoutError):
            pipe.read(1, time.monotonic() + 0.03)
        self.assertLess(self.api.read_pipe.call_count, 10)

    def test_alive_is_read_only_and_tristate(self):
        with patch.object(host.os, "kill", side_effect=AssertionError("Windows kill(0) is not a probe")):
            self.api.open_process.return_value = 101
            self.api.running.return_value = True
            self.assertTrue(host.process_alive(123))
            self.api.close.assert_called_with(101)
            self.api.running.return_value = False
            self.assertFalse(host.process_alive(123))
            self.api.open_process.return_value = None
            self.assertFalse(host.process_alive(123))
            self.api.open_process.side_effect = PermissionError()
            self.assertIsNone(host.process_alive(123))

    def test_verified_termination_uses_the_same_handle_and_rejects_reused_pid(self):
        original = {"platform": "windows", "pid": 123, "created": 4, "executable": "C:\\owner.exe"}
        self.api.open_process.return_value = 111
        self.api.identity.return_value = dict(original, created=5)
        self.assertFalse(host.terminate_verified(123, original))
        self.api.terminate.assert_not_called()
        self.api.identity.return_value = original
        self.assertTrue(host.terminate_verified(123, original))
        self.api.open_process.assert_called_with(123, terminate=True)
        self.api.identity.assert_called_with(111, 123)
        self.api.terminate.assert_called_once_with(111)
        self.api.close.assert_called_with(111)

    def test_spawned_identity_uses_original_handle_not_reopened_pid(self):
        process = Mock(pid=123, _handle=111)
        expected = {"platform": "windows", "pid": 123, "created": 4, "executable": "owner.exe"}
        self.api.identity.return_value = expected
        self.assertEqual(host.spawned_identity(process), expected)
        self.api.identity.assert_called_once_with(111, 123)
        self.api.open_process.assert_not_called()
        self.api.close.assert_not_called()  # Popen owns this handle.

    def test_unknown_identity_does_not_authorize_termination(self):
        self.api.open_process.side_effect = PermissionError()
        with self.assertRaises(OSError):
            host.terminate_verified(123, {"pid": 123})
        self.api.terminate.assert_not_called()
        self.assertIsNone(host.process_identity(123))

    def test_spawn_assigns_job_before_resuming_and_closes_it_on_cleanup(self):
        order = []
        process = Mock()
        process.poll.return_value = 0
        process.wait.return_value = 0
        self.api.create_job.return_value = 91
        self.api.assign_job.side_effect = lambda *args: order.append("assign")
        self.api.resume_primary.side_effect = lambda *args: order.append("resume")
        with patch.object(host.subprocess, "Popen", side_effect=lambda *a, **k: (order.append("spawn"), process)[1]) as popen:
            self.assertIs(host.spawn(["program"], tree=True), process)
        self.assertEqual(order, ["spawn", "assign", "resume"])
        flags = popen.call_args.kwargs["creationflags"]
        self.assertTrue(flags & 4)  # CREATE_SUSPENDED, not post-execution assignment.
        self.assertTrue(popen.call_args.kwargs["close_fds"])
        host.terminate_tree(process)
        self.api.terminate_job.assert_called_once_with(91)
        self.api.close.assert_called_once_with(91)
        self.assertIsNone(process._mneme_job)

    def test_job_assignment_failure_kills_suspended_child_without_resume(self):
        process = Mock(spec=subprocess.Popen)
        process.stdin = process.stdout = process.stderr = None
        self.api.create_job.return_value = 91
        self.api.assign_job.side_effect = OSError("nested job refused")
        with patch.object(host.subprocess, "Popen", return_value=process):
            with self.assertRaises(OSError):
                host.spawn(["program"], tree=True)
        self.api.resume_primary.assert_not_called()
        process.kill.assert_called_once()
        process.wait.assert_called_once_with(timeout=2)
        self.api.close.assert_called_once_with(91)

    def test_spawn_cleanup_closes_job_even_when_child_wait_fails(self):
        process = Mock(spec=subprocess.Popen)
        process.stdin = process.stdout = process.stderr = None
        process.wait.side_effect = subprocess.TimeoutExpired("program", 2)
        self.api.create_job.return_value = 91
        self.api.assign_job.side_effect = OSError("nested job refused")
        with patch.object(host.subprocess, "Popen", return_value=process):
            with self.assertRaises(subprocess.TimeoutExpired):
                host.spawn(["program"], tree=True)
        self.api.close.assert_called_once_with(91)
        self.api.resume_primary.assert_not_called()

    def test_detached_process_is_not_put_in_launcher_owned_job(self):
        with patch.object(host.subprocess, "Popen") as popen:
            host.spawn(["program"], detached=True, stdin=subprocess.DEVNULL)
        self.assertEqual(popen.call_args.kwargs["creationflags"] & 0x208, 0x208)
        self.api.create_job.assert_not_called()


class Win32BoundaryTests(unittest.TestCase):
    """Exercise native error-code interpretation even on non-Windows CI."""

    def setUp(self):
        self.api = object.__new__(host._WindowsApi)
        self.api.kernel = Mock()
        self.api.error = Mock(return_value=OSError("native refusal"))

    def test_pipe_zero_data_and_eof_are_distinct(self):
        self.api.kernel.ReadFile.return_value = False
        with patch.object(ctypes, "get_last_error", return_value=232, create=True):
            self.assertIsNone(self.api.read_pipe(1, 2))
        for code in (109, 233):
            with patch.object(ctypes, "get_last_error", return_value=code, create=True):
                self.assertEqual(self.api.read_pipe(1, 2), b"")
        with patch.object(ctypes, "get_last_error", return_value=5, create=True):
            with self.assertRaises(OSError):
                self.api.read_pipe(1, 2)

    def test_closed_write_is_not_confused_with_backpressure(self):
        self.api.kernel.WriteFile.return_value = False
        with self.assertRaises(OSError):
            self.api.write_pipe(1, b"x")
        self.api.kernel.WriteFile.return_value = True
        self.assertEqual(self.api.write_pipe(1, b"x"), 0)

    def test_liveness_requests_only_query_and_wait_rights(self):
        self.api.kernel.OpenProcess.return_value = 321
        self.assertEqual(self.api.open_process(123), 321)
        self.api.kernel.OpenProcess.assert_called_with(0x101000, False, 123)
        self.api.open_process(123, terminate=True)
        self.api.kernel.OpenProcess.assert_called_with(0x101001, False, 123)

    def test_access_denied_is_not_reported_absent(self):
        self.api.kernel.OpenProcess.return_value = None
        with patch.object(ctypes, "get_last_error", return_value=87, create=True):
            self.assertIsNone(self.api.open_process(123))
        with patch.object(ctypes, "get_last_error", return_value=5, create=True):
            with self.assertRaises(OSError):
                self.api.open_process(123)

    def test_job_handle_is_not_inherited_and_kills_on_close(self):
        self.api.kernel.CreateJobObjectW.return_value = 44
        self.api.kernel.SetInformationJobObject.return_value = True
        self.assertEqual(self.api.create_job(), 44)
        self.api.kernel.CreateJobObjectW.assert_called_once_with(None, None)
        args = self.api.kernel.SetInformationJobObject.call_args.args
        self.assertEqual(args[0:2], (44, 9))
        self.assertEqual(args[2]._obj.BasicLimitInformation.LimitFlags, 0x2000)



class BootWitnessTests(unittest.TestCase):
    def setUp(self):
        host.boot_identity.cache_clear()
        self.addCleanup(host.boot_identity.cache_clear)
        windows = patch.object(host, "IS_WINDOWS", True)
        windows.start()
        self.addCleanup(windows.stop)
        root = patch.dict(os.environ, {"SystemRoot": os.path.abspath("windows-test-root")})
        root.start()
        self.addCleanup(root.stop)

    def test_boot_timestamp_is_exact_and_cached_without_clock_estimate(self):
        stamp = b"2026-10-11T12:34:56.1234567Z"
        process = Mock()
        process.wait.return_value = 0
        pipe = Mock()
        pipe.read.side_effect = [stamp, b""]
        with patch.object(host, "spawn", return_value=process) as spawn, \
                patch.object(host, "BoundedPipe", return_value=pipe), \
                patch.object(host, "terminate_tree") as cleanup, \
                patch.object(host.time, "time", side_effect=AssertionError("not a wall-minus-uptime estimate")):
            self.assertEqual(host.boot_identity(), "windows-cim:" + stamp.decode("ascii"))
            self.assertEqual(host.boot_identity(), "windows-cim:" + stamp.decode("ascii"))
        spawn.assert_called_once()
        self.assertIn("Get-CimInstance", spawn.call_args.args[0][-1])
        self.assertTrue(spawn.call_args.kwargs["tree"])
        self.assertLessEqual(process.wait.call_args.kwargs["timeout"], 2)
        pipe.close.assert_called_once()
        cleanup.assert_called_once_with(process, timeout=0.25)

    def test_unknown_boot_is_none_not_a_shared_epoch(self):
        with patch.object(host, "spawn", side_effect=FileNotFoundError):
            self.assertIsNone(host.boot_identity())

    def test_boot_query_rejects_excess_output_and_timeout(self):
        for result in (b"x" * 129, TimeoutError()):
            host.boot_identity.cache_clear()
            process, pipe = Mock(), Mock()
            if isinstance(result, Exception):
                pipe.read.side_effect = result
            else:
                pipe.read.return_value = result
            with patch.object(host, "spawn", return_value=process), \
                    patch.object(host, "BoundedPipe", return_value=pipe), \
                    patch.object(host, "terminate_tree") as cleanup:
                self.assertIsNone(host.boot_identity())
            pipe.close.assert_called_once()
            cleanup.assert_called_once()

@unittest.skipUnless(host.IS_WINDOWS, "requires native Windows kernel32")
class NativeWindowsTests(unittest.TestCase):
    child = PipeTests.child
    cleanup_child = staticmethod(PipeTests.cleanup_child)

    def test_current_process_liveness_probe_is_not_destructive(self):
        self.assertTrue(host.process_alive(os.getpid()))
        identity = host.process_identity(os.getpid())
        self.assertEqual(identity["pid"], os.getpid())
        self.assertEqual(identity["platform"], "windows")
        self.assertIsInstance(identity["created"], int)
        self.assertGreater(identity["created"], 0)
        # Windows virtualenv's sys.executable can name its redirector rather
        # than the actual base-Python image. Obtain that image independently.
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        get_image = kernel.GetModuleFileNameW
        get_image.argtypes = [ctypes.c_void_p, ctypes.c_wchar_p, ctypes.c_uint32]
        get_image.restype = ctypes.c_uint32
        image = ctypes.create_unicode_buffer(32768)
        length = get_image(None, image, len(image))
        self.assertGreater(length, 0)
        self.assertLess(length, len(image))
        self.assertEqual(os.path.normcase(identity["executable"]), os.path.normcase(image.value))

    def test_native_boot_witness_is_stable_or_explicitly_unavailable(self):
        host.boot_identity.cache_clear()
        first = host.boot_identity()
        host.boot_identity.cache_clear()
        second = host.boot_identity()
        if first is None or second is None:
            self.skipTest("Windows CIM boot witness unavailable within finite budget")
        self.assertEqual(first, second)
        self.assertTrue(first.startswith("windows-cim:"))

    def test_native_identity_mismatch_does_not_terminate_child(self):
        process = self.child("import time; time.sleep(30)")
        identity = host.process_identity(process.pid)
        self.assertIsNotNone(identity)
        self.assertFalse(host.terminate_verified(process.pid, dict(identity, created=identity["created"] + 1)))
        self.assertIsNone(process.poll())
        self.assertTrue(host.terminate_verified(process.pid, identity))
        process.wait(timeout=5)
        self.assertFalse(host.process_alive(process.pid))

    def test_native_tree_cleanup_terminates_grandchild(self):
        process = self.child(
            "import subprocess, sys, time; "
            "child=subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)']); "
            "print(child.pid, flush=True); time.sleep(30)", tree=True)
        pipe = host.BoundedPipe(process)
        raw = b""
        deadline = time.monotonic() + 5
        while b"\n" not in raw:
            raw += pipe.read(32, deadline)
        descendant = int(raw.strip())
        self.assertTrue(host.process_alive(descendant))
        host.terminate_tree(process)
        until = time.monotonic() + 5
        while host.process_alive(descendant) is True and time.monotonic() < until:
            time.sleep(0.01)
        self.assertFalse(host.process_alive(descendant))


if __name__ == "__main__":
    unittest.main()

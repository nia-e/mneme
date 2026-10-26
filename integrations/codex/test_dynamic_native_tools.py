"""Executable replacement preserves semantic selection, never operation authority."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from host_platform import python_hook_argv
from registry_recording import config_digest
from store_registry import NativeCommandError, NativeSelection, native_select, native_bridge


class DynamicNativeToolsTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        self.log = self.root / 'launches'
        self.snapshot = {'schema': 'mneme.config-selection.v1', 'digest': 'a' * 64,
                         'cwd': str(self.root), 'config': {}, 'active': []}
        self.config = str(self.root / 'memory.toml')

    def executable(self, directory, edition, snapshot=None, code=0):
        directory.mkdir(exist_ok=True)
        path = directory / 'mnemed'
        path.write_text('#!' + sys.executable + '\nimport sys\n'
            + f'open({str(self.log)!r}, "a").write({edition!r} + "\\n")\n'
            + f'print({json.dumps(self.snapshot if snapshot is None else snapshot)!r})\n'
            + f'sys.exit({code})\n')
        path.chmod(0o700)
        return path

    def test_path_replacement_replays_same_owner_binding_without_binary_pin(self):
        first = self.executable(self.root / 'first', 'first')
        second = self.executable(self.root / 'second', 'second')
        with patch.dict(os.environ, {'PATH': str(first.parent)}):
            selected = native_select(self.config, 'mnemed', self.root)
        frozen = selected.to_dict()
        config = {'frozen_selection': frozen, 'project_root': str(self.root),
                  'schema': 'mneme.codex-hooks.config.v13'}
        with patch.dict(os.environ, {'PATH': str(first.parent)}):
            digest = config_digest(config)
        with patch.dict(os.environ, {'PATH': str(second.parent)}):
            current = NativeSelection.from_dict(frozen)
            self.assertEqual(config_digest(config), digest)
        self.assertEqual(current.to_dict(), frozen)
        self.assertIsNone(frozen['binary_pin'])
        self.assertEqual(self.log.read_text().splitlines(), ['first', 'first', 'second', 'second'])

    def test_absolute_symlink_replacement_preserves_selector_and_rejects_semantic_drift(self):
        first = self.executable(self.root / 'first', 'first')
        second = self.executable(self.root / 'second', 'second')
        link = self.root / 'current'
        link.symlink_to(first)
        selected = native_select(self.config, str(link), self.root)
        link.unlink()
        link.symlink_to(second)
        self.assertEqual(selected.revalidate().to_dict(), selected.to_dict())
        changed = {**self.snapshot, 'digest': 'b' * 64}
        self.executable(second.parent, 'changed', changed)
        with self.assertRaisesRegex(ValueError, 'changed'):
            selected.revalidate()
        self.assertEqual(selected.native_cli, str(link))

    def test_missing_and_failed_commands_are_single_ordinary_failures(self):
        with patch.dict(os.environ, {'PATH': str(self.root)}):
            with self.assertRaisesRegex(ValueError, 'unavailable'):
                native_select(self.config, 'mnemed', self.root)
        failed = self.executable(self.root / 'failed', 'failed', code=2)
        with patch.dict(os.environ, {'PATH': str(failed.parent)}):
            with self.assertRaisesRegex(ValueError, 'request failed'):
                native_select(self.config, 'mnemed', self.root)
        self.assertEqual(self.log.read_text().splitlines(), ['failed'])

    def test_nonzero_exit_preserves_only_recognized_bounded_structured_error(self):
        envelope = {"schema": "mneme.config-call.v1", "ok": False,
                    "error": {"kind": "protocol", "message": "unsupported frozen flag\n" + "x" * 1024},
                    "result": {"body": "private-body-must-not-escape"}}
        failed = self.executable(self.root / 'failed', 'failed', envelope, code=2)
        with self.assertRaises(NativeCommandError) as caught:
            native_bridge(str(failed), ["call"])
        self.assertIn("exit status 2", str(caught.exception))
        self.assertIn("protocol: unsupported frozen flag", str(caught.exception))
        self.assertNotIn("private-body", str(caught.exception))
        self.assertEqual(len(caught.exception.details["message"]), 512)
        self.assertNotIn("\n", caught.exception.details["message"])
        for response in ({"error": {"message": "private-body-must-not-escape"}},
                         {**envelope, "schema": "unknown"}, {**envelope, "ok": True}):
            self.executable(failed.parent, 'failed', response, code=1)
            with self.assertRaises(NativeCommandError) as unknown:
                native_bridge(str(failed), ["call"])
            self.assertEqual(str(unknown.exception), "native configuration request failed (exit status 1)")
            self.assertIsNone(unknown.exception.details)

    def test_hook_reports_command_failure_without_fresh_session_version_advice(self):
        import hooks
        config = self.root / "hooks.json"
        config.write_text(json.dumps({"schema": "mneme.codex-hooks.config.v13",
            "state_dir": str(self.root / "state"), "memory_config": self.config,
            "native_cli": "mnemed", "reader_codex": "codex"}))
        with patch("store_registry.native_select", side_effect=NativeCommandError("unavailable")):
            with self.assertRaisesRegex(hooks.HookError, "Memory command failed: unavailable") as caught:
                hooks._config(config, cwd=str(self.root))
        self.assertNotIn("fresh session", str(caught.exception))
        self.assertFalse((self.root / "state").exists())

    def diagnostic_executable(self, body):
        path = self.root / 'diagnostic-mnemed'
        path.write_text('#!' + sys.executable + '\nimport os,sys,time\n' + body)
        path.chmod(0o700)
        return path

    def test_native_loader_stderr_is_retained_without_tool_body_or_credentials(self):
        path = self.diagnostic_executable(
            'os.write(2, b"mnemed: libstdc++.so.6: GLIBCXX_3.4.36 not found\\nTOKEN=private-token\\n")\n'
            'print(\'{"body":"private-tool-body"}\')\nsys.exit(127)\n')
        with self.assertRaises(NativeCommandError) as caught:
            native_bridge(str(path), ['select'])
        self.assertIn('GLIBCXX_3.4.36 not found', caught.exception.diagnostic)
        self.assertIn('[redacted]', caught.exception.diagnostic)
        self.assertNotIn('private-token', caught.exception.diagnostic)
        self.assertNotIn('private-tool-body', str(caught.exception) + caught.exception.diagnostic)
        self.assertFalse(any(t.name == 'mneme-native-stderr' for t in threading.enumerate()))

    def test_verbose_stderr_does_not_fail_or_block_successful_native_result(self):
        path = self.diagnostic_executable(
            'os.write(2, b"warning\\n" * 200000)\n' + f'print({json.dumps(self.snapshot)!r})\n')
        self.assertEqual(native_bridge(str(path), ['select'], timeout=3), self.snapshot)
        self.assertFalse(any(t.name == 'mneme-native-stderr' for t in threading.enumerate()))

    def test_stderr_capture_stays_bounded_and_reports_truncation(self):
        path = self.diagnostic_executable('os.write(2, b"failure " * 20000)\nsys.exit(1)\n')
        with self.assertRaises(NativeCommandError) as caught:
            native_bridge(str(path), ['select'])
        self.assertLessEqual(len(caught.exception.diagnostic), 2070)
        self.assertTrue(caught.exception.diagnostic.endswith('[stderr truncated]'))

    def test_stderr_flood_timeout_cleans_reader_and_child(self):
        path = self.diagnostic_executable('while True: os.write(2, b"warning " * 1024)\n')
        started = time.monotonic()
        with self.assertRaisesRegex(NativeCommandError, 'timed out'):
            native_bridge(str(path), ['select'], timeout=.2)
        self.assertLess(time.monotonic() - started, 2)
        self.assertFalse(any(t.name == 'mneme-native-stderr' for t in threading.enumerate()))

    def test_loader_failure_during_request_write_keeps_stderr_and_is_not_retried(self):
        path = self.diagnostic_executable(
            'os.close(0)\nos.write(2, b"mnemed: GLIBCXX_3.4.36 not found\\n")\nsys.exit(127)\n')
        with self.assertRaises(NativeCommandError) as caught:
            native_bridge(str(path), ['call'], payload={'request': 'x' * 100000})
        self.assertIn('GLIBCXX_3.4.36 not found', caught.exception.diagnostic)

    def test_stderr_collector_failure_cleans_child_and_every_pipe_before_input(self):
        import host_process
        path = self.diagnostic_executable('time.sleep(30)\n')
        launched = []
        spawn = host_process.spawn
        def record_spawn(*args, **kwargs):
            process = spawn(*args, **kwargs)
            launched.append(process)
            return process
        with patch('host_process.spawn', side_effect=record_spawn), \
             patch('threading.Thread.start', side_effect=RuntimeError('thread unavailable')), \
             patch('host_process.BoundedPipe.write') as write:
            with self.assertRaisesRegex(NativeCommandError, 'cannot capture native diagnostics; request was not retried'):
                native_bridge(str(path), ['call'], payload={'request': 'test'})
        write.assert_not_called()
        self.assertEqual(len(launched), 1)
        self.assertIsNotNone(launched[0].poll())
        self.assertTrue(all(stream.closed for stream in (launched[0].stdin, launched[0].stdout, launched[0].stderr)))

    @unittest.skipUnless(os.name == 'posix', 'POSIX process group')
    def test_descendant_held_stderr_does_not_hold_collector_open(self):
        path = self.diagnostic_executable(
            'import subprocess\nsubprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"], '
            'stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL)\n'
            + f'print({json.dumps(self.snapshot)!r})\n')
        started = time.monotonic()
        self.assertEqual(native_bridge(str(path), ['select'], timeout=3), self.snapshot)
        self.assertLess(time.monotonic() - started, 2)
        self.assertFalse(any(t.name == 'mneme-native-stderr' for t in threading.enumerate()))

    @unittest.skipUnless(os.name == 'posix', 'POSIX hook launch')
    def test_direct_default_python_skips_old_system_and_resolves_new_path_each_launch(self):
        old = self.root / 'python3'
        old.write_text('#!/bin/sh\nexit 1\n')
        old.chmod(0o700)
        for edition in ('first', 'second'):
            candidate = self.root / 'python3.14'
            candidate.write_text('#!/bin/sh\nif [ "$1" = -c ]; then exit 0; fi\nprintf "%s\\n" ' + edition + '\n')
            candidate.chmod(0o700)
            result = subprocess.run(python_hook_argv('python3', ['fixture.py']),
                env={**os.environ, 'PATH': str(self.root)}, capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), edition.encode())

    @unittest.skipUnless(os.name == 'posix', 'POSIX hook launch')
    def test_direct_default_python_finds_shadowed_unversioned_interpreter(self):
        first, second = self.root / 'old space [x]', self.root / "new café's *"
        first.mkdir()
        second.mkdir()
        old = first / 'python3'
        old.write_text('#!/bin/sh\necho probe-noise\nexit 1\n')
        old.chmod(0o700)
        (second / 'python3').symlink_to(sys.executable)
        arguments = ['', 'space path', 'quote\"', "apostrophe'", '$(not-code)']
        result = subprocess.run(python_hook_argv('python3',
            ['-c', 'import json,sys; print(json.dumps(sys.argv[1:]))', *arguments]),
            env={**os.environ, 'PATH': os.pathsep.join(map(str, (first, second)))},
            capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), arguments)
        self.assertEqual(result.stderr, b'')

    @unittest.skipUnless(os.name == 'posix', 'POSIX hook launch')
    def test_direct_default_python_preserves_empty_and_relative_path_entries(self):
        (self.root / 'python3').symlink_to(sys.executable)
        (self.root / '-odd').mkdir()
        (self.root / '-odd' / 'python3').symlink_to(sys.executable)
        for path in ('', 'missing:', ':missing', 'missing::other', '.', '-odd'):
            with self.subTest(path=path):
                result = subprocess.run(python_hook_argv('python3', ['-c', 'print("supported")']),
                    cwd=self.root, env={**os.environ, 'PATH': path}, capture_output=True, timeout=5)
                self.assertEqual((result.returncode, result.stdout, result.stderr), (0, b'supported\n', b''))

"""Concurrent lifecycle admission and paired wake/close; no services or models."""
import hashlib
import json
import os
from pathlib import Path
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

import hooks
import host_files
import registry_runtime
import store_registry


class HookWakeupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.config = dict(schema=hooks.CONFIG_SCHEMA_V13, state_dir=self.root / 'state',
            project_root=self.root, _config_path=self.root / 'config.json',
            memory_scope='registry', memory_mode='async', recall_mode='automatic',
            recording_mode='automatic', _background_maintenance=False)
        self.event = dict(hook_event_name='UserPromptSubmit', session_id='session',
            turn_id='turn', cwd=str(self.root), prompt='Fix the background memory worker')
        self.selected = SimpleNamespace(cwd=str(self.root), digest='selection')
        self.worker, self.recorder = Mock(), Mock()
        self.worker.notice.return_value = {'outcome': 'queued'}
        self.worker.background.return_value = {'outcome': 'running'}
        self.recorder.notice.return_value = {'outcome': 'admitted'}
        self.addCleanup(patch.stopall)
        patch.object(registry_runtime, 'selection', return_value=self.selected).start()
        patch.object(hooks, '_reader_worker', return_value=self.worker).start()
        patch.object(hooks, '_recording_jobs', return_value=self.recorder).start()
        registry_runtime.admit_session(self.config, self.event)

    def test_both_admission_orders_survive_short_contention(self):
        for background in (False, True):
            with self.subTest(background=background):
                self.worker.reset_mock()
                locked, release = threading.Event(), threading.Event()
                def holder():
                    with hooks._locked_state(self.config['state_dir'], 'session'):
                        locked.set()
                        release.wait(2)
                thread = threading.Thread(target=holder)
                thread.start()
                self.assertTrue(locked.wait(1))
                timer = threading.Timer(.04, release.set)
                timer.start()
                try:
                    result = hooks.handle_event(self.event, self.config, reader_background=background)
                finally:
                    release.set(); thread.join(2); timer.join(1)
                self.worker.background.assert_called_once()
                if background:
                    self.worker.notice.assert_not_called()
                else:
                    self.worker.notice.assert_called_once()
                    self.recorder.notice.assert_called()
                    self.assertIn('hookSpecificOutput', result)

    def test_held_admission_is_finite_and_does_not_run_handlers(self):
        before = hooks._state_path(self.config['state_dir'], 'session').read_bytes()
        with hooks._locked_state(self.config['state_dir'], 'session'):
            for background in (False, True):
                start = time.monotonic()
                self.assertEqual(hooks.handle_event(self.event, self.config,
                    reader_background=background), {})
                self.assertLess(time.monotonic() - start, .9)
        self.assertEqual(before, hooks._state_path(self.config['state_dir'], 'session').read_bytes())
        self.worker.notice.assert_not_called()
        self.worker.background.assert_not_called()
        self.recorder.notice.assert_not_called()

    def test_foreground_closes_before_waking_even_without_async_dispatch(self):
        order = []
        self.worker.close_turn.side_effect = lambda *_: order.append('reader-close')
        self.recorder.close_turn.side_effect = lambda *_, **kw: order.append('record-close')
        self.worker.background.side_effect = lambda *_: order.append('wake')
        self.assertEqual(hooks.handle_event({**self.event, 'hook_event_name': 'Stop'}, self.config), {})
        self.assertEqual(order, ['reader-close', 'record-close', 'wake'])

    def test_async_stop_repeats_exact_close_before_wake(self):
        event = {**self.event, 'hook_event_name': 'Stop'}
        self.assertEqual(hooks.handle_event(event, self.config, reader_background=True), {})
        self.worker.close_turn.assert_called_once_with(self.config, 'session', 'turn')
        self.recorder.close_turn.assert_called_once_with(self.config, 'session', 'turn', cancel=False)
        self.worker.background.assert_called_once_with(self.config, event)

    def test_optional_reader_failure_does_not_drop_recorder_or_wake(self):
        self.worker.notice.side_effect = RuntimeError('optional')
        hooks.handle_event(self.event, self.config)
        self.recorder.notice.assert_called_once()
        self.worker.background.assert_called_once()
        self.worker.close_turn.side_effect = RuntimeError('optional')
        hooks.handle_event({**self.event, 'hook_event_name': 'Stop'}, self.config)
        self.recorder.close_turn.assert_called_once()
        self.assertEqual(self.worker.background.call_count, 2)

    def test_interrupt_closes_without_starting_worker(self):
        hooks.handle_event({**self.event, 'hook_event_name': 'Interrupt'}, self.config)
        self.recorder.close_turn.assert_called_once_with(self.config, 'session', 'turn', cancel=True)
        self.worker.background.assert_not_called()

    def test_first_binding_refuses_policy_drift_during_lock_wait(self):
        # Keep real selection's adapter hash check; only its parsed native
        # snapshot is a fixture so no CLI/owner access is needed.
        patch.stopall()
        raw = b'{"fixture":"initial"}'
        path = self.root / 'drift.json'
        path.write_bytes(raw)
        config = {**self.config, '_config_path': path,
            '_registry_config_sha256': hashlib.sha256(raw).hexdigest(),
            'frozen_selection': {}}
        event = {**self.event, 'session_id': 'unbound'}
        waited = threading.Event()
        outcome = []
        sleep = host_files._lock_sleep
        def waiting(delay):
            waited.set()
            sleep(delay)
        def admit():
            try:
                registry_runtime.admit_session(config, event)
            except ValueError as error:
                outcome.append(str(error))
        with patch.object(store_registry, 'selection_from_dict', return_value=self.selected), \
                patch.object(host_files, '_lock_sleep', side_effect=waiting):
            with hooks._locked_state(config['state_dir'], 'unbound'):
                thread = threading.Thread(target=admit)
                thread.start()
                try:
                    self.assertTrue(waited.wait(1))
                    path.write_bytes(b'{"fixture":"changed"}')
                finally:
                    pass  # release before joining the bounded waiter
            thread.join(2)
            self.assertFalse(thread.is_alive())
        self.assertEqual(outcome, ['hook policy changed; reload before work'])
        self.assertFalse(hooks._state_path(config['state_dir'], 'unbound').exists())


if __name__ == '__main__':
    unittest.main()

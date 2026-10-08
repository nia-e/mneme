"""Provider-free tests for disposable async instrumentation and its call cap."""
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import textwrap
import types
import unittest
from unittest.mock import Mock, patch

SPEC = importlib.util.spec_from_file_location("async_task_observer", Path(__file__).with_name("async_task_observer.py"))
observer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(observer)


class ObserverTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.trace = self.root / "trace.jsonl"
        self.obs = observer.Observer(self.root, self.trace)

    def rows(self):
        return [json.loads(line) for line in self.trace.read_text().splitlines()]

    def modules(self):
        hooks = types.SimpleNamespace(handle_event=lambda *a, **k: {})
        def state(config, session, mutate, **kwargs):
            result, changed = mutate(config)
            return True, result
        rw = types.SimpleNamespace(_state=state, consume=lambda *a, **k: {"outcome": "empty", "cards": []},
                                   subprocess=types.SimpleNamespace(Popen=Mock()))
        recall = types.SimpleNamespace(collect_reader=Mock(return_value={"outcome": "ok", "cards": []}))
        class Runtime:
            def select(self, dialogue, cards):
                return {"selected_ids": [], "reason": "abstained", "usage": {"input_tokens": 3},
                        "provider_attempt": True}
        runtime = types.SimpleNamespace(ReaderRuntime=Runtime)
        self.obs.install(hooks, rw, recall, runtime)
        return hooks, rw, recall, runtime

    def test_installer_changes_only_exact_installed_command(self):
        prefix = self.root / "prefix with spaces"
        config = self.root / "hooks.json"
        command = shlex.join([sys.executable, str(prefix / "lib/hooks.py"), "--config", "config path"])
        document = {"hooks": {"PostToolUse": [{"hooks": [
            {"type": "command", "command": command, "timeout": 5},
            {"type": "command", "command": "echo untouched"}]}]}}
        config.write_text(json.dumps(document))
        meta = observer.install_observer(config, prefix, self.trace)
        result = json.loads(config.read_text())["hooks"]["PostToolUse"][0]["hooks"]
        words = shlex.split(result[0]["command"])
        self.assertEqual(words[:2], [sys.executable, str(observer.SCRIPT)])
        self.assertEqual(words[-3:], ["--", "--config", "config path"])
        self.assertEqual(result[0]["timeout"], 5)
        self.assertEqual(result[1], document["hooks"]["PostToolUse"][0]["hooks"][1])
        self.assertEqual(meta["hook_commands_wrapped"], 1)
        self.assertEqual(meta["visibility"], "unknown")

    def test_fresh_receipt_required(self):
        self.trace.write_text("retained")
        with self.assertRaisesRegex(ValueError, "fresh"):
            observer.install_observer(self.root / "missing", self.root, self.trace)

    def test_log_caps_and_errors(self):
        self.obs.record("oversized", data="x" * observer.MAX_EVENT)
        self.assertFalse(self.trace.exists())
        self.trace.write_bytes(b"x" * (observer.MAX_TRACE - 5))
        self.obs.record("beyond_limit")
        self.assertEqual(self.trace.stat().st_size, observer.MAX_TRACE - 5)
        with patch.object(observer.os, "open", side_effect=OSError("not persisted")):
            self.obs.record("unavailable")

    def test_each_event_is_one_append_write(self):
        original = os.write
        with patch.object(observer.os, "write", wraps=original) as write:
            self.obs.record("one")
        self.assertEqual(write.call_count, 1)
        self.assertEqual(self.rows()[0]["event"], "one")

    def test_reader_cap_is_shared_and_conservative(self):
        _, _, _, runtime = self.modules()
        first = runtime.ReaderRuntime().select([], [])
        second = runtime.ReaderRuntime().select([], [])
        self.assertTrue(first["provider_attempt"])
        self.assertEqual(second["reason"], "experiment_reader_cap")
        self.assertFalse(second["provider_attempt"])
        self.assertFalse(observer.Observer(self.root, self.trace).reserve())
        self.assertEqual([r["event"] for r in self.rows()],
                         ["reader_select_enter", "reader_select_complete", "reader_select_enter", "reader_cap_refused"])

    def test_collect_result_unchanged_and_no_summary_retained(self):
        _, _, recall, _ = self.modules()
        expected = {"outcome": "ok", "cards": [{"id": "opaque", "fingerprint": "abc", "summary": "SECRET SUMMARY"}],
                    "observation": {"schema": 1, "learning": "disabled", "cards": []}}
        # The wrapper closes over this mocked original function.
        original = next(cell.cell_contents for cell in recall.collect_reader.__closure__ if isinstance(cell.cell_contents, Mock))
        original.return_value = expected
        self.assertIs(recall.collect_reader(), expected)
        self.assertNotIn("SECRET SUMMARY", self.trace.read_text())
        self.assertEqual(self.rows()[0]["cards"][0]["id"], "opaque")

    def test_state_records_after_original_returns_not_inside_lock(self):
        locked = False
        def state(config, session, mutate, **kwargs):
            nonlocal locked
            locked = True
            value, changed = mutate(config)
            locked = False
            return True, value
        hooks = types.SimpleNamespace(handle_event=lambda *a, **k: {})
        rw = types.SimpleNamespace(_state=state, consume=lambda *a, **k: {}, subprocess=types.SimpleNamespace(Popen=Mock()))
        recall = types.SimpleNamespace(collect_reader=lambda *a, **k: {})
        runtime = types.SimpleNamespace(ReaderRuntime=type("Runtime", (), {"select": lambda *a: {}}))
        self.obs.install(hooks, rw, recall, runtime)
        original_record = self.obs.record
        def record(*args, **kwargs):
            self.assertFalse(locked)
            original_record(*args, **kwargs)
        self.obs.record = record
        config = {"active": {"turn": "t", "request": "digest"}, "counts": {}, "ready": None}
        def publish(data):
            data["ready"] = {"turn": "t", "request": "digest", "cards": [{"id": "c", "summary": "HIDDEN"}]}
            return "unchanged result", True
        self.assertEqual(rw._state(config, "s", publish), (True, "unchanged result"))
        row = self.rows()[0]
        self.assertEqual(row["operation"], "publish")
        self.assertLessEqual(row["start_monotonic_ns"], row["end_monotonic_ns"])
        self.assertEqual(row["after"]["ready"]["cards"][0]["id"], "c")
        self.assertNotIn("HIDDEN", self.trace.read_text())

    def test_hook_emission_only_after_successful_flush(self):
        hooks, _, _, _ = self.modules()
        def main(args):
            hooks.handle_event({"session_id": "s", "turn_id": "t", "tool_use_id": "tool",
                                "hook_event_name": "PostToolUse"}, {})
            print(json.dumps({"hookSpecificOutput": {"additionalContext": "SECRET CONTEXT"}}))
            return 17
        hooks.main = main
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            self.assertEqual(self.obs.run_hook(hooks, []), 17)
        self.assertIn("SECRET CONTEXT", output.getvalue())
        row = self.rows()[-1]
        self.assertEqual(row["event"], "hook_stdout_flushed")
        self.assertEqual(row["context_bytes"], len("SECRET CONTEXT"))
        self.assertEqual(row["tool_use_id"], "tool")
        self.assertNotIn("SECRET CONTEXT", self.trace.read_text())

    def test_worker_only_exact_spawn_is_rerouted(self):
        _, rw, _, _ = self.modules()
        original = next(cell.cell_contents for cell in rw.subprocess.Popen.__closure__ if isinstance(cell.cell_contents, Mock))
        argv = [sys.executable, str((self.root / "reader_worker.py").resolve()), "--serve", "--config", "config", "--session-id", "s"]
        rw.subprocess.Popen(argv, start_new_session=True)
        actual = original.call_args.args[0]
        self.assertEqual(actual[:2], [sys.executable, str(observer.SCRIPT)])
        self.assertEqual(actual[-5:], ["--serve", "--config", "config", "--session-id", "s"])
        other = [sys.executable, str(self.root / "different.py"), "--serve"]
        rw.subprocess.Popen(other)
        self.assertEqual(original.call_args.args[0], other)

    def test_malformed_telemetry_does_not_replace_original_result(self):
        _, _, _, runtime = self.modules()
        result = runtime.ReaderRuntime().select([], [{"id": "a", "native": None}])
        self.assertEqual(result["reason"], "abstained")

    def test_failed_stdout_flush_never_claims_emission(self):
        hooks, _, _, _ = self.modules()
        hooks.main = lambda args: print('{}')
        output = io.StringIO()
        output.flush = Mock(side_effect=OSError("closed stream"))
        with contextlib.redirect_stdout(output), self.assertRaises(OSError):
            self.obs.run_hook(hooks, [])
        self.assertFalse(self.trace.exists())

    def test_fresh_dynamic_worker_loader_observes_consume_and_preserves_hook_identity(self):
        worker_path = self.root / "reader_worker.py"
        worker_path.write_text(textwrap.dedent('''
            import subprocess
            def _state(config, session_id, mutate, **kwargs):
                value, changed = mutate(config)
                return True, value
            def consume(config, event):
                def claim(data):
                    if event['turn_id'] != data['active']['turn']:
                        return {'outcome': 'empty', 'cards': []}, False
                    cards = data['ready']['cards']
                    data['ready'] = None
                    return {'outcome': 'emitted', 'cards': cards}, True
                return _state(config, event['session_id'], claim)[1]
        '''))
        # Same fresh spec/module/exec pattern as the actual hooks._reader_worker.
        def load_worker():
            spec = importlib.util.spec_from_file_location("mneme_codex_reader_worker", worker_path)
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            return module
        initial = load_worker()
        hooks = types.SimpleNamespace(_reader_worker=load_worker)
        def handle(event, config):
            result = hooks._reader_worker().consume(config, event)
            return {"hookSpecificOutput": {"additionalContext": "card"}} if result['cards'] else {}
        hooks.handle_event = handle
        recall = types.SimpleNamespace(collect_reader=lambda *a, **k: {})
        runtime = types.SimpleNamespace(ReaderRuntime=type("Runtime", (), {"select": lambda *a: {}}))
        # Keep the process-global subprocess module unchanged outside this test.
        with patch.object(subprocess, "Popen", Mock()):
            self.obs.install(hooks, initial, recall, runtime)
            one, two = hooks._reader_worker(), hooks._reader_worker()
            self.assertIsNot(one, two)
            self.assertIs(one._async_task_observer, self.obs)
            self.assertIs(two._async_task_observer, self.obs)
            state = {"active": {"turn": "right", "request": "hash"}, "counts": {},
                     "ready": {"cards": [{"id": "card", "fingerprint": "fp"}]}}
            def main(args):
                result = hooks.handle_event({"hook_event_name": "PostToolUse", "session_id": "s",
                    "turn_id": args[0], "tool_use_id": "tool-" + args[0]}, state)
                print(json.dumps(result))
                return 0
            hooks.main = main
            with contextlib.redirect_stdout(io.StringIO()):
                self.obs.run_hook(hooks, ["wrong"])
                self.obs.run_hook(hooks, ["right"])
        rows = self.rows()
        flushed = [row for row in rows if row['event'] == 'hook_stdout_flushed']
        self.assertEqual([row['turn_id'] for row in flushed], ['wrong', 'right'])
        self.assertEqual(flushed[0]['cards'], [])
        self.assertEqual(flushed[1]['cards'], [{"id": "card", "fingerprint": "fp"}])
        self.assertEqual(len([row for row in rows if row['event'] == 'state_transaction']), 1)

    def test_real_child_imports_are_observed_without_provider(self):
        bundle = self.root / "bundle"
        bundle.mkdir()
        fake = {
            "hooks.py": '''
                import json
                import reader_worker
                def handle_event(event, config, **kwargs):
                    reader_worker.background()
                    return {}
                def main(args):
                    print(json.dumps(handle_event({"hook_event_name":"UserPromptSubmit", "session_id":"s", "turn_id":"t"}, {})))
                    return 0
            ''',
            "reader_worker.py": '''
                import subprocess, sys
                from pathlib import Path
                def _state(*args, **kwargs): return True, None
                def consume(*args, **kwargs): return {}
                def background():
                    child = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "--serve", "--config", "unused", "--session-id", "s"])
                    child.wait(timeout=10)
                def main(args):
                    from hook_recall import collect_reader
                    from reader_runtime import ReaderRuntime
                    result = collect_reader()
                    ReaderRuntime().select([], result["cards"])
                    return 0
                if __name__ == "__main__": raise RuntimeError("unobserved direct-script child")
            ''',
            "hook_recall.py": '''
                def collect_reader():
                    return {"outcome":"ok", "cards":[{"id":"card", "fingerprint":"fp", "summary":"NEVER LOG"}]}
            ''',
            "reader_runtime.py": '''
                class ReaderRuntime:
                    def select(self, dialogue, cards):
                        return {"selected_ids":["card"], "reason":"selected", "usage":{}, "provider_attempt":False}
            '''}
        for name, source in fake.items():
            (bundle / name).write_text(textwrap.dedent(source))
        result = subprocess.run([sys.executable, str(observer.SCRIPT), "--bundle-lib", str(bundle),
                                 "--trace", str(self.trace), "--", "--config", "unused"],
                                capture_output=True, timeout=15, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), {})
        rows = self.rows()
        self.assertTrue(any(r["event"] == "worker_enter" for r in rows))
        self.assertTrue(any(r["event"] == "native_pool" and r["cards"][0]["id"] == "card" for r in rows))
        self.assertTrue(any(r["event"] == "reader_select_complete" for r in rows))
        self.assertNotIn("NEVER LOG", self.trace.read_text())
        self.assertEqual(len({r["pid"] for r in rows}), 2)


if __name__ == "__main__":
    unittest.main()

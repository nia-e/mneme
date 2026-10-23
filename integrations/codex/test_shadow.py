"""Offline shadow-worker contract tests. No model or Mneme service is contacted."""
from __future__ import annotations

import hashlib
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

import host_files
import shadow

ID = "01ARZ3NDEKTSV4RRFFQ69G5FAV"


class ShadowTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        if os.name == "nt":
            self.root = self.root / "state"
            host_files.ensure_private_directory(self.root)
        self.codex = self.root / "missing-codex"
        self.payload = {"schema": shadow.SCHEMA, "turn_key": "a" * 64,
                        "cue": "Review the graph parser", "answer": "Fixed the parser.",
                        "cards": [{"id": ID, "summary": "Earlier parser guard",
                                   "display_sha256": "b" * 64}],
                        "model": "gpt-5.6-sol", "codex": str(self.codex),
                        "codex_sha256": "c" * 64, "state_dir": str(self.root)}

    def test_explicit_model_and_pinned_binary_fail_open(self):
        self.assertEqual(shadow._run(shadow._validate(self.payload))["status"], "model_unavailable")
        self.codex.write_text("#!/bin/sh\nexit 0\n")
        self.codex.chmod(0o755)
        with patch.object(shadow.host_process, "spawn", side_effect=AssertionError("provider called")):
            self.assertEqual(shadow._run(shadow._validate(self.payload))["status"], "model_unavailable")
        self.payload["codex_sha256"] = hashlib.sha256(self.codex.read_bytes()).hexdigest()
        for bad in ("gpt-6-astra", "", None):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                shadow._validate({**self.payload, "model": bad})

    def test_unpinned_path_command_resolves_for_each_spawn(self):
        command = self.root / ("codex.exe" if os.name == "nt" else "codex")
        command.write_bytes(b"fake executable")
        command.chmod(0o700)
        payload = {key: value for key, value in self.payload.items()
                   if key not in ("codex", "codex_sha256")}
        with patch.dict(os.environ, {"PATH": str(self.root)}):
            for contents in (b"first", b"replacement"):
                command.write_bytes(contents)
                with patch.object(shadow.host_process, "spawn", side_effect=OSError("ordinary failure")) as spawn:
                    result = shadow._run(shadow._validate(payload))
                self.assertEqual(spawn.call_args.args[0][0], str(command))
                self.assertEqual(result["status"], "model_unavailable")
        with patch.dict(os.environ, {"PATH": str(self.root / "missing")}):
            self.assertEqual(shadow._run(shadow._validate(payload))["status"], "model_unavailable")

    def test_command_is_ephemeral_and_disables_recursive_hooks_and_tools(self):
        command = shadow._command(str(self.codex), "gpt-5.6-sol", self.root / "schema",
                                  self.root / "out", self.root)
        self.assertIn("--ignore-user-config", command)
        self.assertIn("--ephemeral", command)
        self.assertIn("read-only", command)
        for feature in ("hooks", "memories", "multi_agent", "shell_tool"):
            self.assertIn(["--disable", feature], [command[i:i + 2] for i in range(len(command) - 1)])
        self.assertNotIn("reflect", command)
        self.assertTrue(shadow._events_are_tool_free(
            b'{"type":"turn.started"}\n{"type":"item.completed","item":{"type":"agent_message"}}\n'))
        self.assertFalse(shadow._events_are_tool_free(
            b'{"type":"item.completed","item":{"type":"command_execution"}}\n'))
        self.assertFalse(shadow._events_are_tool_free(b'{"type":"error"}\n'))

    def test_result_sidecar_contains_only_bounded_metadata(self):
        result = {"status": "observed", "judgments": [
            {"id": ID, "outcome": "unknown", "evidence": "No visible use."}],
                  "delivered": [{"id": ID, "display_sha256": "b" * 64}]}
        shadow._write_result(self.root, "a" * 64, result)
        saved = json.loads((self.root / "shadow" / (("a" * 64) + ".json")).read_text())
        self.assertEqual(saved["judgments"][0]["outcome"], "unknown")
        self.assertNotIn("Review the graph parser", json.dumps(saved))
        self.assertNotIn("Earlier parser guard", json.dumps(saved))
        self.assertNotIn("Fixed the parser", json.dumps(saved))

    def test_launch_bounds_input_before_spawning(self):
        with patch.object(shadow.host_process, "spawn", side_effect=AssertionError("spawned")):
            self.assertFalse(shadow.launch(self.root, "session", "turn", "x" * 2000,
                                           "answer", [], self.codex, "c" * 64, "gpt-5.6-sol"))

    def test_disposable_worker_records_model_unavailable_without_provider(self):
        result = subprocess.run([sys.executable, str(Path(shadow.__file__)), "--worker"],
                                input=json.dumps(self.payload).encode(), capture_output=True,
                                timeout=5, check=True)
        self.assertEqual(result.stdout, b"")
        saved = json.loads((self.root / "shadow" / (("a" * 64) + ".json")).read_text())
        self.assertEqual(saved["status"], "model_unavailable")
        self.assertEqual(saved["task_cue"], self.payload["cue"])
        self.assertEqual(saved["candidate_cards"], [{"id": ID, "summary": "Earlier parser guard"}])
        self.assertNotIn(self.payload["answer"], json.dumps(saved))
        self.assertEqual(saved["judgments"], [])

    def test_timeout_kills_owned_process_tree_without_labels(self):
        self.codex.write_bytes(b"fake executable")
        self.codex.chmod(0o700)
        self.payload["codex_sha256"] = hashlib.sha256(self.codex.read_bytes()).hexdigest()
        child = Mock(stdin=io.BytesIO(), stdout=io.BytesIO(), returncode=None)
        pipes = Mock()
        pipes.read.side_effect = TimeoutError("deadline")
        with patch.object(shadow.host_process, "spawn", return_value=child) as spawn, \
             patch.object(shadow.host_process, "BoundedPipe", return_value=pipes), \
             patch.object(shadow.host_process, "terminate_tree") as kill:
            result = shadow._run(shadow._validate(self.payload))
        self.assertEqual(result, {"status": "timeout", "judgments": []})
        self.assertIs(spawn.call_args.kwargs["tree"], True)
        self.assertNotIn("detached", spawn.call_args.kwargs)
        kill.assert_called_once_with(child, timeout=2)
        pipes.close.assert_called_once_with()

    def test_unknown_id_or_malformed_model_output_cannot_become_negative(self):
        self.codex.write_bytes(b"fake executable")
        self.codex.chmod(0o700)
        self.payload["codex_sha256"] = hashlib.sha256(self.codex.read_bytes()).hexdigest()
        for output in (
            {"judgments": [{"id": "unknown", "outcome": "unhelpful", "evidence": "no"}]},
            {"judgments": [{"id": ID, "outcome": "unhelpful"}]},
            {"judgments": []},
        ):
            def fake_spawn(command, **_kwargs):
                Path(command[command.index("-o") + 1]).write_text(json.dumps(output))
                return Mock(stdin=io.BytesIO(), stdout=io.BytesIO(), returncode=0)
            pipes = Mock()
            pipes.read.side_effect = [b'{"type":"turn.completed"}\n', b""]
            with self.subTest(output=output), \
                 patch.object(shadow.host_process, "spawn", side_effect=fake_spawn), \
                 patch.object(shadow.host_process, "BoundedPipe", return_value=pipes), \
                 patch.object(shadow.host_process, "terminate_tree") as cleanup:
                result = shadow._run(shadow._validate(self.payload))
                self.assertEqual(result, {"status": "invalid_output", "judgments": []})
                cleanup.assert_called_once()
                pipes.close.assert_called_once_with()

    def test_worker_is_detached_not_owned_by_launcher(self):
        child = Mock(stdin=io.BytesIO())
        with patch.object(shadow.host_process, "spawn", return_value=child) as spawn:
            self.assertTrue(shadow.launch(self.root, "session", "turn", "cue", "answer",
                                         self.payload["cards"], self.codex, "c" * 64, "gpt-5.6-sol"))
        self.assertIs(spawn.call_args.kwargs["detached"], True)
        self.assertNotIn("tree", spawn.call_args.kwargs)
        self.assertTrue(child.stdin.closed)

    def test_sidecar_publication_is_private_and_refuses_hardlinked_target(self):
        target = self.root / "shadow" / ("a" * 64 + ".json")
        shadow._write_result(self.root, "a" * 64, {"status": "busy", "judgments": []})
        host_files.check_private_directory(target.parent)
        host_files.check_private(target)
        alias = self.root / "alias"
        os.link(target, alias)
        before = target.read_bytes()
        with self.assertRaises(ValueError):
            shadow._write_result(self.root, "a" * 64, {"status": "observed", "judgments": []})
        self.assertEqual(target.read_bytes(), before)
        self.assertEqual(alias.read_bytes(), before)

    def test_worker_lock_contention_does_not_start_assessment(self):
        lock = host_files.open_regular(self.root / "shadow.lock", os.O_CREAT | os.O_RDWR)
        try:
            host_files.lock_file(lock, blocking=False)
            subprocess.run([sys.executable, str(Path(shadow.__file__)), "--worker"],
                           input=json.dumps(self.payload).encode(), capture_output=True,
                           timeout=5, check=True)
        finally:
            os.close(lock)
        saved = json.loads((self.root / "shadow" / ("a" * 64 + ".json")).read_bytes())
        self.assertEqual(saved["status"], "busy")
        self.assertEqual(saved["judgments"], [])

    @unittest.skipIf(os.name == "nt", "Retained Unix state-directory mode contract")
    def test_existing_0755_state_and_shadow_directories_keep_modes(self):
        state = self.root / "retained-state"
        state.mkdir(mode=0o755)
        state.chmod(0o755)
        observations = state / "shadow"
        observations.mkdir(mode=0o755)
        observations.chmod(0o755)
        payload = {**self.payload, "state_dir": str(state)}
        subprocess.run([sys.executable, str(Path(shadow.__file__)), "--worker"],
                       input=json.dumps(payload).encode(), capture_output=True,
                       timeout=5, check=True)
        saved = json.loads((observations / ("a" * 64 + ".json")).read_bytes())
        self.assertEqual(saved["status"], "model_unavailable")
        self.assertEqual(stat.S_IMODE(state.stat().st_mode), 0o755)
        self.assertEqual(stat.S_IMODE(observations.stat().st_mode), 0o755)
        host_files.check_private(observations / ("a" * 64 + ".json"))

    def test_event_limit_stops_local_child_and_cleans_owned_tree(self):
        self.codex.write_bytes(b"fake executable")
        self.codex.chmod(0o700)
        self.payload["codex_sha256"] = hashlib.sha256(self.codex.read_bytes()).hexdigest()
        command = [sys.executable, "-c", "import os,sys,time; sys.stdin.buffer.read(); "
                   "os.write(1, b'x' * 32768); time.sleep(30)"]
        with patch.object(shadow, "_command", return_value=command), \
             patch.object(shadow.host_process, "terminate_tree",
                          wraps=shadow.host_process.terminate_tree) as cleanup:
            result = shadow._run(self.payload)
        self.assertEqual(result, {"status": "model_unavailable", "judgments": []})
        self.assertIsNotNone(cleanup.call_args.args[0].poll())
        cleanup.assert_called_once()

    def test_local_child_deadline_is_finite_and_cleans_owned_tree(self):
        self.codex.write_bytes(b"fake executable")
        self.codex.chmod(0o700)
        self.payload["codex_sha256"] = hashlib.sha256(self.codex.read_bytes()).hexdigest()
        command = [sys.executable, "-c", "import time; time.sleep(30)"]
        with patch.object(shadow, "_command", return_value=command), \
             patch.object(shadow, "TIMEOUT", 0.05), \
             patch.object(shadow.host_process, "terminate_tree",
                          wraps=shadow.host_process.terminate_tree) as cleanup:
            result = shadow._run(self.payload)
        self.assertEqual(result, {"status": "timeout", "judgments": []})
        self.assertIsNotNone(cleanup.call_args.args[0].poll())
        cleanup.assert_called_once()

    def test_successful_local_child_is_reaped_and_prompt_excludes_native_metadata(self):
        self.codex.write_bytes(b"fake executable")
        self.codex.chmod(0o700)
        self.payload["codex_sha256"] = hashlib.sha256(self.codex.read_bytes()).hexdigest()
        self.payload["cards"][0]["native"] = {"node_id": ID, "graph_path": "not model context"}
        def fake_command(_codex, _model, _schema, output, _cwd):
            script = ("import json,sys; from pathlib import Path; "
                      "prompt=sys.stdin.buffer.read(); assert b'graph_path' not in prompt; "
                      "Path(sys.argv[1]).write_text(json.dumps({'judgments': "
                      "[{'id': sys.argv[2], 'outcome': 'unknown', 'evidence': 'No evidence.'}]})); "
                      "print('{\"type\":\"turn.completed\"}')")
            return [sys.executable, "-c", script, str(output), ID]
        with patch.object(shadow, "_command", side_effect=fake_command), \
             patch.object(shadow.host_process, "terminate_tree",
                          wraps=shadow.host_process.terminate_tree) as cleanup:
            result = shadow._run(shadow._validate(self.payload))
        self.assertEqual(result["status"], "observed")
        self.assertEqual(result["judgments"][0]["outcome"], "unknown")
        self.assertIsNotNone(cleanup.call_args.args[0].poll())
        cleanup.assert_called_once()

    def test_retention_removes_every_expired_observation_not_every_other_one(self):
        directory = self.root / "shadow"
        host_files.ensure_private_directory(directory)
        old = []
        for index in range(4):
            path = directory / (f"{index:064x}" + ".json")
            host_files.atomic_write(path, b"{}")
            os.utime(path, (1, 1))
            old.append(path)
        shadow._write_result(self.root, "a" * 64, {"status": "busy", "judgments": []})
        self.assertFalse(any(path.exists() for path in old))
        self.assertEqual(len(list(directory.iterdir())), 1)

    def test_worst_bounded_task_cards_and_paths_fit_disposable_sidecar(self):
        ids = (ID, "01ARZ3NDEKTSV4RRFFQ69G5FAW")
        cards = []
        for identifier in ids:
            native = {"node_id": identifier, "lane": "primary", "card_sha256": "d" * 64,
                      "graph_path": [{"previous": ID, "target": identifier, "from": ID,
                                      "to": identifier, "kind": "supports", "anchor": "a" * 850}]}
            self.assertLessEqual(len(json.dumps(native).encode()), 1400)
            cards.append({"id": identifier, "summary": "s" * shadow.MAX_CARD_TEXT,
                          "display_sha256": "b" * 64, "native": native})
        payload = {**self.payload, "cue": "c" * shadow.MAX_CUE,
                   "answer": "a" * shadow.MAX_ANSWER, "cards": cards}
        shadow._validate(payload)
        self.assertLessEqual(len(json.dumps(payload).encode()), shadow.MAX_INPUT)
        worker = subprocess.run([sys.executable, str(Path(shadow.__file__)), "--worker"],
                                input=json.dumps(payload).encode(), capture_output=True,
                                timeout=5, check=True)
        self.assertEqual(worker.stdout, b"")
        path = self.root / "shadow" / (("a" * 64) + ".json")
        saved = json.loads(path.read_text())
        self.assertEqual(saved["status"], "model_unavailable")
        self.assertEqual(len(saved["delivered"]), 2)
        self.assertEqual(saved["delivered"][1]["native"]["graph_path"][0]["anchor"], "a" * 850)
        self.assertLessEqual(path.stat().st_size, shadow.MAX_SIDECAR)
        shadow._write_result(self.root, payload["turn_key"], {
            "status": "observed", "task_cue": payload["cue"],
            "candidate_cards": [{"id": card["id"], "summary": card["summary"]} for card in cards],
            "delivered": [{"id": card["id"], "display_sha256": card["display_sha256"],
                           "native": card["native"]} for card in cards],
            "judgments": [{"id": identifier, "outcome": "unknown", "evidence": "e" * 200}
                          for identifier in ids],
        })
        self.assertEqual(json.loads(path.read_text())["status"], "observed")
        self.assertLessEqual(path.stat().st_size, shadow.MAX_SIDECAR)


if __name__ == "__main__":
    unittest.main()

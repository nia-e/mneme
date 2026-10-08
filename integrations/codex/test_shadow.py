"""Offline shadow-worker contract tests. No model or Mneme service is contacted."""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import shadow

ID = "01ARZ3NDEKTSV4RRFFQ69G5FAV"


class ShadowTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
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
        with patch.object(shadow.subprocess, "Popen", side_effect=AssertionError("provider called")):
            self.assertEqual(shadow._run(shadow._validate(self.payload))["status"], "model_unavailable")
        self.payload["codex_sha256"] = hashlib.sha256(self.codex.read_bytes()).hexdigest()
        for bad in ("gpt-6-astra", "", None):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                shadow._validate({**self.payload, "model": bad})

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
        with patch.object(shadow.subprocess, "Popen", side_effect=AssertionError("spawned")):
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

    def test_timeout_kills_owned_process_group_without_labels(self):
        self.codex.write_bytes(b"fake executable")
        self.codex.chmod(0o700)
        self.payload["codex_sha256"] = hashlib.sha256(self.codex.read_bytes()).hexdigest()
        class TimedOut:
            pid = 4321
            returncode = None
            calls = 0
            def communicate(self, *_args, **_kwargs):
                self.calls += 1
                if self.calls == 1:
                    raise subprocess.TimeoutExpired(cmd="codex", timeout=shadow.TIMEOUT)
                return b"", b""
        child = TimedOut()
        with patch.object(shadow.subprocess, "Popen", return_value=child), \
             patch.object(shadow.os, "killpg") as kill:
            result = shadow._run(shadow._validate(self.payload))
        self.assertEqual(result, {"status": "timeout", "judgments": []})
        kill.assert_called_once_with(4321, shadow.signal.SIGKILL)
        self.assertEqual(child.calls, 2)

    def test_unknown_id_or_malformed_model_output_cannot_become_negative(self):
        self.codex.write_bytes(b"fake executable")
        self.codex.chmod(0o700)
        self.payload["codex_sha256"] = hashlib.sha256(self.codex.read_bytes()).hexdigest()
        class Completed:
            returncode = 0
            def communicate(self, *_args, **_kwargs):
                return b'{"type":"turn.completed"}\n', b""
        for output in (
            {"judgments": [{"id": "unknown", "outcome": "unhelpful", "evidence": "no"}]},
            {"judgments": [{"id": ID, "outcome": "unhelpful"}]},
            {"judgments": []},
        ):
            def fake_spawn(command, **_kwargs):
                Path(command[command.index("-o") + 1]).write_text(json.dumps(output))
                return Completed()
            with self.subTest(output=output), patch.object(shadow.subprocess, "Popen", side_effect=fake_spawn):
                result = shadow._run(shadow._validate(self.payload))
                self.assertEqual(result, {"status": "invalid_output", "judgments": []})

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

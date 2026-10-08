"""Provider-free checks for the single-model supplementary probe."""
from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import codex_reader_astra_probe as probe


class AstraProbeTests(unittest.TestCase):
    def fake(self, root: Path, *, run: bool, failure: int | None = None):
        home = root / "fake-home"
        binary = home / ".local/bin/codex"
        binary.parent.mkdir(parents=True, exist_ok=True)
        binary.write_text("fake")
        binary.chmod(0o755)
        auth = home / ".codex/auth.json"
        auth.parent.mkdir()
        auth.write_text("{}")
        output = root / "results"
        calls = []

        def select(dialogue, cards, ledger, **kwargs):
            calls.append((copy.deepcopy(dialogue), copy.deepcopy(cards), kwargs))
            return {"selected_ids": [], "reason": "timeout" if len(calls) == failure else "abstained",
                    "provider_attempt": True, "cache_hit": False,
                    "usage": {"input_tokens": 20, "cached_input_tokens": 10,
                              "uncached_input_tokens": 10, "output_tokens": 2}, "elapsed_ms": 5}

        with patch.object(Path, "home", return_value=home), \
             patch.dict(os.environ, {"CODEX_HOME": str(auth.parent)}), \
             patch.object(probe.reader, "select", side_effect=select), \
             patch("builtins.print"):
            with patch.object(sys, "argv", ["astra", "--output", str(output)]):
                probe.main()
            prepared = json.loads((output / "results.json").read_text())
            if run:
                with patch.object(sys, "argv", ["astra", "--output", str(output), "--run"]):
                    probe.main()
                    with self.assertRaisesRegex(ValueError, "untouched prepared"):
                        probe.main()
        return prepared, json.loads((output / "results.json").read_text()), calls

    def test_prepare_is_provider_free_and_hashes_sources(self):
        with tempfile.TemporaryDirectory() as name:
            prepared, result, calls = self.fake(Path(name), run=False)
        self.assertEqual((prepared["status"], calls), ("prepared", []))
        self.assertEqual(prepared, result)
        self.assertEqual(len(prepared["files"]), 8)

    def test_one_ordered_pass_without_host_labels(self):
        with tempfile.TemporaryDirectory() as name:
            _, result, calls = self.fake(Path(name), run=True)
        self.assertEqual((result["status"], result["attempts"], len(calls)), ("completed", 12, 12))
        self.assertEqual([r["case"] for r in result["results"]], [f"c{i:02}" for i in range(1, 13)])
        self.assertEqual({call[2]["model"] for call in calls}, {probe.MODEL})
        self.assertEqual({call[2]["effort"] for call in calls}, {"low"})
        self.assertEqual({call[2]["workdir"] for call in calls}.__len__(), 1)
        for dialogue, cards, _ in calls:
            self.assertTrue(dialogue)
            self.assertTrue(all(set(card) == {"id", "summary", "source", "fingerprint"} for card in cards))
        self.assertEqual(result["summary"]["usage"]["input_tokens"], 240)

    def test_failure_stops_without_retry_or_grading_failure(self):
        with tempfile.TemporaryDirectory() as name:
            _, result, calls = self.fake(Path(name), run=True, failure=3)
        self.assertEqual((result["status"], result["attempts"], len(calls)), ("stopped", 3, 3))
        self.assertIsNone(result["results"][-1]["grade"])
        self.assertEqual(result["summary"]["valid"], 2)


if __name__ == "__main__":
    unittest.main()

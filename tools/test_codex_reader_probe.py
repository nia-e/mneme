"""No-provider tests for the evaluation-only multi-turn reader harness."""
from __future__ import annotations

import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import codex_reader_probe as probe


class CodexReaderProbeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.document = json.loads(probe.CASES.read_text(encoding="utf-8"))

    def test_frozen_multiturn_inventory(self):
        development = probe.inventory(self.document, "development")
        holdout = probe.inventory(self.document, "holdout")
        self.assertEqual((len(development), len(holdout)), (4, 4))
        self.assertEqual(sum(len(c["stages"]) for c in development + holdout), 18)
        self.assertTrue(all(2 <= len(c["stages"]) <= 3 for c in development + holdout))

    def test_dialogue_is_bounded_and_current_task_first_in_hook_cue(self):
        stage = self.document["cases"][0]["stages"][1]
        previous = [{"role": "user", "text": "old " * 200}]
        prompt = probe.hook_prompt(stage, previous)
        cue, fragment = probe.hook_cue(prompt, "previous fragment")
        self.assertIn(stage["scene"]["brief"][:30], fragment)
        self.assertIn("Recent task: previous fragment", cue)
        self.assertLessEqual(len(probe.dialogue_text(previous * 9).encode()),
                             probe.MAX_DIALOGUE_BYTES)

    def test_actor_retains_prior_actual_injections_not_other_answers(self):
        memory, visible = probe.actor_memory([
            {"context": "hook context stage one", "delivered_ids": ["a"]},
            {"context": "", "delivered_ids": []},
            {"context": "hook context stage three", "delivered_ids": ["b"]}])
        self.assertEqual(visible, ["a", "b"])
        self.assertIn("stage one", memory)
        self.assertIn("stage three", memory)
        with self.assertRaisesRegex(RuntimeError, "unfair seen suppression"):
            probe.actor_memory([{"context": "x" * (probe.MAX_ACTOR_MEMORY_BYTES + 1),
                                 "delivered_ids": ["a"]}])

    def test_reader_sees_only_native_cards_and_dialogue_with_seen(self):
        pool = {"outcome": "ok", "cards": [
            {"id": "a", "summary": "Alpha", "source": "fixture:a", "fingerprint": "fa"},
            {"id": "b", "summary": "Beta", "source": "fixture:b", "fingerprint": "fb"}],
            "observation": {"schema": 1, "learning": "disabled", "cards": [
                {"node_id": "a", "graph_path": [{"target": "b"}]},
                {"node_id": "b", "graph_path": None}]}}
        dialogue = [{"role": "user", "text": "Current task needs Alpha"}]
        seen = [("b", "fb")]
        calls = []
        def fake_select(messages, cards, ledger, **kwargs):
            calls.append((messages, cards, kwargs))
            return {"selected_ids": ["a"], "provider_attempt": False}
        with patch.object(probe.reader, "select", side_effect=fake_select):
            result, decision = probe.arm_result("reader", pool, dialogue, seen,
                Path("/tmp/unused"), False, Path("/tmp/codex"), None,
                Path("/tmp/empty"), {}, "gpt-5.6-sol", "low", "case:reader")
        self.assertEqual(calls[0][0], dialogue)
        self.assertEqual(calls[0][2]["seen"], tuple(seen))
        self.assertEqual(calls[0][1][0]["native"]["graph_path"], [{"target": "b"}])
        self.assertNotIn("expected_retention", repr(calls))
        self.assertEqual([c["id"] for c in result["cards"]], ["a"])
        self.assertEqual(result["observation"]["cards"], pool["observation"]["cards"][:1])
        self.assertIsNotNone(decision)

    def test_baseline_is_first_two_from_same_pool_even_when_seen(self):
        pool = {"outcome": "ok", "cards": [{"id": x, "summary": x}
                 for x in ("a", "b", "c")], "observation": None}
        result, decision = probe.arm_result("all", pool, [], [("a", "fa")],
            Path("/tmp/unused"), False, Path("/tmp/codex"), None,
            Path("/tmp/empty"), {}, "gpt-5.6-sol", "low", "case:all")
        self.assertEqual([c["id"] for c in result["cards"]], ["a", "b"])
        self.assertIsNone(decision)

    def test_stage_pool_is_read_once_then_replayed_three_arms(self):
        case = self.document["cases"][0]
        lookup = {c["id"]: c for c in self.document["cards"]}
        counts = {"native": 0, "hook": 0}
        workdirs = []
        first_id = case["seed_before_stage"][0]
        node_id = "0" * 26
        def fake_fixture(_case, _lookup, _arm, root, *_rest):
            project = root / "project"
            project.mkdir()
            return project, project / "db", {first_id: node_id}
        def fake_pool(*_args):
            counts["native"] += 1
            return {"result": {"outcome": "ok", "cards": [{"id": node_id,
                     "summary": "memory", "fingerprint": "f"}],
                     "observation": {"schema": 1, "learning": "disabled", "cards": []}},
                    "calls": [], "elapsed_ms": 1}
        def fake_hook(_project, _root, arm, index, *_args):
            counts["hook"] += 1
            workdirs.append(_args[9])
            context = "injected " + str(index) if arm != "off" else ""
            ids = [node_id] if context else []
            return {"context": context, "delivered_ids": ids, "selected_ids": ids,
                    "selector": None}
        with tempfile.TemporaryDirectory() as name, \
             patch.object(probe.transfer, "fixture_case", side_effect=fake_fixture), \
             patch.object(probe.transfer, "seed"), \
             patch.object(probe.transfer, "add_associations", return_value=[]), \
             patch.object(probe.transfer, "logical_cozo", return_value={"sha256": "same"}), \
             patch.object(probe, "native_pool", side_effect=fake_pool), \
             patch.object(probe, "hook_stage", side_effect=fake_hook):
            rows = list(probe.run_case(case, lookup, Path(name), Path("mnemed"),
                                       Path("mcp"), Path("codex"), None,
                                       Path("unused-ledger"), False, False,
                                       "gpt-5.6-sol", "low",
                                       {"actor_calls": 0, "actor_reuses": 0}))
        self.assertEqual(counts["native"], len(case["stages"]))
        self.assertEqual(counts["hook"], len(case["stages"]) * 3)
        self.assertEqual(len(rows), len(case["stages"]) * 3)
        self.assertEqual(len(set(workdirs)), 1)
        self.assertIn(node_id, rows[-1]["actor_visible_memory_ids"])

    def test_default_prepare_no_provider_and_no_overwrite(self):
        with tempfile.TemporaryDirectory() as name:
            output = Path(name) / "prepared.json"
            argv = ["probe", "--output", str(output)]
            with patch.object(sys, "argv", argv):
                probe.main()
            data = json.loads(output.read_text())
            self.assertEqual(data["status"], "prepared")
            self.assertEqual(data["results"], [])
            with patch.object(sys, "argv", argv), \
                 self.assertRaisesRegex(RuntimeError, "overwrite"):
                probe.main()

    def test_holdout_native_preflight_refused_before_binary_checks(self):
        with tempfile.TemporaryDirectory() as name:
            argv = ["probe", "--split", "holdout", "--preflight-development",
                    "--output", str(Path(name) / "out.json")]
            with patch.object(sys, "argv", argv), \
                 self.assertRaisesRegex(RuntimeError, "development-only"):
                probe.main()


if __name__ == "__main__":
    unittest.main()

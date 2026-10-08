"""No-provider contract tests for the finite reflexive transfer probe."""
from __future__ import annotations

import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import reflexive_transfer_probe as probe


class ProbeTests(unittest.TestCase):
    def test_counts_include_only_prescribed_shuffled_controls(self):
        document = json.loads(probe.CASES.read_text(encoding="utf-8"))
        development = probe.case_inventory(document, "development")
        all_cases = probe.case_inventory(document, "all")
        self.assertEqual(probe.planned_counts(development), {
            "cases": 7, "stages": 9, "actor_calls": 28,
            "assessor_calls_max": 19, "shuffled_controls": 1})
        self.assertEqual(probe.planned_counts(all_cases), {
            "cases": 11, "stages": 13, "actor_calls": 41,
            "assessor_calls_max": 28, "shuffled_controls": 2})

    def test_score_harm_order_and_detour(self):
        host = {"accepted_actions": [["wait", "publish"]], "damaging_actions": [],
                "order_hazard": {"action": "publish", "requires_prior": "wait"}}
        commands = ["wait", "publish", "inspect"]
        self.assertEqual(probe.score(["wait", "publish"], host, commands)["success"], True)
        self.assertEqual(probe.score(["publish", "wait"], host, commands)["harm"], True)
        self.assertEqual(probe.score(["inspect", "wait", "publish"], host, commands)["detours"], 1)
        self.assertEqual(probe.score(["inspect", "wait", "publish"], host, commands)["harm"], False)

    def test_actor_projection_excludes_host_and_fixture_metadata(self):
        scene = {"brief": "Publish the board", "commands": ["wait", "publish"]}
        prompt = probe.actor_prompt(scene, "hook context marker")
        self.assertIn("hook context marker", prompt)
        self.assertIn("Publish the board", prompt)
        self.assertNotIn("accepted_actions", prompt)
        self.assertNotIn("memory_role", prompt)
        self.assertNotIn("graph_on", prompt)
        self.assertEqual(probe.actor_schema(scene["commands"])["properties"]["actions"]["items"]["enum"],
                         scene["commands"])

    def test_native_proxy_changes_only_depth(self):
        recorded = []
        class FakeClient:
            def __init__(self, *args, **kwargs):
                pass
            def call_tool(self, name, arguments):
                recorded.append((name, arguments))
                return {"schema": "mneme.context.v5",
                        "observation": {"schema": 1, "cards": []}} if name == "recall_context" else {}
        def fake_collect(*_args, **_kwargs):
            client = probe.hook_recall.McpClient("http://127.0.0.1:7777/")
            client.call_tool("recall_context", {"db": "project", "depth": 2,
                                                "observe": True, "k": 2, "max_nodes": 4})
            return {"outcome": "empty", "cards": []}
        native = []
        with patch.object(probe.mcp_client, "McpClient", FakeClient), \
             patch.object(probe.hook_recall, "_collect", fake_collect):
            with probe.recall_override(Path("/tmp/cfg"), Path("/tmp/project"), 0, native):
                probe.hooks._recall_cards({}, "cue")
        self.assertEqual(recorded[0][1], {"db": "project", "depth": 0,
                                          "observe": True, "k": 2, "max_nodes": 4})
        self.assertEqual(native[0]["request"], recorded[0][1])
        self.assertEqual(set(native[0]["lane_ids"]), {"primary", "episodes"})

    def test_graph_inventory_matches_main_arms_and_shuffles_only_control(self):
        case = {"associations": {"genuine": [["anchor", "lesson"]],
                                  "shuffled": [["anchor", "distractor"]]}}
        ids = {"anchor": "a", "lesson": "b", "distractor": "c"}
        calls = []
        with patch.object(probe, "cli", side_effect=lambda *args, **kwargs: calls.append(args)):
            for arm in ("off", "graph_off", "graph_on"):
                edges = probe.add_associations(case, arm, Path("."), Path("db"), Path("mnemed"), {}, ids)
                self.assertEqual(edges, [{"from": "a", "to": "b", "kind": "associative"}])
            edges = probe.add_associations(case, "shuffled", Path("."), Path("db"), Path("mnemed"), {}, ids)
            self.assertEqual(edges, [{"from": "a", "to": "c", "kind": "associative"}])
        self.assertEqual(len(calls), 4)

    def test_parse_events_rejects_tools(self):
        raw = b'{"type":"turn.completed","usage":{"input_tokens":4}}\n'
        self.assertEqual(probe.parse_events(raw)["usage"], {"input_tokens": 4})
        bad = b'{"type":"item.completed","item":{"type":"command_execution"}}\n'
        with self.assertRaises(RuntimeError):
            probe.parse_events(bad)

    def test_preflight_refuses_holdout_without_touching_store(self):
        with tempfile.TemporaryDirectory() as name:
            with self.assertRaisesRegex(RuntimeError, "development-only"):
                probe.preflight_development([{"split": "holdout"}], {}, Path("x"), Path("y"),
                                            {}, Path(name) / "out.json")


if __name__ == "__main__":
    unittest.main()

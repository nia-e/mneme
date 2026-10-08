"""Provider-free fixtures, actor separation, scoring and preparation contracts."""
from __future__ import annotations

import contextlib
from copy import deepcopy
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import async_task_cases as probe


def tiny():
    return {"version": 1, "cards": [{"key": "memory", "summary": "Earlier experience.",
            "body": "HOST_MEMORY_BODY", "kind": "semantic", "thread": None}],
            "cases": [{"id": "example", "split": "development", "family": "HOST_FAMILY",
             "scene": {"prompt": "Inspect the job and choose a plan.",
                       "files": {"job.txt": "FILE_CONTENT_NOT_INITIAL_PROMPT"},
                       "actions": ["inspect", "apply", "discard"], "max_actions": 2,
                       "tool_policy": "allowed"},
             "memory_keys": ["memory"], "edges": [],
             "host": {"accepted_plans": [["inspect", "apply"]], "deferred_plans": [],
                      "forbidden_actions": ["discard"],
                      "required_memory": ["memory"], "irrelevant_memory": [],
                      "memory_expectation": "needed", "information_gap": "HOST_GAP",
                      "decision_rationale": "HOST_GOLD"},
             "timing": {"kind": "organic_tools", "decision_event": "first_submission",
                        "notes": "HOST_TIMING"}}]}


class CaseTests(unittest.TestCase):
    def test_pack_split_and_diversity(self):
        document = probe.load()
        counts = probe.inventory(document)
        self.assertEqual(counts["cases"], 10)
        self.assertEqual(counts["splits"], {"development": 6, "holdout": 4})
        self.assertEqual(counts["base_actor_calls_planned"], 30)
        self.assertEqual(counts["provider_calls_made"], 0)
        self.assertFalse(counts["native_retrieval_performed"])
        self.assertGreaterEqual(counts["no_tool_cases"], 1)
        self.assertGreaterEqual(sum(c["host"]["memory_expectation"] == "unnecessary"
                                    for c in document["cases"]), 2)
        self.assertTrue(any(c["kind"] == "episode" for c in document["cards"]))
        self.assertTrue(any(c["edges"] for c in document["cases"]))

    def test_projection_excludes_gold_memories_and_file_contents(self):
        case = tiny()["cases"][0]
        payload = probe.actor_scene(case)
        text = json.dumps(payload)
        for secret in ("HOST_", "FILE_CONTENT_NOT_INITIAL_PROMPT", "memory_keys", "accepted_plans"):
            self.assertNotIn(secret, text)
        self.assertEqual(payload["file_paths"], ["job.txt"])
        self.assertEqual(probe.workspace_files(case)["job.txt"], "FILE_CONTENT_NOT_INITIAL_PROMPT")
        payload["actions"].clear()
        self.assertEqual(len(case["scene"]["actions"]), 3)

    def test_every_accepted_plan_scores_but_wrong_and_unknown_are_distinct(self):
        for case in probe.load()["cases"]:
            for plan in case["host"]["accepted_plans"]:
                self.assertEqual(probe.grade(case, plan),
                                 {"valid": True, "success": True, "forbidden_action": False,
                                  "resolution": "deferred" if plan in case["host"]["deferred_plans"] else "ready"})
        case = tiny()["cases"][0]
        self.assertEqual(probe.grade(case, ["discard"]),
                         {"valid": True, "success": False, "forbidden_action": True,
                          "resolution": "incorrect"})
        self.assertFalse(probe.grade(case, ["made-up"])["valid"])
        self.assertTrue(probe.grade(case, ["discard", "made-up"])["forbidden_action"])
        self.assertFalse(probe.grade(case, "inspect")["valid"])
        self.assertFalse(probe.grade(case, ["inspect"] * 3)["valid"])

    def test_safe_deferral_is_not_same_progress_as_ready_plan(self):
        case = tiny()["cases"][0]
        case["host"]["accepted_plans"].append(["inspect"])
        case["host"]["deferred_plans"].append(["inspect"])
        self.assertTrue(probe.grade(case, ["inspect"])["success"])
        self.assertEqual(probe.grade(case, ["inspect"])["resolution"], "deferred")
        self.assertEqual(probe.grade(case, ["inspect", "apply"])["resolution"], "ready")

    def test_first_submission_not_corrected_or_checkpoint_answer(self):
        def event(actions):
            return {"type": "item.completed", "item": {"type": "agent_message",
                    "text": json.dumps({"actions": actions, "rationale": "choice"})}}
        rows = [{"type": "item.completed", "item": {"type": "agent_message", "text": "working"}},
                event(["discard"]), event(["inspect", "apply"]),
                {"type": "item.completed", "item": {"type": "agent_message", "text": "checkpoint none"}}]
        answer = probe.first_submission(tiny()["cases"][0], rows)
        self.assertEqual(answer["event_index"], 1)
        self.assertFalse(answer["grade"]["success"])
        self.assertEqual(answer["memory_visibility"], "unknown")
        self.assertEqual(answer["timing"], "event_order_only")
        rows[1] = event(["made-up"])
        self.assertFalse(probe.first_submission(tiny()["cases"][0], rows)["grade"]["valid"])
        rows[1]["item"]["text"] = json.dumps({"actions": ["inspect", "apply"]})
        self.assertFalse(probe.first_submission(tiny()["cases"][0], rows)["grade"]["valid"])

    def test_no_submission_is_not_success(self):
        self.assertIsNone(probe.first_submission(tiny()["cases"][0], []))

    def test_oversized_or_malformed_first_plan_cannot_be_replaced(self):
        def event(raw):
            return {"type": "item.completed", "item": {"type": "agent_message", "text": raw}}
        corrected = event(json.dumps({"actions": ["inspect", "apply"], "rationale": "later"}))
        for raw in (json.dumps({"actions": ["discard"], "rationale": "x" * 4100}),
                    '{"actions":["discard"],', '```json\n{"actions":["discard"]}\n```',
                    '[{"actions":["discard"],"rationale":"choice"}]',
                    '{"wrapper":{"actions":["discard"]}}',
                    'Plan: {"actions":["discard"],"rationale":"first"}',
                    '{"actions":["discard"],"actions":["inspect","apply"],"rationale":"duplicate"}'):
            with self.subTest(raw=raw[:50]):
                result = probe.first_submission(tiny()["cases"][0], [event(raw), corrected])
                self.assertEqual(result["event_index"], 0)
                self.assertFalse(result["grade"]["valid"])
                self.assertFalse(result["grade"]["success"])
                self.assertIsNone(result["answer"])

    def test_invalid_fixtures(self):
        changes = [lambda d: d.update(version=True),
                   lambda d: d["cards"].append(deepcopy(d["cards"][0])),
                   lambda d: d["cases"][0]["scene"]["files"].update({"../oops": "bad"}),
                   lambda d: d["cases"][0]["scene"].update(max_actions=True),
                   lambda d: d["cases"][0]["memory_keys"].append("missing"),
                   lambda d: d["cases"][0]["host"].update(accepted_plans=[["discard"]]),
                   lambda d: d["cases"][0]["host"].update(deferred_plans=[["discard"]]),
                   lambda d: d["cases"][0]["host"].update(irrelevant_memory=["memory"]),
                   lambda d: d["cases"][0]["host"].update(memory_expectation="unnecessary"),
                   lambda d: d["cases"][0]["scene"].update(tool_policy="none"),
                   lambda d: d["cards"][0].update(summary="x" * 701),
                   lambda d: d["cards"][0].update(thread="semantic-has-no-thread")]
        for change in changes:
            with self.subTest(change=change):
                document = tiny()
                change(document)
                with self.assertRaises(ValueError):
                    probe.validate(document)

    def test_edge_validation(self):
        document = tiny()
        second = deepcopy(document["cards"][0]); second["key"] = "second"
        document["cards"].append(second)
        case = document["cases"][0]; case["memory_keys"].append("second")
        edge = {"from": "memory", "to": "second", "kind": "associative", "weight": .5}
        case["edges"] = [edge]
        probe.validate(document)
        for weight in (True, float("nan"), 0, 1.1):
            edge["weight"] = weight
            with self.assertRaises(ValueError):
                probe.validate(document)

    def test_help_no_mode_unknown_arguments_and_holdout_are_inert(self):
        for args, code in ((["--help"], 0), ([], 2), (["--run"], 2)):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as caught:
                    probe.main(args)
            self.assertEqual(caught.exception.code, code)
        holdout = next(c for c in probe.load()["cases"] if c["split"] == "holdout")
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as caught:
            probe.main(["--actor-case", holdout["id"]])
        self.assertEqual(caught.exception.code, 2)

    def test_preparation_frozen_and_never_overwritten(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "receipt.json"
            receipt = probe.prepare(output)
            self.assertEqual(receipt["status"], "prepared_not_run")
            self.assertEqual(len(receipt["manifest"]), 4)
            before = output.read_bytes()
            with self.assertRaisesRegex(ValueError, "refusing to overwrite"):
                probe.prepare(output)
            self.assertEqual(output.read_bytes(), before)


if __name__ == "__main__":
    unittest.main()

"""Black-box tests: no model calls, remote machines, or live memory stores."""

import datetime
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from heartbeat_test_support import HeartbeatHarness, WRAPPER


class HeartbeatTests(HeartbeatHarness, unittest.TestCase):
    def test_progress_creates_complete_run_artifacts(self):
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, (payload, process.stderr))
        receipt_path, receipt = self.receipt()
        self.assertEqual(receipt["status"], "completed")
        self.assertEqual(receipt["schema"], "mneme.workshop.receipt.v3")
        self.assertEqual(receipt["cycle_class"], "background")
        self.assertEqual(receipt["outcome"], "progress")
        self.assertEqual(receipt["stage"], "work")
        self.assertTrue(receipt["orientation_valid"])
        self.assertEqual(receipt["invocations_attempted"], 2)
        self.assertEqual(receipt["codex_exit_code"], 0)
        self.assertEqual(payload["run_id"], receipt_path.parent.name)
        for name in ("orientation/prompt.txt", "orientation/events.jsonl",
                     "orientation/stderr.log", "orientation/result.json",
                     "work/prompt.txt", "work/events.jsonl", "work/stderr.log",
                     "work/result.json", "result.json", "receipt.json"):
            self.assertTrue((receipt_path.parent / name).is_file(), name)
        self.assertEqual(json.loads((receipt_path.parent / "result.json").read_text()), self.result())
        orientation, work = self.invocations()
        self.assertEqual((orientation["stage"], work["stage"]), ("orientation", "work"))
        for invocation in (orientation, work):
            self.assertIn("exec", invocation["args"])
            self.assertIn("--json", invocation["args"])
            self.assertIn("--output-schema", invocation["args"])
        self.assertIn("Explore one bounded question.", orientation["prompt"])
        state = json.loads((self.root / "state.json").read_text())
        self.assertEqual(state["schema"], "mneme.workshop.state.v2")
        self.assertIn("interactive", state)
        self.assertEqual(state["previous"]["summary"], self.result()["summary"])

    def test_execution_policy_default_is_workspace_in_both_stages(self):
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual([entry["stage"] for entry in self.invocations()], ["orientation", "work"])
        for entry in self.invocations():
            args = entry["args"]
            self.assertEqual(args[args.index("--sandbox") + 1], "workspace-write")
            self.assertEqual(args[args.index("-a") + 1], "never")
            self.assertIn("Do not alter runs/", entry["prompt"])
            self.assertIn("harness, services", entry["prompt"])
            self.assertIn("Keep ordinary files inside this workshop", entry["prompt"])

    def test_execution_policy_owner_matches_prompt_and_sandbox_in_both_stages(self):
        process, payload = self.call(options=("--execution-policy", "owner"))
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual([entry["stage"] for entry in self.invocations()], ["orientation", "work"])
        for entry in self.invocations():
            args = entry["args"]
            self.assertEqual(args[args.index("--sandbox") + 1], "danger-full-access")
            self.assertEqual(args[args.index("-a") + 1], "never")
            self.assertIn("I run this machine", entry["prompt"])
            self.assertIn("rules, configuration, runner", entry["prompt"])
            self.assertIn("holds the single-session lock", entry["prompt"])
            self.assertNotIn("Do not alter runs/", entry["prompt"])
            self.assertNotIn("harness, services", entry["prompt"])
            self.assertNotIn("Keep ordinary files inside this workshop", entry["prompt"])
            self.assertNotIn("report\nblocked", entry["prompt"])
            self.assertNotIn("appoint yourself new jobs", entry["prompt"])
            self.assertIn("If memory is unavailable, I use the context I have", entry["prompt"])

    def test_both_policies_allow_opportunistic_signal_reads_not_extra_structured_replies(self):
        heartbeat = self.heartbeat_module()
        for policy in ("workspace", "owner"):
            for orientation in (None, self.orientation()):
                with self.subTest(policy=policy, orientation=orientation):
                    text = heartbeat.prompt("agenda", "journal", None, "runs/test", {}, [],
                                            orientation, execution_policy=policy)
                    normalized = " ".join(text.split())
                    self.assertIn("session start or compact is an opportunity, not an interruption or obligation",
                                  normalized)
                    self.assertIn("During work", text)
                    self.assertIn("Signal history", text)
                    self.assertIn("send_message for newer messages when useful", normalized)
                    self.assertIn("initial claimed batch eligible for structured replies", normalized)
                    self.assertNotIn("New arrivals wait for a later cycle", text)

    def test_unknown_execution_policy_refuses_before_side_effects(self):
        process = subprocess.run(self.command(options=("--execution-policy", "alien")),
                                 env=self.environment(), capture_output=True, text=True, timeout=8)
        self.assertNotEqual(process.returncode, 0)
        self.assertIn("invalid choice", process.stderr)
        self.assertEqual(self.receipts(), [])
        self.assertFalse((self.root / "state.json").exists())
        self.assertFalse((self.root / ".workshop.lock").exists())
        self.assertEqual(self.invocations(), [])

        heartbeat = self.heartbeat_module()
        args = SimpleNamespace(root=self.root, codex=self.fake, command="run", timeout=6,
                               max_log_bytes=1024, max_starts=4, execution_policy="alien")
        with self.assertRaisesRegex(heartbeat.Refusal, "Unknown execution policy"):
            heartbeat.execute(args)
        self.assertFalse((self.root / ".workshop.lock").exists())

    def test_rest_and_blocked_are_valid_results(self):
        for state in ("rest", "blocked"):
            with self.subTest(state=state):
                root = self.make_root(state)
                env = self.environment(result=self.result(status=state, artifacts=[]),
                                       FAKE_ORIENTATION_RESULT=json.dumps(self.orientation(
                                           status=state, task="")))
                process = subprocess.run(self.command(root=root), env=env, capture_output=True,
                                         text=True, timeout=8)
                payload = json.loads(process.stdout)
                self.assertEqual(process.returncode, 0, payload)
                self.assertEqual(self.receipt(root)[1]["status"], "completed")
                self.assertEqual(self.receipt(root)[1]["outcome"], state)
                self.assertEqual(self.receipt(root)[1]["invocations_attempted"], 1)
                self.assertFalse((self.receipt(root)[0].parent / "work").exists())

    def test_orientation_contract_is_strict_and_never_starts_work_if_invalid(self):
        bad = [self.orientation(status="progress"), self.orientation(status="work", task=""),
               self.orientation(summary=""), self.orientation(summary="x" * 2001),
               self.orientation(task="x" * 4001), self.orientation(effort="ultra"),
               self.orientation(next_wake_seconds=899),
               self.orientation(next_wake_seconds=86401),
               self.orientation(next_wake_seconds=True),
               self.orientation(extra="nope")]
        missing = self.orientation()
        del missing["effort"]
        bad.extend((missing, []))
        for index, orientation in enumerate(bad):
            with self.subTest(index=index):
                root = self.make_root("bad-orientation-" + str(index))
                process, payload = self.call(root=root, orientation=orientation)
                self.assertEqual(process.returncode, 1, payload)
                self.assertEqual(self.receipt(root)[1]["status"], "invalid_result")
                self.assertEqual(self.invocations()[-1]["stage"], "orientation")

    def test_effort_presets_have_fixed_model_and_reasoning_argv(self):
        expected = {"brief": ("gpt-6-sol", "low"),
                    "normal": ("gpt-6-sol", "medium"),
                    "deep": ("gpt-6-sol", "xhigh"),
                    "maximal": ("gpt-6-astra", "ultra")}
        for effort, (model, reasoning) in expected.items():
            with self.subTest(effort=effort):
                root = self.make_root("effort-" + effort)
                process, payload = self.call(root=root,
                                             orientation=self.orientation(effort=effort))
                self.assertEqual(process.returncode, 0, payload)
                orientation, work = self.invocations()[-2:]
                self.assertEqual(orientation["args"][orientation["args"].index("-m") + 1],
                                 "gpt-6-sol")
                self.assertEqual(work["args"][work["args"].index("-m") + 1], model)
                self.assertTrue(any("model_reasoning_effort" in arg and reasoning in arg
                                    for arg in work["args"]))

    def test_not_due_skips_child_but_new_event_wakes_it(self):
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        (self.base / "invocation.json").unlink()
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual(len(self.receipts()), 1)
        self.assertEqual(self.invocations(), [])
        self.enqueue()
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual(len(self.receipts()), 2)
        self.assertEqual(len(self.invocations()), 2)

    def test_failed_run_cools_down_but_later_event_wakes_it(self):
        self.enqueue(event_id="claimed-before-failure")
        process, payload = self.call(mode="failed")
        self.assertEqual(process.returncode, 1, payload)
        queue = self.queue_records()
        self.assertEqual(queue[0]["status"], "unfinished")
        (self.base / "invocation.json").unlink()
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual(len(self.receipts()), 1)
        self.assertEqual(self.invocations(), [])
        self.enqueue()
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual(len(self.receipts()), 2)

    def test_failed_work_event_survives_unrelated_cycle_and_gets_final_reply(self):
        self.enqueue(event_id="original-question")
        failed, payload = self.call(FAKE_WORK_MODE="failed")
        self.assertEqual(failed.returncode, 1, payload)
        first_path, first_receipt = self.receipt()
        first_run_id = first_receipt["run_id"]
        self.assertTrue(first_receipt["orientation_valid"])
        self.assertEqual(self.queue_records()[0]["status"], "unfinished")
        self.assertIn(first_run_id, self.queue_records()[0]["detail"])
        skipped, payload = self.call()
        self.assertEqual(skipped.returncode, 0, payload)
        self.assertEqual(payload["status"], "not_due")
        self.assertEqual(len(self.receipts()), 1)

        self.enqueue(event_id="unrelated-question")
        rested, payload = self.call(orientation=self.orientation(
            status="rest", task="", summary="Unrelated rest cycle."))
        self.assertEqual(rested.returncode, 0, payload)
        self.assertEqual(json.loads((self.root / "state.json").read_text())
                         ["previous"]["summary"], "Unrelated rest cycle.")
        records = self.queue_records()
        self.assertEqual([item["status"] for item in records], ["unfinished", "delivered"])

        self.due()
        answer = self.reply(event_id="original-question", text="Answer after retry.")
        completed, payload = self.call(result=self.result(replies=[answer]))
        self.assertEqual(completed.returncode, 0, payload)
        self.assertEqual(len(self.receipts()), 3)
        final_path = self.receipts()[-1]
        final_receipt = json.loads(final_path.read_text())
        self.assertEqual(final_receipt["event_ids"], [
            {"source": "test", "event_id": "original-question"}])
        self.assertEqual(json.loads((final_path.parent / "result.json").read_text())["replies"],
                         [answer])
        self.assertIn(first_run_id, self.invocations()[-2]["prompt"])
        self.assertEqual(self.queue_records()[0]["status"], "delivered")

    def test_stale_orientation_valid_run_cools_down_and_retries_same_event(self):
        self.enqueue(event_id="stale-question")
        completed, payload = self.call()
        self.assertEqual(completed.returncode, 0, payload)
        old_path, old_receipt = self.receipt()
        old_receipt["status"] = "running"
        old_receipt.pop("outcome", None)
        old_path.write_text(json.dumps(old_receipt))
        skipped, payload = self.call()
        self.assertEqual(skipped.returncode, 0, payload)
        self.assertEqual(payload["status"], "not_due")
        self.assertEqual(json.loads(old_path.read_text())["status"], "interrupted")
        self.assertEqual(self.queue_records()[0]["status"], "unfinished")
        self.due()
        answer = self.reply(event_id="stale-question", text="Recovered answer.")
        retried, payload = self.call(result=self.result(replies=[answer]))
        self.assertEqual(retried.returncode, 0, payload)
        final_path = self.receipts()[-1]
        self.assertEqual(json.loads(final_path.read_text())["event_ids"], [
            {"source": "test", "event_id": "stale-question"}])
        self.assertEqual(json.loads((final_path.parent / "result.json").read_text())["replies"],
                         [answer])

    def test_corrupt_state_refuses_without_starting_child(self):
        (self.root / "state.json").write_text("not JSON")
        process, payload = self.call()
        self.assertEqual(process.returncode, 1, payload)
        self.assertEqual(payload["status"], "refused")
        self.assertEqual(self.receipts(), [])
        self.assertEqual(self.invocations(), [])

    def test_legacy_v1_receipt_remains_readable(self):
        legacy = self.root / "runs" / "legacy"
        legacy.mkdir(parents=True)
        (legacy / "receipt.json").write_text(json.dumps({
            "schema": "mneme.workshop.receipt.v1", "run_id": "legacy",
            "status": "completed",
            "started_at": (datetime.datetime.now(datetime.timezone.utc)
                           - datetime.timedelta(days=1)).isoformat()}))
        (legacy / "prompt.txt").write_text("Old flat artifact layout.")
        process, payload = self.call("status")
        self.assertEqual(process.returncode, 0, payload)
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual(len(self.receipts()), 2)

    def test_legacy_v1_state_and_v2_receipt_load_then_write_new_schemas(self):
        old_state = {"schema": "mneme.workshop.state.v1",
                     "next_due_at": "1970-01-01T00:00:00+00:00",
                     "previous": {"summary": "Old background context.",
                                  "next_step": "Continue old question."},
                     "event_after_sequence": 0}
        (self.root / "state.json").write_text(json.dumps(old_state))
        yesterday = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=1)
        self.fake_receipt("legacy-v2", "mneme.workshop.receipt.v2", None, yesterday)
        status, payload = self.call("status", options=self.signal_options())
        self.assertEqual(status.returncode, 0, payload)
        self.assertEqual(payload["starts_today"], 0)
        run, payload = self.call(options=self.signal_options())
        self.assertEqual(run.returncode, 0, payload)
        self.assertEqual(json.loads((self.root / "state.json").read_text())["schema"],
                         "mneme.workshop.state.v2")
        self.assertEqual([json.loads(path.read_text())["schema"] for path in self.receipts()]
                         .count("mneme.workshop.receipt.v3"), 1)
        self.assertIn("Old background context.", self.invocations()[0]["prompt"])

    def test_legacy_queue_requires_explicit_upgrade_before_work(self):
        self.enqueue(event_id="legacy-read-only")
        queue_path = self.root / "events.json"
        prior = json.loads(queue_path.read_text())
        prior["schema"] = "mneme.workshop.events.v3"
        queue_path.write_text(json.dumps(prior))
        original = queue_path.read_bytes()
        run, _payload = self.call()
        self.assertNotEqual(run.returncode, 0)
        self.assertEqual(queue_path.read_bytes(), original)
        self.assertFalse((self.root / "state.json").exists())
        self.assertEqual(self.receipts(), [])
        self.assertEqual(self.invocations(), [])

    def test_current_queue_fence_survives_state_publication_crash_and_retries_event(self):
        self.enqueue(event_id="fenced-original")
        queue_path = self.root / "events.json"
        prior = json.loads(queue_path.read_text())
        prior["schema"] = "mneme.workshop.events.v2"
        queue_path.write_text(json.dumps(prior))
        heartbeat = self.heartbeat_module()
        heartbeat.wake.upgrade(self.root)
        args = SimpleNamespace(root=self.root, codex=self.fake, command="run", timeout=6,
                               max_log_bytes=1024, max_starts=4, signal_source=None,
                               max_interactive_starts=48,
                               max_interactive_starts_per_hour=8)
        write_json = heartbeat.write_json
        def crash_on_state(path, value):
            if path == self.root / "state.json":
                raise OSError("simulated crash before state publication")
            return write_json(path, value)
        with patch.object(heartbeat, "write_json", side_effect=crash_on_state):
            with self.assertRaisesRegex(OSError, "simulated crash"):
                heartbeat.execute(args)
        after_crash = json.loads(queue_path.read_text())
        self.assertEqual(after_crash["schema"], heartbeat.wake.SCHEMA)
        self.assertEqual(after_crash["records"], prior["records"])
        self.assertFalse((self.root / "state.json").exists())
        self.assertEqual(self.receipts(), [])
        self.assertEqual(self.invocations(), [])
        retried, payload = self.call(result=self.result(replies=[self.reply(
            event_id="fenced-original")]))
        self.assertEqual(retried.returncode, 0, payload)
        self.assertEqual(len(self.receipts()), 1)
        self.assertEqual(json.loads(self.receipts()[0].read_text())["event_ids"], [
            {"source": "test", "event_id": "fenced-original"}])

    def test_strict_result_fields_and_types(self):
        invalid = [
            {"status": "done"}, {"summary": 123}, {"summary": "x" * 4001},
            {"next_step": None}, {"next_step": "x" * 2001},
            {"artifacts": "artifacts/note.md"}, {"artifacts": [1]},
            {"artifacts": ["artifacts/note.md"] * 17},
            {"memory_candidates": "a memory"}, {"memory_candidates": [False]},
            {"memory_candidates": ["x"] * 9}, {"memory_candidates": ["x" * 2001]},
            {"next_wake_seconds": 899}, {"next_wake_seconds": 86401},
            {"next_wake_seconds": True},
            {"extra": "not in schema"},
        ]
        for index, changes in enumerate(invalid):
            with self.subTest(changes=changes):
                self.assert_rejected(self.result(**changes), self.make_root("invalid-" + str(index)))
        missing = self.result()
        del missing["next_step"]
        self.assert_rejected(missing, self.make_root("missing-field"))
        self.assert_rejected([], self.make_root("not-object"))

    def test_output_schemas_leave_artifact_uniqueness_to_local_validation(self):
        # The real Codex work request rejected uniqueItems before generation.
        # Keep provider schemas compatible without weakening result admission.
        for name in ("orientation.schema.json", "result.schema.json"):
            schema = json.loads(WRAPPER.with_name(name).read_text())
            pending = [schema]
            while pending:
                item = pending.pop()
                if isinstance(item, dict):
                    self.assertNotIn("uniqueItems", item, name)
                    pending.extend(item.values())
                elif isinstance(item, list):
                    pending.extend(item)
        self.assert_rejected(self.result(artifacts=["artifacts/note.md"] * 2), self.root)

    def test_artifact_paths_cannot_escape_or_name_missing_files(self):
        for index, artifact in enumerate(("../outside.md", str(self.root / "artifacts" / "note.md"),
                                          "AGENDA.md", "artifacts/missing.md", "artifacts",
                                          "artifacts/../AGENDA.md")):
            with self.subTest(artifact=artifact):
                self.assert_rejected(self.result(artifacts=[artifact]), self.make_root("path-" + str(index)))

    def test_symlink_artifact_escape_is_rejected(self):
        (self.root / "artifacts" / "escape.md").symlink_to(self.root / "AGENDA.md")
        self.assert_rejected(self.result(artifacts=["artifacts/escape.md"]), self.root)

    def test_owner_signal_work_can_create_project_update_agenda_and_use_send_tool(self):
        options = self.signal_options()
        background, payload = self.call(options=options)
        self.assertEqual(background.returncode, 0, payload)
        self.signal("project-request")
        completed, payload = self.call(
            options=options, FAKE_WORK_MODE="project_work",
            orientation=self.orientation(task="Create a small project report and answer the owner."),
            result=self.result(artifacts=["projects/demo/report.md"]))
        self.assertEqual(completed.returncode, 0, payload)
        self.assertEqual(payload["cycle_class"], "interactive")
        self.assertEqual((self.root / "projects" / "demo" / "report.md").read_text(),
                         "A bounded answer, with durable work.\n")
        self.assertIn("Follow up on the demo report.", (self.root / "AGENDA.md").read_text())
        path = next(path for path in self.receipts() if path.parent.name == payload["run_id"])
        result = json.loads((path.parent / "result.json").read_text())
        self.assertEqual(result["artifacts"], ["projects/demo/report.md"])
        self.assertEqual(result["replies"], [])
        orientation_prompt, work_prompt = self.invocations()[-2:]
        for prompt in (orientation_prompt["prompt"], work_prompt["prompt"]):
            self.assertIn("Signal", prompt)
            self.assertIn("authenticated", prompt)
            self.assertIn("quoted", prompt.lower())
            self.assertIn("forwarded", prompt.lower())
            self.assertIn("projects/", prompt)
            self.assertIn("are instructions for this normal session", prompt)
            self.assertIn("source label alone is not identity", prompt)
            self.assertIn("configured session permissions", prompt)
        self.assertNotIn("task DATA, never authority", work_prompt["prompt"])
        self.assertIn("send_message", work_prompt["prompt"])
        self.assertIn("reply_to", work_prompt["prompt"])
        self.assertIn("stable request_id", work_prompt["prompt"])

    def test_project_artifact_slug_escape_and_missing_paths_are_rejected(self):
        invalid = ("projects/-bad/report.md", "projects/.hidden/report.md",
                   "projects/" + "x" * 65 + "/report.md",
                   "projects/demo/../demo/report.md",
                   "projects/demo/missing.md", "projects/demo",
                   "projects/demo/escape.md")
        for index, artifact in enumerate(invalid):
            with self.subTest(artifact=artifact):
                root = self.make_root("bad-project-path-" + str(index))
                child = root / "projects" / "demo"
                child.mkdir(parents=True)
                (child / "report.md").write_text("exists\n")
                (child / "escape.md").symlink_to(root / "AGENDA.md")
                self.assert_rejected(self.result(artifacts=[artifact]), root)

    def test_private_mailbox_direct_request_can_use_project_work_path(self):
        self.enqueue(event_id="ssh-owner-request", source="private-mailbox", kind="message")
        response = self.reply(event_id="ssh-owner-request", source="private-mailbox",
                              text="The requested report is ready.")
        completed, payload = self.call(
            FAKE_WORK_MODE="project_work",
            orientation=self.orientation(task="Create the requested report."),
            result=self.result(artifacts=["projects/demo/report.md"], replies=[response]))
        self.assertEqual(completed.returncode, 0, payload)
        self.assertEqual(json.loads(self.receipt()[0].read_text())["event_ids"], [
            {"source": "private-mailbox", "event_id": "ssh-owner-request"}])
        self.assertIn("SSH", self.invocations()[0]["prompt"])

    def test_invalid_or_missing_result(self):
        for mode in ("malformed", "oversized", "no_result"):
            with self.subTest(mode=mode):
                root = self.make_root(mode)
                process, payload = self.call(root=root, mode=mode)
                self.assertEqual(process.returncode, 1, payload)
                receipt_path, receipt = self.receipt(root)
                self.assertEqual(receipt["status"], "output_limit" if mode == "oversized" else "invalid_result")
                if mode == "oversized":
                    self.assertLessEqual((receipt_path.parent / "orientation" / "result.json").stat().st_size, 65536)

    def test_failed_codex_records_its_exit_code(self):
        process, payload = self.call(mode="failed")
        self.assertEqual(process.returncode, 1, payload)
        self.assertEqual(self.receipt()[1]["status"], "failed")
        self.assertEqual(self.receipt()[1]["codex_exit_code"], 7)

    def test_pause_resume_and_status_never_start_codex(self):
        process, payload = self.call("pause")
        self.assertEqual(process.returncode, 0, payload)
        self.assertTrue((self.root / "PAUSED").is_file())
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual(payload["status"], "paused")
        self.assertEqual(self.receipts(), [])
        process, payload = self.call("status")
        self.assertEqual(process.returncode, 0, payload)
        self.assertFalse((self.base / "invocation.json").exists())
        process, payload = self.call("resume")
        self.assertEqual(process.returncode, 0, payload)
        self.assertFalse((self.root / "PAUSED").exists())
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual(len(self.receipts()), 1)

    def test_nonblocking_lock_skips_overlapping_run(self):
        with (self.root / ".workshop.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual(payload["status"], "busy")
        self.assertEqual(self.receipts(), [])
        self.assertFalse((self.base / "invocation.json").exists())

    def test_daily_start_quota_includes_failed_runs(self):
        options = ("--max-starts", "1")
        process, payload = self.call(options=options, mode="failed")
        self.assertEqual(process.returncode, 1, payload)
        process, payload = self.call(options=options)
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual(payload["status"], "quota")
        self.assertEqual(len(self.receipts()), 1)

    def test_previous_utc_day_does_not_use_todays_quota(self):
        options = ("--max-starts", "1")
        process, payload = self.call(options=options)
        self.assertEqual(process.returncode, 0, payload)
        path, receipt = self.receipt()
        yesterday = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=1)
        receipt["started_at"] = yesterday.isoformat()
        path.write_text(json.dumps(receipt))
        self.due()
        process, payload = self.call(options=options)
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual(len(self.receipts()), 2)

    def test_signal_burst_runs_after_background_daily_quota_is_exhausted(self):
        options = self.signal_options("--max-starts", "1")
        first, payload = self.call(options=options)
        self.assertEqual(first.returncode, 0, payload)
        self.assertEqual(self.receipt()[1]["cycle_class"], "background")
        self.signal()
        second, payload = self.call(options=options)
        self.assertEqual(second.returncode, 0, payload)
        receipts = [json.loads(path.read_text()) for path in self.receipts()]
        self.assertEqual([item["cycle_class"] for item in receipts],
                         ["background", "interactive"])
        self.assertEqual(receipts[1]["event_ids"], [
            {"source": "signal-owner", "event_id": "signal-question"}])
        self.assertEqual(receipts[1]["interactive_budget_mode"], "bounded")
        self.assertEqual(receipts[1]["max_starts_utc_day"], 48)
        self.assertEqual(receipts[1]["max_starts_rolling_hour"], 8)

    def test_default_private_mailbox_message_remains_background(self):
        self.mailbox()
        completed, payload = self.call()
        self.assertEqual(completed.returncode, 0, payload)
        receipt = self.receipt()[1]
        self.assertEqual(receipt["cycle_class"], "background")
        self.assertEqual(receipt["event_ids"], [
            {"source": "private-mailbox", "event_id": "mailbox-request"}])

    def test_mailbox_inbox_runs_when_background_quota_is_exhausted(self):
        options = ("--mailbox-inbox", "--max-starts", "1")
        background, payload = self.call(options=options)
        self.assertEqual(background.returncode, 0, payload)
        before = json.loads((self.root / "state.json").read_text())
        self.mailbox("mailbox-over-bg-quota")
        interactive, payload = self.call(options=options)
        self.assertEqual(interactive.returncode, 0, payload)
        self.assertEqual(payload["cycle_class"], "interactive")
        receipt = json.loads(self.receipts()[-1].read_text())
        self.assertEqual(receipt["event_ids"], [
            {"source": "private-mailbox", "event_id": "mailbox-over-bg-quota"}])
        after = json.loads((self.root / "state.json").read_text())
        for key in ("next_due_at", "previous", "event_after_sequence"):
            self.assertEqual(after[key], before[key], key)
        status, payload = self.call("status", options=options)
        self.assertEqual(status.returncode, 0, payload)
        self.assertEqual(payload["starts_today"], 1)
        self.assertEqual(payload["interactive_starts_today"], 1)

    def test_mailbox_inbox_routes_exact_messages_not_technical_hints(self):
        options = ("--mailbox-inbox",)
        self.enqueue(event_id="technical-wake", source="local-shell", kind="notification")
        self.mailbox("direct-request")
        interactive, payload = self.call(options=options)
        self.assertEqual(interactive.returncode, 0, payload)
        receipt = self.receipt()[1]
        self.assertEqual(receipt["cycle_class"], "interactive")
        self.assertEqual(receipt["event_ids"], [
            {"source": "private-mailbox", "event_id": "direct-request"}])
        prompt = self.invocations()[-2]["prompt"]
        self.assertIn("Signal", prompt)
        self.assertIn("SSH", prompt)
        self.assertIn("peer-agent", prompt)
        self.assertIn("do not impersonate the human owner", prompt)
        background, payload = self.call(options=options)
        self.assertEqual(background.returncode, 0, payload)
        receipt = json.loads(self.receipts()[-1].read_text())
        self.assertEqual(receipt["cycle_class"], "background")
        self.assertEqual(receipt["event_ids"], [
            {"source": "local-shell", "event_id": "technical-wake"}])

    def test_mailbox_only_inbox_does_not_steal_signal_burst(self):
        options = ("--mailbox-inbox",)
        background, payload = self.call(options=options)
        self.assertEqual(background.returncode, 0, payload)
        self.signal("signal-not-opted-in")
        skipped, payload = self.call(options=options)
        self.assertEqual(skipped.returncode, 0, payload)
        self.assertEqual(len(self.receipts()), 1)
        self.assertEqual(self.queue_records()[0]["status"], "pending")
        self.mailbox("mailbox-opted-in")
        interactive, payload = self.call(options=options)
        self.assertEqual(interactive.returncode, 0, payload)
        receipt = json.loads(self.receipts()[-1].read_text())
        self.assertEqual(receipt["cycle_class"], "interactive")
        self.assertEqual(receipt["event_ids"], [
            {"source": "private-mailbox", "event_id": "mailbox-opted-in"}])
        self.assertEqual(self.queue_records()[0]["status"], "pending")

    def test_signal_and_mailbox_share_bounded_interactive_batch(self):
        options = self.signal_options("--mailbox-inbox")
        background, payload = self.call(options=options)
        self.assertEqual(background.returncode, 0, payload)
        self.signal("signal-a")
        self.mailbox("mailbox-a")
        self.signal("signal-b")
        self.mailbox("mailbox-b")
        interactive, payload = self.call(options=options)
        self.assertEqual(interactive.returncode, 0, payload)
        receipt = json.loads(self.receipts()[-1].read_text())
        self.assertEqual(receipt["cycle_class"], "interactive")
        self.assertEqual(receipt["event_ids"], [
            {"source": "signal-owner", "event_id": "signal-a"},
            {"source": "private-mailbox", "event_id": "mailbox-a"},
            {"source": "signal-owner", "event_id": "signal-b"},
            {"source": "private-mailbox", "event_id": "mailbox-b"}])

    def test_signal_cycle_preserves_background_schedule_and_handoff(self):
        options = self.signal_options()
        first, payload = self.call(options=options)
        self.assertEqual(first.returncode, 0, payload)
        before = json.loads((self.root / "state.json").read_text())
        self.signal()
        second, payload = self.call(options=options,
                                    result=self.result(summary="Private chat response.",
                                                       next_step="Await more Signal input."))
        self.assertEqual(second.returncode, 0, payload)
        after = json.loads((self.root / "state.json").read_text())
        for field in ("next_due_at", "previous", "event_after_sequence"):
            self.assertEqual(after[field], before[field], field)
        self.assertEqual(after["interactive"]["previous"]["summary"], "Private chat response.")
        self.assertEqual(json.loads(self.receipts()[-1].read_text())["cycle_class"], "interactive")

    def test_signal_cycle_never_claims_non_signal_or_wrong_kind(self):
        options = self.signal_options()
        first, payload = self.call(options=options)
        self.assertEqual(first.returncode, 0, payload)
        self.signal("chat")
        chat, payload = self.call(options=options)
        self.assertEqual(chat.returncode, 0, payload)
        self.assertEqual(json.loads(self.receipts()[-1].read_text())["event_ids"], [
            {"source": "signal-owner", "event_id": "chat"}])
        self.enqueue(event_id="wrong-kind", source="signal-owner", kind="notice")
        self.enqueue(event_id="background-event", source="test", kind="notice")
        self.due()
        background, payload = self.call(options=options)
        self.assertEqual(background.returncode, 0, payload)
        last = json.loads(self.receipts()[-1].read_text())
        self.assertEqual(last["cycle_class"], "background")
        self.assertEqual(last["event_ids"], [
            {"source": "test", "event_id": "background-event"}])
        records = self.queue_records()
        self.assertEqual(next(r for r in records if r["event"]["event_id"] == "wrong-kind")
                         ["status"], "pending")

    def test_owner_inbox_wins_over_simultaneous_due_background(self):
        options = self.signal_options()
        first, payload = self.call(options=options)
        self.assertEqual(first.returncode, 0, payload)
        self.signal("simultaneous-chat")
        self.enqueue(event_id="simultaneous-background")
        self.due()
        background, payload = self.call(options=options)
        self.assertEqual(background.returncode, 0, payload)
        receipt = json.loads(self.receipts()[-1].read_text())
        self.assertEqual(receipt["cycle_class"], "interactive")
        self.assertEqual(receipt["event_ids"], [
            {"source": "signal-owner", "event_id": "simultaneous-chat"}])
        chat, payload = self.call(options=options)
        self.assertEqual(chat.returncode, 0, payload)
        receipt = json.loads(self.receipts()[-1].read_text())
        self.assertEqual(receipt["cycle_class"], "background")
        self.assertEqual(receipt["event_ids"], [
            {"source": "test", "event_id": "simultaneous-background"}])

    def test_signal_opt_in_disabled_does_not_create_interactive_cycle(self):
        first, payload = self.call()
        self.assertEqual(first.returncode, 0, payload)
        self.signal()
        second, payload = self.call()
        self.assertEqual(second.returncode, 0, payload)
        self.assertTrue(all(json.loads(p.read_text())["cycle_class"] == "background"
                            for p in self.receipts()))

    def test_signal_burst_never_creates_empty_interactive_cycle(self):
        options = self.signal_options()
        first, payload = self.call(options=options)
        self.assertEqual(first.returncode, 0, payload)
        count = len(self.receipts())
        second, payload = self.call(options=options)
        self.assertEqual(second.returncode, 0, payload)
        self.assertEqual(len(self.receipts()), count)
        self.assertEqual(len(self.invocations()), 2)

    def test_paused_workshop_does_not_start_signal_cycle(self):
        options = self.signal_options()
        self.signal()
        paused, payload = self.call("pause", options=options)
        self.assertEqual(paused.returncode, 0, payload)
        skipped, payload = self.call(options=options)
        self.assertEqual(skipped.returncode, 0, payload)
        self.assertEqual(payload["status"], "paused")
        self.assertEqual(self.receipts(), [])

    def test_failed_interactive_cycle_counts_toward_hourly_quota(self):
        options = self.signal_options("--max-interactive-starts-per-hour", "1")
        background, payload = self.call(options=options)
        self.assertEqual(background.returncode, 0, payload)
        self.signal("failed-chat")
        failed, payload = self.call(options=options, FAKE_WORK_MODE="failed")
        self.assertEqual(failed.returncode, 1, payload)
        self.assertEqual(json.loads(self.receipts()[-1].read_text())["cycle_class"], "interactive")
        self.signal("fresh-chat")
        before = len(self.receipts())
        denied, payload = self.call(options=options)
        self.assertEqual(denied.returncode, 0, payload)
        self.assertEqual(payload["status"], "quota")
        self.assertEqual(payload["cycle_class"], "interactive")
        self.assertEqual(len(self.receipts()), before)
        status, payload = self.call("status", options=options)
        self.assertEqual(status.returncode, 0, payload)
        self.assertEqual(payload["starts_today"], 1)
        self.assertEqual(payload["interactive_starts_last_hour"], 1)

    def test_ninth_interactive_hourly_start_is_denied(self):
        options = self.signal_options()
        background, payload = self.call(options=options)
        self.assertEqual(background.returncode, 0, payload)
        now = datetime.datetime.now(datetime.timezone.utc)
        for index in range(8):
            self.fake_receipt(f"hourly-{index}", "mneme.workshop.receipt.v3",
                              "interactive", now,
                              status="failed" if index == 0 else "completed")
        self.signal()
        denied, payload = self.call(options=options)
        self.assertEqual(denied.returncode, 0, payload)
        self.assertEqual(payload["status"], "quota")
        self.assertEqual(len(self.receipts()), 9)
        status, payload = self.call("status", options=options)
        self.assertEqual(status.returncode, 0, payload)
        self.assertEqual(payload["interactive_starts_last_hour"], 8)

    def test_forty_ninth_interactive_daily_start_is_denied(self):
        heartbeat = self.heartbeat_module()
        now = datetime.datetime(2026, 9, 26, 12, tzinfo=datetime.timezone.utc)
        args = SimpleNamespace(root=self.root, codex=self.fake, command="run", timeout=6,
                               max_starts=4, max_log_bytes=1024, signal_source="signal-owner",
                               max_interactive_starts=48, max_interactive_starts_per_hour=8)
        # Keep today's receipts outside the rolling hour. Using real midnight
        # lets the hourly cap mask a broken daily cap during UTC's first hour.
        with patch.object(heartbeat, "utc_now", return_value=now), patch.dict(os.environ, self.environment()):
            payload, code = heartbeat.execute(args)
            self.assertEqual(code, 0, payload)
            for index in range(48):
                self.fake_receipt(f"daily-{index}", "mneme.workshop.receipt.v3",
                                  "interactive", now - datetime.timedelta(hours=2),
                                  status="failed" if index == 0 else "completed")
            self.signal()
            invocations = self.invocations()
            payload, code = heartbeat.execute(args)
            self.assertEqual(code, 0, payload)
            self.assertEqual(payload, {
                "status": "quota", "cycle_class": "interactive",
                "note": "Interactive UTC-day or rolling-hour start limit reached"})
            self.assertEqual(len(self.receipts()), 49)
            self.assertEqual(self.invocations(), invocations)
            args.command = "status"
            payload, code = heartbeat.execute(args)
            self.assertEqual(code, 0, payload)
            self.assertEqual(payload["starts_today"], 1)
            self.assertEqual(payload["interactive_starts_today"], 48)
            self.assertEqual(payload["interactive_starts_last_hour"], 0)

    def test_unlimited_interactive_bypasses_both_caps_but_retains_counters(self):
        options = self.signal_options("--unlimited-interactive")
        background, payload = self.call(options=options)
        self.assertEqual(background.returncode, 0, payload)
        now = datetime.datetime.now(datetime.timezone.utc)
        for index in range(48):
            self.fake_receipt(f"limit-{index}", "mneme.workshop.receipt.v3",
                              "interactive", now,
                              status="failed" if index == 0 else "completed")
        self.signal("above-both-caps")
        started, payload = self.call(options=options)
        self.assertEqual(started.returncode, 0, payload)
        self.assertEqual(payload["cycle_class"], "interactive")
        receipt = json.loads(next(path for path in self.receipts()
                                  if path.parent.name == payload["run_id"]).read_text())
        self.assertEqual(receipt["event_ids"], [
            {"source": "signal-owner", "event_id": "above-both-caps"}])
        self.assertEqual(receipt["interactive_budget_mode"], "unlimited")
        self.assertIsNone(receipt["max_starts_utc_day"])
        self.assertIsNone(receipt["max_starts_rolling_hour"])
        status, payload = self.call("status", options=options)
        self.assertEqual(status.returncode, 0, payload)
        self.assertEqual(payload["starts_today"], 1)
        self.assertEqual(payload["interactive_starts_today"], 49)
        self.assertEqual(payload["interactive_starts_last_hour"], 49)

    def test_unlimited_interactive_accepts_mailbox_only_opt_in(self):
        options = ("--mailbox-inbox", "--unlimited-interactive")
        background, payload = self.call(options=options)
        self.assertEqual(background.returncode, 0, payload)
        now = datetime.datetime.now(datetime.timezone.utc)
        for index in range(48):
            self.fake_receipt(f"mailbox-limit-{index}", "mneme.workshop.receipt.v3",
                              "interactive", now)
        self.mailbox("above-mailbox-caps")
        completed, payload = self.call(options=options)
        self.assertEqual(completed.returncode, 0, payload)
        self.assertEqual(payload["cycle_class"], "interactive")
        receipt = json.loads(next(path for path in self.receipts()
                                  if path.parent.name == payload["run_id"]).read_text())
        self.assertEqual(receipt["event_ids"], [
            {"source": "private-mailbox", "event_id": "above-mailbox-caps"}])
        self.assertEqual(receipt["interactive_budget_mode"], "unlimited")
        self.assertIsNone(receipt["max_starts_utc_day"])
        self.assertIsNone(receipt["max_starts_rolling_hour"])
        status, payload = self.call("status", options=options)
        self.assertEqual(status.returncode, 0, payload)
        self.assertEqual(payload["interactive_starts_today"], 49)
        self.assertEqual(payload["interactive_starts_last_hour"], 49)

    def test_unlimited_interactive_requires_an_inbox_before_any_start(self):
        process = subprocess.run(self.command(options=("--unlimited-interactive",)),
                                 env=self.environment(), capture_output=True, text=True, timeout=4)
        self.assertNotEqual(process.returncode, 0)
        self.assertEqual(self.receipts(), [])
        self.assertEqual(self.invocations(), [])

    def test_unlimited_interactive_keeps_cooldown_and_pause(self):
        options = self.signal_options("--unlimited-interactive")
        background, payload = self.call(options=options)
        self.assertEqual(background.returncode, 0, payload)
        self.signal("old-unlimited")
        failed, payload = self.call(options=options, FAKE_WORK_MODE="failed")
        self.assertEqual(failed.returncode, 1, payload)
        count = len(self.receipts())
        skipped, payload = self.call(options=options)
        self.assertEqual(skipped.returncode, 0, payload)
        self.assertEqual(len(self.receipts()), count)
        self.signal("fresh-unlimited")
        paused, payload = self.call("pause", options=options)
        self.assertEqual(paused.returncode, 0, payload)
        skipped, payload = self.call(options=options)
        self.assertEqual(skipped.returncode, 0, payload)
        self.assertEqual(payload["status"], "paused")
        self.assertEqual(len(self.receipts()), count)
        resumed, payload = self.call("resume", options=options)
        self.assertEqual(resumed.returncode, 0, payload)
        retried, payload = self.call(options=options)
        self.assertEqual(retried.returncode, 0, payload)
        self.assertEqual(json.loads(self.receipts()[-1].read_text())["event_ids"], [
            {"source": "signal-owner", "event_id": "fresh-unlimited"}])

    def test_unlimited_interactive_does_not_expand_background_budget(self):
        options = self.signal_options("--unlimited-interactive")
        first, payload = self.call(options=options)
        self.assertEqual(first.returncode, 0, payload)
        now = datetime.datetime.now(datetime.timezone.utc)
        for index in range(3):
            self.fake_receipt(f"background-quota-{index}", "mneme.workshop.receipt.v3",
                              "background", now)
        self.signal("chat-over-background-cap")
        chat, payload = self.call(options=options)
        self.assertEqual(chat.returncode, 0, payload)
        self.assertEqual(payload["cycle_class"], "interactive")
        self.due()
        before = len(self.receipts())
        denied, payload = self.call(options=options)
        self.assertEqual(denied.returncode, 0, payload)
        self.assertEqual(payload["status"], "quota")
        self.assertEqual(payload["cycle_class"], "background")
        self.assertEqual(len(self.receipts()), before)

    def test_rolling_hour_crosses_utc_midnight_but_daily_counter_resets(self):
        heartbeat = self.heartbeat_module()
        now = datetime.datetime(2026, 9, 26, 0, 15, tzinfo=datetime.timezone.utc)
        entries = []
        for index in range(8):
            receipt = {"schema": "mneme.workshop.receipt.v3", "cycle_class": "interactive",
                       "status": "failed" if index == 0 else "completed",
                       "started_at": (now - datetime.timedelta(minutes=30)).isoformat()}
            entries.append((Path(f"prior-{index}"), receipt))
        entries.append((Path("background-v2"), {"schema": "mneme.workshop.receipt.v2",
                          "status": "completed", "started_at": now.isoformat()}))
        counts = heartbeat.quota_counts(entries, now)
        self.assertEqual(counts, {"background_today": 1,
                                  "interactive_today": 0,
                                  "interactive_last_hour": 8})

    def test_old_receipt_classes_remain_background_for_quota(self):
        heartbeat = self.heartbeat_module()
        now = datetime.datetime.now(datetime.timezone.utc)
        entries = [(Path("v1"), {"schema": "mneme.workshop.receipt.v1",
                                  "status": "completed", "started_at": now.isoformat()}),
                   (Path("v2"), {"schema": "mneme.workshop.receipt.v2",
                                  "status": "failed", "started_at": now.isoformat()}),
                   (Path("v3"), {"schema": "mneme.workshop.receipt.v3",
                                  "cycle_class": "interactive", "status": "completed",
                                  "started_at": now.isoformat()})]
        self.assertEqual(heartbeat.quota_counts(entries, now),
                         {"background_today": 2,
                          "interactive_today": 1,
                          "interactive_last_hour": 1})

    def test_failed_interactive_cooldown_ignores_old_batch_but_new_burst_wakes(self):
        options = self.signal_options()
        background, payload = self.call(options=options)
        self.assertEqual(background.returncode, 0, payload)
        self.signal("old-chat")
        failed, payload = self.call(options=options, FAKE_WORK_MODE="failed")
        self.assertEqual(failed.returncode, 1, payload)
        self.assertEqual(self.queue_records()[0]["status"], "unfinished")
        count = len(self.receipts())
        skipped, payload = self.call(options=options)
        self.assertEqual(skipped.returncode, 0, payload)
        self.assertEqual(len(self.receipts()), count)
        self.signal("new-chat")
        completed, payload = self.call(options=options)
        self.assertEqual(completed.returncode, 0, payload)
        event_ids = json.loads(self.receipts()[-1].read_text())["event_ids"]
        self.assertEqual(event_ids, [{"source": "signal-owner", "event_id": "new-chat"}])
        self.assertEqual(self.queue_records()[0]["status"], "unfinished")

    def test_stale_interactive_recovery_only_cools_interactive_lane(self):
        options = self.signal_options()
        background, payload = self.call(options=options)
        self.assertEqual(background.returncode, 0, payload)
        self.signal("stale-chat")
        chat, payload = self.call(options=options)
        self.assertEqual(chat.returncode, 0, payload)
        state_before = json.loads((self.root / "state.json").read_text())
        old_path = self.receipts()[-1]
        old = json.loads(old_path.read_text())
        old["status"] = "running"
        old.pop("outcome", None)
        old_path.write_text(json.dumps(old))
        skipped, payload = self.call(options=options)
        self.assertEqual(skipped.returncode, 0, payload)
        self.assertEqual(payload["status"], "not_due")
        self.assertEqual(json.loads(old_path.read_text())["status"], "interrupted")
        self.assertEqual(self.queue_records()[0]["status"], "unfinished")
        state_after = json.loads((self.root / "state.json").read_text())
        for field in ("next_due_at", "previous", "event_after_sequence"):
            self.assertEqual(state_after[field], state_before[field], field)
        self.due(lane="interactive")
        retried, payload = self.call(options=options)
        self.assertEqual(retried.returncode, 0, payload)
        self.assertEqual(json.loads(self.receipts()[-1].read_text())["event_ids"], [
            {"source": "signal-owner", "event_id": "stale-chat"}])

    def test_timeout_kills_spawned_child(self):
        process, payload = self.call(options=("--timeout", "0.75"), mode="child")
        self.assertEqual(process.returncode, 1, payload)
        self.assertTrue((self.base / "ready").exists(), "fake child did not start")
        self.assertEqual(self.receipt()[1]["status"], "timeout")
        time.sleep(1.7)
        self.assertFalse((self.base / "sentinel").exists(), "child outlived timeout cleanup")

    def test_bounded_stdout_and_stderr(self):
        for mode, filename in (("stdout_flood", "events.jsonl"), ("stderr_flood", "stderr.log")):
            with self.subTest(mode=mode):
                root = self.make_root(mode)
                process, payload = self.call(root=root, options=("--max-log-bytes", "1024"), mode=mode)
                self.assertEqual(process.returncode, 1, payload)
                receipt_path, receipt = self.receipt(root)
                self.assertIn("output_limit", json.dumps({"payload": payload, "receipt": receipt}))
                self.assertLessEqual((receipt_path.parent / "orientation" / filename).stat().st_size, 1024)

    def test_stdout_and_stderr_have_independent_budgets(self):
        process, payload = self.call(options=("--max-log-bytes", "1024"), mode="both_logs")
        self.assertEqual(process.returncode, 0, payload)
        path, receipt = self.receipt()
        self.assertEqual(receipt["status"], "completed")
        for stage in ("orientation", "work"):
            for filename in ("events.jsonl", "stderr.log"):
                self.assertGreaterEqual((path.parent / stage / filename).stat().st_size, 900)
                self.assertLessEqual((path.parent / stage / filename).stat().st_size, 1024)

    def test_sigterm_interrupts_run_and_kills_spawned_child(self):
        process = subprocess.Popen(self.command(options=("--timeout", "6")),
                                   env=self.environment("child"), stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True)
        self.addCleanup(lambda: process.poll() is None and process.kill())
        deadline = time.monotonic() + 4
        while not (self.base / "ready").exists() and time.monotonic() < deadline:
            if process.poll() is not None:
                break
            time.sleep(0.02)
        self.assertTrue((self.base / "ready").exists(), "fake child did not start")
        process.send_signal(signal.SIGTERM)
        stdout, stderr = process.communicate(timeout=5)
        self.assertNotEqual(process.returncode, 0, (stdout, stderr))
        self.assertEqual(self.receipt()[1]["status"], "interrupted")
        self.assertEqual(len(stdout.splitlines()), 1, stdout)
        time.sleep(1.7)
        self.assertFalse((self.base / "sentinel").exists(), "child outlived interruption cleanup")

    def test_stale_running_receipt_is_recovered_before_next_start(self):
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        path, receipt = self.receipt()
        receipt["status"] = "running"
        receipt["codex_exit_code"] = None
        path.write_text(json.dumps(receipt))
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual(json.loads(path.read_text())["status"], "interrupted")
        self.assertEqual(payload["status"], "not_due")
        self.due()
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual(len(self.receipts()), 2)

    def test_killed_wrapper_does_not_release_lock_held_by_codex(self):
        process = subprocess.Popen(self.command(options=("--timeout", "6")),
                                   env=self.environment("wait"), stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True)
        child_pid = None
        try:
            deadline = time.monotonic() + 4
            while not (self.base / "ready").exists() and time.monotonic() < deadline:
                if process.poll() is not None:
                    break
                time.sleep(0.02)
            self.assertTrue((self.base / "ready").exists(), "fake Codex did not start")
            child_pid = int((self.base / "ready").read_text())
            process.kill()
            process.communicate(timeout=3)
            next_process, payload = self.call()
            self.assertEqual(next_process.returncode, 0, payload)
            self.assertEqual(payload["status"], "busy")
            receipt_path, receipt = self.receipt()
            self.assertEqual(receipt["status"], "running")
        finally:
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=3)
            if child_pid is not None:
                try:
                    os.killpg(child_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        deadline = time.monotonic() + 3
        with (self.root / ".workshop.lock").open("a") as lock:
            while True:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except BlockingIOError:
                    self.assertLess(time.monotonic(), deadline, "Codex child still holds lock")
                    time.sleep(0.02)
        recovered, payload = self.call()
        self.assertEqual(recovered.returncode, 0, payload)
        self.assertEqual(json.loads(receipt_path.read_text())["status"], "interrupted")

    def test_pause_interrupts_an_active_run_without_starting_another(self):
        process = subprocess.Popen(self.command(options=("--timeout", "6")),
                                   env=self.environment("wait"), stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True)
        self.addCleanup(lambda: process.poll() is None and process.kill())
        deadline = time.monotonic() + 4
        while not (self.base / "ready").exists() and time.monotonic() < deadline:
            if process.poll() is not None:
                break
            time.sleep(0.02)
        self.assertTrue((self.base / "ready").exists(), "fake Codex did not start")
        pause, payload = self.call("pause")
        self.assertEqual(pause.returncode, 0, payload)
        stdout, stderr = process.communicate(timeout=5)
        self.assertNotEqual(process.returncode, 0, (stdout, stderr))
        self.assertEqual(self.receipt()[1]["status"], "interrupted")
        skipped, payload = self.call()
        self.assertEqual(skipped.returncode, 0, payload)
        self.assertEqual(payload["status"], "paused")
        self.assertEqual(len(self.receipts()), 1)

    def test_corrupt_receipt_refuses_instead_of_resetting_quota(self):
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        path, _ = self.receipt()
        path.write_text("not valid JSON")
        (self.base / "invocation.json").unlink()
        process, payload = self.call()
        self.assertEqual(process.returncode, 1, payload)
        self.assertEqual(len(self.receipts()), 1)
        self.assertFalse((self.base / "invocation.json").exists())

    def test_wrong_receipt_status_type_produces_json_refusal(self):
        for index, status in enumerate(([], {})):
            with self.subTest(status=status):
                root = self.make_root("bad-receipt-type-" + str(index))
                process, payload = self.call(root=root)
                self.assertEqual(process.returncode, 0, payload)
                path, receipt = self.receipt(root)
                receipt["status"] = status
                path.write_text(json.dumps(receipt))
                (self.base / "invocation.json").unlink()
                process, payload = self.call(root=root)
                self.assertEqual(process.returncode, 1, payload)
                self.assertEqual(payload["status"], "refused")
                self.assertNotIn("Traceback", process.stderr)
                self.assertEqual(len(self.receipts(root)), 1)
                self.assertFalse((self.base / "invocation.json").exists())

    def test_exact_run_count_cap_refuses_new_start_but_allows_status(self):
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        (self.base / "invocation.json").unlink()
        sys.path.insert(0, str(WRAPPER.parent))
        self.addCleanup(lambda: sys.path.remove(str(WRAPPER.parent)))
        specification = importlib.util.spec_from_file_location("workshop_under_test", WRAPPER)
        heartbeat = importlib.util.module_from_spec(specification)
        specification.loader.exec_module(heartbeat)
        args = SimpleNamespace(root=self.root, codex=self.fake, command="status", timeout=6,
                               max_log_bytes=1024, max_starts=4)
        with patch.object(heartbeat, "MAX_RUNS", 1):
            status, code = heartbeat.execute(args)
            self.assertEqual(code, 0)
            self.assertEqual(status["starts_today"], 1)
            self.due()
            args.command = "run"
            with self.assertRaisesRegex(heartbeat.Refusal, "ledger is full"):
                heartbeat.execute(args)
        self.assertEqual(len(self.receipts()), 1)
        self.assertFalse((self.base / "invocation.json").exists())

    def test_aggregate_log_budget_refuses_without_deleting_old_runs(self):
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        path, _ = self.receipt()
        events = path.parent / "orientation" / "events.jsonl"
        with events.open("r+b") as stream:
            stream.truncate(64 * 1024 * 1024)
        (self.base / "invocation.json").unlink()
        self.due()
        process, payload = self.call()
        self.assertEqual(process.returncode, 1, payload)
        self.assertEqual(len(self.receipts()), 1)
        self.assertEqual(events.stat().st_size, 64 * 1024 * 1024)
        self.assertFalse((self.base / "invocation.json").exists())

    def test_relative_and_absent_roots_are_rejected(self):
        for root in (Path("relative-workshop"), self.base / "absent"):
            with self.subTest(root=root):
                process = subprocess.run(self.command(root=root), env=self.environment(),
                                         capture_output=True, text=True, timeout=4)
                self.assertNotEqual(process.returncode, 0, process.stdout)
        self.assertFalse((self.base / "absent").exists())

    def test_symlinked_root_is_canonicalized(self):
        alias = self.base / "alias"
        alias.symlink_to(self.root, target_is_directory=True)
        process, payload = self.call(root=alias)
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual(self.receipt()[1]["status"], "completed")
        invocation = self.invocations()[0]
        self.assertNotIn(str(alias), invocation["prompt"])

    def test_event_during_work_does_not_interrupt_child(self):
        process = subprocess.Popen(self.command(options=("--timeout", "6")),
                                   env=self.environment(FAKE_WORK_MODE="wait"),
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(lambda: process.poll() is None and process.kill())
        deadline = time.monotonic() + 4
        while not (self.base / "ready").exists() and time.monotonic() < deadline:
            if process.poll() is not None:
                break
            time.sleep(0.02)
        self.assertTrue((self.base / "ready").exists(), "fake work did not start")
        self.assertEqual(self.invocations()[-1]["stage"], "work")
        self.enqueue(event_id="arrived-during-work")
        time.sleep(0.2)
        self.assertIsNone(process.poll(), "event interrupted in-flight work")
        pause, payload = self.call("pause")
        self.assertEqual(pause.returncode, 0, payload)
        stdout, stderr = process.communicate(timeout=5)
        self.assertNotEqual(process.returncode, 0, (stdout, stderr))
        self.assertEqual(self.receipt()[1]["status"], "interrupted")

    def test_rest_reply_is_published_for_claimed_event(self):
        self.enqueue()
        response = self.reply()
        process, payload = self.call(
            orientation=self.orientation(status="rest", task="", replies=[response]))
        self.assertEqual(process.returncode, 0, payload)
        path, receipt = self.receipt()
        self.assertEqual(receipt["outcome"], "rest")
        self.assertEqual(receipt["event_ids"], [{"source": "test", "event_id": "new-information"}])
        self.assertEqual(json.loads((path.parent / "result.json").read_text())["replies"], [response])
        self.assertEqual(len(self.invocations()), 1)

    def test_work_reply_is_published_only_from_final_result(self):
        self.enqueue()
        draft = self.reply(text="Orientation draft; not publishable.")
        final = self.reply(text="Final answer after doing the work.")
        process, payload = self.call(
            orientation=self.orientation(replies=[draft]), result=self.result(replies=[final]))
        self.assertEqual(process.returncode, 0, payload)
        path, receipt = self.receipt()
        self.assertEqual(receipt["event_ids"], [{"source": "test", "event_id": "new-information"}])
        self.assertEqual(json.loads((path.parent / "result.json").read_text())["replies"], [final])

    def test_failed_work_never_publishes_orientation_draft(self):
        self.enqueue()
        process, payload = self.call(
            orientation=self.orientation(replies=[self.reply()]), FAKE_WORK_MODE="failed")
        self.assertEqual(process.returncode, 1, payload)
        path, receipt = self.receipt()
        self.assertEqual(receipt["status"], "failed")
        self.assertFalse((path.parent / "result.json").exists())

    def test_replies_must_bind_to_claimed_batch_with_strict_shape(self):
        invalid = [
            [self.reply(event_id="forged")],
            [self.reply(), self.reply()],
            "not an array", [False], [self.reply(text="")],
            [self.reply(text="x" * 4001)], [self.reply(source="x" * 65)],
            [self.reply(event_id="x" * 129)], [self.reply(extra="field")],
        ]
        for index, replies in enumerate(invalid):
            with self.subTest(index=index):
                root = self.make_root("invalid-reply-" + str(index))
                self.enqueue(root)
                process, payload = self.call(root=root, result=self.result(replies=replies))
                self.assertEqual(process.returncode, 1, payload)
                path, receipt = self.receipt(root)
                self.assertEqual(receipt["status"], "invalid_result")
                self.assertFalse((path.parent / "result.json").exists())

    def test_orientation_replies_are_validated_before_work(self):
        self.enqueue()
        process, payload = self.call(orientation=self.orientation(
            replies=[self.reply(event_id="unknown")]))
        self.assertEqual(process.returncode, 1, payload)
        path, receipt = self.receipt()
        self.assertEqual(receipt["status"], "invalid_result")
        self.assertEqual(len(self.invocations()), 1)
        self.assertFalse((path.parent / "work").exists())

    def test_legacy_signal_orientation_reply_remains_valid(self):
        options = self.signal_options()
        self.call(options=options)
        self.signal("owner-hello")
        process, payload = self.call(options=options, orientation=self.orientation(
            status="rest", task="", replies=[self.reply(event_id="owner-hello", source="signal-owner")]))
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual(json.loads(self.receipts()[-1].read_text())["status"], "completed")
        self.assertEqual(self.invocations()[-1]["stage"], "orientation")

    def test_reply_to_queued_but_unclaimed_event_is_rejected(self):
        for number in range(5):
            self.enqueue(event_id=f"batch-{number}")
        process, payload = self.call(result=self.result(replies=[self.reply(event_id="batch-4")]))
        self.assertEqual(process.returncode, 1, payload)
        path, receipt = self.receipt()
        self.assertEqual(receipt["status"], "invalid_result")
        self.assertEqual(len(receipt["event_ids"]), 4)
        self.assertFalse((path.parent / "result.json").exists())

    def test_cli_bounds_are_enforced(self):
        for options in (("--timeout", "0"), ("--timeout", "-1"), ("--timeout", "nan"),
                        ("--timeout", "none"), ("--timeout", "1e999"),
                        ("--timeout", "inf"), ("--max-starts", "0"), ("--max-starts", "5"),
                        ("--max-log-bytes", "1023"), ("--max-log-bytes", "1048577"),
                        ("--max-interactive-starts", "0"),
                        ("--max-interactive-starts", "49"),
                        ("--max-interactive-starts-per-hour", "0"),
                        ("--max-interactive-starts-per-hour", "9"),
                        ("--signal-source", "not-signal-owner")):
            with self.subTest(options=options):
                process = subprocess.run(self.command(options=options), env=self.environment(),
                                         capture_output=True, text=True, timeout=4)
                self.assertNotEqual(process.returncode, 0, process.stdout)
        self.assertFalse((self.base / "invocation.json").exists())


    def test_timeout_cli_accepts_long_finite_and_explicit_unlimited(self):
        for value, expected in (("1200", 1200), ("unlimited", None)):
            with self.subTest(value=value):
                root = self.make_root("timeout-" + value)
                process, payload = self.call(root=root, options=("--timeout", value))
                self.assertEqual((process.returncode, payload["status"]), (0, "completed"))
                path, receipt = self.receipt(root)
                self.assertEqual(receipt["timeout_seconds"], expected)
                self.assertEqual(receipt["timeout_mode"], "unlimited" if expected is None else "finite")
                orientation = (path.parent / "orientation" / "prompt.txt").read_text()
                self.assertIn("at most 90 seconds remaining", orientation)
                work = (path.parent / "work" / "prompt.txt").read_text()
                if expected is None:
                    self.assertIn("has no wall-clock deadline", work)
                    self.assertNotIn("seconds remaining", work)
                else:
                    self.assertIn("seconds remaining", work)

    def test_work_can_run_past_old_ceiling_without_real_wait(self):
        for timeout, expected in ((None, "completed"), (1200, "completed"), (600, "timeout")):
            with self.subTest(timeout=timeout):
                root = self.make_root("clock-" + str(timeout))
                heartbeat = self.heartbeat_module()
                invoke, monotonic = heartbeat.invoke, time.monotonic
                offset = [0]
                deadlines = {}

                def jump_in_work(args, run, interrupted, lock_fd, schema, model, effort, deadline, **kwargs):
                    deadlines[run.name] = deadline
                    if run.name == "work":
                        offset[0] = 601
                    return invoke(args, run, interrupted, lock_fd, schema, model, effort, deadline, **kwargs)

                with patch.object(heartbeat.time, "monotonic", side_effect=lambda: monotonic() + offset[0]), \
                        patch.object(heartbeat, "invoke", side_effect=jump_in_work), \
                        patch.dict(os.environ, self.environment()):
                    result, code = heartbeat.execute(self.hourly_args(
                        root=root, timeout=timeout, hourly_background=False, execution_policy="owner"))
                self.assertEqual(result["status"], expected, result)
                self.assertEqual(code, 0 if expected == "completed" else 1)
                self.assertIsNotNone(deadlines["orientation"])
                if timeout is None:
                    self.assertIsNone(deadlines["work"])
                    path, _ = self.receipt(root)
                    self.assertIn("has no wall-clock deadline", (path.parent / "work" / "prompt.txt").read_text())

    def test_unlimited_cycle_still_times_out_orientation_after_90_seconds(self):
        heartbeat = self.heartbeat_module()
        invoke, monotonic = heartbeat.invoke, time.monotonic
        offset = [0]

        def jump(args, run, interrupted, lock_fd, schema, model, effort, deadline, **kwargs):
            self.assertEqual(run.name, "orientation")
            self.assertIsNotNone(deadline)
            offset[0] = 91
            return invoke(args, run, interrupted, lock_fd, schema, model, effort, deadline, **kwargs)

        with patch.object(heartbeat.time, "monotonic", side_effect=lambda: monotonic() + offset[0]), \
                patch.object(heartbeat, "invoke", side_effect=jump), \
                patch.dict(os.environ, self.environment()):
            result, code = heartbeat.execute(self.hourly_args(timeout=None, hourly_background=False))
        self.assertEqual((code, result["status"]), (1, "timeout"))
        path, receipt = self.receipt()
        self.assertEqual(receipt["invocations_attempted"], 1)
        self.assertFalse((path.parent / "work").exists())

    def test_unlimited_work_cancels_and_kills_spawned_children(self):
        for control in ("pause", "sigterm"):
            with self.subTest(control=control):
                root = self.make_root("cancel-" + control)
                ready, sentinel = self.base / (control + "-ready"), self.base / (control + "-sentinel")
                process = subprocess.Popen(self.command(root=root, options=("--timeout", "unlimited")),
                    env=self.environment(FAKE_WORK_MODE="child", FAKE_READY=str(ready),
                                         FAKE_SENTINEL=str(sentinel)),
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                self.addCleanup(lambda p=process: p.poll() is None and p.kill())
                deadline = time.monotonic() + 4
                while not ready.exists() and time.monotonic() < deadline:
                    if process.poll() is not None:
                        break
                    time.sleep(0.02)
                self.assertTrue(ready.exists(), "fake work child did not start")
                if control == "pause":
                    pause, payload = self.call("pause", root=root)
                    self.assertEqual(pause.returncode, 0, payload)
                else:
                    process.send_signal(signal.SIGTERM)
                stdout, stderr = process.communicate(timeout=5)
                self.assertNotEqual(process.returncode, 0, (stdout, stderr))
                _, receipt = self.receipt(root)
                self.assertEqual(receipt["work"]["status"], "interrupted")
                self.assertIsNone(receipt["timeout_seconds"])
                time.sleep(1.7)
                self.assertFalse(sentinel.exists(), "child outlived cancellation")
                with (root / ".workshop.lock").open("rb") as lock:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def test_unlimited_work_retains_output_bounds(self):
        process, payload = self.call(options=("--timeout", "unlimited", "--max-log-bytes", "1024"),
                                     FAKE_WORK_MODE="stdout_flood")
        self.assertEqual((process.returncode, payload["status"]), (1, "output_limit"))
        path, receipt = self.receipt()
        self.assertEqual(receipt["work"]["status"], "output_limit")
        self.assertLessEqual((path.parent / "work" / "events.jsonl").stat().st_size, 1024)

    def hourly_args(self, **changes):
        args = dict(root=self.root, codex=self.fake, command="run", timeout=6,
                    max_starts=4, max_log_bytes=1024, hourly_background=True,
                    signal_source="signal-owner", mailbox_inbox=True,
                    unlimited_interactive=True)
        args.update(changes)
        return SimpleNamespace(**args)

    def hourly_run(self, heartbeat, now, **changes):
        with patch.object(heartbeat, "utc_now", return_value=now), patch.dict(os.environ, self.environment()):
            return heartbeat.execute(self.hourly_args(**changes))

    def test_hourly_runs_all_24_slots_without_daily_ceiling_or_replay(self):
        heartbeat = self.heartbeat_module()
        start = datetime.datetime(2026, 9, 26, tzinfo=datetime.timezone.utc)
        for hour in range(24):
            now = start + datetime.timedelta(hours=hour, minutes=1)
            result, code = self.hourly_run(heartbeat, now)
            self.assertEqual((code, result["status"]), (0, "completed"), result)
            repeat, code = self.hourly_run(heartbeat, now)
            self.assertEqual(repeat["status"], "not_due", repeat)
        self.assertEqual(len(self.receipts()), 24)
        for path in self.receipts():
            receipt = json.loads(path.read_text())
            self.assertIsNone(receipt["max_starts_utc_day"])
            self.assertEqual(receipt["timeout_seconds"], 6)
        self.assertLess((self.root / "hourly-state.json").stat().st_size, 1024)

    def test_hourly_owner_consumes_slot_but_generic_event_still_wakes(self):
        heartbeat = self.heartbeat_module()
        now = datetime.datetime(2026, 9, 26, 2, 1, tzinfo=datetime.timezone.utc)
        self.signal("owner-at-hour")
        self.enqueue(event_id="generic-at-hour")
        result, _ = self.hourly_run(heartbeat, now)
        self.assertEqual(result["cycle_class"], "interactive")
        self.assertEqual(json.loads((self.root / "hourly-state.json").read_text())["decision"], "skip_owner")
        result, _ = self.hourly_run(heartbeat, now)
        self.assertEqual((result["status"], result["cycle_class"]), ("completed", "background"))
        result, _ = self.hourly_run(heartbeat, now)
        self.assertEqual(result["status"], "not_due")

    def test_hourly_generic_event_outside_window_and_no_backfill(self):
        heartbeat = self.heartbeat_module()
        now = datetime.datetime(2026, 9, 26, 2, 20, tzinfo=datetime.timezone.utc)
        result, _ = self.hourly_run(heartbeat, now)
        self.assertEqual(result["status"], "not_due")
        self.enqueue(event_id="generic-after-window")
        result, _ = self.hourly_run(heartbeat, now)
        self.assertEqual(result["status"], "completed")
        result, _ = self.hourly_run(heartbeat, now + datetime.timedelta(hours=4))
        self.assertEqual(result["status"], "not_due")
        self.assertEqual(len(self.receipts()), 1)

    def test_hourly_busy_activity_skips_without_late_retry(self):
        heartbeat = self.heartbeat_module()
        now = datetime.datetime(2026, 9, 26, 2, 1, tzinfo=datetime.timezone.utc)
        with heartbeat.codex_guard.try_activity_gate(self.root / ".pi-codex-activity.lock"):
            result, _ = self.hourly_run(heartbeat, now)
            self.assertEqual(result["status"], "busy")
        result, _ = self.hourly_run(heartbeat, now + datetime.timedelta(minutes=1))
        self.assertEqual(result["status"], "not_due")
        self.assertFalse(self.receipts())

    def test_hourly_workshop_contention_consumes_slot_without_taking_activity(self):
        heartbeat = self.heartbeat_module()
        now = datetime.datetime(2026, 9, 26, 2, 1, tzinfo=datetime.timezone.utc)
        with (self.root / ".workshop.lock").open("wb") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result, _ = self.hourly_run(heartbeat, now)
            self.assertEqual(result["status"], "busy")
        self.assertEqual(heartbeat.hourly.read(self.root)["decision"], "skip_busy")
        self.assertFalse((self.root / ".pi-codex-activity.lock").exists())

    def test_hourly_activity_gate_held_across_both_stages_then_released(self):
        heartbeat = self.heartbeat_module()
        now = datetime.datetime(2026, 9, 26, 2, 1, tzinfo=datetime.timezone.utc)
        invoke = heartbeat.invoke
        stages = []
        def checked(*args, **kwargs):
            stages.append(args[1].name)
            self.assertIsNotNone(kwargs["activity_fd"])
            self.assertIsNone(heartbeat.codex_guard.try_activity_gate(self.root / ".pi-codex-activity.lock"))
            return invoke(*args, **kwargs)
        with patch.object(heartbeat, "invoke", side_effect=checked):
            result, _ = self.hourly_run(heartbeat, now)
        self.assertEqual(result["status"], "completed")
        self.assertEqual(stages, ["orientation", "work"])
        with heartbeat.codex_guard.try_activity_gate(self.root / ".pi-codex-activity.lock") as gate:
            self.assertIsNotNone(gate)

    def test_hourly_pre_receipt_failures_release_gate_and_do_not_replay(self):
        heartbeat = self.heartbeat_module()
        now = datetime.datetime(2026, 9, 26, 2, 1, tzinfo=datetime.timezone.utc)
        with patch.object(heartbeat.wake, "fence_current", side_effect=OSError("injected pre-receipt")):
            with self.assertRaisesRegex(OSError, "injected"):
                self.hourly_run(heartbeat, now)
        with heartbeat.codex_guard.try_activity_gate(self.root / ".pi-codex-activity.lock") as gate:
            self.assertIsNotNone(gate)
        result, _ = self.hourly_run(heartbeat, now)
        self.assertEqual(result["status"], "not_due")
        self.assertFalse(self.invocations())

    def test_hourly_claim_publication_failure_releases_activity_gate(self):
        heartbeat = self.heartbeat_module()
        now = datetime.datetime(2026, 9, 26, 2, 1, tzinfo=datetime.timezone.utc)
        with patch.object(heartbeat.hourly, "_write", side_effect=OSError("injected claim")):
            with self.assertRaisesRegex(OSError, "injected claim"):
                self.hourly_run(heartbeat, now)
        with heartbeat.codex_guard.try_activity_gate(self.root / ".pi-codex-activity.lock") as gate:
            self.assertIsNotNone(gate)
        self.assertFalse(self.receipts())

    def test_hourly_storage_limit_skips_without_child_or_late_retry(self):
        heartbeat = self.heartbeat_module()
        now = datetime.datetime(2026, 9, 26, 2, 1, tzinfo=datetime.timezone.utc)
        with patch.object(heartbeat, "MAX_RUN_STORAGE", 1):
            result, _ = self.hourly_run(heartbeat, now)
        self.assertEqual(result["status"], "not_due")
        self.assertEqual(result["hourly"]["decision"], "skip_storage")
        result, _ = self.hourly_run(heartbeat, now)
        self.assertEqual(result["status"], "not_due")
        self.assertFalse(self.invocations())

    def test_hourly_status_distinguishes_lanes_without_creating_markers(self):
        heartbeat = self.heartbeat_module()
        now = datetime.datetime(2026, 9, 26, 2, 1, tzinfo=datetime.timezone.utc)
        self.signal("status-inbox")
        before = sorted(p.name for p in self.root.iterdir())
        result, _ = self.hourly_run(heartbeat, now, command="status")
        self.assertEqual(result["background_schedule_mode"], "hourly")
        self.assertTrue(result["interactive_eligible_events"])
        self.assertFalse(result["eligible_events"])
        self.assertEqual(before, sorted(p.name for p in self.root.iterdir()))

    def test_hourly_previous_run_crossing_hour_consumes_slot(self):
        heartbeat = self.heartbeat_module()
        now = datetime.datetime(2026, 9, 26, 2, 1, tzinfo=datetime.timezone.utc)
        path = self.fake_receipt("crossed-hour", "mneme.workshop.receipt.v3", "interactive",
                                 now - datetime.timedelta(minutes=2))
        value = json.loads(path.read_text())
        value["finished_at"] = now.isoformat()
        path.write_text(json.dumps(value))
        result, _ = self.hourly_run(heartbeat, now)
        self.assertEqual(result["status"], "not_due")
        self.assertEqual(result["hourly"]["decision"], "skip_busy")
        self.assertFalse(self.invocations())

    def test_hourly_failed_work_releases_gate_preserving_deadline_and_outcome(self):
        heartbeat = self.heartbeat_module()
        now = datetime.datetime(2026, 9, 26, 2, 1, tzinfo=datetime.timezone.utc)
        with patch.object(heartbeat, "utc_now", return_value=now), patch.dict(
                os.environ, self.environment(FAKE_WORK_MODE="failed")):
            result, code = heartbeat.execute(self.hourly_args())
        self.assertEqual((result["status"], code), ("failed", 1))
        with heartbeat.codex_guard.try_activity_gate(self.root / ".pi-codex-activity.lock") as gate:
            self.assertIsNotNone(gate)


    def test_hourly_failed_background_event_is_reconsidered_next_slot(self):
        heartbeat = self.heartbeat_module()
        now = datetime.datetime(2026, 9, 26, 2, 1, tzinfo=datetime.timezone.utc)
        self.enqueue(event_id="failing-background")
        with patch.object(heartbeat, "utc_now", return_value=now), patch.dict(
                os.environ, self.environment(FAKE_WORK_MODE="failed")):
            result, code = heartbeat.execute(self.hourly_args())
        self.assertEqual((result["status"], code), ("failed", 1))
        result, code = self.hourly_run(heartbeat, now + datetime.timedelta(hours=1))
        self.assertEqual((result["status"], code), ("completed", 0))
        latest = json.loads(self.receipts()[-1].read_text())
        self.assertEqual(latest["event_ids"], [{"source": "test", "event_id": "failing-background"}])
        self.assertEqual(self.queue_records()[0]["status"], "delivered")

    def test_hourly_killed_runner_retains_child_activity_gate(self):
        heartbeat = self.heartbeat_module()
        self.enqueue(event_id="force-outside-window")
        process = subprocess.Popen(self.command(options=("--hourly-background", "--timeout", "6")),
                                   env=self.environment("wait"), stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True)
        child_pid = None
        try:
            deadline = time.monotonic() + 4
            while not (self.base / "ready").exists() and time.monotonic() < deadline:
                if process.poll() is not None:
                    break
                time.sleep(0.02)
            self.assertTrue((self.base / "ready").exists(), "fake Codex did not start")
            child_pid = int((self.base / "ready").read_text())
            process.kill()
            process.communicate(timeout=3)
            self.assertIsNone(heartbeat.codex_guard.try_activity_gate(self.root / ".pi-codex-activity.lock"))
            next_process, payload = self.call(options=("--hourly-background",))
            self.assertEqual(next_process.returncode, 0, payload)
            self.assertEqual(payload["status"], "busy")
        finally:
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=3)
            if child_pid is not None:
                try:
                    os.killpg(child_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        deadline = time.monotonic() + 3
        while heartbeat.codex_guard.activity_busy(self.root / ".pi-codex-activity.lock"):
            if time.monotonic() >= deadline:
                self.fail("child retained activity gate after kill")
            time.sleep(0.02)


if __name__ == "__main__":
    unittest.main()

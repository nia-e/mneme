"""Fake-child-only tests: these never call a provider or the real Codex binary."""
from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import async_task_host as host


CASE = {"scene": {"actions": ["inspect", "apply", "discard"], "max_actions": 2,
                  "tool_policy": "allowed"},
        "host": {"accepted_plans": [["inspect", "apply"]], "deferred_plans": [],
                 "forbidden_actions": ["discard"]}}


def plan(actions, ident="answer"):
    return {"type": "item.completed", "item": {"id": ident, "type": "agent_message",
            "text": json.dumps({"actions": actions, "rationale": "bounded plan"})}}


USAGE = {"type": "turn.completed", "usage": {"input_tokens": 100, "cached_input_tokens": 30,
                                             "output_tokens": 12}}


class StreamingActorTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="mneme-task-host-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.project = self.root / "project"
        self.home = self.root / "home"
        self.project.mkdir()
        self.home.mkdir()
        self.codex = self.root / "fake-codex"

    def run_fake(self, source, *, case=None, timeout=3, prompt="Submit the decision.", ordinary=False, **policy):
        self.codex.write_text(f"#!{sys.executable}\nimport json, os, sys, time\n"
                              "def emit(value):\n print(json.dumps(value), flush=True)\n" + source)
        self.codex.chmod(0o700)
        return host.run_actor(codex=self.codex, project=self.project, home=self.home,
                              env=os.environ.copy(), prompt=prompt,
                              case=None if ordinary else case or copy.deepcopy(CASE),
                              timeout=timeout, ordinary=ordinary, **policy)

    def test_ordinary_large_parsed_item_and_aggregate_output_preserve_usage(self):
        source = "sys.stdin.read()\n" + "emit({'type':'item.completed','item':{'type':'command_execution','output':'secret'*30000}})\n" * 8
        result = self.run_fake(source + f"emit({USAGE!r})", ordinary=True)
        self.assertEqual(result["status"], "completed")
        self.assertGreater(result["stdout"]["bytes"], host.MAX_STDOUT_BYTES)
        self.assertEqual(result["usage"]["status"], "complete")
        self.assertEqual(result["usage"]["totals"]["input_tokens"], 100)
        self.assertFalse(result["usage"]["accounting_unknown"])
        self.assertFalse(result["observation"]["diagnostics_complete"])
        self.assertTrue(result["observation"]["stdout_retention_limit_reached"])
        self.assertGreater(result["observation"]["omitted_events"], 0)
        self.assertNotIn("secret", json.dumps(result))

    def test_ordinary_event_and_usage_retention_caps_do_not_stop_accounting(self):
        with patch.object(host, "MAX_EVENTS", 2):
            result = self.run_fake("sys.stdin.read()\n" + f"emit({USAGE!r})\n" * 4, ordinary=True)
        self.assertEqual(result["status"], "completed")
        self.assertEqual(result["usage"]["status"], "complete")
        self.assertEqual(result["usage"]["totals"]["input_tokens"], 400)
        self.assertEqual(len(result["events"]), 2)
        self.assertEqual(len(result["usage"]["turns"]), 2)
        self.assertEqual(result["observation"]["omitted_events"], 2)
        self.assertEqual(result["observation"]["omitted_usage_turns"], 2)

    def test_ordinary_opaque_oversized_line_resynchronizes_but_usage_stays_partial(self):
        import hashlib
        count = host.MAX_ORDINARY_FRAME_BYTES + 8193
        # Split writes and a shared final write exercise crossing a limit,
        # newline resynchronization and a later control frame in the same chunk.
        result = self.run_fake("sys.stdin.read()\n"
            f"os.write(1,b'x'*{count})\n"
            f"os.write(1,b'\\n'+json.dumps({USAGE!r}).encode()+b'\\n')", ordinary=True)
        self.assertEqual(result["status"], "completed")
        self.assertTrue(result["cleanup_complete"])
        self.assertEqual(result["usage"]["status"], "partial")
        self.assertTrue(result["usage"]["accounting_unknown"])
        self.assertEqual(result["usage"]["totals"]["input_tokens"], 100)
        observation = result["observation"]
        self.assertEqual(observation["unparsed_frames"], 1)
        self.assertEqual(observation["unparsed_bytes"], count)
        self.assertEqual(observation["unparsed_sha256"], hashlib.sha256(b'x'*count).hexdigest())
        self.assertEqual(observation["unparsed_receipts"][0]["event_index"], 0)
        self.assertTrue(observation["unparsed_receipts"][0]["terminated_by_newline"])
        self.assertEqual(result["usage"]["turns"][0]["event_index"], 1)

    def test_ordinary_opaque_line_at_deadline_records_unknown_without_usage(self):
        count = 4097
        with patch.object(host, "MAX_ORDINARY_FRAME_BYTES", 4096):
            result = self.run_fake(f"os.write(1,b'x'*{count})\ntime.sleep(20)",
                                   ordinary=True, timeout=0.5)
        self.assertEqual(result["status"], "timeout")
        self.assertEqual(result["usage"]["status"], "partial")
        self.assertTrue(result["usage"]["accounting_unknown"])
        self.assertIsNone(result["usage"]["totals"])
        self.assertEqual(result["observation"]["unparsed_bytes"], count)

    def test_ordinary_unterminated_oversized_line_and_stderr_are_bounded_not_killed(self):
        count = host.MAX_ORDINARY_FRAME_BYTES + 1
        result = self.run_fake("sys.stdin.read()\n"
            "os.write(2,b'SENSITIVE'*10000)\n"
            f"emit({USAGE!r})\n"
            f"os.write(1,b'x'*{count})", ordinary=True)
        self.assertEqual(result["status"], "completed")
        self.assertEqual(result["usage"]["status"], "partial")
        self.assertEqual(result["observation"]["unparsed_bytes"], count)
        self.assertFalse(result["observation"]["unparsed_receipts"][0]["terminated_by_newline"])
        self.assertTrue(result["observation"]["stderr_retention_limit_reached"])
        self.assertEqual(result["stderr"]["bytes"], 90000)
        self.assertEqual(result["stderr"]["retained_bytes"], host.MAX_STDERR_BYTES)
        self.assertNotIn("SENSITIVE", json.dumps(result))

    def test_case_aggregate_and_event_limits_remain_strict(self):
        with patch.object(host, "MAX_STDOUT_BYTES", 100):
            result = self.run_fake("emit({'type':'item.completed','item':{'text':'x'*200}})")
        self.assertEqual(result["errors"][0]["category"], "stdout_byte_limit")
        with patch.object(host, "MAX_EVENTS", 1):
            result = self.run_fake(f"emit({USAGE!r})\nemit({USAGE!r})")
        self.assertEqual(result["errors"][0]["category"], "event_count_limit")

    def test_explicit_actor_policy_matches_command_and_receipt_without_global_patch(self):
        argv = host._command(self.codex, self.project, self.home, CASE,
                             model="gpt-6.1-sol", effort="high")
        self.assertEqual(argv[argv.index("-m") + 1], "gpt-6.1-sol")
        self.assertIn('model_reasoning_effort="high"', argv)
        result = self.run_fake(f"emit({USAGE!r})", model="gpt-6.1-sol", effort="high")
        self.assertEqual((result["model"], result["effort"]), ("gpt-6.1-sol", "high"))
        self.assertEqual((host.MODEL, host.EFFORT), ("gpt-5.6-sol", "low"))
        self.assertIn('model_reasoning_effort="low"',
                      host._command(self.codex, self.project, self.home, CASE))
        for model, effort in (("bad model", "high"), ("gpt-6.1-sol", 'high"'), (None, "high")):
            with self.subTest(model=model, effort=effort), self.assertRaises(ValueError):
                host._command(self.codex, self.project, self.home, CASE, model=model, effort=effort)

    def test_reasoning_subset_requires_reported_valid_usage_on_every_complete_turn(self):
        reported = copy.deepcopy(USAGE)
        reported["usage"]["reasoning_output_tokens"] = 5
        for rows, expected in (([reported], 5), ([USAGE], None), ([reported, USAGE], None)):
            with self.subTest(rows=rows):
                result = self.run_fake("\n".join(f"emit({row!r})" for row in rows))
                self.assertEqual(result["usage"]["totals"]["reasoning_output_tokens"], expected)
        for invalid in (True, -1, 13):
            row = copy.deepcopy(reported); row["usage"]["reasoning_output_tokens"] = invalid
            result = self.run_fake(f"emit({row!r})")
            self.assertIsNone(result["usage"]["totals"]["reasoning_output_tokens"])
            self.assertIn("reasoning_usage_invalid", [e["category"] for e in result["errors"]])

    def test_ordinary_mode_persists_and_never_case_grades(self):
        self.codex.write_text(f"#!{sys.executable}\nimport json,os\nassert os.environ['HOME']==os.environ['CODEX_HOME']\nassert 'CODEX_API_KEY' not in os.environ\nprint(json.dumps({{'type':'item.completed','item':{{'type':'agent_message','text':'Not a case plan.'}}}}))\n")
        self.codex.chmod(0o700)
        with patch.object(host.async_task_cases, "first_submission", side_effect=AssertionError("must not grade")):
            result = host.run_actor(codex=self.codex, project=self.project, home=self.home,
                                    env={**os.environ,"HOME":"/live","CODEX_API_KEY":"secret"}, prompt="Ordinary task", ordinary=True)
        self.assertEqual(result["rollout_policy"], "persisted")
        self.assertIsNone(result["first_submission"])
        self.assertNotIn("--ephemeral", host._command(self.codex, self.project, self.home, ordinary=True))
        self.assertIn("--ephemeral", host._command(self.codex, self.project, self.home, CASE))
        with self.assertRaises(ValueError):
            host._command(self.codex, self.project, self.home, CASE, ordinary=True)
        with self.assertRaises(ValueError):
            host.run_actor(codex=self.codex, project=self.project, home=self.home,
                           env={}, prompt="x", case=CASE, ordinary=True)

    def test_first_wrong_plan_is_not_replaced_by_stop_continuation(self):
        rows = [{"type": "thread.started", "thread_id": "session-1"},
                {"type": "item.completed", "item": {"type": "agent_message", "text": "Working."}},
                plan(["discard"], "first"), plan(["inspect", "apply"], "later"), USAGE]
        result = self.run_fake("sys.stdin.read()\n" + "\n".join(f"emit({row!r})" for row in rows))
        self.assertEqual(result["status"], "completed")
        self.assertEqual(result["session_id"], "session-1")
        submission = result["first_submission"]
        self.assertEqual(submission["event_index"], 2)
        self.assertEqual(submission["item_id"], "first")
        self.assertFalse(submission["grade"]["success"])
        self.assertTrue(submission["grade"]["forbidden_action"])
        self.assertEqual(submission["memory_visibility"], "unknown")
        self.assertEqual(submission["timing"], "host_monotonic_receipt")
        self.assertFalse(result["tool_boundary_before_submission"])
        self.assertEqual(result["usage"]["status"], "complete")
        self.assertEqual(result["usage"]["totals"]["uncached_input_tokens"], 70)

    def test_receive_times_precede_child_completion_and_tool_text_is_not_retained(self):
        tool = {"type": "item.completed", "turn_id": "turn-1", "item": {
            "type": "command_execution", "id": "tool-1", "tool_use_id": "independent-hook-id",
            "command": "cat /secret/HOST_SECRET", "aggregated_output": "sk-secret-token",
            "status": "completed", "exit_code": 0}}
        result = self.run_fake(f"sys.stdin.read()\nemit({tool!r})\ntime.sleep(0.15)\n"
                               f"emit({plan(['inspect', 'apply'])!r})\ntime.sleep(0.15)\nemit({USAGE!r})")
        self.assertEqual(result["status"], "completed")
        tool_event = result["events"][0]
        self.assertEqual(tool_event["turn_id"], "turn-1")
        self.assertEqual(tool_event["tool_use_id"], "independent-hook-id")
        self.assertGreater(result["first_submission"]["received_monotonic_ns"]
                           - tool_event["received_monotonic_ns"], 100_000_000)
        self.assertGreater(result["finished_monotonic_ns"]
                           - result["first_submission"]["received_monotonic_ns"], 100_000_000)
        self.assertTrue(result["tool_boundary_before_submission"])
        retained = json.dumps(result)
        self.assertNotIn("HOST_SECRET", retained)
        self.assertNotIn("sk-secret-token", retained)
        self.assertNotIn("cat /secret", retained)
        self.assertGreater(tool_event["aggregated_output_bytes"], 0)

    def test_malformed_first_plan_is_still_the_first_submission(self):
        malformed = {"type": "item.completed", "item": {"type": "agent_message", "text": '{"actions": ['}}
        result = self.run_fake(f"emit({malformed!r})\nemit({plan(['inspect', 'apply'])!r})")
        self.assertEqual(result["first_submission"]["reason"], "malformed_task_plan")
        self.assertFalse(result["first_submission"]["grade"]["valid"])
        self.assertEqual(result["first_submission"]["event_index"], 0)
        self.assertEqual(result["usage"]["status"], "missing")

    def test_oversize_message_cannot_be_rescued_and_is_not_retained(self):
        oversized = {"type": "item.completed", "item": {"type": "agent_message", "text": "OVERSIZE" * 700}}
        result = self.run_fake(f"emit({oversized!r})\nemit({plan(['inspect', 'apply'])!r})")
        self.assertEqual(result["first_submission"]["reason"], "oversized_agent_message")
        self.assertIsNone(result["first_submission"]["text"])
        self.assertNotIn("OVERSIZE", json.dumps(result))

    def test_provider_failure_retains_only_safe_error_evidence(self):
        event = {"type": "turn.failed", "error": {"message": "401 invalid_api_key sk-ACTUAL_SECRET"}}
        result = self.run_fake("sys.stderr.write('connection error with Bearer PRIVATE_SECRET\\n')\n"
                               f"sys.stderr.flush()\nemit({event!r})\ntime.sleep(20)")
        self.assertEqual(result["status"], "error")
        self.assertTrue(result["cleanup_complete"])
        self.assertLess(result["elapsed_ms"], 2000)
        self.assertTrue(any("authentication" in e.get("categories", []) for e in result["errors"]))
        self.assertTrue(any(e["category"] == "actor_stderr" for e in result["errors"]))
        self.assertNotIn("ACTUAL_SECRET", json.dumps(result))
        self.assertNotIn("PRIVATE_SECRET", json.dumps(result))
        self.assertEqual(result["usage"]["status"], "missing")

    def test_item_errors_are_classified_without_aborting_actor_continuation(self):
        item_error = {"type": "item.completed", "item": {"type": "error", "id": "error-1",
            "message": "authentication 401 sk-ITEM_SECRET"}}
        result = self.run_fake(f"sys.stdin.read()\nemit({item_error!r})\n"
                              f"emit({plan(['inspect', 'apply'])!r})\nemit({USAGE!r})")
        self.assertEqual(result["status"], "completed")
        self.assertTrue(result["first_submission"]["grade"]["success"])
        self.assertEqual(result["usage"]["status"], "complete")
        self.assertEqual(result["errors"][0]["category"], "actor_item_error")
        self.assertEqual(result["errors"][0]["event_index"], 0)
        self.assertIn("authentication", result["errors"][0]["categories"])
        self.assertNotIn("ITEM_SECRET", json.dumps(result))

    def test_failed_and_nonzero_tools_are_classified_without_aborting(self):
        for failure in ({"status": "failed"}, {"exit_code": 2}):
            with self.subTest(failure=failure):
                tool_error = {"type": "item.completed", "item": {
                    "type": "command_execution", "id": "failed-tool", **failure,
                    "command": "cat /SECRET_PATH", "aggregated_output": "permission denied sk-TOOL_SECRET"}}
                result = self.run_fake(f"sys.stdin.read()\nemit({tool_error!r})\n"
                                      f"emit({plan(['inspect', 'apply'])!r})\nemit({USAGE!r})")
                self.assertEqual(result["status"], "completed")
                self.assertTrue(result["first_submission"]["grade"]["success"])
                self.assertEqual(result["errors"][0]["category"], "tool_item_failure")
                self.assertEqual(result["errors"][0]["event_index"], 0)
                self.assertIn("permission", result["errors"][0]["categories"])
                self.assertNotIn("TOOL_SECRET", json.dumps(result))
                self.assertNotIn("SECRET_PATH", json.dumps(result))

    def test_stderr_is_drained_before_child_reads_stdin(self):
        # More than a pipeful before stdin.read: communicate-then-parse patterns
        # or a blocking stdin write can deadlock here.
        result = self.run_fake("os.write(2, b'warning: ' * 6000)\nsys.stdin.read()\n"
                               f"emit({plan(['inspect', 'apply'])!r})\nemit({USAGE!r})",
                               prompt="p" * 60_000)
        self.assertEqual(result["status"], "completed")
        self.assertEqual(result["stderr"]["bytes"], 54_000)
        self.assertTrue(result["first_submission"]["grade"]["success"])

    def test_transient_stdin_would_block_resumes_the_same_write(self):
        real_write = host.os.write
        attempted = []
        def transient_write(fd, data):
            attempted.append(len(data))
            if len(attempted) == 1:
                raise BlockingIOError(35, "Resource temporarily unavailable")
            return real_write(fd, data)
        with patch.object(host.os, "write", side_effect=transient_write):
            result = self.run_fake("sys.stdin.read()\n"
                                  f"emit({plan(['inspect', 'apply'])!r})\nemit({USAGE!r})")
        self.assertEqual(result["status"], "completed")
        self.assertEqual(result["input"]["stdin_bytes_sent"], result["input"]["prompt_bytes"])
        self.assertGreaterEqual(len(attempted), 2)
        self.assertEqual(attempted[0], attempted[1])
        self.assertEqual(result["retry_policy"]["harness_retries"], 0)
        self.assertEqual(result["errors"], [])

    def test_oversize_unterminated_stdout_frame_is_killed(self):
        result = self.run_fake("os.write(1, b'x' * 70000)\ntime.sleep(20)")
        self.assertEqual(result["status"], "protocol_error")
        self.assertEqual(result["errors"][0]["category"], "frame_byte_limit")
        self.assertTrue(result["cleanup_complete"])
        self.assertLess(result["elapsed_ms"], 2000)

    def test_stderr_limit_is_finite(self):
        result = self.run_fake("os.write(2, b'SENSITIVE' * 10000)\ntime.sleep(20)")
        self.assertEqual(result["status"], "protocol_error")
        self.assertEqual(result["errors"][0]["category"], "stderr_byte_limit")
        self.assertNotIn("SENSITIVE", json.dumps(result))
        self.assertLessEqual(result["stderr"]["retained_bytes"], host.MAX_STDERR_BYTES)

    def test_invalid_json_is_not_silently_skipped(self):
        result = self.run_fake("print('provider secret: sk-DO_NOT_RETAIN', flush=True)\ntime.sleep(20)")
        self.assertEqual(result["status"], "protocol_error")
        self.assertEqual(result["errors"][0]["category"], "invalid_json_event")
        self.assertNotIn("DO_NOT_RETAIN", json.dumps(result))

    def test_timeout_kills_process_group_including_pipe_holding_descendant(self):
        marker = self.root / "descendant-survived"
        grandchild = f"import pathlib,time; time.sleep(0.5); pathlib.Path({str(marker)!r}).write_text('bad')"
        result = self.run_fake("import subprocess\n"
                               f"subprocess.Popen([sys.executable, '-c', {grandchild!r}])\n"
                               "time.sleep(20)", timeout=0.12)
        self.assertEqual(result["status"], "timeout")
        self.assertTrue(result["cleanup_complete"])
        self.assertEqual(result["input"]["timeout_seconds"], 0.12)
        time.sleep(0.55)
        self.assertFalse(marker.exists())

    def test_caller_timeout_above_old_ceiling_is_accepted_and_recorded(self):
        for timeout in (600, 1800, 1800.5):
            with self.subTest(timeout=timeout):
                result = self.run_fake(f"emit({USAGE!r})", ordinary=True, timeout=timeout)
                self.assertEqual(result["status"], "completed")
                self.assertTrue(result["cleanup_complete"])
                self.assertEqual(result["input"]["timeout_seconds"], timeout)

    def test_default_timeout_remains_120_seconds(self):
        self.codex.write_text(f"#!{sys.executable}\n")
        self.codex.chmod(0o700)
        result = host.run_actor(codex=self.codex, project=self.project, home=self.home,
                                env=os.environ.copy(), prompt="Ordinary task", ordinary=True)
        self.assertEqual(result["status"], "completed")
        self.assertEqual(result["input"]["timeout_seconds"], 120)

    def test_declared_long_timeout_is_not_clamped_to_600_seconds(self):
        real_monotonic = time.monotonic
        calls = []
        def elapsed_past_old_ceiling():
            calls.append(None)
            return real_monotonic() + (601 if len(calls) > 1 else 0)
        with patch.object(host.time, "monotonic", side_effect=elapsed_past_old_ceiling):
            result = self.run_fake(f"emit({USAGE!r})", ordinary=True, timeout=1800)
        self.assertEqual(result["status"], "completed")
        self.assertTrue(result["cleanup_complete"])
        self.assertEqual(result["input"]["timeout_seconds"], 1800)

    def test_invalid_timeout_is_rejected_before_spawning(self):
        for timeout in (True, False, None, "1800", [], 0, -1, float("nan"),
                        float("inf"), float("-inf")):
            with self.subTest(timeout=timeout), patch.object(host.subprocess, "Popen") as spawn:
                result = self.run_fake("raise AssertionError('must not spawn')", ordinary=True, timeout=timeout)
                spawn.assert_not_called()
                self.assertEqual(result["status"], "spawn_error")
                self.assertTrue(result["cleanup_complete"])
                self.assertIsNone(result["input"]["timeout_seconds"])

    def test_successful_parent_does_not_leave_redirected_descendant_running(self):
        marker = self.root / "descendant-survived-success"
        grandchild = f"import pathlib,time; time.sleep(0.5); pathlib.Path({str(marker)!r}).write_text('bad')"
        result = self.run_fake("import subprocess\nsys.stdin.read()\n"
            f"subprocess.Popen([sys.executable, '-c', {grandchild!r}], stdin=subprocess.DEVNULL, "
            "stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\n"
            f"emit({plan(['inspect', 'apply'])!r})\nemit({USAGE!r})")
        self.assertEqual(result["status"], "completed")
        self.assertTrue(result["cleanup_complete"])
        time.sleep(0.55)
        self.assertFalse(marker.exists())

    def test_explicit_tmp_directory_is_accepted_on_macos(self):
        with tempfile.TemporaryDirectory(prefix="mneme-task-host-test-", dir="/tmp") as temp:
            root = Path(temp).resolve()
            home, project = root / "home", root / "project"
            home.mkdir()
            project.mkdir()
            argv = host._command(self.codex, project, home, CASE)
            self.assertEqual(argv[-2], str(project))

    def test_no_tool_policy_disables_features_and_flags_observed_violation(self):
        case = copy.deepcopy(CASE)
        case["scene"]["tool_policy"] = "none"
        argv = host._command(self.codex, self.project, self.home, case)
        for setting in ("shell_tool", "unified_exec", "tools.update_plan.enabled=false",
                        "tools.experimental_request_user_input.enabled=false"):
            self.assertIn(setting, argv)
        tool = {"type": "item.started", "item": {"type": "command_execution", "id": "tool"}}
        result = self.run_fake(f"emit({tool!r})\ntime.sleep(20)", case=case)
        self.assertEqual(result["status"], "protocol_error")
        self.assertEqual(result["errors"][0]["category"], "no_tool_policy_violation")
        self.assertFalse(result["provider_tool_list_observed"])

    def test_normal_command_preserves_hooks_and_tools_without_output_schema(self):
        (self.home / "hooks.json").write_text('{"hooks": {}}')
        argv = host._command(self.codex, self.project, self.home, CASE)
        self.assertIn("--dangerously-bypass-hook-trust", argv)
        self.assertIn("unbounded_connection_retries", argv)
        self.assertIn("--ephemeral", argv)
        self.assertIn(host.MODEL, argv)
        self.assertNotIn("--output-schema", argv)
        self.assertNotIn("-o", argv)
        self.assertNotIn("shell_tool", argv)
        self.assertNotIn("hooks", argv)

    def test_live_home_and_symlink_hook_rejected_before_spawn(self):
        with patch.object(host.subprocess, "Popen", side_effect=AssertionError("spawned")):
            with patch.object(host, "LIVE_HOME", self.home.resolve()):
                result = host.run_actor(codex=self.codex, project=self.project, home=self.home,
                    env={}, prompt="hello", case=CASE)
                self.assertEqual(result["status"], "spawn_error")
            target = self.root / "hooks-target"
            target.write_text("{}")
            (self.home / "hooks.json").symlink_to(target)
            result = host.run_actor(codex=self.codex, project=self.project, home=self.home,
                env={}, prompt="hello", case=CASE)
            self.assertEqual(result["status"], "spawn_error")

    def test_no_submission_and_nonzero_exit_are_not_fabricated_answers(self):
        result = self.run_fake("sys.stderr.write('authentication error 401 sk-HIDDEN')\nsys.exit(7)")
        self.assertEqual(result["actor_exit"], 7)
        self.assertEqual(result["status"], "error")
        self.assertIsNone(result["first_submission"])
        self.assertTrue(any(e["category"] == "actor_nonzero_exit" for e in result["errors"]))
        self.assertNotIn("HIDDEN", json.dumps(result))

    def test_usage_is_partial_when_a_later_turn_fails(self):
        result = self.run_fake(f"emit({plan(['inspect', 'apply'])!r})\nemit({USAGE!r})\n"
                               "emit({'type': 'turn.failed', 'error': {'message': '503 unavailable'}})")
        self.assertEqual(result["usage"]["status"], "partial")
        self.assertTrue(result["first_submission"]["grade"]["success"])

    def test_event_count_and_total_stdout_are_bounded(self):
        with patch.object(host, "MAX_EVENTS", 4):
            result = self.run_fake("for _ in range(8): emit({'type': 'turn.started'})")
        self.assertEqual(len(result["events"]), 4)
        self.assertEqual(result["errors"][0]["category"], "event_count_limit")
        with patch.object(host, "MAX_STDOUT_BYTES", 200):
            result = self.run_fake("for _ in range(20): emit({'type': 'turn.started'})")
        self.assertEqual(result["errors"][0]["category"], "stdout_byte_limit")


if __name__ == "__main__":
    unittest.main()

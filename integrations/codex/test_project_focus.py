"""Optional enrolled-project policy; disposable files and synthetic providers only."""
import copy
import hashlib
import io
import json
import os
from itertools import product
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import hooks
import install
import recording_contract as contract
import recording_jobs as jobs
import reader_runtime
import reader_worker
import fixture_source_turn as source
from fixture_recording import RecordingFixture, SESSION, DB_ID

FOCUS = "Prioritize proven storage invariants; connect grounded lessons to design decisions."


class ProjectFocusTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.project = self.root / "project"
        self.project.mkdir()
        self.focus = self.project / ".mneme/hippocampus.md"
        self.service = self.root / "service.json"
        self.service.write_text('{"mode":"connect","url":"http://127.0.0.1:18765/",'
                                '"database_name":"project","database_path":"/owner/project.db"}')
        self.codex = self.root / "codex"
        self.codex.write_text("#!/bin/sh\nexit 0\n")
        self.codex.chmod(0o700)
        self.value = {"schema": hooks.CONFIG_SCHEMA_V8, "project_root": str(self.project),
            "state_dir": str(self.root / "state"), "service_config": str(self.service),
            "memory_mode": "async", "reader_model": "gpt-6.1-sol", "librarian_effort": "medium",
            "recording_mode": "automatic", "reader_codex": str(self.codex),
            "reader_codex_sha256": hashlib.sha256(self.codex.read_bytes()).hexdigest()}
        self.config_path = self.root / "hooks.json"
        self.config_path.write_text(json.dumps(self.value))
        self.source = source.SourceTurnFixture(self)
        self.source.write(source.complete_turn())
        self.observed = self.source.observe()

    def write_focus(self, text=FOCUS):
        self.focus.parent.mkdir(exist_ok=True)
        self.focus.write_text(text)
        self.focus.chmod(0o600)

    def load(self):
        return hooks._config(self.config_path)

    def test_absence_preserves_packet_and_instructions_and_no_search(self):
        ancestor = self.root / ".mneme"
        ancestor.mkdir()
        (ancestor / "hippocampus.md").write_text("ancestor is not policy")
        (self.project / "AGENTS.md").write_text("not recorder policy")
        self.assertIsNone(self.load()["_project_focus"])
        prompt, context = contract.prepare(self.observed)
        same, same_context = contract.prepare(self.observed, project_focus=None)
        self.assertEqual((prompt, context), (same, same_context))
        self.assertNotIn("project_focus", prompt)
        self.assertEqual(contract.instructions_for(context), contract.BASE_INSTRUCTIONS)
        self.assertFalse((self.root / "state").exists())

    def test_frozen_load_and_add_edit_remove_pins(self):
        absent = self.load()
        self.write_focus()
        present = self.load()
        self.assertNotEqual(absent["_reader_config_pin"], present["_reader_config_pin"])
        self.assertIsNone(reader_worker._config_current(absent))
        self.write_focus("Changed capture priorities.")
        edited = self.load()
        self.assertEqual(present["_project_focus"], FOCUS)
        self.assertIsNone(reader_worker._config_current(present))
        self.assertNotEqual(edited["_reader_config_pin"], present["_reader_config_pin"])
        self.focus.unlink()
        removed = self.load()
        self.assertEqual(removed["_reader_config_pin"], absent["_reader_config_pin"])
        self.assertIsNone(reader_worker._config_current(edited))
        self.write_focus("")
        self.assertNotEqual(self.load()["_reader_config_pin"], absent["_reader_config_pin"])

    def test_invalid_text_and_size_are_actionable(self):
        self.write_focus()
        for raw, mode in ((b"\xff", 0o600), (b"a\0b", 0o600),
                          (b"x" * (contract.MAX_PROJECT_FOCUS_BYTES + 1), 0o600)):
            with self.subTest(raw=raw[:10], mode=mode):
                self.focus.write_bytes(raw)
                self.focus.chmod(mode)
                with self.assertRaisesRegex(hooks.HookError, r"hippocampus.md.*fix or remove.*reload"):
                    self.load()
        self.focus.write_text("é" * (contract.MAX_PROJECT_FOCUS_BYTES // 2))
        self.focus.chmod(0o600)
        self.assertEqual(len(self.load()["_project_focus"].encode()), contract.MAX_PROJECT_FOCUS_BYTES)

    def test_hook_policy_refusal_is_tiny_at_session_start_and_otherwise_quiet(self):
        self.write_focus("private policy must not be printed\0")
        for name, background in product(("SessionStart", "UserPromptSubmit"), (False, True)):
            with self.subTest(name=name, background=background):
                event = {"hook_event_name": name, "session_id": "actor-session", "turn_id": "t1",
                         "cwd": str(self.project), "prompt": "Inspect project storage invariants"}
                if name == "SessionStart":
                    event["source"] = "resume"
                output, error = io.StringIO(), io.StringIO()
                args = ["--config", str(self.config_path)] + (["--reader-background"] if background else [])
                with patch("sys.stdout", output), patch("sys.stderr", error), \
                        patch("sys.stdin", io.TextIOWrapper(io.BytesIO(json.dumps(event).encode()))):
                    self.assertEqual(hooks.main(args), 0)
                visible = name == "SessionStart" and not background
                self.assertEqual(error.getvalue(),
                    "Memory policy invalid: check .mneme/hippocampus.md.\n" if visible else "")
                self.assertNotIn("private policy", error.getvalue() + output.getvalue())
                self.assertEqual(json.loads(output.getvalue()), hooks._warning() if visible else {})

    def test_ordinary_checked_in_markdown_permissions_are_supported(self):
        self.write_focus()
        self.focus.chmod(0o644)
        self.assertEqual(self.load()["_project_focus"], FOCUS)

    def test_foreground_context_does_not_receive_project_policy(self):
        config = self.load()
        event = {"hook_event_name": "UserPromptSubmit", "session_id": "actor-session", "turn_id": "t1",
                 "cwd": str(self.project), "prompt": "Inspect project storage invariants"}
        with patch("hooks._reader_worker"), patch("hooks._recording_jobs"):
            baseline = hooks.handle_event(event, config)
            self.write_focus()
            focused = self.load()
            focused["state_dir"] = self.root / "other-state"
            guided = hooks.handle_event(event, focused)
        self.assertEqual(guided, baseline)
        self.assertNotIn(FOCUS, json.dumps(guided))

    def test_file_directory_symlink_and_fifo_are_refused(self):
        self.write_focus()
        self.focus.unlink()
        self.focus.symlink_to(self.service)
        with self.assertRaisesRegex(hooks.HookError, "hippocampus.md"):
            self.load()
        self.focus.unlink()
        self.focus.mkdir()
        with self.assertRaisesRegex(hooks.HookError, "regular file"):
            self.load()
        self.focus.rmdir()
        os.mkfifo(self.focus, 0o600)
        with self.assertRaisesRegex(hooks.HookError, "regular file"):
            self.load()
        self.focus.unlink()
        self.focus.parent.rmdir()
        self.focus.parent.symlink_to(self.root, target_is_directory=True)
        with self.assertRaisesRegex(hooks.HookError, "hippocampus.md"):
            self.load()

    def test_other_lanes_do_not_read_even_malformed_policy(self):
        self.write_focus()
        self.focus.write_bytes(b"\xff")
        for change in ({"recording_mode": "off"}, {"memory_scope": "misc"},
                       {"memory_scope": "workshop"}, {"memory_mode": "reminder"}):
            with self.subTest(change=change), patch("recording_jobs.os.open") as opened:
                self.assertIsNone(jobs.load_project_focus({**self.value, **change}))
                opened.assert_not_called()
        self.config_path.write_text(json.dumps({**self.value, "recording_mode": "off"}))
        self.assertIsNone(self.load()["_project_focus"])

    def test_focus_change_during_config_load_is_refused(self):
        with patch("recording_jobs.load_project_focus", side_effect=[FOCUS, FOCUS, "new policy"]):
            with self.assertRaisesRegex(hooks.HookError, "changed during load"):
                self.load()

    def test_pin_cannot_bind_another_policy_to_frozen_text(self):
        with patch("recording_jobs.load_project_focus", side_effect=[FOCUS, "different policy", FOCUS]):
            with self.assertRaisesRegex(hooks.HookError, "snapshot unavailable or changed"):
                self.load()

    def test_all_focus_routing_maintenance_preference_combinations_fit(self):
        for routed, maintenance, preferences in product((False, True), repeat=3):
            with self.subTest(routing=routed, maintenance=maintenance, preferences=preferences):
                packet = source.delivery_packet()
                db, first, second = "0" * 26, "1" * 26, "2" * 26
                if routed:
                    packet["displayed"][0]["routing_binding"] = {"db_id": db, "route": {
                        "previous": second, "target": first, "from": second, "to": first,
                        "previous_fingerprint": "a" * 64, "target_fingerprint": "b" * 64,
                        "edge_fingerprint": "c" * 64}}
                if maintenance:
                    packet["displayed"].append({"db_id": db, "node_id": second, "kind": "semantic",
                        "shown_summary": "Second version", "full_get_fingerprint": "b" * 64})
                    row = {"notice": {"binding": {"key": {"lo": first, "hi": second, "kind": "disagreement"},
                        "endpoints": [{"id": first, "meaning": "a" * 64}, {"id": second, "meaning": "b" * 64}]},
                        "concern": "Version differs", "missing_fact": "Installed version?"}, "finding": None}
                    packet["concerns"] = [{"shown_text": "Versions differ", "displayed_endpoint_ids": [first, second],
                                           "expected_row": row}]
                source.refresh_delivery_packet(packet)
                self.source.write(source.opening() + [source.delivered(packet), source.call(), source.output(),
                                                       source.assistant(), source.event("task_complete")])
                observed = self.source.observe(delivery=packet)
                prompt, context = contract.prepare(observed, project_focus=FOCUS, global_preferences_enabled=preferences)
                self.assertIsNotNone(prompt, context)
                self.assertEqual(context.routing_enabled, routed)
                self.assertEqual(bool(context.concern_bindings), maintenance)
                self.assertLessEqual(context.authored_bytes, contract.MAX_AUTHORED_BYTES)
                self.assertEqual(context.authored_bytes, len(prompt.encode()) + len(contract.instructions_for(context).encode())
                                 + len(contract._encoded(contract.schema_for(context))))

    def test_reload_invalidates_ready_work_without_new_allowance(self):
        old = self.load()
        event = {"session_id": "focus-session", "turn_id": "t1", "prompt": "Inspect storage invariants"}
        reader_worker.notice(old, event, True)
        def spent(data):
            data.update(attempts=3, input_tokens=91, output_tokens=12)
            return None, True
        self.assertTrue(reader_worker._state(old, "focus-session", spent)[0])
        path, _ = reader_worker._paths(old, "focus-session")
        before = json.loads(path.read_bytes())
        self.write_focus()
        new = self.load()
        reader_worker.notice(new, {**event, "turn_id": "t2"}, True)
        after = json.loads(path.read_bytes())
        self.assertNotEqual(before["generation"], after["generation"])
        for field in ("attempts", "input_tokens", "output_tokens"):
            self.assertEqual(after[field], before[field])
        self.assertIsNone(after["ready"])

    def test_scoped_packet_exact_budget_and_no_evidence_id(self):
        self.source.write(source.opening() + [source.assistant(), source.event("task_complete")])
        self.observed = self.source.observe()
        prompt, context = contract.prepare(self.observed, project_focus=FOCUS)
        packet = json.loads(prompt[len(contract.PROMPT_PREFIX):])
        self.assertEqual(packet["project_focus"], FOCUS)
        self.assertNotIn(FOCUS, [binding.text for binding in context.bindings])
        self.assertEqual(context.authored_bytes, len(prompt.encode()) +
            len(contract.instructions_for(context).encode()) + len(contract._encoded(contract.schema_for(context))))
        self.assertIn(contract.PROJECT_FOCUS_INSTRUCTIONS, contract.instructions_for(context))
        self.assertLessEqual(context.authored_bytes, contract.MAX_AUTHORED_BYTES)
        with patch.object(contract, "MAX_AUTHORED_BYTES", context.authored_bytes):
            self.assertIsNotNone(contract.prepare(self.observed, project_focus=FOCUS)[0])
        with patch.object(contract, "MAX_AUTHORED_BYTES", context.authored_bytes - 1):
            self.assertEqual(contract.prepare(self.observed, project_focus=FOCUS), (None, "authored_input_limit"))
        self.assertEqual(contract.prepare(self.observed, project_focus=FOCUS, recording_scope="workshop"),
                         (None, "invalid_project_focus_scope"))
        for bad in (True, {}, "x" * (contract.MAX_PROJECT_FOCUS_BYTES + 1), "\0"):
            self.assertEqual(contract.prepare(self.observed, project_focus=bad), (None, "invalid_project_focus"))

    def test_escaped_policy_is_charged_exactly_and_never_truncated(self):
        focus = '\\' * contract.MAX_PROJECT_FOCUS_BYTES
        prompt, context = contract.prepare(self.observed, project_focus=focus)
        self.assertEqual(json.loads(prompt[len(contract.PROMPT_PREFIX):])["project_focus"], focus)
        self.assertEqual(context.authored_bytes, len(prompt.encode()) +
                         len(contract.instructions_for(context).encode()) + len(contract._encoded(contract.schema_for(context))))

    def test_association_and_global_lane_keep_existing_evidence_rules(self):
        cards = [{"id": DB_ID, "kind": "semantic", "summary": "Storage invariants"}]
        prompt, context = contract.prepare(self.observed, cards, project_focus=FOCUS,
                                          global_preferences_enabled=True)
        user = next(item.evidence_id for item in context.bindings if item.kind == "user_statement")
        assistant = next(item.evidence_id for item in context.bindings if item.kind == "assistant_assertion")
        answer = {"proposal": {"kind": "lesson", "destination": "project", "summary": "Observed invariant",
                  "body": "The result supported this project condition.", "evidence_ids": [user],
                  "associate_with": "overlap001"}}
        proposal = contract.validate_answer(answer, context)["proposal"]
        self.assertEqual(proposal.associate_with.native_id, DB_ID)
        for field, value in (("associate_with", "project_focus"), ("evidence_ids", ["project_focus"])):
            bad = copy.deepcopy(answer)
            bad["proposal"][field] = value
            with self.assertRaises(ValueError):
                contract.validate_answer(bad, context)
        global_answer = copy.deepcopy(answer)
        global_answer["proposal"].update(destination="global_preference", associate_with=None)
        self.assertEqual(contract.validate_answer(global_answer, context)["proposal"].destination, "global_preference")
        global_answer["proposal"]["evidence_ids"] = [assistant]
        with self.assertRaises(ValueError):
            contract.validate_answer(global_answer, context)
        self.assertIn(contract.GLOBAL_PREFERENCE_INSTRUCTIONS, contract.instructions_for(context))

    def test_overlap_and_runtime_prepare_same_scoped_packet(self):
        _, expected = contract.prepare(self.observed, project_focus=FOCUS)
        with patch.object(contract, "prepare", wraps=contract.prepare) as prepared:
            contract.overlap_plan(self.observed, project_focus=FOCUS)
            self.assertEqual(prepared.call_args.kwargs["project_focus"], FOCUS)
        runtime = reader_runtime.ReaderRuntime(self.value, self.root / "scratch")
        def offline(preparation, **options):
            _, actual = preparation()
            self.assertEqual(actual, expected)
            return {"assessment": None}
        with patch.object(runtime, "_fresh_assessment", side_effect=offline):
            runtime.assess(self.observed, project_focus=FOCUS)

    def test_selector_router_receive_no_focus_payload(self):
        self.value["_project_focus"] = FOCUS
        runtime = reader_runtime.ReaderRuntime(self.value, self.root / "scratch")
        dialogue = [{"role": "user", "text": "Inspect storage"}]
        with patch("reader_runtime.prepare", return_value=(None, "invalid_input")) as prepared:
            runtime.select(dialogue, [])
            self.assertEqual(prepared.call_args.args, (dialogue, []))
            self.assertNotIn("project_focus", prepared.call_args.kwargs)
        with patch("routing_contract.prepare", return_value=(None, "invalid_input")) as prepared:
            def offline(preparation, **options):
                return preparation()
            with patch.object(runtime, "_fresh_assessment", side_effect=offline):
                runtime.route("storage", [], expected_db_id=DB_ID)
            self.assertNotIn("project_focus", prepared.call_args.kwargs)

    def test_copied_runtime_reads_policy_without_installed_hooks_or_stores(self):
        self.write_focus()
        plan = install.prepare(self.project, self.root / "runtime", self.codex, self.codex, 18765,
            recall_mode="async", reader_model="gpt-6.1-sol", reader_codex=self.codex,
            recording_mode="automatic")
        receipt = install.apply(plan)
        prefix = Path(plan["prefix"])
        config = prefix / "config/hooks.json"
        for name in ("hooks.py", "recording_jobs.py", "recording_contract.py", "reader_runtime.py"):
            self.assertEqual((prefix / "lib" / name).read_bytes(), (Path(__file__).parent / name).read_bytes())
        code = ("import hooks,recording_jobs; from pathlib import Path; "
                f"c=hooks._config(Path({str(config)!r})); assert c['_project_focus']=={FOCUS!r}; "
                "assert c['_reader_config_pin']==recording_jobs._config_digest(c); print('copied-project-focus: ok')")
        result = subprocess.run([sys.executable, "-B", "-c", code], cwd=prefix / "lib",
                                capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("copied-project-focus: ok", result.stdout)
        self.assertFalse((self.project / ".mneme/codex-memory.db").exists())
        install.uninstall(Path(receipt["receipt"]))


class ProjectFocusJobTests(unittest.TestCase, RecordingFixture):
    def setUp(self):
        RecordingFixture.__init__(self, self)
        focus = self.root / ".mneme/hippocampus.md"
        focus.parent.mkdir()
        focus.write_text(FOCUS)
        focus.chmod(0o600)
        self.config["_project_focus"] = jobs.load_project_focus(self.config)

    def test_job_threads_frozen_policy_only_into_assessment(self):
        self.closed()
        owner = self
        class Runtime:
            def assess(self, observation, overlap, *, timeout, **options):
                owner.assertEqual(options["project_focus"], FOCUS)
                prompt, context = contract.prepare(observation, overlap, **options)
                owner.assertEqual(json.loads(prompt[len(contract.PROMPT_PREFIX):])["project_focus"], FOCUS)
                return {"reason": "abstained", "provider_attempt": True,
                        "usage": {"input_tokens": 20, "output_tokens": 5}, "proposal": None}
        self.step(Runtime())
        self.assertEqual(len(self.reservations), 1)
        self.assertEqual(self.writes, [])
        for call in self.overlap.call_args_list + self.identity.call_args_list:
            self.assertNotIn("project_focus", call.kwargs)
        self.assertNotIn(FOCUS, json.dumps(self.state()))

    def test_policy_edit_cancels_pending_job_before_new_provider_or_write(self):
        self.closed()
        focus = self.root / ".mneme/hippocampus.md"
        focus.write_text("Changed policy")
        self.step()
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.accounts, [])
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["receipts"][-1]["reason"], "config_changed")


if __name__ == "__main__":
    unittest.main()

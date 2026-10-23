"""Body-free activity producers; all state, bindings and owners are fixtures."""
import io
import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
from contextlib import contextmanager
import unittest
from unittest.mock import Mock, patch

import core_hook
import hook_launcher
import hooks


NODE = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
OWNER = "01ARZ3NDEKTSV4RRFFQ69G5FAW"
DIGEST = "a" * 64


class ActivityHookTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.workspace = self.root / "workspace"
        self.workspace.mkdir()
        self.config = {"schema": hooks.CONFIG_SCHEMA_V13, "project_root": self.workspace,
                       "state_dir": self.root / "state", "memory_scope": "project",
                       "memory_mode": "async", "recall_mode": "off", "recording_mode": "off"}
        self.event = {"hook_event_name": "UserPromptSubmit", "cwd": str(self.workspace),
                      "session_id": "fixture-session", "turn_id": "fixture-turn", "prompt": "SECRET prompt"}
        # No ambient manifest or live host configuration may enter a fixture.
        self.env = patch.dict(os.environ, {"MNEME_ACTIVITY_CONFIG": str(self.root / "missing.json")})
        self.env.start()
        self.addCleanup(self.env.stop)

    def capture(self):
        calls = []
        def emit(event, kind, outcome, **fields):
            calls.append({"event": {k: event.get(k) for k in ("session_id", "turn_id", "cwd")},
                          "kind": kind, "outcome": outcome, **fields})
            return True
        return calls, patch("session_activity.emit", side_effect=emit)

    def test_disabled_before_async_early_return(self):
        calls, emitter = self.capture()
        with emitter, patch("registry_runtime.admit_session"), patch("hooks._admit_async_state"), \
                patch("hooks._reader_worker", side_effect=AssertionError("no worker in disabled mode")):
            self.assertEqual(hooks.handle_event(self.event, self.config), {})
        self.assertEqual([(x["kind"], x["outcome"], x.get("reason")) for x in calls],
                         [("recall_admission", "skipped", "disabled_mode")])
        self.assertNotIn("SECRET", json.dumps(calls))

    def test_lifecycle_modes_and_exact_startup_marker(self):
        event = {**self.event, "hook_event_name": "SessionStart", "source": "resume"}
        calls, emitter = self.capture()
        with emitter, patch("registry_runtime.admit_session"), patch("hooks._admit_async_state"), \
                patch("hooks._reader_worker", return_value=SimpleNamespace(reset=lambda *_: None)):
            output = hooks.handle_event(event, self.config)
        self.assertEqual([(x["kind"], x["outcome"]) for x in calls],
                         [("lifecycle", "resume"), ("recall_admission", "mode_disabled"),
                          ("recording", "mode_disabled")])
        text = output["hookSpecificOutput"]["additionalContext"]
        self.assertIn("codex:fixture-session", text)
        self.assertIn("instead of guessing latest", text)
        self.assertLessEqual(len(text.encode()), hooks.MAX_CONTEXT_BYTES)

    def test_failed_logging_does_not_change_hook_result(self):
        event = {**self.event, "hook_event_name": "SessionStart", "source": "startup"}
        output = hooks._context("SessionStart", "Existing normal context.")
        with patch("hooks._handle_event", return_value=output), \
                patch("session_activity.emit", side_effect=RuntimeError("secret filesystem error")):
            self.assertIs(hooks.handle_event(event, self.config), output)
        self.assertEqual(list(self.root.iterdir()), [self.workspace])

    def test_posttool_wire_still_discards_tool_fields(self):
        event = {**self.event, "hook_event_name": "PostToolUse", "tool_input": {"secret": "SECRET"},
                 "tool_output": "SECRET", "tool_name": "mcp__owner__save"}
        admitted = hooks._read_event(io.BytesIO(json.dumps(event).encode()))
        self.assertEqual(set(admitted), {"hook_event_name", "cwd", "session_id", "turn_id"})
        calls, emitter = self.capture()
        with emitter, patch("hooks._handle_event", return_value={}):
            self.assertEqual(hooks.handle_event(admitted, self.config), {})
        self.assertEqual(calls, [])  # Explicit tool operation is not attributed.

    def test_turnless_lifecycle_does_not_invent_retry_identity(self):
        event = {**self.event, "hook_event_name": "SessionStart", "source": "resume", "turn_id": None}
        calls, emitter = self.capture()
        with emitter:
            hooks._activity_lifecycle(event, self.config)
        self.assertIsNone(calls[0]["origin"]["attempt"])
        self.assertNotIn("key", calls[0])  # Writer supplies occurrence, not unique real start proof.

    def test_late_lifecycle_completion_keeps_its_original_occurrence(self):
        event = {**self.event, "hook_event_name": "SessionStart", "source": "resume"}
        clock = [100]
        calls, emitter = self.capture()
        def complete(*_args, **_kwargs):
            clock[0] = 300
            return hooks._context("SessionStart", "normal output")
        with emitter, patch("hooks.time.time", side_effect=lambda: clock[0]), \
                patch("hooks._handle_event", side_effect=complete):
            hooks.handle_event(event, self.config)
        self.assertEqual([c["at"] for c in calls], [100, 100, 100])

    def test_lifecycle_same_turn_deduplicates_and_turns_stay_distinct(self):
        activity_root = self.root / "activity"
        activity_root.mkdir(mode=0o700)
        manifest = self.root / "activity.json"
        manifest.write_text(json.dumps({"schema": "mneme.session-activity.config.v1", "root": str(activity_root),
            "workspaces": [{"path": str(self.workspace), "source": "codex"}], "device": None}))
        manifest.chmod(0o600)
        with patch.dict(os.environ, {"MNEME_ACTIVITY_CONFIG": str(manifest)}):
            for turn in ("first", "first", "second"):
                self.assertTrue(hooks._activity_lifecycle({**self.event, "hook_event_name": "Interrupt", "turn_id": turn}, self.config))
        records = list(activity_root.glob("codex-*.json"))
        self.assertEqual(len(records), 1)
        events = json.loads(records[0].read_text())["events"]
        self.assertEqual([e["origin"]["turn"] for e in events], ["first", "second"])

    def delivery(self):
        card = {"id": NODE, "summary": "SECRET summary", "status": "active", "source": "fixture", "fingerprint": "c" * 64}
        activity = {"origin": {"turn": "fixture-turn", "generation": "b" * 32,
                                "context_generation": 8, "attempt": "attempt-7"},
                    "selection_digest": DIGEST, "groups": [{"owner": {"name": "project", "id": OWNER, "db": "project"},
                                                             "nodes": [NODE], "count": 1, "omitted": 0}]}
        return {"outcome": "emitted", "cards": [card], "db_id": OWNER, "_activity": activity}

    def test_final_output_provenance_and_recording_packet_are_separate(self):
        result = self.delivery()
        calls, emitter = self.capture()
        recorder = Mock()
        worker = SimpleNamespace(consume=Mock(return_value=result))
        event = {**self.event, "hook_event_name": "PostToolUse"}
        config = {**self.config, "recall_mode": "automatic", "recording_mode": "automatic"}
        with emitter, patch("hooks._admit_async_state"), patch("hooks._reader_worker", return_value=worker), \
                patch("hooks._recording_jobs", return_value=recorder):
            output = hooks._handle_async(event, config)
        self.assertIn("SECRET summary", output["hookSpecificOutput"]["additionalContext"])
        self.assertEqual(calls[0]["outcome"], "prepared")
        self.assertEqual(calls[0]["origin"], result["_activity"]["origin"])
        self.assertNotIn("SECRET", json.dumps(calls))
        self.assertEqual(recorder.attach_delivery.call_args.kwargs, {})
        self.assertNotIn("_activity", recorder.attach_delivery.call_args.args[1])

    def test_consumption_is_not_final_output_on_packing_mismatch(self):
        result = self.delivery()
        calls, emitter = self.capture()
        event = {**self.event, "hook_event_name": "PostToolUse"}
        with emitter, patch("hooks._admit_async_state"), \
                patch("hooks._reader_worker", return_value=SimpleNamespace(consume=lambda *_: result)), \
                patch("hooks._pack_async_delivery", return_value={"cards": []}):
            output = hooks._handle_async(event, {**self.config, "recall_mode": "automatic"})
        self.assertEqual(output, {})
        self.assertEqual([(x["kind"], x["outcome"], x["reason"]) for x in calls],
                         [("hook_output", "omitted", "packing_mismatch")])

    def test_replayed_packet_output_keeps_direct_reader_origin_without_admission_link(self):
        event = {**self.event, "hook_event_name": "PostToolUse"}
        origins = []
        for generation in ("b" * 32, "c" * 32):
            result = self.delivery()
            result["_activity"]["origin"]["generation"] = generation
            calls, emitter = self.capture()
            recorder = SimpleNamespace(attach_delivery=lambda *_args: {"outcome": "replay"})
            with emitter, patch("hooks._admit_async_state"), \
                    patch("hooks._reader_worker", return_value=SimpleNamespace(consume=lambda *_: result)), \
                    patch("hooks._recording_jobs", return_value=recorder):
                output = hooks._handle_async(event, {**self.config, "recall_mode": "automatic", "recording_mode": "automatic"})
            self.assertIn("hookSpecificOutput", output)
            self.assertEqual(calls[0]["kind"], "hook_output")
            self.assertNotIn("handoff", json.dumps(calls))
            origins.append(calls[0]["origin"])
        self.assertNotEqual(*origins)

    def test_output_survives_malformed_optional_projection(self):
        result = self.delivery()
        result["_activity"]["groups"] = [None]
        event = {**self.event, "hook_event_name": "PostToolUse"}
        with patch("hooks._admit_async_state"), \
                patch("hooks._reader_worker", return_value=SimpleNamespace(consume=lambda *_: result)):
            output = hooks._handle_async(event, {**self.config, "recall_mode": "automatic"})
        self.assertIn("hookSpecificOutput", output)

    def test_output_survives_writer_failure_without_retry(self):
        result = self.delivery()
        worker = SimpleNamespace(consume=Mock(return_value=result))
        event = {**self.event, "hook_event_name": "PostToolUse"}
        expected = hooks._context("PostToolUse", hooks._pack_async_delivery(result["cards"],
            event["session_id"], event["turn_id"], OWNER)["context"])
        with patch("hooks._admit_async_state"), patch("hooks._reader_worker", return_value=worker), \
                patch("session_activity.emit", side_effect=OSError("SECRET writer error")):
            self.assertEqual(hooks._handle_async(event, {**self.config, "recall_mode": "automatic"}), expected)
        self.assertEqual(worker.consume.call_count, 1)

    def test_checkpoint_claim_is_published_after_state_lock(self):
        calls, emitter = self.capture()
        def seed(data, now):
            data["turns"]["fixture-turn"] = {"at": now}
            return None, True
        hooks._with_state(self.config["state_dir"], "fixture-session", seed)
        depth = [0]
        original = hooks._with_state
        def with_state(*args, **kwargs):
            depth[0] += 1
            try:
                return original(*args, **kwargs)
            finally:
                depth[0] -= 1
        def emit(event, kind, outcome, **fields):
            self.assertEqual(depth[0], 0)
            calls.append({"kind": kind, "outcome": outcome, **fields})
            return True
        with patch("hooks._with_state", side_effect=with_state), patch("session_activity.emit", side_effect=emit):
            result = hooks.checkpoint(self.config, "fixture-session", "fixture-turn", "captured", [NODE])
        self.assertEqual(result, {"checkpoint": "captured", "already_recorded": False})
        self.assertEqual(calls[0]["outcome"], "actor_reported_capture")
        self.assertEqual((calls[0]["count"], calls[0]["omitted"]), (1, 1))
        self.assertNotIn("nodes", calls[0])  # Raw actor IDs were not owner-qualified.
        def reset(data, _now):
            data["context_epoch"] += 1
            return None, True
        hooks._with_state(self.config["state_dir"], "fixture-session", reset)
        with patch("session_activity.emit", side_effect=emit):
            replay = hooks.checkpoint(self.config, "fixture-session", "fixture-turn", "captured", [NODE])
        self.assertTrue(replay["already_recorded"])
        self.assertEqual(len(calls), 1)  # Never relabel the original claim with the newer epoch.

    def test_core_projection_is_pure_and_after_final_revalidation(self):
        store = SimpleNamespace(name="project", db_id=OWNER, db_alias="project")
        selected = SimpleNamespace(cwd=str(self.workspace), core=[store], digest=DIGEST, omissions=[])
        event = {**self.event, "hook_event_name": "SessionStart", "source": "startup"}
        sections = [{"scope": "project", "db": "project", "outcome": "ok", "total": 1,
                     "cards": [{"body": "SECRET core body"}]}]
        with patch("store_registry.revalidate", return_value=selected) as revalidate, \
                patch("core_hook.collect_registry_core", return_value=sections), \
                patch("session_activity.emit", side_effect=AssertionError("passive collector must not log")):
            prepared = core_hook.handle_selection(event, selected, with_activity=True)
        self.assertEqual(revalidate.call_count, 2)
        self.assertIn("SECRET core body", json.dumps(prepared.output))
        self.assertNotIn("SECRET", repr(prepared.observations))
        self.assertEqual(prepared.observations[0].count, 1)
        calls, emitter = self.capture()
        with emitter, patch("core_hook.handle_hook_event", return_value=prepared):
            self.assertIs(hook_launcher._core_event(event, self.config), prepared.output)
        self.assertEqual(calls[0]["kind"], "core")
        self.assertEqual(calls[0]["owner"]["id"], OWNER)
        self.assertNotIn("SECRET", json.dumps(calls))
        with patch("store_registry.revalidate", side_effect=[selected, ValueError("revoked")]), \
                patch("core_hook.collect_registry_core", return_value=sections):
            with self.assertRaises(ValueError):
                core_hook.handle_selection(event, selected, with_activity=True)

    def test_outer_core_revalidation_failure_never_publishes_prepared_counts(self):
        store = SimpleNamespace(name="project", db_id=OWNER, db_alias="project", contradiction_notice=False)
        selected = SimpleNamespace(cwd=str(self.workspace), core=[store], digest=DIGEST, omissions=[])
        event = {**self.event, "hook_event_name": "SessionStart", "source": "startup"}
        config = {**self.config, "_config_path": self.root / "config.json", "_registry_config_sha256": DIGEST}
        prepared = core_hook.PreparedCore({}, (core_hook.CoreObservation("project", OWNER, "project", "ok", 2),), DIGEST, 123)
        calls, emitter = self.capture()
        with emitter, patch("registry_runtime.selection", side_effect=[selected, ValueError("revoked")]), \
                patch("core_hook.handle_selection", return_value=prepared):
            with self.assertRaises(ValueError):
                hook_launcher._core_event(event, config)
        self.assertEqual([(c["kind"], c["outcome"]) for c in calls], [("core", "unavailable")])
        self.assertNotIn("count", calls[0])

    def test_core_lifecycle_sources_have_distinct_event_keys(self):
        prepared = core_hook.PreparedCore({}, (core_hook.CoreObservation("project", OWNER, "project", "ok", 2),), DIGEST, 123)
        for turn in ("same-turn", None):
            calls, emitter = self.capture()
            with emitter:
                for source in ("startup", "compact"):
                    hooks._activity_core({**self.event, "hook_event_name": "SessionStart", "source": source, "turn_id": turn}, prepared)
            self.assertNotEqual(calls[0]["key"], calls[1]["key"])

    def test_launcher_flushes_only_after_native_budget_exit(self):
        budget_active = [False]
        writes = []
        @contextmanager
        def budget(_seconds):
            budget_active[0] = True
            try:
                yield
            finally:
                budget_active[0] = False
        def write(*args, **kwargs):
            self.assertFalse(budget_active[0])
            writes.append((args, kwargs))
            return True
        def dispatch(event, _path, **_kwargs):
            self.assertTrue(budget_active[0])
            hooks._activity_emit(event, "recall_admission", "queued")
            self.assertEqual(writes, [])
            return {"normal": "unchanged"}
        with patch("store_registry.native_selection_budget", side_effect=budget), \
                patch("hook_launcher._handle_event", side_effect=dispatch), patch("session_activity._append", side_effect=write):
            self.assertEqual(hook_launcher.handle_event(self.event, self.root / "unused.json"), {"normal": "unchanged"})
        self.assertEqual(len(writes), 1)

    def test_deferred_startup_marker_requires_successful_flush_and_fits(self):
        event = {**self.event, "hook_event_name": "SessionStart", "source": "startup"}
        def dispatch(event, _path, **_kwargs):
            hooks._activity_lifecycle(event, self.config)
            return hooks._context("SessionStart", "Existing normal context.")
        for success in (False, True):
            with patch("hook_launcher._handle_event", side_effect=dispatch), \
                    patch("session_activity._append", return_value=success):
                output = hook_launcher.handle_event(event, self.root / "unused.json")
            self.assertEqual("codex:fixture-session" in json.dumps(output), success)
        packed = hooks._context("SessionStart", "x" * hooks.MAX_CONTEXT_BYTES)
        self.assertIs(hooks._activity_startup_marker(event, packed, True), packed)


if __name__ == "__main__":
    unittest.main()

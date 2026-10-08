"""Offline Codex lifecycle fixtures; no Mneme service or transcript access."""
from __future__ import annotations

import importlib.util
import io
import json
from pathlib import Path
import shlex
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from types import SimpleNamespace
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("mneme_codex_hooks", Path(__file__).with_name("hooks.py"))
hooks = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(hooks)


class HookTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "project"
        self.root.mkdir()
        self.state = Path(self.temp.name) / "state"
        self.config_path = Path(self.temp.name) / "config with spaces.json"
        self.config_path.write_text(json.dumps({"schema": hooks.CONFIG_SCHEMA, "project_root": str(self.root),
                                                "state_dir": str(self.state)}))
        self.config = hooks._config(self.config_path)
        self.base = {"cwd": str(self.root), "session_id": "session_1", "turn_id": "turn_1"}

    def event(self, name, **extra):
        return {**self.base, "hook_event_name": name, **extra}

    def state_json(self):
        return json.loads(hooks._state_path(self.state, "session_1").read_text())

    def test_v1_start_and_compact_are_bounded_and_nonmutating(self):
        for source in ("startup", "resume", "clear", "compact"):
            result = hooks.handle_event(self.event("SessionStart", source=source), self.config)
            self.assertEqual(result["hookSpecificOutput"]["hookEventName"], "SessionStart")
            self.assertLessEqual(len(result["hookSpecificOutput"]["additionalContext"].encode()), hooks.MAX_CONTEXT_BYTES)
            self.assertIn("configured", json.dumps(result))
            self.assertNotIn("is available", json.dumps(result))
        self.assertFalse(self.state.exists())

    def test_irrelevant_and_unknown_project_are_noops(self):
        for prompt in ("ok", "thanks!", "what time is it?", "translate hello"):
            self.assertEqual(hooks.handle_event(self.event("UserPromptSubmit", prompt=prompt), self.config), {})
        outside = self.event("UserPromptSubmit", cwd=self.temp.name, prompt="Implement the graph refactor")
        self.assertEqual(hooks.handle_event(outside, self.config), {})
        self.assertFalse(self.state.exists())

    def scoped_config(self, scope="workshop", *, schema=hooks.CONFIG_SCHEMA, mode="reminder"):
        data = {"schema": schema, "project_root": str(self.root),
                "state_dir": str(self.state), "memory_scope": scope}
        if schema == hooks.CONFIG_SCHEMA_V2:
            data.update(service_config=str(Path(self.temp.name) / "service.json"), recall_mode=mode)
        self.config_path.write_text(json.dumps(data))
        return hooks._config(self.config_path)

    def test_scope_defaults_and_explicit_opt_in_are_typed(self):
        self.assertEqual(self.config["memory_scope"], "project")
        self.assertEqual(self.v2()["memory_scope"], "project")
        for schema in (hooks.CONFIG_SCHEMA, hooks.CONFIG_SCHEMA_V2):
            for scope in ("project", "workshop"):
                with self.subTest(schema=schema, scope=scope):
                    config = self.scoped_config(scope, schema=schema)
                    self.assertEqual(config["memory_scope"], scope)
                    self.assertEqual(config["recall_mode"], "reminder")
            for invalid in (None, "", "global", "personal", True, 1, [], {}):
                with self.subTest(schema=schema, invalid=invalid), self.assertRaisesRegex(hooks.HookError, "invalid memory_scope"):
                    self.scoped_config(invalid, schema=schema)
        with self.assertRaisesRegex(hooks.HookError, "requires reminder"):
            self.scoped_config(schema=hooks.CONFIG_SCHEMA_V2, mode="automatic")
        self.assertFalse(self.state.exists())

    def test_workshop_lifecycle_routes_memories_without_reading_or_resetting(self):
        for schema in (hooks.CONFIG_SCHEMA, hooks.CONFIG_SCHEMA_V2):
            config = self.scoped_config(schema=schema)
            with patch.object(hooks, "_recall_cards", side_effect=AssertionError("passive read attempted")):
                contexts = [hooks.handle_event(self.event("SessionStart", source=source), config)
                            ["hookSpecificOutput"]["additionalContext"]
                            for source in ("startup", "resume", "clear", "compact")]
                self.assertFalse(self.state.exists())
                submit = self.event("UserPromptSubmit", prompt="Review the workshop experiment.")
                context = hooks.handle_event(submit, config)["hookSpecificOutput"]["additionalContext"]
                contexts.append(context)
                self.assertEqual(hooks.handle_event(submit, config)["hookSpecificOutput"]["additionalContext"], context)
                before = self.state_json()
                hooks.handle_event(self.event("SessionStart", source="compact"), config)
                self.assertEqual(before, self.state_json())
                stop = hooks.handle_event(self.event("Stop", stop_hook_active=False), config)
                contexts.append(stop["reason"])
                self.assertEqual(hooks.handle_event(self.event("Stop", stop_hook_active=False), config), {})
                synthetic = self.event("UserPromptSubmit", turn_id="synthetic", prompt=stop["reason"])
                self.assertEqual(hooks.handle_event(synthetic, config), {})
            for context in contexts:
                self.assertIn("configured global user store only for personal/shared continuity", context)
                self.assertIn("Project details belong in their explicitly configured project store", context)
                self.assertIn("keep them in artifacts, not global memory", context)
                self.assertIn("save with kind episode", context)
                self.assertIn("Neither is required", context)
                self.assertLessEqual(len(context.encode()), hooks.MAX_CONTEXT_BYTES)
                self.assertNotIn("this allowlisted project", context)
            for context in contexts[:-1]:
                self.assertIn("wake/resume, meaningful task changes", context)
                self.assertIn("before decisions", context)
                self.assertIn("no per-tool retrieval or repeated dumps", context)
            self.assertIn("no passive read was attempted", contexts[-2])
            state = self.state_json()
            self.assertEqual(state["context_epoch"], 0)
            self.assertEqual(set(state["turns"]), {"turn_1"})
            self.assertEqual(state["turns"]["turn_1"]["recall"]["outcome"], "skipped")
            hooks.checkpoint(config, "session_1", "turn_1", "none", [])
            self.assertEqual(hooks.handle_event(self.event("Stop", stop_hook_active=False), config), {})
            hooks._state_path(self.state, "session_1").unlink()
            hooks._state_path(self.state, "session_1").with_suffix(".lock").unlink()
            self.state.rmdir()

    def test_workshop_does_not_cross_nested_or_isolated_profiles(self):
        config = self.scoped_config()
        # An existing parent opportunity must not produce a continuation once
        # a nearer project profile or isolated root becomes authoritative.
        hooks.handle_event(self.event("UserPromptSubmit", prompt="Review the workshop."), config)
        before = self.state_json()
        nested = self.root / "nested"
        nested.mkdir()
        for root, modes in ((nested, ("default", "private", "isolated")), (self.root, ("isolated",))):
            (root / ".mneme").mkdir(exist_ok=True)
            for mode in modes:
                (root / ".mneme/profile.json").write_text(json.dumps({"schema": "mneme.profile.v1", "mode": mode}))
                for event in (self.event("SessionStart", cwd=str(root), source="compact"),
                              self.event("UserPromptSubmit", cwd=str(root), prompt="Review the workshop."),
                              self.event("Stop", cwd=str(root), stop_hook_active=False)):
                    with self.subTest(root=root, mode=mode, event=event["hook_event_name"]):
                        self.assertEqual(hooks.handle_event(event, config), {})
                        self.assertEqual(self.state_json(), before)

    def test_workshop_trivial_subagent_and_outside_root_do_not_create_opportunities(self):
        config = self.scoped_config()
        events = (self.event("UserPromptSubmit", prompt="ok"),
                  self.event("UserPromptSubmit", prompt="Review the workshop.", agent_id="child"),
                  self.event("SessionStart", source="startup", agent_type="worker"),
                  self.event("UserPromptSubmit", prompt="Review the workshop.", cwd=self.temp.name))
        for event in events:
            self.assertEqual(hooks.handle_event(event, config), {})
        self.assertFalse(self.state.exists())

    def test_all_checkpoint_cues_offer_optional_episodes_without_forcing_lessons(self):
        contexts = [hooks.handle_event(self.event("SessionStart", source=source), self.config)
                    ["hookSpecificOutput"]["additionalContext"]
                    for source in ("startup", "resume", "clear", "compact")]
        prompt = hooks.handle_event(self.event("UserPromptSubmit", prompt="Review the experiment."), self.config)
        contexts.append(prompt["hookSpecificOutput"]["additionalContext"])
        stop = hooks.handle_event(self.event("Stop", stop_hook_active=False), self.config)
        contexts.append(stop["reason"])
        for context in contexts:
            with self.subTest(context=context):
                self.assertIn("save with kind episode", context)
                self.assertIn("even without a lesson", context)
                self.assertIn("kind note for a reusable lesson", context)
                self.assertIn("installed tool catalog lacks save", context)
                self.assertIn("Neither is required", context)
                self.assertIn("No duplicate retelling", context)
                self.assertLessEqual(len(context.encode()), hooks.MAX_CONTEXT_BYTES)
        for context in contexts[-2:]:
            self.assertIn("read-back note IDs or episode edition_ids", context)
            self.assertIn("--outcome none", context)
            self.assertIn("--outcome deferred", context)
        self.assertEqual(hooks.handle_event(self.event("Stop", stop_hook_active=False), self.config), {})
        self.assertIsNone(self.state_json()["turns"]["turn_1"]["checkpoint"])

    def test_episode_edition_checkpoint_uses_existing_id_contract(self):
        hooks.handle_event(self.event("UserPromptSubmit", prompt="Review the experiment."), self.config)
        # The hook records agent-declared read-back IDs; it must not substitute
        # an episode root or pretend that metadata alone verifies a store write.
        root_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
        edition_id = "01ARZ3NDEKTSV4RRFFQ69G5FAW"
        result = hooks.checkpoint(self.config, "session_1", "turn_1", "captured", [edition_id])
        self.assertFalse(result["already_recorded"])
        saved = self.state_json()["turns"]["turn_1"]["checkpoint"]
        self.assertEqual(saved, {"outcome": "captured", "ids": [edition_id]})
        self.assertNotIn(root_id, saved["ids"])
        self.assertTrue(hooks.checkpoint(self.config, "session_1", "turn_1", "captured", [edition_id])["already_recorded"])
        self.assertEqual(hooks.handle_event(self.event("Stop", stop_hook_active=False), self.config), {})

    def test_isolated_profile_disables_automatic_prompt_recall(self):
        (self.root / '.mneme').mkdir()
        (self.root / '.mneme/profile.json').write_text(json.dumps({
            'schema': 'mneme.profile.v1', 'mode': 'isolated'}))
        configured = {**self.config, 'recall_mode': 'automatic',
                      'service_config': Path(self.temp.name) / 'service.json'}
        event = self.event('UserPromptSubmit', prompt='Implement the graph refactor')
        with patch.object(hooks, '_recall_cards', side_effect=AssertionError('automatic recall contacted service')):
            result = hooks.handle_event(event, configured)
        self.assertIn('Reminder-only mode', result['hookSpecificOutput']['additionalContext'])

    def test_prompt_checkpoint_and_replay_continuation_guard(self):
        text = "Implement the project graph refactor, test the failure path, and record what was learned."
        prompt = self.event("UserPromptSubmit", prompt=text)
        context = hooks.handle_event(prompt, self.config)
        self.assertIn("session_id=session_1, turn_id=turn_1", context["hookSpecificOutput"]["additionalContext"])
        command = hooks._checkpoint_command(self.config, "session_1", "turn_1")
        self.assertEqual(shlex.split(command), [sys.executable, str(Path(hooks.__file__).resolve()), "checkpoint",
                                                "--config", str(self.config_path.resolve()), "--session-id", "session_1",
                                                "--turn-id", "turn_1"])
        self.assertIn(command + " --outcome none", context["hookSpecificOutput"]["additionalContext"])
        self.assertEqual(hooks.handle_event(prompt, self.config), context)
        stop = self.event("Stop", stop_hook_active=False, last_assistant_message="secret result")
        continuation = hooks.handle_event(stop, self.config)
        self.assertEqual(continuation["decision"], "block")
        self.assertIn(command + " --outcome none", continuation["reason"])
        self.assertEqual(hooks.handle_event(stop, self.config), {})
        self.assertEqual(hooks.handle_event({**stop, "stop_hook_active": True}, self.config), {})
        synthetic = self.event("UserPromptSubmit", turn_id="turn_2", prompt=continuation["reason"])
        self.assertEqual(hooks.handle_event(synthetic, self.config), {})
        self.assertEqual(hooks.handle_event(self.event("Stop", turn_id="turn_2", stop_hook_active=True), self.config), {})
        self.assertEqual(set(self.state_json()["turns"]), {"turn_1"})
        self.assertEqual(hooks.checkpoint(self.config, "session_1", "turn_1", "captured", ["01ARZ3NDEKTSV4RRFFQ69G5FAV"])["checkpoint"], "captured")
        self.assertEqual(hooks.handle_event(stop, self.config), {})
        self.assertEqual(hooks.handle_event(synthetic, self.config), {})
        serialized = json.dumps(self.state_json())
        self.assertNotIn(text, serialized)
        self.assertNotIn("secret result", serialized)
        self.assertIn("prompt_sha256", serialized)

    def test_checkpoint_none_prevents_continuation(self):
        hooks.handle_event(self.event("UserPromptSubmit", prompt="Review the repository and verify the new parser behavior."), self.config)
        hooks.checkpoint(self.config, "session_1", "turn_1", "none", [])
        self.assertEqual(hooks.handle_event(self.event("Stop", stop_hook_active=False), self.config), {})
        self.assertTrue(hooks.checkpoint(self.config, "session_1", "turn_1", "none", [])["already_recorded"])
        with self.assertRaises(hooks.HookError):
            hooks.checkpoint(self.config, "session_1", "turn_1", "captured", ["01ARZ3NDEKTSV4RRFFQ69G5FAV"])

    def test_deferred_is_honest_checkpoint(self):
        hooks.handle_event(self.event("UserPromptSubmit", prompt="Research the transport design and test the CLI."), self.config)
        hooks.checkpoint(self.config, "session_1", "turn_1", "deferred", [])
        self.assertEqual(hooks.handle_event(self.event("Stop", stop_hook_active=False), self.config), {})
        self.assertEqual(self.state_json()["turns"]["turn_1"]["checkpoint"]["outcome"], "deferred")

    def test_crash_before_atomic_replace_keeps_old_state(self):
        hooks.handle_event(self.event("UserPromptSubmit", prompt="Design the new repository data model."), self.config)
        before = self.state_json()
        original_replace = hooks.os.replace
        def crash(_source, _target):
            raise OSError("simulated crash")
        try:
            hooks.os.replace = crash
            with self.assertRaises(OSError):
                hooks.checkpoint(self.config, "session_1", "turn_1", "none", [])
        finally:
            hooks.os.replace = original_replace
        self.assertEqual(self.state_json(), before)

    def test_corrupt_state_error_does_not_claim_checkpoint(self):
        self.state.mkdir()
        hooks._state_path(self.state, "session_1").write_text("{not JSON")
        with self.assertRaises(hooks.HookError):
            hooks.handle_event(self.event("UserPromptSubmit", prompt="Implement the test runner."), self.config)

    def test_subagent_and_missing_ids_graceful(self):
        self.assertEqual(hooks.handle_event(self.event("UserPromptSubmit", agent_id="agent_1", prompt="Implement tests."), self.config), {})
        self.assertEqual(hooks.handle_event({"cwd": str(self.root), "hook_event_name": "Stop"}, self.config), {})
        self.assertFalse(self.state.exists())

    def test_ttl_and_size_bounded(self):
        hooks.handle_event(self.event("UserPromptSubmit", prompt="Investigate the repository integration."), self.config)
        current = self.state_json()
        current["turns"]["turn_1"]["at"] = 0
        hooks._atomic_write(hooks._state_path(self.state, "session_1"), current)
        hooks.handle_event(self.event("UserPromptSubmit", turn_id="turn_2", prompt="Investigate the repository transport."), self.config)
        self.assertEqual(set(self.state_json()["turns"]), {"turn_2"})
        with self.assertRaises(hooks.HookError):
            hooks.checkpoint(self.config, "session_1", "turn_1", "none", [])

    def test_checkpoint_cli_accepts_config_after_subcommand(self):
        hooks.handle_event(self.event("UserPromptSubmit", prompt="Investigate the repository integration."), self.config)
        path = Path(self.temp.name) / "config.json"
        path.write_text(json.dumps({"schema": hooks.CONFIG_SCHEMA, "project_root": str(self.root),
                                    "state_dir": str(self.state)}))
        output = io.StringIO()
        with redirect_stdout(output):
            status = hooks.main(["checkpoint", "--config", str(path), "--session-id", "session_1",
                                 "--turn-id", "turn_1", "--outcome", "none"])
        self.assertEqual(status, 0)
        self.assertEqual(json.loads(output.getvalue())["checkpoint"], "none")

    def test_captured_ids_must_be_canonical_ulids(self):
        hooks.handle_event(self.event("UserPromptSubmit", prompt="Investigate the repository integration."), self.config)
        for bad in ("memory.abc", "01ARZ3NDEKTSV4RRFFQ69G5FAV ", "81ARZ3NDEKTSV4RRFFQ69G5FAV", "01ARZ3NDEKTSV4RRFFQ69G5FAO"):
            with self.assertRaises(hooks.HookError):
                hooks.checkpoint(self.config, "session_1", "turn_1", "captured", [bad])

    def test_stale_continuation_marker_does_not_open_new_turn(self):
        prompt = "MNEME_CHECKPOINT_CONTINUATION source_session_id=session_1 source_turn_id=gone token=" + "a" * 32
        result = hooks.handle_event(self.event("UserPromptSubmit", prompt=prompt), self.config)
        self.assertIn("systemMessage", result)
        self.assertEqual(hooks.handle_event(self.event("Stop", stop_hook_active=False), self.config), {})
        self.assertFalse(hooks._state_path(self.state, "session_1").exists())

    def v2(self, mode="automatic"):
        path = Path(self.temp.name) / "v2 config with spaces.json"
        path.write_text(json.dumps({"schema": hooks.CONFIG_SCHEMA_V2, "project_root": str(self.root),
                                    "state_dir": str(self.state), "service_config": str(Path(self.temp.name) / "service.json"),
                                    "recall_mode": mode}))
        return hooks._config(path)

    def v3(self):
        path = Path(self.temp.name) / "shadow config.json"
        path.write_text(json.dumps({"schema": hooks.CONFIG_SCHEMA_V3,
                                    "project_root": str(self.root), "state_dir": str(self.state),
                                    "service_config": str(Path(self.temp.name) / "service.json"),
                                    "memory_mode": "shadow", "shadow_model": "gpt-5.6-sol",
                                    "shadow_codex": str(Path(self.temp.name) / "codex"),
                                    "shadow_codex_sha256": "a" * 64}))
        return hooks._config(path)

    def test_shadow_opt_in_bounded_recent_cue_delivery_and_background_once(self):
        config = self.v3()
        card = {"id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "summary": "Use the parser guard",
                "status": "active", "source": "project", "fingerprint": "fp-a"}
        observation = {"schema": 1, "learning": "disabled", "cards": [
            {"node_id": card["id"], "lane": "primary", "card_sha256": "b" * 64,
             "graph_path": None}]}
        self.assertEqual(hooks.handle_event(self.event("SessionStart", source="startup"), config), {})
        with patch.object(hooks, "_recall_cards", return_value={"outcome": "ok", "cards": [card],
                                                           "elapsed_ms": 5, "observation": observation}) as recall:
            event = self.event("UserPromptSubmit", prompt="Review the parser guard before editing the API.")
            first = hooks.handle_event(event, config)
            self.assertIn(card["id"], json.dumps(first))
            self.assertNotIn("checkpoint", json.dumps(first).lower())
            self.assertEqual(hooks.handle_event(event, config), {})
            self.assertEqual(recall.call_count, 1)
            self.assertIn("Current task:", recall.call_args.args[1])
        saved = self.state_json()["turns"]["turn_1"]["shadow"]
        self.assertEqual(saved["cards"][0]["native"]["card_sha256"], "b" * 64)
        self.assertEqual(len(saved["cards"][0]["display_sha256"]), 64)
        with patch.object(hooks, "_shadow_launch", return_value=True) as launch:
            stop = self.event("Stop", last_assistant_message="Fixed the parser guard.", stop_hook_active=False)
            self.assertEqual(hooks.handle_event(stop, config), {})
            self.assertEqual(hooks.handle_event(stop, config), {})
            self.assertEqual(launch.call_count, 1)
            self.assertEqual(launch.call_args.args[2], "turn_1")
        self.assertEqual(self.state_json()["turns"]["turn_1"]["shadow"]["assessment"], "queued")

    def test_shadow_recent_window_compaction_replay_and_unknown_failure(self):
        config = self.v3()
        prompts = ["Investigate the parser state and report the bug.",
                   "Implement the parser fix with a regression test."]
        with patch.object(hooks, "_recall_cards", return_value={"outcome": "timeout", "cards": [],
                                                           "elapsed_ms": 1500}) as recall, \
             patch.object(hooks, "_shadow_launch", side_effect=AssertionError("assessment attempted")):
            for i, prompt in enumerate(prompts):
                self.assertEqual(hooks.handle_event(self.event("UserPromptSubmit", turn_id=f"t{i}", prompt=prompt), config), {})
                self.assertEqual(hooks.handle_event(self.event("Stop", turn_id=f"t{i}",
                                                               last_assistant_message="Done."), config), {})
            self.assertIn("Recent task:", recall.call_args.args[1])
            self.assertIn("Current task:", recall.call_args.args[1])
            hooks.handle_event(self.event("SessionStart", source="compact"), config)
            self.assertEqual(hooks.handle_event(self.event("UserPromptSubmit", turn_id="t1", prompt=prompts[1]), config), {})
            self.assertEqual(recall.call_count, 2)
        state = self.state_json()
        self.assertEqual(state["turns"]["t1"]["recall"]["outcome"], "timeout")
        self.assertNotIn("unhelpful", json.dumps(state))
        self.assertEqual(len(state["shadow_recent"]), 1)

    def test_shadow_is_rejected_for_workshop_and_project_only_in_isolated_profile(self):
        config = self.v3()
        raw = json.loads(config["_config_path"].read_text())
        raw["memory_scope"] = "workshop"
        config["_config_path"].write_text(json.dumps(raw))
        with self.assertRaisesRegex(hooks.HookError, "requires reminder"):
            hooks._config(config["_config_path"])
        raw.pop("memory_scope")
        config["_config_path"].write_text(json.dumps(raw))
        (self.root / ".mneme").mkdir()
        (self.root / ".mneme/profile.json").write_text(json.dumps({"schema": "mneme.profile.v1", "mode": "isolated"}))
        with patch.object(hooks, "_recall_cards", return_value={"outcome": "empty", "cards": [], "elapsed_ms": 5}) as recall:
            self.assertEqual(hooks.handle_event(self.event("UserPromptSubmit", prompt="Implement the parser."), config), {})
            self.assertEqual(recall.call_count, 1)
            self.assertEqual(recall.call_args.args[0]["project_root"], self.root.resolve())

    def test_v1_and_v2_reminder_never_call_helper(self):
        with patch.object(hooks, "_recall_cards", side_effect=AssertionError("network attempted")):
            for index, config in enumerate((self.config, self.v2("reminder"))):
                hooks.handle_event(self.event("SessionStart", source="startup"), config)
                event = self.event("UserPromptSubmit", turn_id=f"turn_{index}", prompt="Investigate the repository integration.")
                result = hooks.handle_event(event, config)
                self.assertIn("Reminder-only", result["hookSpecificOutput"]["additionalContext"])
        self.assertEqual(self.state_json()["context_epoch"], 0)

    def test_max_source_unicode_boundary_and_context_cap(self):
        config = self.v2()
        source = "ø" * 128  # 256 UTF-8 bytes, despite only 128 characters.
        self.assertEqual(len(source.encode()), 256)
        summary = "é" * 350  # helper's 700-byte summary bound.
        cards = [{"id": identifier, "summary": summary, "status": "active", "source": source,
                  "fingerprint": "f" * 64} for identifier in
                 ("01ARZ3NDEKTSV4RRFFQ69G5FAV", "01ARZ3NDEKTSV4RRFFQ69G5FAW")]
        self.assertTrue(all(hooks._valid_card(card) for card in cards))
        self.assertFalse(hooks._valid_card({**cards[0], "source": source + "ø"}))
        with patch.object(hooks, "_recall_cards", return_value={"outcome": "ok", "cards": cards, "elapsed_ms": 5}):
            result = hooks.handle_event(self.event("UserPromptSubmit", prompt="Investigate the repository integration."), config)
        context = result["hookSpecificOutput"]["additionalContext"]
        self.assertLessEqual(len(context.encode("utf-8")), hooks.MAX_CONTEXT_BYTES)
        self.assertEqual(context.count(source), 2)
        self.assertEqual(context.count(summary), 2)

    def test_episodic_recall_card_is_typed_in_final_model_context(self):
        card = {"id": "01ARZ3NDEKTSV4RRFFQ69G5FAW", "kind": "episode",
                "summary": "Removed the workshop deadline", "status": "active",
                "source": "codex:workshop", "fingerprint": "f" * 64,
                "episode_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "edition_id": "01ARZ3NDEKTSV4RRFFQ69G5FAW", "revision": 1,
                "current_edition_id": "01ARZ3NDEKTSV4RRFFQ69G5FAW",
                "occurred": {"kind": "point", "at": 12}, "recorded_at": 15,
                "edition_recorded_at": 20, "thread": "workshop",
                "recording_session": None, "origins": [{"kind": "lexical"}],
                "occurrence_contexts": [{"namespace": "session", "key": "pi", "label": "Earlier work"}]}
        self.assertTrue(hooks._valid_card(card))
        self.assertFalse(hooks._valid_card({**card, "edition_id": "other"}))
        self.assertFalse(hooks._valid_card({**card, "kind": "unknown"}))
        with patch.object(hooks, "_recall_cards", return_value={"outcome": "ok", "cards": [card], "elapsed_ms": 5}):
            result = hooks.handle_event(self.event("UserPromptSubmit", prompt="Investigate the workshop deadline."), self.v2())
        context = result["hookSpecificOutput"]["additionalContext"]
        encoded = context[context.index('[{"id"'):]
        rendered, _ = json.JSONDecoder().raw_decode(encoded)
        self.assertEqual(rendered[0]["kind"], "episode")
        for key in hooks.EPISODE_CARD_FIELDS:
            self.assertEqual(rendered[0][key], card[key])
        self.assertEqual(rendered[0]["occurrence_contexts"], card["occurrence_contexts"])
        self.assertLessEqual(len(context.encode()), hooks.MAX_CONTEXT_BYTES)

    def test_escaped_card_overflow_is_not_marked_seen(self):
        config = self.v2()
        summary = "\\\n" * 350  # 700 input bytes, much larger after JSON escaping.
        source = "\\" * 256
        cards = [{"id": identifier, "summary": summary, "status": "active", "source": source,
                  "fingerprint": fingerprint} for identifier, fingerprint in
                 (("01ARZ3NDEKTSV4RRFFQ69G5FAV", "fp-a"), ("01ARZ3NDEKTSV4RRFFQ69G5FAW", "fp-b"))]
        responses = iter(({"outcome": "ok", "cards": pair, "elapsed_ms": 5}
                          for pair in (cards, [cards[1]])))
        with patch.object(hooks, "_recall_cards", side_effect=lambda *_: next(responses)):
            first = hooks.handle_event(self.event("UserPromptSubmit", turn_id="turn_1", prompt="Investigate the repository integration."), config)
            first_text = first["hookSpecificOutput"]["additionalContext"]
            self.assertLessEqual(len(first_text.encode("utf-8")), hooks.MAX_CONTEXT_BYTES)
            self.assertIn(cards[0]["id"], first_text)
            self.assertNotIn(cards[1]["id"], first_text)
            self.assertIn("context budget", first_text)
            self.assertEqual(self.state_json()["turns"]["turn_1"]["recall"]["overflow_dropped"], 1)
            self.assertNotIn(cards[1]["id"], self.state_json()["seen_ids"])
            second = hooks.handle_event(self.event("UserPromptSubmit", turn_id="turn_2", prompt="Review the repository parser."), config)
        self.assertIn(cards[1]["id"], second["hookSpecificOutput"]["additionalContext"])
        self.assertIn(cards[1]["id"], self.state_json()["seen_ids"])

    def test_overlong_base_fails_before_passive_read_or_journal(self):
        config = self.v2()
        with patch.object(hooks, "_checkpoint_command", return_value="x" * hooks.MAX_CONTEXT_BYTES), \
             patch.object(hooks, "_recall_cards", side_effect=AssertionError("read attempted")):
            result = hooks.handle_event(self.event("UserPromptSubmit", prompt="Investigate the repository integration."), config)
        self.assertIn("systemMessage", result)
        self.assertFalse(self.state.exists())

    def test_automatic_two_cards_dedup_replay_and_reset(self):
        config = self.v2()
        cards = [
            {"id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "summary": "First source note", "status": "active", "source": "project", "fingerprint": "fp-a"},
            {"id": "01ARZ3NDEKTSV4RRFFQ69G5FAW", "summary": "Second source note", "status": "active", "source": "project", "fingerprint": "fp-b"},
        ]
        fake = {"outcome": "ok", "cards": cards, "elapsed_ms": 5}
        one = self.event("UserPromptSubmit", prompt="Investigate the repository integration.")
        with patch.object(hooks, "_recall_cards", return_value=fake) as recall:
            first = hooks.handle_event(one, config)
            self.assertEqual(recall.call_count, 1)
            self.assertIn("First source note", json.dumps(first))
            self.assertIn("Second source note", json.dumps(first))
            self.assertLessEqual(len(first["hookSpecificOutput"]["additionalContext"].encode()), hooks.MAX_CONTEXT_BYTES)
            replay = hooks.handle_event(one, config)
            self.assertNotIn("First source note", json.dumps(replay))
            self.assertEqual(recall.call_count, 1)
            second = hooks.handle_event(self.event("UserPromptSubmit", turn_id="turn_2", prompt="Review the project parser."), config)
            self.assertNotIn("First source note", json.dumps(second))
            self.assertEqual(recall.call_count, 2)
            hooks.handle_event(self.event("SessionStart", source="compact"), config)
            third = hooks.handle_event(self.event("UserPromptSubmit", turn_id="turn_3", prompt="Review the project parser again."), config)
            self.assertIn("First source note", json.dumps(third))
            self.assertEqual(recall.call_count, 3)
        state = self.state_json()
        self.assertEqual(state["turns"]["turn_1"]["recall"]["card_ids"], [c["id"] for c in cards])
        self.assertNotIn("First source note", json.dumps(state))

    def test_changed_content_same_id_reinjects_once(self):
        config = self.v2()
        identifier = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
        old = {"id": identifier, "summary": "Old state", "status": "active", "source": "project", "fingerprint": "fp-old"}
        new = {"id": identifier, "summary": "Corrected state", "status": "active", "source": "project", "fingerprint": "fp-new"}
        responses = iter(({"outcome": "ok", "cards": [card], "elapsed_ms": 5} for card in (old, new, new)))
        with patch.object(hooks, "_recall_cards", side_effect=lambda *_: next(responses)):
            first = hooks.handle_event(self.event("UserPromptSubmit", turn_id="turn_1", prompt="Review the project status."), config)
            second = hooks.handle_event(self.event("UserPromptSubmit", turn_id="turn_2", prompt="Review the project status again."), config)
            third = hooks.handle_event(self.event("UserPromptSubmit", turn_id="turn_3", prompt="Review the project status once more."), config)
        self.assertIn("Old state", json.dumps(first))
        self.assertIn("Corrected state", json.dumps(second))
        self.assertNotIn("Corrected state", json.dumps(third))
        self.assertEqual(list(zip(self.state_json()["seen_ids"], self.state_json()["seen_fingerprints"])),
                         [(identifier, "fp-old"), (identifier, "fp-new")])

    def test_automatic_skips_trivial_scope_subagent_and_synthetic(self):
        config = self.v2()
        with patch.object(hooks, "_recall_cards", side_effect=AssertionError("should not call")):
            self.assertEqual(hooks.handle_event(self.event("UserPromptSubmit", prompt="ok"), config), {})
            self.assertEqual(hooks.handle_event(self.event("UserPromptSubmit", agent_id="child", prompt="Implement the parser"), config), {})
            self.assertEqual(hooks.handle_event(self.event("UserPromptSubmit", cwd=self.temp.name, prompt="Implement the parser"), config), {})
            marker = "MNEME_CHECKPOINT_CONTINUATION source_session_id=session_1 source_turn_id=gone token=" + "a" * 32
            self.assertIn("systemMessage", hooks.handle_event(self.event("UserPromptSubmit", prompt=marker), config))

    def test_automatic_unavailable_and_stale_context_not_absence(self):
        config = self.v2()
        with patch.object(hooks, "_recall_cards", return_value={"outcome": "timeout", "cards": [], "elapsed_ms": 1500}):
            result = hooks.handle_event(self.event("UserPromptSubmit", prompt="Investigate the repository integration."), config)
        self.assertIn("unavailable or timed out", json.dumps(result))
        self.assertNotIn("no relevant memory", json.dumps(result).lower())
        card = {"id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "summary": "Stale note", "status": "active", "source": "project", "fingerprint": "fp-a"}
        def slow(_config, _prompt):
            hooks.handle_event(self.event("SessionStart", source="compact"), config)
            return {"outcome": "ok", "cards": [card], "elapsed_ms": 5}
        with patch.object(hooks, "_recall_cards", side_effect=slow):
            stale = hooks.handle_event(self.event("UserPromptSubmit", turn_id="turn_2", prompt="Implement the repository parser."), config)
        self.assertNotIn("Stale note", json.dumps(stale))
        self.assertNotIn(card["id"], self.state_json()["seen_ids"])

    def async_config(self, **extra):
        data = {"schema": hooks.CONFIG_SCHEMA_V8, "project_root": str(self.root),
                "state_dir": str(self.state), "service_config": str(Path(self.temp.name) / "service.json"),
                "memory_mode": "async", "reader_model": "gpt-6.1-sol", "librarian_effort":"medium", "recording_mode":"off",
                "reader_codex": "/usr/bin/false", "reader_codex_sha256": "a" * 64, **extra}
        if Path(data["service_config"]).is_absolute():
            Path(data["service_config"]).write_text("{}")
        self.config_path.write_text(json.dumps(data))
        return hooks._config(self.config_path)

    def test_current_reader_policy_requires_explicit_fields_and_reprepare_for_legacy(self):
        from recording_jobs import _config_digest
        for effort in ('low','medium','high'):
            config=self.async_config(librarian_effort=effort)
            self.assertEqual(config['_reader_config_pin'],_config_digest(config))
            self.assertEqual(config['librarian_effort'],effort)
        for schema in (hooks.CONFIG_SCHEMA_V4,hooks.CONFIG_SCHEMA_V5,hooks.CONFIG_SCHEMA_V6,hooks.CONFIG_SCHEMA_V7):
            with self.assertRaisesRegex(hooks.HookError,'repreparation'):
                self.async_config(schema=schema,reader_model='gpt-5.6-sol')
        for field in ('reader_model','librarian_effort','recording_mode'):
            self.async_config();raw=json.loads(self.config_path.read_text());del raw[field]
            self.config_path.write_text(json.dumps(raw))
            with self.assertRaises(hooks.HookError):hooks._config(self.config_path)
        for effort in (True,False,1,[],{},'xhigh'):
            with self.assertRaises(hooks.HookError):self.async_config(librarian_effort=effort)

    def test_loaded_policy_pin_refuses_hook_race_and_unavailable_service_snapshot(self):
        import recording_jobs
        original=recording_jobs._config_digest
        def change_after_digest(config):
            result=original(config)
            raw=json.loads(self.config_path.read_text());raw['librarian_effort']='high'
            self.config_path.write_text(json.dumps(raw))
            return result
        with patch('recording_jobs._config_digest',side_effect=change_after_digest):
            with self.assertRaisesRegex(hooks.HookError,'changed during load'):
                self.async_config()
        self.async_config();Path(self.config['state_dir']).mkdir(exist_ok=True)
        (Path(self.temp.name)/'service.json').unlink()
        with self.assertRaisesRegex(hooks.HookError,'snapshot unavailable'):
            hooks._config(self.config_path)

    def test_async_config_is_typed_and_project_only(self):
        config = self.async_config()
        self.assertEqual(config["memory_mode"], "async")
        self.assertEqual(config["recall_mode"], "automatic")
        self.assertIsInstance(config["reader_codex"], Path)
        for change in ({"reader_model": "gpt-6-astra"}, {"reader_codex": "relative"},
                       {"reader_codex_sha256": "broken"}, {"reader_auth": "relative"},
                       {"memory_scope": "workshop"}, {"surprise": True}):
            with self.subTest(change=change), self.assertRaises(hooks.HookError):
                self.async_config(**change)
        self.assertEqual(self.async_config(reader_auth="/tmp/isolated-test-auth")["reader_auth"],
                         Path("/tmp/isolated-test-auth").resolve())

    def test_async_guidance_avoids_routine_duplicate_recall_but_keeps_fallback(self):
        config = self.async_config()
        worker = SimpleNamespace(reset=lambda *_: None, notice=lambda *_: None)
        with patch.object(hooks, "_reader_worker", return_value=worker), \
             patch.object(hooks, "_recall_cards", side_effect=AssertionError("foreground read")):
            for source in ("startup", "resume", "clear", "compact"):
                with self.subTest(source=source):
                    context = hooks.handle_event(self.event("SessionStart", source=source), config)
                    text = context["hookSpecificOutput"]["additionalContext"]
                    self.assertIn("Continue normal work", text)
                    self.assertIn("do not routinely call status or duplicate recall", text)
                    self.assertIn("history-dependent decision cannot wait", text)
                    self.assertIn("needed delivery failed or missed its boundary", text)
                    self.assertIn("not an empty store", text)
                    self.assertNotIn("Recall a few task-relevant memories", text)
                    self.assertLessEqual(len(text.encode("utf-8")), hooks.MAX_CONTEXT_BYTES)
            opportunity = hooks.handle_event(self.event("UserPromptSubmit", prompt="Review the parser"), config)
            prompt_text = opportunity["hookSpecificOutput"]["additionalContext"]
            self.assertIn("do not duplicate pending recall", prompt_text)
            self.assertIn("explicit scoped recall", prompt_text)
            self.assertIn("history-dependent decision cannot wait", prompt_text)
            self.assertIn("No cards yet does not prove an empty store", prompt_text)

        for legacy in (self.config, self.v2("reminder"), self.v2("automatic")):
            context = hooks.handle_event(self.event("SessionStart", source="startup"), legacy)
            self.assertIn("Recall a few task-relevant memories", context["hookSpecificOutput"]["additionalContext"])

    def test_async_routing_and_checkpoint_are_separate_from_background(self):
        config = self.async_config()
        calls = []
        worker = SimpleNamespace(
            notice=lambda _c, e, substantive: calls.append(("notice", e["turn_id"], substantive)) or {"outcome": "queued"},
            background=lambda _c, e: calls.append(("background", e["turn_id"])),
            consume=lambda _c, e: calls.append(("consume", e["turn_id"])) or {"outcome": "empty", "cards": []},
            reset=lambda _c, sid: calls.append(("reset", sid)),
            close_turn=lambda _c, sid, tid: calls.append(("close", sid, tid)),
            end_session=lambda _c, sid: calls.append(("end", sid)))
        with patch.object(hooks, "_reader_worker", return_value=worker), \
             patch.object(hooks, "_recall_cards", side_effect=AssertionError("foreground read")):
            start = hooks.handle_event(self.event("SessionStart", source="startup"), config)
            self.assertIn("Async project memory", json.dumps(start))
            self.assertEqual(hooks.handle_event(self.event("UserPromptSubmit", prompt="ok"), config), {})
            prompt = self.event("UserPromptSubmit", prompt="Review the repository parser")
            opportunity = hooks.handle_event(prompt, config)
            self.assertIn("--outcome none", json.dumps(opportunity))
            self.assertNotIn("Passive Mneme project references", json.dumps(opportunity))
            self.assertEqual(hooks.handle_event(prompt, config, reader_background=True), {})
            self.assertEqual(hooks.handle_event(self.event("PostToolUse"), config), {})
            self.assertEqual(hooks.handle_event(self.event("Interrupt"), config), {})
            stop = hooks.handle_event(self.event("Stop", stop_hook_active=False), config)
            self.assertEqual(stop["decision"], "block")
            self.assertEqual(hooks.handle_event(self.event("Stop", stop_hook_active=False), config), {})
            self.assertEqual(hooks.handle_event(self.event("SessionEnd", turn_id=None), config), {})
        self.assertEqual(calls, [("reset", "session_1"), ("notice", "turn_1", False),
                                 ("notice", "turn_1", True), ("background", "turn_1"),
                                 ("consume", "turn_1"), ("close", "session_1", "turn_1"),
                                 ("close", "session_1", "turn_1"),
                                 ("close", "session_1", "turn_1"), ("end", "session_1")])

    def test_async_scope_guards_cover_background_and_isolated_opt_in(self):
        config = self.async_config()
        calls = []
        worker = SimpleNamespace(notice=lambda *_: calls.append("notice"),
                                 background=lambda *_: calls.append("background"))
        prompt = self.event("UserPromptSubmit", prompt="Review the project integration")
        with patch.object(hooks, "_reader_worker", return_value=worker):
            for event in (dict(prompt, cwd=self.temp.name), dict(prompt, agent_id="child"),
                          dict(prompt, agent_type="worker"), dict(prompt, turn_id=None)):
                self.assertEqual(hooks.handle_event(event, config, reader_background=True), {})
            nested = self.root / "nested"
            nested.mkdir()
            (nested / ".mneme").mkdir()
            (nested / ".mneme/profile.json").write_text(json.dumps({"schema": "mneme.profile.v1", "mode": "default"}))
            self.assertEqual(hooks.handle_event(dict(prompt, cwd=str(nested)), config, reader_background=True), {})
            (self.root / ".mneme").mkdir()
            (self.root / ".mneme/profile.json").write_text(json.dumps({"schema": "mneme.profile.v1", "mode": "isolated"}))
            self.assertEqual(hooks.handle_event(prompt, config, reader_background=True), {})
            self.assertIn("--outcome none", json.dumps(hooks.handle_event(prompt, config)))
        self.assertEqual(calls, ["background", "notice"])

    def test_async_delivery_only_valid_whole_cards_and_source_identity(self):
        config = self.async_config()
        card = {"id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "summary": "Source-backed note",
                "status": "active", "source": "project", "fingerprint": "fp-1"}
        worker = SimpleNamespace(consume=lambda *_: {"outcome": "emitted", "cards": [card]})
        with patch.object(hooks, "_reader_worker", return_value=worker):
            self.assertEqual(hooks.handle_event(self.event("PostToolUse", turn_id=None), config), {})
            delivered = hooks.handle_event(self.event("PostToolUse"), config)
            self.assertIn("source session_id=session_1, turn_id=turn_1", json.dumps(delivered))
            self.assertIn("not acknowledged as seen", json.dumps(delivered))
            self.assertIn(card["id"], json.dumps(delivered))
            self.assertLessEqual(len(delivered["hookSpecificOutput"]["additionalContext"].encode()), hooks.MAX_CONTEXT_BYTES)
            worker.consume = lambda *_: {"outcome": "emitted", "cards": [card, {**card, "id": "bad"}]}
            self.assertEqual(hooks.handle_event(self.event("PostToolUse"), config), {})
            worker.consume = lambda *_: {"outcome": "emitted", "cards": [card, card, card]}
            self.assertEqual(hooks.handle_event(self.event("PostToolUse"), config), {})
            worker.consume = lambda *_: {"outcome": "emitted", "cards": [{**card, "source": "\ud800"}]}
            self.assertEqual(hooks.handle_event(self.event("PostToolUse"), config), {})

    def test_main_large_tool_result_delivers_existing_async_card_without_body(self):
        config = self.async_config()
        card = {"id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "summary": "Source-backed note",
                "status": "active", "source": "project", "fingerprint": "fp-1"}
        worker = SimpleNamespace(consume=lambda *_: {"outcome": "emitted", "cards": [card]})
        event = self.event("PostToolUse", transcript_path=None, tool_response="private" + "x" * 4_640_000)
        output = io.StringIO()
        with patch.object(hooks.sys, "stdin", SimpleNamespace(buffer=io.BytesIO(json.dumps(event).encode()))), \
                patch.object(hooks, "_config", return_value=config), \
                patch.object(hooks, "_reader_worker", return_value=worker), \
                patch.object(hooks, "_recording_jobs") as recorder, \
                patch.object(hooks, "_recall_cards") as recall, redirect_stdout(output):
            self.assertEqual(hooks.main(["--config", str(self.config_path)]), 0)
        result = json.loads(output.getvalue())
        self.assertIn(card["id"], result["hookSpecificOutput"]["additionalContext"])
        self.assertNotIn("private", output.getvalue())
        recorder.assert_not_called()
        recall.assert_not_called()

    def test_recording_packet_binds_exact_returned_bytes_and_optional_failure_never_blocks_recall(self):
        config = self.async_config(schema=hooks.CONFIG_SCHEMA_V8, recording_mode="automatic")
        db_id = "01ARZ3NDEKTSV4RRFFQ69G5FAW"
        card = {"id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "summary": "é" * 450,
                "status": "active", "source": "project", "kind": "semantic",
                "fingerprint": "a" * 64}
        worker = SimpleNamespace(consume=lambda *_: {"outcome": "emitted", "cards": [card], "db_id": db_id})
        packets = []
        recording = SimpleNamespace(attach_delivery=lambda _c, packet: packets.append(packet))
        with patch.object(hooks, "_reader_worker", return_value=worker), \
             patch.object(hooks, "_recording_jobs", return_value=recording):
            returned = hooks.handle_event(self.event("PostToolUse"), config)
        shown = returned["hookSpecificOutput"]["additionalContext"]
        packet = packets[0]
        self.assertEqual(packet["rendered_text"], shown)
        self.assertEqual(packet["rendered_sha256"], hooks.hashlib.sha256(shown.encode()).hexdigest())
        self.assertEqual(packet["displayed"][0]["shown_summary"],
                         hooks._limited_text(card["summary"], 800))
        self.assertEqual(packet["displayed"][0]["db_id"], db_id)
        self.assertEqual(packet["displayed"][0]["full_get_fingerprint"], card["fingerprint"])
        recording.attach_delivery = lambda *_: (_ for _ in ()).throw(RuntimeError("optional busy"))
        with patch.object(hooks, "_reader_worker", return_value=worker), \
             patch.object(hooks, "_recording_jobs", return_value=recording):
            self.assertEqual(hooks.handle_event(self.event("PostToolUse"), config), returned)
        worker.consume = lambda *_: {"outcome": "emitted", "cards": [card]}
        with patch.object(hooks, "_reader_worker", return_value=worker), \
             patch.object(hooks, "_recording_jobs", side_effect=AssertionError("unbound packet")):
            self.assertEqual(hooks.handle_event(self.event("PostToolUse"), config), returned)

    def test_async_variable_card_delivery_records_only_exact_packed_output(self):
        config = self.async_config(schema=hooks.CONFIG_SCHEMA_V8, recording_mode="automatic")
        db_id = "0" * 26
        cards = AsyncBytePackingTests.cards("é" * 200)
        packed = hooks._pack_async_delivery(cards, "session_1", "turn_1", db_id)
        self.assertGreater(len(packed["cards"]), 2)
        self.assertGreater(packed["budget_omitted_count"], 0)
        worker = SimpleNamespace(consume=lambda *_: {"outcome": "emitted", "db_id": db_id, **packed})
        packets = []
        recording = SimpleNamespace(attach_delivery=lambda _c, packet: packets.append(packet))
        with patch.object(hooks, "_reader_worker", return_value=worker), \
             patch.object(hooks, "_recording_jobs", return_value=recording):
            returned = hooks.handle_event(self.event("PostToolUse"), config)
            self.assertEqual(returned["hookSpecificOutput"]["additionalContext"], packed["context"])
            self.assertEqual(packets[0]["displayed"], packed["displayed"])
            worker.consume = lambda *_: {"outcome": "emitted", "db_id": db_id, **packed, "context": "forged"}
            self.assertEqual(hooks.handle_event(self.event("PostToolUse"), config), {})
        self.assertEqual(len(packets), 1)

    def test_async_optional_failures_and_nonblocking_checkpoint_journal(self):
        config = self.async_config()
        prompt = self.event("UserPromptSubmit", prompt="Investigate the repository integration")
        worker = SimpleNamespace(notice=lambda *_: (_ for _ in ()).throw(RuntimeError("optional unavailable")),
                                 close_turn=lambda *_: (_ for _ in ()).throw(RuntimeError("optional unavailable")),
                                 consume=lambda *_: (_ for _ in ()).throw(RuntimeError("malformed slot")))
        with patch.object(hooks, "_reader_worker", return_value=worker):
            self.assertIn("--outcome none", json.dumps(hooks.handle_event(prompt, config)))
            self.assertEqual(hooks.handle_event(self.event("PostToolUse"), config), {})
            stop = hooks.handle_event(self.event("Stop", stop_hook_active=False), config)
            self.assertEqual(stop["decision"], "block")
            self.assertEqual(hooks.handle_event(self.event("UserPromptSubmit", turn_id="synthetic", prompt=stop["reason"]), config), {})
        self.assertFalse(self.state_json()["turns"]["turn_1"]["checkpoint"])
        def busy(_file, flags):
            self.assertTrue(flags & hooks.fcntl.LOCK_NB)
            raise BlockingIOError
        with patch.object(hooks.fcntl, "flock", side_effect=busy):
            with patch.object(hooks, "_reader_worker", return_value=worker):
                self.assertEqual(hooks.handle_event(self.event("UserPromptSubmit", turn_id="turn_2", prompt="Review the parser"), config), {})
                self.assertEqual(hooks.handle_event(self.event("Stop", stop_hook_active=False), config), {})


    def test_recording_config_is_explicit_v6_only(self):
        config = self.async_config(schema=hooks.CONFIG_SCHEMA_V8, recording_mode="automatic")
        self.assertEqual(config["recording_mode"], "automatic")
        for extra in ({"recording_mode": None},
                      {"schema": hooks.CONFIG_SCHEMA_V5},
                      {"schema": hooks.CONFIG_SCHEMA_V5, "recording_mode": True},
                      {"schema": hooks.CONFIG_SCHEMA_V5, "recording_mode": "automatic", "memory_scope": "workshop"}):
            with self.subTest(extra=extra), self.assertRaises(hooks.HookError):
                self.async_config(**extra)

    def test_recording_admits_short_prompt_and_never_asks_actor_checkpoint(self):
        config = self.async_config(schema=hooks.CONFIG_SCHEMA_V8, recording_mode="automatic")
        calls = []
        worker = SimpleNamespace(reset=lambda *a: None, notice=lambda *a: None,
            close_turn=lambda *a: None, end_session=lambda *a: None,
            background=lambda *a: calls.append("wake"))
        recording = SimpleNamespace(session_start=lambda *a: calls.append("start"),
            notice=lambda _c, e: calls.append(e["prompt"]),
            close_turn=lambda *a, **kw: calls.append(("close", kw.get("cancel"))))
        with patch.object(hooks, "_reader_worker", return_value=worker), \
             patch.object(hooks, "_recording_jobs", return_value=recording), \
             patch.object(hooks, "_stop_checkpoint", side_effect=AssertionError("actor continuation")):
            start = hooks.handle_event(self.event("SessionStart", source="startup"), config)
            self.assertIn("no actor checkpoint", json.dumps(start))
            self.assertEqual(hooks.handle_event(self.event("UserPromptSubmit", prompt="no, R5"), config), {})
            result = hooks.handle_event(self.event("UserPromptSubmit", prompt="Review the repository"), config)
            self.assertNotIn("--outcome", json.dumps(result))
            for name in ("Stop", "SessionEnd"):
                self.assertEqual(hooks.handle_event(self.event(name), config), {})
                self.assertEqual(hooks.handle_event(self.event(name), config, reader_background=True), {})
            hooks.handle_event(self.event("Interrupt"), config)
        self.assertIn("no, R5", calls)
        self.assertEqual(calls.count("wake"), 3)
        self.assertIn(("close", True), calls)

    def test_recording_scope_and_continuation_never_admit(self):
        config = self.async_config(schema=hooks.CONFIG_SCHEMA_V8, recording_mode="automatic")
        with patch.object(hooks, "_recording_jobs", side_effect=AssertionError("recording admission")):
            for event in (self.event("UserPromptSubmit", prompt="no, R5", agent_id="child"),
                          self.event("UserPromptSubmit", prompt="no, R5", cwd="/elsewhere"),
                          self.event("UserPromptSubmit", prompt="MNEME_CHECKPOINT_CONTINUATION bad")):
                hooks.handle_event(event, config)


class AsyncBytePackingTests(unittest.TestCase):
    @staticmethod
    def cards(summary="Short useful note"):
        return [{"id": "01ARZ3NDEKTSV4RRFFQ69G5FA" + str(i), "summary": summary,
                 "source": "project", "status": "active", "fingerprint": str(i) * 64}
                for i in range(8)]

    def test_same_byte_budget_delivers_more_short_than_long_cards(self):
        short = hooks._pack_async_delivery(self.cards(), "s", "t", "0" * 26)
        long = hooks._pack_async_delivery(self.cards("é" * 350), "s", "t", "0" * 26)
        self.assertEqual(len(short["cards"]), 8)
        self.assertLess(len(long["cards"]), len(short["cards"]))
        self.assertEqual(long["budget_omitted_count"], 8 - len(long["cards"]))
        for packet in (short, long):
            self.assertLessEqual(len(packet["context"].encode()), hooks.MAX_CONTEXT_BYTES)
            self.assertEqual([c["node_id"] for c in packet["displayed"]], [c["id"] for c in packet["cards"]])
        self.assertNotIn("Selected-batch", short["context"])
        self.assertIn("Selected-batch", long["context"])

    def test_longest_identity_prefix_unicode_json_escaping_and_episode_facets(self):
        cards = self.cards('é"\\\n' * 80)
        for c in cards:
            c.update(kind="episode", episode_id="0" * 26, edition_id=c["id"], revision=1,
                     current_edition_id=c["id"], occurred={"kind": "point", "at": 12},
                     recording_session=None, origins=[{"kind": "lexical"}],
                     recorded_at=15, edition_recorded_at=20, thread='thread"\\é')
        packed = hooks._pack_async_delivery(cards, "s" * 160, "t" * 160, "0" * 26)
        self.assertTrue(packed["cards"])
        self.assertGreater(packed["budget_omitted_count"], 0)
        self.assertLessEqual(len(packed["context"].encode()), 4096)
        self.assertIn("s" * 160, packed["context"])
        items, _ = json.JSONDecoder().raw_decode(packed["context"][packed["context"].index('[{"id"'):])
        for shown, card in zip(items, packed["cards"]):
            for key in hooks.EPISODE_CARD_FIELDS:
                self.assertEqual(shown[key], card[key])
            self.assertEqual(shown["summary"], card["summary"])
        repeated = hooks._pack_async_delivery(packed["cards"], "s" * 160, "t" * 160, "0" * 26,
                                             prior_budget_omitted=packed["budget_omitted_count"])
        self.assertEqual(repeated, packed)

    def test_skip_oversized_whole_episode_then_deliver_short_card(self):
        first, second = self.cards()[:2]
        first.update(kind="episode", episode_id="0" * 26, edition_id=first["id"], revision=1,
                     current_edition_id=first["id"], occurred={"kind": "point", "at": 12},
                     recording_session=None, origins=[{"kind": "lexical"}],
                     recorded_at=15, edition_recorded_at=20, thread="identity" * 1000)
        packed = hooks._pack_async_delivery([first, second], "s", "t", "0" * 26)
        self.assertEqual(packed["cards"], [second])
        self.assertEqual(packed["budget_omitted_count"], 1)
        self.assertNotIn(first["id"], packed["context"])
        omitted = hooks._pack_async_delivery([first], "s", "t", "0" * 26)
        self.assertEqual(omitted["cards"], [])
        self.assertEqual(omitted["displayed"], [])
        self.assertIn("Selected-batch", omitted["context"])

    def test_exact_byte_boundary_includes_prefix_and_no_unneeded_omission_reserve(self):
        card = self.cards()[0]
        card.update(kind="episode", episode_id="0" * 26, edition_id=card["id"], revision=1,
                    current_edition_id=card["id"], occurred={"kind": "point", "at": 12},
                    recording_session=None, origins=[{"kind": "lexical"}],
                     recorded_at=15, edition_recorded_at=20, thread="x")
        baseline = hooks._pack_async_delivery([card], "s" * 160, "t" * 160)
        card["thread"] += "x" * (4096 - len(baseline["context"].encode()))
        exact = hooks._pack_async_delivery([card], "s" * 160, "t" * 160)
        self.assertEqual(len(exact["context"].encode()), 4096)
        self.assertEqual(exact["cards"], [card])
        self.assertEqual(exact["budget_omitted_count"], 0)
        card["thread"] += "x"
        self.assertLess(len(hooks._render_cards([card]).encode()), 4096)
        too_long = hooks._pack_async_delivery([card], "s" * 160, "t" * 160)
        self.assertEqual(too_long["cards"], [])
        self.assertEqual(too_long["budget_omitted_count"], 1)

    def test_optional_routing_metadata_sheds_before_useful_context(self):
        cards = self.cards("x" * 270)
        for c in cards:
            c["routing_binding"] = {"db_id": "0" * 26, "route": {
                "previous": "1" * 26, "target": c["id"], "from": "1" * 26, "to": c["id"],
                "previous_fingerprint": "a" * 64, "target_fingerprint": "b" * 64,
                "edge_fingerprint": "c" * 64}}
        packed = hooks._pack_async_delivery(cards, "s", "t", "0" * 26)
        baseline = hooks._pack_async_delivery([{k: v for k, v in c.items() if k != "routing_binding"}
                                               for c in cards], "s", "t", "0" * 26)
        self.assertEqual(packed["context"], baseline["context"])
        self.assertEqual([c["id"] for c in packed["cards"]], [c["id"] for c in baseline["cards"]])
        self.assertTrue(all("routing_binding" not in c for c in packed["displayed"]))

    def test_added_occurrence_context_is_not_shed_at_delivery_boundary(self):
        episode, later = self.cards()[:2]
        episode.update(kind="episode", episode_id="0" * 26, edition_id=episode["id"], revision=1,
                       current_edition_id=episode["id"], occurred={"kind": "unknown"},
                       recording_session=None, origins=[{"kind": "lexical"}],
                     recorded_at=15, edition_recorded_at=20, thread=None)
        baseline = hooks._pack_async_delivery([episode, later], "s", "t", "0" * 26)
        episode["occurrence_contexts"] = [{"namespace": "s", "key": "pi", "label": "x" * 800}]
        with patch.object(hooks, "MAX_CONTEXT_BYTES", len(baseline["context"].encode())):
            packed = hooks._pack_async_delivery([episode, later], "s", "t", "0" * 26)
        self.assertEqual(packed["cards"], [later])
        self.assertEqual(packed["budget_omitted_count"], 1)
        self.assertNotIn(episode["id"], packed["context"])
        self.assertNotIn("occurrence_contexts", packed["context"])

    def test_reference_origins_are_atomic_and_oversized_view_is_omitted_whole(self):
        episode, later = self.cards()[:2]
        episode.update(kind="episode", episode_id="0" * 26, edition_id=episode["id"], revision=1,
                       current_edition_id="1" * 26, occurred={"kind": "unknown"},
                       recorded_at=15, edition_recorded_at=20, thread=None,
                       recording_session=None, origins=[{"kind": "reference",
                           "anchor": {"kind": "semantic", "node_id": "0" * 24 + f"{i:02}"},
                           "from": "0" * 24 + f"{i:02}", "to": episode["id"],
                           "edge_kind": "Associative", "body_anchor": None} for i in range(20)])
        packed = hooks._pack_async_delivery([episode, later], "s", "t", "0" * 26)
        self.assertEqual(packed["cards"], [later])
        self.assertEqual(packed["budget_omitted_count"], 1)
        self.assertNotIn(episode["id"], packed["context"])
        self.assertNotIn("origins", packed["context"])
        self.assertEqual([item["node_id"] for item in packed["displayed"]], [later["id"]])


class HookInputTests(unittest.TestCase):
    """Wire payload is not a retained cue, owner request or model allowance."""

    base = {"cwd": "/configured/project", "session_id": "session", "turn_id": "turn",
            "transcript_path": "/codex/sessions/source.jsonl"}

    def event(self, name="PostToolUse", **extra):
        return {**self.base, "hook_event_name": name, **extra}

    def encoded(self, event):
        return json.dumps(event, ensure_ascii=False).encode()

    def run_main(self, raw, *, background=False):
        output, errors = io.StringIO(), io.StringIO()
        args = ["--config", "/not-opened.json"] + (["--reader-background"] if background else [])
        with patch.object(hooks.sys, "stdin", SimpleNamespace(buffer=io.BytesIO(raw))), \
                patch.object(hooks, "_config", return_value={}) as config, \
                patch.object(hooks, "handle_event", return_value={"delivery": "metadata-only"}) as handler, \
                redirect_stdout(output), redirect_stderr(errors):
            self.assertEqual(hooks.main(args), 0)
        return json.loads(output.getvalue()), errors.getvalue(), config, handler

    def test_large_root_tool_body_projects_metadata_and_reaches_delivery(self):
        for size in (70_000, 4_640_000):
            event = self.event(tool_response={"private_tool_result": "x" * size},
                               tool_input={"also_private": "x" * 70_000},
                               tool_name="mcp__techtree__list", tool_use_id="tool_1")
            result, errors, config, handler = self.run_main(self.encoded(event))
            self.assertEqual(result, {"delivery": "metadata-only"})
            self.assertEqual(errors, "")
            config.assert_called_once()
            projected = handler.call_args.args[0]
            self.assertEqual(projected, self.event())
            self.assertLess(len(self.encoded(projected)), hooks.MAX_STDIN)
            self.assertNotIn("private", self.encoded(projected).decode())

    def test_large_child_unknown_and_unaccepted_start_are_silent_before_owner(self):
        for event in (self.event(agent_id="child", tool_response="x" * 4_640_000),
                      self.event(agent_type="explorer", tool_input="x" * 70_000),
                      self.event("PreToolUse", tool_response="x" * 70_000),
                      self.event("SessionStart", source="fork")):
            for background in (False, True):
                with self.subTest(name=event["hook_event_name"], background=background):
                    result, errors, config, handler = self.run_main(self.encoded(event), background=background)
                    self.assertEqual(result, {})
                    self.assertEqual(errors, "")
                    config.assert_not_called()
                    handler.assert_not_called()

    def test_all_relevant_lifecycle_fields_and_nullable_witnesses_survive(self):
        cases = (("SessionStart", {"source": "compact"}),
                 ("UserPromptSubmit", {"prompt": "Review exact source."}),
                 ("PostToolUse", {}),
                 ("Stop", {"stop_hook_active": False, "last_assistant_message": "closed answer"}),
                 ("Interrupt", {}), ("SessionEnd", {"reason": "other"}))
        for name, fields in cases:
            event = self.event(name, **fields)
            result, _, _, handler = self.run_main(self.encoded({**event, "tool_response": "x" * 70_000}))
            self.assertEqual(result, {"delivery": "metadata-only"})
            self.assertEqual(handler.call_args.args[0], event)
        nullable = self.event("Stop", transcript_path=None, last_assistant_message=None, stop_hook_active=True)
        self.assertEqual(hooks._read_event(io.BytesIO(self.encoded(nullable))), nullable)
        for name in ("SessionStart", "SessionEnd"):
            event = self.event(name, turn_id=None, **({"source": "startup"} if name == "SessionStart" else {"reason": "other"}))
            self.assertEqual(hooks._read_event(io.BytesIO(self.encoded(event))), event)

    def test_prompt_and_retained_contract_remain_strict_and_untruncated(self):
        prompt = "Review " + "é" * 1000
        event = self.event("UserPromptSubmit", prompt=prompt, extra_wire_body="x" * 70_000)
        result = hooks._read_event(io.BytesIO(self.encoded(event)))
        self.assertEqual(result["prompt"], prompt)
        for event in (self.event("UserPromptSubmit", prompt="x" * (hooks.MAX_STDIN + 1)),
                      self.event("UserPromptSubmit", prompt="é" * (hooks.MAX_STDIN // 2 + 1)),
                      self.event("UserPromptSubmit", prompt="x" * hooks.MAX_STDIN),
                      self.event("Stop", last_assistant_message="x" * hooks.MAX_STDIN, stop_hook_active=False),
                      self.event(transcript_path="x" * hooks.MAX_STDIN)):
            result, _, config, handler = self.run_main(self.encoded(event))
            self.assertIn("retained_limit", result["systemMessage"])
            config.assert_not_called()
            handler.assert_not_called()

    def test_malformed_truncated_nested_and_wrong_types_deny_before_owner(self):
        cases = ((b'{"private_secret":"do-not-echo",BAD}', "invalid_json"),
                 (self.encoded(self.event())[:-1], "invalid_json"),
                 (b"\xff", "invalid_json"),
                 (b"[" * 2000 + b"0" + b"]" * 2000, "wire_depth"),
                 (b"[]", "invalid_event"),
                 (self.encoded(self.event("UserPromptSubmit", prompt=["private_secret"])), "invalid_event"),
                 (self.encoded(self.event("Stop", stop_hook_active="false")), "invalid_event"),
                 (self.encoded(self.event(session_id=["private_secret"])), "invalid_event"),
                 (json.dumps(self.event("UserPromptSubmit", prompt="\ud800")).encode(), "invalid_event"))
        for raw, reason in cases:
            with self.subTest(reason=reason, size=len(raw)):
                result, errors, config, handler = self.run_main(raw)
                self.assertIn(f"omitted ({reason})", result["systemMessage"])
                self.assertNotIn("unavailable", result["systemMessage"])
                self.assertNotIn("private_secret", result["systemMessage"])
                self.assertEqual(errors, "")
                config.assert_not_called()
                handler.assert_not_called()

    def test_wire_fuse_exact_boundary_and_bounded_read(self):
        raw = self.encoded(self.event())
        at_limit = raw + b" " * (hooks.MAX_WIRE_BYTES - len(raw))
        self.assertEqual(hooks._read_event(io.BytesIO(at_limit)), self.event())
        stream = io.BytesIO(at_limit + b" " * 10)
        with self.assertRaisesRegex(hooks.HookInputError, "wire_limit"):
            hooks._read_event(stream)
        self.assertEqual(stream.tell(), hooks.MAX_WIRE_BYTES + 1)
        for background in (False, True):
            result, errors, config, handler = self.run_main(at_limit + b" ", background=background)
            if background:
                self.assertEqual(result, {})
                self.assertIn("omitted (wire_limit)", errors)
            else:
                self.assertIn("omitted (wire_limit)", result["systemMessage"])
            config.assert_not_called()
            handler.assert_not_called()

    def test_wire_depth_guard_ignores_quoted_brackets_and_escaped_quotes(self):
        body = '[{\\"' * 1000 + '"\\}]' * 1000
        event = self.event(tool_response=body)
        self.assertEqual(hooks._read_event(io.BytesIO(self.encoded(event))), self.event())
        for depth in (hooks.MAX_WIRE_DEPTH - 1, hooks.MAX_WIRE_DEPTH):
            raw = self.encoded(self.event())[:-1] + b',"tool_response":' + b'[' * depth + b'0' + b']' * depth + b'}'
            if depth + 1 <= hooks.MAX_WIRE_DEPTH:
                self.assertEqual(hooks._read_event(io.BytesIO(raw)), self.event())
            else:
                with self.assertRaisesRegex(hooks.HookInputError, "wire_depth"):
                    hooks._read_event(io.BytesIO(raw))


if __name__ == "__main__":
    unittest.main()


class ConcernPackingTests(unittest.TestCase):
    def cards(self, n=5, summary="Useful short advice"):
        return [{"id": str(i) * 26, "summary": summary, "fingerprint": str(i) * 64,
                 "kind": "semantic", "status": "active", "source": "fixture"} for i in range(1, n + 1)]

    def case(self, a=1, b=2):
        from test_turn_observer import concern_row
        return {"shown_text": 'Scopes differ 🐙 "quoted".',
                "displayed_endpoint_ids": [str(a) * 26, str(b) * 26],
                "expected_row": concern_row(str(a) * 26, str(b) * 26)}

    def test_multiple_cases_more_than_two_cards_and_exact_repack(self):
        cases = [self.case(1, i) for i in range(2, 5)]
        packed = hooks._pack_async_delivery(self.cards(), "s" * 160, "t" * 160, "0" * 26, concerns=cases)
        self.assertEqual(len(packed["cards"]), 5)
        self.assertEqual(len(packed["concerns"]), 3)
        self.assertLessEqual(len(packed["context"].encode()), 4096)
        self.assertEqual(hooks._pack_async_delivery(packed["cards"], "s" * 160, "t" * 160,
                         "0" * 26, concerns=packed["concerns"])["context"], packed["context"])

    def test_oversized_component_has_no_half_warning_and_cards_remain_useful(self):
        cards = self.cards(3, '"\\' * 350)
        cards[2]["summary"] = "Small later note"
        case = {**self.case(), "shown_text": "🐙" * 250}
        packed = hooks._pack_async_delivery(cards, "s" * 160, "t" * 160, "0" * 26, concerns=[case])
        self.assertNotIn(case["shown_text"], packed["context"])
        self.assertEqual(packed["concerns"], [])
        self.assertEqual(packed["concern_omitted_count"], 1)
        self.assertIn(cards[2], packed["cards"])

    def test_no_database_identity_still_surfaces_readonly_warning(self):
        packed = hooks._pack_async_delivery(self.cards(2), "s", "t", concerns=[self.case()])
        self.assertIn(self.case()["shown_text"], packed["context"])
        self.assertIsNone(packed["concerns"][0]["expected_row"])
        self.assertEqual(packed["displayed"], [])

    def test_optional_cases_preserve_baseline_cards_and_identical_text_labels_pairs(self):
        cards = self.cards(7, '"\\' * 100)
        baseline = hooks._pack_async_delivery(cards, "s", "t", "0" * 26)
        cases = [{**self.case(1, 2), "shown_text": "🐙" * 250}, self.case(3, 4)]
        packed = hooks._pack_async_delivery(cards, "s", "t", "0" * 26, concerns=cases)
        self.assertEqual([c["id"] for c in packed["cards"]], [c["id"] for c in baseline["cards"]])
        short = self.cards(4)
        cases = [self.case(1, 2), self.case(3, 4)]
        packed = hooks._pack_async_delivery(short, "s", "t", "0" * 26, concerns=cases)
        for case in cases:
            self.assertIn("(" + " ↔ ".join(case["displayed_endpoint_ids"]) + "): " + case["shown_text"], packed["context"])
        self.assertEqual(len(packed["concerns"]), 2)

    def test_private_byte_pressure_sheds_authority_before_warning_or_cards(self):
        from test_turn_observer import concern_row
        row = concern_row(finding={"scope": '"' * 500, "observation": '"' * 1000,
                     "evidence": [{"source_ref": '"' * 250, "digest": "c" * 64} for _ in range(3)]})
        cards = self.cards(3, '"\\' * 50)
        case = {**self.case(), "expected_row": row}
        baseline = hooks._pack_async_delivery(cards, "s", "t", "0" * 26)
        packed = hooks._pack_async_delivery(cards, "s", "t", "0" * 26, concerns=[case])
        self.assertEqual([c["id"] for c in packed["cards"]], [c["id"] for c in baseline["cards"]])
        self.assertEqual(len(packed["concerns"]), 1)
        self.assertIsNone(packed["concerns"][0]["expected_row"])
        self.assertIn(case["shown_text"], packed["context"])


class ConditionalDisplayTests(unittest.TestCase):
    def card(self):
        from test_routing_memory import binding, TARGET
        return {"id": TARGET, "summary": "A prior bounded journal recommendation", "status": "active",
                "source": "project", "fingerprint": "a" * 64, "entry_kind": "conditional",
                "conditional_binding": binding()}

    def test_exact_display_binding_without_fake_graph_or_native_fingerprints_in_text(self):
        from test_routing_memory import DB
        card = self.card()
        text, shown = hooks._render_cards_with_display([card], DB)
        self.assertEqual(shown[0]["conditional_binding"], card["conditional_binding"])
        self.assertEqual(shown[0]["entry_kind"], "conditional")
        self.assertNotIn("routing_binding", shown[0])
        self.assertNotIn("graph_path", shown[0])
        self.assertNotIn("conditional_binding", text)
        packet = hooks._pack_async_delivery([card], "s", "t", DB)
        self.assertEqual(packet["displayed"], shown)
        self.assertEqual(packet["cards"], [card])

    def test_display_missing_bad_wrong_target_and_path_conflict_are_optional(self):
        from test_routing_memory import DB, binding
        bad_target = binding(); bad_target["route"]["target"] = bad_target["route"]["previous"]
        for changes in ({"conditional_binding": None}, {"conditional_binding": "x" * 2000},
                        {"conditional_binding": bad_target}, {"routing_binding": binding()},
                        {"graph_path": [{"forged": "hop"}]}, {"graph_path": {}}):
            card = {**self.card(), **changes}
            text, shown = hooks._render_cards_with_display([card], DB)
            self.assertIn(card["summary"], text)
            self.assertEqual(shown[0]["entry_kind"], "conditional")
            self.assertNotIn("conditional_binding", shown[0])
            self.assertNotIn("routing_binding", shown[0])
        card = self.card(); card.pop("entry_kind")
        self.assertNotIn("conditional_binding", hooks._render_cards_with_display([card], DB)[1][0])

    def test_unknown_marker_and_orphan_binding_never_salvage_graph_display(self):
        from test_routing_memory import DB, binding
        for fields in ({"entry_kind": "future"}, {"entry_kind": None},
                       {"conditional_binding": binding()}):
            card = self.card()
            card.pop("entry_kind"); card.pop("conditional_binding")
            card.update(fields, routing_binding=binding())
            text, shown = hooks._render_cards_with_display([card], DB)
            self.assertIn(card["summary"], text)
            for field in ("entry_kind", "conditional_binding", "routing_binding"):
                self.assertNotIn(field, shown[0])

    def test_conditional_binding_pressure_never_evicts_useful_cards(self):
        from test_routing_memory import DB, PREVIOUS
        cards = AsyncBytePackingTests.cards("x" * 270)
        for card in cards:
            card.update(entry_kind="conditional", conditional_binding={"db_id": DB, "route": {
                "previous": PREVIOUS, "target": card["id"], "from": PREVIOUS, "to": card["id"],
                "previous_fingerprint": "a" * 64, "target_fingerprint": "b" * 64,
                "edge_fingerprint": "c" * 64}})
        packed = hooks._pack_async_delivery(cards, "s", "t", DB)
        baseline = hooks._pack_async_delivery([{k: v for k, v in c.items()
                if k not in {"entry_kind", "conditional_binding"}} for c in cards], "s", "t", DB)
        self.assertEqual(packed["context"], baseline["context"])
        self.assertEqual([c["id"] for c in packed["cards"]], [c["id"] for c in baseline["cards"]])
        self.assertTrue(all("conditional_binding" not in row for row in packed["displayed"]))

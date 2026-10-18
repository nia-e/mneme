"""Hook composition keeps foreground nominations separate from recall evidence."""
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import Mock, patch

import hooks
from host_platform import executable_name


class TouchstoneHookTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name).resolve()
        reader = root / executable_name("reader")
        reader.write_text("#!/bin/sh\nexit 0\n")
        reader.chmod(0o700)
        service = root / "service.json"
        service.write_text(json.dumps({"mode": "connect", "url": "http://127.0.0.1:18766/",
                                      "database_name": "project", "database_path": "/fixture/project.db"}))
        registry = root / "stores.json"
        registry.write_text(json.dumps({"schema": "mneme.stores.v1", "stores": []}))
        config_path = root / "hooks.json"
        config_path.write_text(json.dumps({"schema": hooks.CONFIG_SCHEMA_V12,
            "project_root": str(root), "service_config": str(service),
            "store_registry": str(registry), "state_dir": str(root / "state"), "memory_mode": "async",
            "reader_model": "gpt-6.1-sol", "librarian_effort": "medium", "reader_codex": str(reader),
            "reader_codex_sha256": hashlib.sha256(reader.read_bytes()).hexdigest(),
            "recording_mode": "automatic", "tag_stewardship": False}))
        self.config = hooks._config(config_path, cwd=root)
        self.event = {"cwd": str(root), "session_id": "session", "turn_id": "next",
                      "hook_event_name": "UserPromptSubmit", "prompt": "ok"}
        self.order = []
        self.reader = SimpleNamespace(notice=lambda *a: self.order.append("notice"),
                                      consume=lambda *a: {"outcome": "none"},
                                      background=lambda *a: None)
        self.recorder = Mock()
        self.nomination = {"outcome": "emitted", "context": "A candidate, not an approval.", "ids": ["candidate"]}

    def run_hook(self, *, event=None, nomination=None):
        def consume(_config, _event, *, max_bytes):
            self.order.append("nomination")
            self.assertLessEqual(max_bytes, hooks.MAX_CONTEXT_BYTES)
            return self.nomination if nomination is None else nomination
        with patch.object(hooks, "_reader_worker", return_value=self.reader), \
                patch.object(hooks, "_recording_jobs", return_value=self.recorder), \
                patch("touchstone_handoff.consume", side_effect=consume) as handoff:
            result = hooks.handle_event(event or self.event, self.config)
        return result, handoff

    def test_short_prompt_notices_turn_before_nomination_and_never_attaches_delivery(self):
        result, handoff = self.run_hook()
        self.assertEqual(self.order, ["notice", "nomination"])
        self.assertEqual(result["hookSpecificOutput"]["additionalContext"], self.nomination["context"])
        handoff.assert_called_once()
        self.recorder.attach_delivery.assert_not_called()

    def test_substantive_prompt_composes_within_same_aggregate_limit(self):
        result, _ = self.run_hook(event={**self.event, "prompt": "Review the repository design"})
        context = result["hookSpecificOutput"]["additionalContext"]
        self.assertIn("Continue normal work", context)
        self.assertTrue(context.endswith(self.nomination["context"]))
        self.assertLessEqual(len(context.encode()), hooks.MAX_CONTEXT_BYTES)
        self.recorder.attach_delivery.assert_not_called()

    def test_empty_tool_boundary_is_nomination_fallback(self):
        result, handoff = self.run_hook(event={**self.event, "hook_event_name": "PostToolUse"})
        self.assertEqual(result["hookSpecificOutput"]["additionalContext"], self.nomination["context"])
        handoff.assert_called_once()
        self.recorder.attach_delivery.assert_not_called()

    def test_recall_delivery_remains_exact_and_nomination_waits(self):
        card = {"id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "summary": "A useful memory",
                "status": "active", "source": "project", "fingerprint": "f" * 64}
        packet = hooks._pack_async_delivery([card], "session", "next", "1" * 26)
        self.reader.consume = lambda *a: {"outcome": "emitted", "cards": [card],
                                         "context": packet["context"], "db_id": "1" * 26}
        result, handoff = self.run_hook(event={**self.event, "hook_event_name": "PostToolUse"})
        handoff.assert_not_called()
        context = result["hookSpecificOutput"]["additionalContext"]
        delivery = self.recorder.attach_delivery.call_args.args[1]
        self.assertEqual(context, packet["context"])
        self.assertEqual(delivery["rendered_text"], context)
        self.assertEqual(delivery["rendered_sha256"], hashlib.sha256(context.encode()).hexdigest())
        self.assertNotIn("candidate", context)

    def test_unavailable_and_oversize_handoff_do_not_break_normal_prompt(self):
        for result in ({"outcome": "pending", "context": "not yet"},
                       {"outcome": "emitted", "context": "é" * hooks.MAX_CONTEXT_BYTES},
                       {"outcome": "emitted", "context": None}):
            with self.subTest(result=str(result)[:60]):
                output, _ = self.run_hook(nomination=result)
                self.assertEqual(output, {})
        with patch("touchstone_handoff.consume", side_effect=ValueError("bad state")):
            self.assertEqual(hooks._with_touchstone_nomination(self.config, self.event, "normal"),
                             hooks._context("UserPromptSubmit", "normal"))

    def test_subagent_outside_workspace_background_and_continuation_never_consume(self):
        cases = ({**self.event, "agent_id": "child"},
                 {**self.event, "cwd": "/elsewhere"},
                 {**self.event, "prompt": "MNEME_CHECKPOINT_CONTINUATION bad"})
        for event in cases:
            with self.subTest(event=event):
                _, handoff = self.run_hook(event=event)
                handoff.assert_not_called()
        with patch.object(hooks, "_reader_worker", return_value=self.reader), \
                patch("touchstone_handoff.consume") as handoff:
            hooks.handle_event(self.event, self.config, reader_background=True)
            handoff.assert_not_called()

    def test_legacy_adapter_does_not_import_or_consume_handoff(self):
        with patch("touchstone_handoff.consume") as handoff:
            config = {**self.config, "schema": hooks.CONFIG_SCHEMA_V8}
            result = hooks._with_touchstone_nomination(config, self.event, "old guidance")
            self.assertEqual(result, hooks._context("UserPromptSubmit", "old guidance"))
            handoff.assert_not_called()


if __name__ == "__main__":
    unittest.main()

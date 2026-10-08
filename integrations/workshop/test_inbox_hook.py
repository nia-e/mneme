"""Finite local inbox hook tests: no model, network, delivery, or live queue."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import inbox_hook
import wake


def record(number, source=wake.SIGNAL_SOURCE, kind=wake.SIGNAL_KIND, status="pending"):
    return {"sequence": number, "status": status,
            "claimed_run": None if status in ("pending", "consumed") else "private-run-id",
            "detail": "PRIVATE DETAIL",
            "event": {"source": source, "kind": kind, "event_id": f"private-event-id-{number}",
                      "summary": "PRIVATE SUMMARY", "body": "PRIVATE MESSAGE BODY",
                      "reference": "PRIVATE REFERENCE"}}


def snapshot(root):
    return {str(item.relative_to(root)): (item.lstat().st_mode, item.lstat().st_mtime_ns,
                                        item.read_bytes() if item.is_file() else None)
            for item in root.rglob("*")}


class InboxHookTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        self.event = {"hook_event_name": "SessionStart", "source": "startup",
                      "session_id": "PRIVATE SESSION", "prompt": "PRIVATE PROMPT"}

    def queue(self, records):
        (self.root / "events.json").write_text(json.dumps({
            "schema": wake.SCHEMA, "next_sequence": len(records) + 1, "records": records}))

    def context(self, result):
        self.assertEqual(set(result), {"hookSpecificOutput"})
        specific = result["hookSpecificOutput"]
        self.assertEqual(set(specific), {"hookEventName", "additionalContext"})
        self.assertEqual(specific["hookEventName"], "SessionStart")
        text = specific["additionalContext"]
        self.assertLessEqual(len(text.encode("utf-8")), inbox_hook.MAX_CONTEXT_BYTES)
        for private in ("PRIVATE", "private-event-id", "private-run-id", str(self.root), "unread"):
            self.assertNotIn(private, text)
        self.assertIn("read_history", text)
        self.assertIn("before replying", text)
        return text

    def cli(self, raw=None, *, script=None, root=None):
        # Deliberately omit -B: the hook itself must suppress imported bytecode.
        completed = subprocess.run(
            [sys.executable, str(script or Path(inbox_hook.__file__).resolve()),
             "--root", str(root or self.root)],
            input=json.dumps(self.event).encode() if raw is None else raw,
            capture_output=True, timeout=3, check=False)
        self.assertEqual(completed.returncode, 0, completed.stderr.decode(errors="replace"))
        self.assertEqual(completed.stderr, b"")
        self.assertLess(len(completed.stdout), 800)
        return json.loads(completed.stdout)

    def test_missing_and_valid_empty_queue_are_quiet_without_writes(self):
        self.assertEqual(inbox_hook.handle_event(self.event, self.root), {})
        self.assertEqual(self.cli(), {})
        self.assertEqual(list(self.root.iterdir()), [])
        self.queue([])
        before = snapshot(self.root)
        self.assertEqual(self.cli(), {})
        self.assertEqual(snapshot(self.root), before)

    def test_mixed_inboxes_emit_metadata_only_and_preserve_every_file(self):
        self.queue([record(1), record(2),
                    record(3, wake.MAILBOX_SOURCE, wake.MAILBOX_KIND),
                    record(4, status="unfinished"),
                    record(5, wake.MAILBOX_SOURCE, wake.MAILBOX_KIND, "unfinished"),
                    record(6, status="held"), record(7, status="delivered"),
                    record(8, wake.MAILBOX_SOURCE, wake.MAILBOX_KIND, "held"),
                    record(9, wake.MAILBOX_SOURCE, wake.MAILBOX_KIND, "delivered"),
                    record(10, "technical", "hint"), record(11, kind="wrong-kind"),
                    record(12, wake.MAILBOX_SOURCE, "wrong-kind"),
                    record(13, status="consumed")])
        (self.root / "state.json").write_text('{"event_after_sequence": 100}')
        before = snapshot(self.root)
        text = self.context(self.cli())
        self.assertIn("pending Signal bursts: 2; pending private-mailbox messages: 1", text)
        self.assertIn("Unfinished Signal bursts: 1; unfinished private-mailbox messages: 1", text)
        self.assertIn("not individual text-message counts", text)
        self.assertEqual(snapshot(self.root), before)

    def test_unfinished_is_separate_and_held_delivered_or_technical_only_is_quiet(self):
        self.queue([record(1, status="unfinished")])
        text = self.context(inbox_hook.handle_event(self.event, self.root))
        self.assertIn("pending Signal bursts: 0", text)
        self.assertIn("Unfinished Signal bursts: 1", text)
        self.queue([record(1, status="held"), record(2, status="delivered"),
                    record(3, "technical", "hint"), record(4, kind="wrong-kind")])
        self.assertEqual(inbox_hook.handle_event(self.event, self.root), {})

    def test_each_session_start_source_including_compact(self):
        self.queue([record(1)])
        for source in ("startup", "resume", "clear", "compact"):
            with self.subTest(source=source):
                self.context(self.cli(json.dumps({**self.event, "source": source}).encode()))

    def test_other_hooks_sources_and_subagents_never_read_queue(self):
        ignored = [{**self.event, "hook_event_name": name}
                   for name in ("Stop", "PostToolUse", "UserPromptSubmit", "PreCompact", "PostCompact")]
        ignored.extend({**self.event, "source": source} for source in (None, "unknown", [], 1))
        for field in ("agent_id", "agent_type"):
            ignored.extend({**self.event, field: value} for value in ("subagent", False, 0, [], {}))
        with patch.object(wake, "inbox_summary", side_effect=AssertionError("must not read queue")):
            for event in ignored:
                with self.subTest(event=event):
                    self.assertEqual(inbox_hook.handle_event(event, self.root / "missing"), {})

    def test_maximum_valid_queue_stays_under_context_bound(self):
        self.queue([record(number) for number in range(1, wake.MAX_RECORDS + 1)])
        text = self.context(self.cli())
        self.assertIn("pending Signal bursts: 64", text)

    def test_bad_queue_fails_open_without_echoing_content_or_mutating(self):
        for raw in (b"PRIVATE CORRUPT QUEUE", b'{"schema":"unknown"}',
                    b'{"schema":"a","schema":"b"}',
                    b"[" * 2000 + b"]" * 2000,
                    b" " * (wake.MAX_BYTES + 1)):
            with self.subTest(length=len(raw)):
                (self.root / "events.json").write_bytes(raw)
                before = snapshot(self.root)
                text = self.context(self.cli())
                self.assertIn("summary unavailable", text)
                self.assertNotIn("pending Signal bursts: 0", text)
                self.assertEqual(snapshot(self.root), before)

    def test_invalid_record_fails_open(self):
        bad = record(1)
        bad["status"] = "unknown"
        self.queue([bad])
        self.assertIn("summary unavailable", self.context(self.cli()))

    def test_unsafe_queue_paths_fail_open_finitely(self):
        queue = self.root / "events.json"
        target = self.root / "target"
        target.write_text("[]")
        for kind in ("symlink", "hardlink", "directory", "fifo"):
            with self.subTest(kind=kind):
                if kind == "symlink":
                    queue.symlink_to(target)
                elif kind == "hardlink":
                    os.link(target, queue)
                elif kind == "directory":
                    queue.mkdir()
                else:
                    os.mkfifo(queue)
                try:
                    self.assertIn("summary unavailable", self.context(self.cli()))
                    self.assertEqual(target.read_text(), "[]")
                finally:
                    queue.rmdir() if kind == "directory" else queue.unlink()

    def test_unsafe_legacy_state_fails_open_finitely(self):
        state = self.root / "state.json"
        os.mkfifo(state)
        for legacy_queue in (False, True):
            with self.subTest(legacy_queue=legacy_queue):
                if legacy_queue:
                    (self.root / "events.json").write_text("[]")
                self.assertIn("summary unavailable", self.context(self.cli()))
        self.assertFalse((self.root / ".events.lock").exists())

    def test_invalid_or_oversized_input_fails_open_without_echo(self):
        inputs = (b"PRIVATE MALFORMED INPUT", b"\xff", b"[]", b"null",
                  b'{"source":"startup","source":"PRIVATE DUPLICATE"}',
                  b'{"number":NaN}', b'{"number":Infinity}',
                  b"[" * 2000 + b"]" * 2000,
                  b" " * (inbox_hook.MAX_STDIN + 1))
        for raw in inputs:
            with self.subTest(length=len(raw)):
                self.assertIn("summary unavailable", self.context(self.cli(raw)))
        self.assertEqual(list(self.root.iterdir()), [])

    def test_read_failure_or_invalid_root_fails_open(self):
        with patch.object(wake, "inbox_summary", side_effect=PermissionError("PRIVATE ERROR")):
            self.assertIn("summary unavailable", self.context(inbox_hook.handle_event(self.event, self.root)))
        self.assertIn("summary unavailable", self.context(self.cli(root=self.root / "missing")))

    def test_script_import_creates_no_bytecode_or_state(self):
        source = Path(inbox_hook.__file__).resolve().parent
        installed = self.root / "installed"
        installed.mkdir()
        for name in ("inbox_hook.py", "wake.py"):
            shutil.copy2(source / name, installed / name)
        self.queue([record(1)])
        before = snapshot(self.root)
        self.context(self.cli(script=installed / "inbox_hook.py"))
        self.assertEqual(snapshot(self.root), before)


if __name__ == "__main__":
    unittest.main()

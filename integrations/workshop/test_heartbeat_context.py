"""Agenda and journal previews are bounded context, not control state."""

import hashlib
import json
import os
from pathlib import Path
import unittest
from unittest.mock import Mock, mock_open, patch

from heartbeat_test_support import HeartbeatHarness


class HeartbeatContextTests(HeartbeatHarness, unittest.TestCase):
    def test_previous_handoff_is_present_in_next_orientation(self):
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        self.due()
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        self.assertIn("Made a small observation.", self.invocations()[-2]["prompt"])
        self.assertIn("Recheck tomorrow.", self.invocations()[-2]["prompt"])


    def test_maximum_unicode_handoff_survives_state_read_and_next_orientation(self):
        summary = "🌿" * 4000
        next_step = "🌿" * 2000
        process, payload = self.call(result=self.result(
            summary=summary, next_step=next_step, memory_candidates=[]))
        self.assertEqual(process.returncode, 0, payload)
        state = json.loads((self.root / "state.json").read_text())
        self.assertEqual(state["previous"], {"summary": summary, "next_step": next_step})
        process, payload = self.call("status")
        self.assertEqual(process.returncode, 0, payload)
        self.due()
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        orientation_prompt = self.invocations()[-2]["prompt"]
        self.assertIn(summary, orientation_prompt)
        self.assertIn(next_step, orientation_prompt)


    def test_latest_dated_journal_is_bounded_context_in_both_stages(self):
        journal = self.root / "artifacts" / "journal"
        journal.mkdir()
        (journal / "2026-09-24.md").write_text("old secret context\n")
        (journal / "2026-09-25.md").write_text("new bounded observation\n")
        (journal / "scratch.md").write_text("undated scratch\n")
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        for invocation in self.invocations():
            prompt = invocation["prompt"]
            self.assertIn("artifacts/journal/2026-09-25.md", prompt)
            self.assertIn("new bounded observation", prompt)
            self.assertNotIn("old secret context", prompt)
            self.assertNotIn("undated scratch", prompt)


    def test_missing_journal_is_explicit_and_unsafe_latest_is_not_read(self):
        heartbeat = self.heartbeat_module()
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        self.assertIn("No dated journal entry is present.", self.invocations()[0]["prompt"])
        for name, make in (("symlink", lambda path: path.symlink_to(self.root / "AGENDA.md")),
                           ("directory", lambda path: path.mkdir()),
                           ("fifo", lambda path: os.mkfifo(path))):
            with self.subTest(name=name):
                root = self.make_root("journal-" + name)
                journal = root / "artifacts" / "journal"
                journal.mkdir()
                make(journal / "2026-09-25.md")
                with self.assertRaisesRegex(heartbeat.Refusal, "regular, non-symlink"):
                    heartbeat.latest_journal(root)
                self.assertEqual(list(root.glob("runs/*")), [])


    def test_oversized_journal_keeps_recent_context_in_both_stages_without_editing(self):
        heartbeat = self.heartbeat_module()
        journal = self.root / "artifacts" / "journal"
        journal.mkdir()
        path = journal / "2026-09-25.md"
        original = (b"Earlier observation that stays only in the full journal.\n" +
                    b"x" * heartbeat.MAX_JOURNAL + b"\nThe latest observation matters.\n")
        path.write_bytes(original)
        before = hashlib.sha256(original).hexdigest()
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, (payload, process.stderr))
        self.assertEqual([entry["stage"] for entry in self.invocations()], ["orientation", "work"])
        for invocation in self.invocations():
            prompt = invocation["prompt"]
            self.assertIn("The latest observation matters.", prompt)
            self.assertIn("earlier text omitted", prompt)
            self.assertIn("Read the full artifacts/journal/2026-09-25.md file", prompt)
            self.assertNotIn("Earlier observation that stays only", prompt)
        self.assertEqual(hashlib.sha256(path.read_bytes()).hexdigest(), before)


    def test_journal_exact_byte_boundary_is_unchanged(self):
        heartbeat = self.heartbeat_module()
        path = self.root / "note.md"
        for size in (0, 12, heartbeat.MAX_JOURNAL):
            with self.subTest(size=size):
                original = ("\U0001f980".encode("utf-8") + b"x" * (size - 4)) if size else b""
                path.write_bytes(original)
                self.assertEqual(heartbeat.journal_preview(path, "note.md"), original.decode("utf-8"))


    def test_journal_tail_aligns_valid_two_three_and_four_byte_characters(self):
        heartbeat = self.heartbeat_module()
        path = self.root / "note.md"
        for character in ("\u00e9", "\u20ac", "\U0001f980"):
            encoded = character.encode("utf-8")
            for removed in range(len(encoded)):
                with self.subTest(character=character, removed=removed):
                    suffix = b"z" * (heartbeat.MAX_JOURNAL - len(encoded) + removed)
                    original = b"earlier " + encoded + suffix
                    path.write_bytes(original)
                    preview = heartbeat.journal_preview(path, "note.md")
                    text = preview.split("\n\n", 1)[1]
                    self.assertEqual(text, (character if removed == 0 else "") + suffix.decode())
                    self.assertNotIn("\ufffd", preview)
                    self.assertLessEqual(len(text.encode("utf-8")), heartbeat.MAX_JOURNAL)
                    self.assertEqual(path.read_bytes(), original)


    def test_journal_tail_does_not_hide_malformed_retained_text(self):
        heartbeat = self.heartbeat_module()
        path = self.root / "note.md"
        # Invalid lead, surrogate, out-of-range scalar and excess continuation:
        # none becomes valid by dropping arbitrary bytes at the tail boundary.
        for boundary in (b"\xc0\x80", b"\xed\xa0\x80", b"\xf4\x90\x80\x80",
                         b"\x80\x80\x80\x80\x80"):
            with self.subTest(boundary=boundary):
                original = b"prefix" + boundary + b"x" * (heartbeat.MAX_JOURNAL - len(boundary) + 1)
                path.write_bytes(original)
                with self.assertRaises(UnicodeDecodeError):
                    heartbeat.journal_preview(path, "note.md")
        for malformed in (b"\xff", b"\xe2\x82", b"valid then \xff invalid"):
            for prefix in (b"", b"x" * heartbeat.MAX_JOURNAL):
                with self.subTest(malformed=malformed, large=bool(prefix)):
                    path.write_bytes(prefix + malformed)
                    with self.assertRaises(UnicodeDecodeError):
                        heartbeat.journal_preview(path, "note.md")


    def test_huge_journal_reads_only_tail_and_three_alignment_bytes(self):
        heartbeat = self.heartbeat_module()
        path = self.root / "huge.md"
        size = 4 * 1024 * 1024 * 1024
        suffix = b"z" * (heartbeat.MAX_JOURNAL - 7) + b"latest!"
        with path.open("wb") as stream:
            stream.seek(size - len(suffix))
            stream.write(suffix)
        with path.open("rb") as stream, patch.object(Path, "open") as opened:
            probe = Mock(wraps=stream)
            opened.return_value.__enter__.return_value = probe
            preview = heartbeat.journal_preview(path, "huge.md")
            opened.assert_called_once_with("rb")
            probe.seek.assert_called_once_with(size - heartbeat.MAX_JOURNAL - 3)
            probe.read.assert_called_once_with(heartbeat.MAX_JOURNAL + 3)
        self.assertEqual(preview.split("\n\n", 1)[1], suffix.decode())
        self.assertEqual(path.stat().st_size, size)


    def test_missing_agenda_warns_without_disabling_codex(self):
        (self.root / "AGENDA.md").unlink()
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual(len(self.invocations()), 2)
        for entry in self.invocations():
            self.assertIn("Context warning: AGENDA.md was not loaded", entry["prompt"])
        self.assertFalse((self.root / "AGENDA.md").exists())


    def test_oversized_agenda_preview_runs_both_stages_without_changing_agenda(self):
        limit = self.heartbeat_module().MAX_AGENDA
        prefix = b"# Large agenda\n".ljust(limit, b"x")
        original = prefix + b"\nThis suffix must remain only in the full agenda.\n"
        for policy in ("workspace", "owner"):
            with self.subTest(policy=policy):
                root = self.make_root("oversized-" + policy)
                (root / "AGENDA.md").write_bytes(original)
                before = len(self.invocations())
                process, payload = self.call(root=root, options=("--execution-policy", policy))
                self.assertEqual(process.returncode, 0, (payload, process.stderr))
                self.assertEqual(self.receipt(root)[1]["status"], "completed")
                invocations = self.invocations()[before:]
                self.assertEqual([entry["stage"] for entry in invocations], ["orientation", "work"])
                for entry in invocations:
                    self.assertIn(prefix.decode("utf-8"), entry["prompt"])
                    self.assertIn(f"AGENDA.md preview truncated at {limit} bytes", entry["prompt"])
                    self.assertIn("Read the full AGENDA.md file", entry["prompt"])
                    self.assertNotIn("This suffix must remain", entry["prompt"])
                self.assertEqual((root / "AGENDA.md").read_bytes(), original)


    def test_exact_limit_agenda_has_no_truncation_cue(self):
        limit = self.heartbeat_module().MAX_AGENDA
        original = b"x" * (limit - 4) + "\U0001f980".encode("utf-8")
        (self.root / "AGENDA.md").write_bytes(original)
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        for entry in self.invocations():
            self.assertIn(original.decode("utf-8"), entry["prompt"])
            self.assertNotIn("AGENDA.md preview truncated", entry["prompt"])
        self.assertEqual((self.root / "AGENDA.md").read_bytes(), original)


    def test_agenda_preview_drops_only_incomplete_character_at_cutoff(self):
        heartbeat = self.heartbeat_module()
        path = self.root / "AGENDA.md"
        for character in ("\u00e9", "\u20ac", "\U0001f980"):
            encoded = character.encode("utf-8")
            for partial in range(1, len(encoded)):
                with self.subTest(character=character, partial=partial):
                    prefix = "x" * (heartbeat.MAX_AGENDA - partial)
                    original = prefix.encode("utf-8") + encoded + b"\nremaining agenda"
                    path.write_bytes(original)
                    preview = heartbeat.agenda_preview(path)
                    self.assertEqual(preview.split("\n\n[AGENDA.md preview truncated", 1)[0], prefix)
                    self.assertIn("Read the full AGENDA.md file", preview)
                    self.assertNotIn("\ufffd", preview)
                    self.assertEqual(path.read_bytes(), original)


    def test_agenda_preview_reads_only_limit_plus_one(self):
        heartbeat = self.heartbeat_module()
        with patch.object(Path, "open", mock_open(read_data=b"x" * (heartbeat.MAX_AGENDA + 20))) as opened:
            preview = heartbeat.agenda_preview(self.root / "AGENDA.md")
        opened.assert_called_once_with("rb")
        opened().read.assert_called_once_with(heartbeat.MAX_AGENDA + 1)
        self.assertIn("AGENDA.md preview truncated", preview)


    def test_generic_bounded_reader_still_refuses_oversized_files(self):
        heartbeat = self.heartbeat_module()
        path = self.root / "AGENDA.md"
        path.write_bytes(b"x" * (heartbeat.MAX_AGENDA + 1))
        with self.assertRaisesRegex(heartbeat.Refusal, "File exceeds"):
            heartbeat.bounded_bytes(path, heartbeat.MAX_AGENDA)


    def test_agenda_symlink_and_nonregular_files_warn_without_reading_them(self):
        target = self.base / "linked-agenda.txt"
        target.write_text("Linked agenda content must not be loaded.")
        for kind in ("symlink", "dangling-symlink", "directory", "fifo"):
            with self.subTest(kind=kind):
                root = self.make_root(kind)
                path = root / "AGENDA.md"
                path.unlink()
                if kind == "symlink":
                    path.symlink_to(target)
                elif kind == "dangling-symlink":
                    path.symlink_to(root / "missing")
                elif kind == "directory":
                    path.mkdir()
                else:
                    os.mkfifo(path)
                process, payload = self.call(root=root)
                self.assertEqual(process.returncode, 0, payload)
                for entry in self.invocations()[-2:]:
                    self.assertIn("Context warning: AGENDA.md was not loaded", entry["prompt"])
                    self.assertIn("regular, non-symlink file", entry["prompt"])
                    self.assertNotIn("Linked agenda content must not be loaded.", entry["prompt"])


    def test_invalid_agenda_utf8_warns_without_changing_file(self):
        limit = self.heartbeat_module().MAX_AGENDA
        invalid = (
            b"agenda\xfftext",
            b"agenda\xff" + b"x" * limit,
            b"unfinished\xe2\x82",
            b"x" * (limit - 2) + b"\xe2\x82",
            b"x" * (limit - 2) + b"\xed\xa0\x80",  # Surrogate, not a valid partial character.
            b"x" * (limit - 2) + b"\xe0\x80\x80",  # Overlong encoding at cutoff.
            b"x" * (limit - 2) + b"\xf4\x90\x80\x80",  # Above Unicode range.
        )
        for number, original in enumerate(invalid):
            with self.subTest(case=number):
                root = self.make_root(f"invalid-utf8-{number}")
                path = root / "AGENDA.md"
                path.write_bytes(original)
                process, payload = self.call(root=root)
                self.assertEqual(process.returncode, 0, payload)
                for entry in self.invocations()[-2:]:
                    self.assertIn("Context warning: AGENDA.md was not loaded", entry["prompt"])
                    self.assertIn("invalid UTF-8", entry["prompt"])
                self.assertEqual(path.read_bytes(), original)


if __name__ == "__main__":
    unittest.main()

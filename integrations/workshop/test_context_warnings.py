"""Optional narrative failures do not acquire authority over whether we wake."""

import errno
import json
from pathlib import Path
import unittest
from unittest.mock import patch

from heartbeat_test_support import HeartbeatHarness


class ContextWarningTests(HeartbeatHarness, unittest.TestCase):
    def setUp(self):
        super().setUp()
        self.heartbeat = self.heartbeat_module()

    def test_expected_loader_errors_become_bounded_source_warnings(self):
        for error in (FileNotFoundError(errno.ENOENT, "gone"),
                      PermissionError(errno.EACCES, "permission denied"),
                      OSError(errno.EIO, "read failed"),
                      self.heartbeat.Refusal("unsafe source\n" + "x" * 5000),
                      UnicodeDecodeError("utf-8", b"\xff", 0, 1, "invalid byte")):
            with self.subTest(error=type(error).__name__):
                def loader():
                    raise error
                result = self.heartbeat.optional_context("test source", loader)
                self.assertIn("Context warning: test source was not loaded", result)
                self.assertIn("No source file was changed", result)
                self.assertLess(len(result.encode("utf-8")), 1400)
                self.assertEqual(result.count("\n"), 1)

    def test_unexpected_programming_error_is_not_hidden(self):
        with self.assertRaisesRegex(RuntimeError, "bug"):
            self.heartbeat.optional_context("test source", lambda: (_ for _ in ()).throw(RuntimeError("bug")))

    def test_journal_discovery_io_error_is_inside_optional_boundary(self):
        journal = self.root / "artifacts" / "journal"
        journal.mkdir()
        with patch.object(Path, "iterdir", side_effect=PermissionError(errno.EACCES, "cannot list")):
            result = self.heartbeat.optional_context(
                "latest journal in artifacts/journal/",
                lambda: self.heartbeat.latest_journal(self.root))
        self.assertIn("latest journal in artifacts/journal/ was not loaded", result)
        self.assertIn("cannot list", result)

    def test_bad_journal_warns_both_stages_without_losing_agenda(self):
        journal = self.root / "artifacts" / "journal"
        journal.mkdir()
        old = journal / "2026-09-26.md"
        old.write_text("Do not silently substitute an older journal.")
        latest = journal / "2026-09-27.md"
        latest.write_bytes(b"bad UTF-8\xff")
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        for invocation in self.invocations():
            text = invocation["prompt"]
            self.assertIn("Context warning: latest journal", text)
            self.assertIn("invalid UTF-8", text)
            self.assertIn("Explore one bounded question.", text)
            self.assertNotIn("Do not silently substitute", text)
        self.assertEqual(latest.read_bytes(), b"bad UTF-8\xff")

    def test_both_optional_sources_can_fail_and_work_still_starts(self):
        (self.root / "AGENDA.md").unlink()
        journal = self.root / "artifacts" / "journal"
        journal.symlink_to(self.root)
        process, payload = self.call()
        self.assertEqual(process.returncode, 0, payload)
        self.assertEqual([item["stage"] for item in self.invocations()], ["orientation", "work"])
        for item in self.invocations():
            self.assertIn("Context warning: AGENDA.md", item["prompt"])
            self.assertIn("Context warning: latest journal", item["prompt"])
        self.assertTrue(journal.is_symlink())

    def test_context_warning_does_not_hide_corrupt_control_state(self):
        (self.root / "AGENDA.md").unlink()
        path = self.root / "state.json"
        original = json.dumps({"schema": "unknown"})
        path.write_text(original)
        process, payload = self.call()
        self.assertNotEqual(process.returncode, 0)
        self.assertEqual(payload["status"], "refused")
        self.assertEqual(self.invocations(), [])
        self.assertEqual(path.read_text(), original)

    def test_service_gates_pause_not_optional_agenda(self):
        unit = Path(__file__).with_name("systemd") / "mneme-workshop.service"
        content = unit.read_text()
        self.assertNotIn("ConditionPathExists=/home/user/workshop/AGENDA.md", content)
        self.assertIn("ConditionPathExists=!/home/user/workshop/PAUSED", content)


if __name__ == "__main__":
    unittest.main()

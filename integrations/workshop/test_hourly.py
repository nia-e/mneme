"""Offline slot state, durability cuts and true multiprocess contention."""
import datetime as dt
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import hourly


class HourlyTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.now = dt.datetime(2026, 9, 26, 12, 1, tzinfo=dt.timezone.utc)

    def test_read_only_absent(self):
        self.assertEqual(hourly.inspect(self.root, self.now)["decision"], "start")
        self.assertEqual(list(self.root.iterdir()), [])

    def test_all_day_constant_storage_and_clock_rollback(self):
        for n in range(24 * 40):
            now = self.now + dt.timedelta(hours=n)
            self.assertEqual(hourly.consume(self.root, now)["decision"], "start")
        self.assertLess((self.root / "hourly-state.json").stat().st_size, 1024)
        self.assertEqual(hourly.consume(self.root, self.now)["decision"], "already_decided")
        self.assertEqual(len(list(self.root.iterdir())), 2)

    def test_grace_boundary_outage_and_utc_conversion(self):
        self.assertEqual(hourly.consume(self.root, self.now.replace(minute=5))["decision"], "missed_window")
        now = self.now + dt.timedelta(hours=4)
        self.assertEqual(hourly.consume(self.root, now)["decision"], "start")
        elsewhere = now.astimezone(dt.timezone(dt.timedelta(hours=7)))
        self.assertEqual(hourly.consume(self.root, elsewhere)["decision"], "already_decided")

    def test_occupied_hour_does_not_retry_after_finish(self):
        hourly.consume(self.root, self.now)
        finish = self.now + dt.timedelta(minutes=60)
        self.assertEqual(hourly.finish_occupied(self.root, self.now, finish)["decision"], "skip_busy")
        self.assertEqual(hourly.consume(self.root, finish)["decision"], "already_decided")
        self.assertIsNone(hourly.finish_occupied(self.root, finish, finish))

    def test_reject_bad_clock_reason_and_unknown_schema(self):
        with self.assertRaises(hourly.Refusal):
            hourly.consume(self.root, self.now.replace(tzinfo=None))
        with self.assertRaises(hourly.Refusal):
            hourly.consume(self.root, self.now, "maybe")
        (self.root / "hourly-state.json").write_text('{"schema":"future"}')
        with self.assertRaises(hourly.Refusal):
            hourly.consume(self.root, self.now)
        self.assertEqual((self.root / "hourly-state.json").read_text(), '{"schema":"future"}')

    def test_torn_duplicate_oversized_or_non_hour_state_refuses(self):
        path = self.root / "hourly-state.json"
        for data in ('{', '{"schema":"a","schema":"b"}', 'x' * 1025,
                     json.dumps({"schema": hourly.SCHEMA, "slot": self.now.isoformat(), "decision": "start"})):
            with self.subTest(data=data[:80]):
                path.write_text(data)
                with self.assertRaises(hourly.Refusal):
                    hourly.consume(self.root, self.now)
                self.assertEqual(path.read_text(), data)

    def test_symlink_and_hardlink_refuse_unchanged(self):
        other = self.root / "other"
        other.write_text("do not modify")
        state = self.root / "hourly-state.json"
        state.symlink_to(other)
        with self.assertRaises(OSError):
            hourly.consume(self.root, self.now)
        state.unlink()
        os.link(other, state)
        with self.assertRaises(hourly.Refusal):
            hourly.consume(self.root, self.now)
        self.assertEqual(other.read_text(), "do not modify")

    def test_pre_rename_failure_retries_without_torn_state(self):
        with patch.object(hourly.os, "replace", side_effect=OSError("before rename")):
            with self.assertRaises(OSError):
                hourly.consume(self.root, self.now)
        self.assertIsNone(hourly.read(self.root))
        self.assertEqual(list(self.root.glob("*.tmp")), [])
        self.assertEqual(hourly.consume(self.root, self.now)["decision"], "start")

    def test_post_rename_sync_failure_is_not_replayed(self):
        sync = hourly.os.fsync
        calls = 0
        def failing(fd):
            nonlocal calls
            calls += 1
            if calls == 2:
                raise OSError("after rename")
            return sync(fd)
        with patch.object(hourly.os, "fsync", side_effect=failing):
            with self.assertRaises(OSError):
                hourly.consume(self.root, self.now)
        self.assertEqual(hourly.consume(self.root, self.now)["decision"], "already_decided")

    def test_two_processes_consume_only_one_slot(self):
        script = "import hourly,datetime,pathlib,sys; print(hourly.consume(pathlib.Path(sys.argv[1]),datetime.datetime.fromisoformat(sys.argv[2]))['decision'])"
        command = [sys.executable, "-B", "-c", script, str(self.root), self.now.isoformat()]
        processes = [subprocess.Popen(command, cwd=Path(hourly.__file__).parent,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                     for _ in range(2)]
        results = [p.communicate(timeout=10) for p in processes]
        self.assertEqual(sorted(out.strip() for out, _ in results), ["already_decided", "start"])
        self.assertTrue(all(p.returncode == 0 for p in processes), results)

    def test_crash_after_claim_loses_slot_instead_of_replay(self):
        script = "import hourly,datetime,pathlib,sys,os; hourly.consume(pathlib.Path(sys.argv[1]),datetime.datetime.fromisoformat(sys.argv[2])); os._exit(17)"
        result = subprocess.run([sys.executable, "-B", "-c", script, str(self.root), self.now.isoformat()],
                                cwd=Path(hourly.__file__).parent, timeout=10)
        self.assertEqual(result.returncode, 17)
        self.assertEqual(hourly.consume(self.root, self.now)["decision"], "already_decided")


if __name__ == "__main__":
    unittest.main()

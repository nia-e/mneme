"""Local-only contract tests for durable workshop event hints."""

import json
import multiprocessing
import datetime as dt
import fcntl
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import wake


def _enqueue_worker(root, number):
    wake.enqueue(root, event(number))


def _hold_lock(root, ready):
    fd = os.open(root / ".events.lock", os.O_RDWR | os.O_CREAT, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        ready.set()
        time.sleep(3)
    finally:
        os.close(fd)


def event(number=1, **changes):
    value = {"source": "test", "event_id": str(number), "kind": "hint",
             "summary": "look here", "body": "untrusted body", "reference": "local:test"}
    value.update(changes)
    return value


class WakeTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()

    def test_absent_status_is_read_only(self):
        self.assertFalse(wake.pending(self.root))
        self.assertEqual(wake.watermark(self.root), 0)
        self.assertEqual(list(self.root.iterdir()), [])

    def test_absent_inbox_summary_is_zero_and_read_only(self):
        self.assertEqual(wake.inbox_summary(self.root), {
            "pending_signal_bursts": 0, "pending_mailbox_messages": 0,
            "unfinished_signal_bursts": 0, "unfinished_mailbox_messages": 0})
        self.assertEqual(list(self.root.iterdir()), [])

    def test_inbox_summary_counts_exact_channels_and_separates_statuses(self):
        records = []
        pairs = ((wake.SIGNAL_SOURCE, wake.SIGNAL_KIND),
                 (wake.MAILBOX_SOURCE, wake.MAILBOX_KIND),
                 (wake.SIGNAL_SOURCE, "wrong-kind"),
                 (wake.MAILBOX_SOURCE, "wrong-kind"),
                 ("technical", wake.MAILBOX_KIND))
        for source, kind in pairs:
            for status in ("pending", "held", "delivered", "unfinished"):
                number = len(records) + 1
                records.append({"sequence": number, "event": event(number, source=source, kind=kind),
                                "status": status, "claimed_run": None if status == "pending" else "run",
                                "detail": ""})
        queue = self.root / "events.json"
        queue.write_text(json.dumps({"schema": wake.SCHEMA, "next_sequence": len(records) + 1,
                                     "records": records}))
        before = queue.read_bytes()
        self.assertEqual(wake.inbox_summary(self.root), {
            "pending_signal_bursts": 1, "pending_mailbox_messages": 1,
            "unfinished_signal_bursts": 1, "unfinished_mailbox_messages": 1})
        self.assertEqual(queue.read_bytes(), before)
        self.assertEqual(list(self.root.iterdir()), [queue])

    def test_inbox_summary_keeps_existing_queue_validation_and_legacy_reads(self):
        wake.enqueue(self.root, event(source=wake.SIGNAL_SOURCE, kind=wake.SIGNAL_KIND))
        queue = self.root / "events.json"
        value = json.loads(queue.read_text())
        for encoding in (value, {**value, "schema": wake.PREVIOUS_SCHEMA},
                         {**value, "schema": wake.V2_SCHEMA}, value["records"]):
            with self.subTest(encoding=type(encoding).__name__):
                queue.write_text(json.dumps(encoding))
                before = queue.read_bytes()
                self.assertEqual(wake.inbox_summary(self.root)["pending_signal_bursts"], 1)
                self.assertEqual(queue.read_bytes(), before)
        queue.write_text('{"schema":"unknown"}')
        with self.assertRaises(wake.Refusal):
            wake.inbox_summary(self.root)

    def test_capacity_stats_are_read_only_and_expose_reservation(self):
        empty = wake.queue_stats(self.root)
        self.assertEqual(empty["active_events"], 0)
        self.assertEqual(list(self.root.iterdir()), [])
        wake.enqueue(self.root, event())
        before = (self.root / "events.json").read_bytes()
        stats = wake.queue_stats(self.root)
        self.assertEqual(stats["active_events"], 1)
        self.assertEqual(stats["encoded_bytes"], len(before))
        self.assertGreater(stats["reserved_bytes"], stats["encoded_bytes"])
        self.assertEqual(stats["max_bytes"], wake.MAX_BYTES)
        self.assertEqual(stats["max_records"], wake.MAX_RECORDS)
        self.assertEqual((self.root / "events.json").read_bytes(), before)

    def test_idempotency_conflict_and_payload_bounds(self):
        first = wake.enqueue(self.root, event())
        self.assertEqual(first["sequence"], 1)
        self.assertEqual(wake.enqueue(self.root, event()), first)
        self.assertEqual(wake.watermark(self.root), 1)
        with self.assertRaises(wake.Refusal):
            wake.enqueue(self.root, event(body="changed"))
        for key, width in wake.FIELDS.items():
            with self.subTest(key=key):
                with self.assertRaises(wake.Refusal):
                    wake.enqueue(self.root, event(2, **{key: "x" * (width + 1)}))
                with self.assertRaises(wake.Refusal):
                    wake.enqueue(self.root, event(2, **{key: 1}))
        with self.assertRaises(wake.Refusal):
            wake.enqueue(self.root, dict(event(2), extra="no"))

    def test_claim_batch_new_arrival_and_lifecycle(self):
        for i in range(1, 6):
            wake.enqueue(self.root, event(i))
        claimed = wake.claim(self.root, "run-1")
        self.assertEqual([r["sequence"] for r in claimed["events"]], [1, 2, 3, 4])
        self.assertEqual(claimed["watermark"], 5)
        self.assertTrue(wake.pending(self.root))
        self.assertFalse(wake.pending(self.root, after_sequence=5))
        wake.enqueue(self.root, event(6))
        self.assertTrue(wake.pending(self.root, after_sequence=5))
        wake.delivered(self.root, "run-1")
        self.assertEqual(wake.claim(self.root, "run-2")["events"][0]["sequence"], 5)
        self.assertFalse(wake.pending(self.root))
        wake.unfinished(self.root, "run-2", "interrupted")
        self.assertEqual(wake.claim(self.root, "run-3")["events"], [])
        retry = wake.claim(self.root, "run-3", include_unfinished=True)
        self.assertEqual([r["sequence"] for r in retry["events"]], [5, 6])

    def test_new_arrival_precedes_old_pending_backlog_after_failure(self):
        for i in range(1, 7):
            wake.enqueue(self.root, event(i))
        first = wake.claim(self.root, "failed")
        self.assertEqual([r["sequence"] for r in first["events"]], [1, 2, 3, 4])
        wake.unfinished(self.root, "failed", "orientation failed")
        self.assertFalse(wake.pending(self.root, after_sequence=first["watermark"]))
        wake.enqueue(self.root, event(7))
        self.assertTrue(wake.pending(self.root, after_sequence=first["watermark"]))
        next_batch = wake.claim(self.root, "new", after_sequence=first["watermark"])
        self.assertEqual([r["sequence"] for r in next_batch["events"]], [7, 5, 6])

    def test_cycle_class_filters_pending_unfinished_and_claim(self):
        ordinary = event(1)
        signal = event(2, source=wake.SIGNAL_SOURCE, kind=wake.SIGNAL_KIND)
        signal_other = event(3, source=wake.SIGNAL_SOURCE, kind="other")
        for item in (ordinary, signal, signal_other):
            wake.enqueue(self.root, item)
        self.assertTrue(wake.pending(self.root, cycle_class="background"))
        self.assertTrue(wake.pending(self.root, cycle_class="interactive"))
        self.assertEqual([r["sequence"] for r in wake.claim(self.root, "background",
                                                           cycle_class="background")["events"]], [1])
        self.assertEqual([r["sequence"] for r in wake.claim(self.root, "interactive",
                                                           cycle_class="interactive")["events"]], [2])
        self.assertEqual([r["sequence"] for r in wake.claim(self.root, "unfiltered")["events"]], [3])
        wake.unfinished(self.root, "interactive", "retry interactive")
        self.assertTrue(wake.pending(self.root, after_sequence=100,
                                     cycle_class="interactive", include_unfinished=True))
        self.assertFalse(wake.pending(self.root, after_sequence=100,
                                      cycle_class="interactive", include_unfinished=False))
        self.assertFalse(wake.pending(self.root, after_sequence=100,
                                      cycle_class="background", include_unfinished=True))
        retried = wake.claim(self.root, "retry", include_unfinished=True,
                             cycle_class="interactive")
        self.assertEqual([r["sequence"] for r in retried["events"]], [2])
        with self.assertRaises(wake.Refusal):
            wake.pending(self.root, cycle_class="unknown")

    def test_independent_mailbox_and_signal_inbox_filters(self):
        fixtures = [event(1, source="technical", kind="hint"),
                    event(2, source=wake.SIGNAL_SOURCE, kind=wake.SIGNAL_KIND),
                    event(3, source=wake.MAILBOX_SOURCE, kind=wake.MAILBOX_KIND),
                    event(4, source=wake.SIGNAL_SOURCE, kind="wrong-kind"),
                    event(5, source=wake.MAILBOX_SOURCE, kind="wrong-kind")]
        cases = ((True, True, [2, 3], [1]),
                 (True, False, [3], [1]),
                 (False, True, [2], [1, 3, 5]),
                 (False, False, [], [1, 3, 5]))
        for mailbox_enabled, signal_enabled, interactive, background in cases:
            with self.subTest(mailbox=mailbox_enabled, signal=signal_enabled):
                root = self.root / f"lane-{mailbox_enabled}-{signal_enabled}"
                root.mkdir()
                for payload in fixtures:
                    wake.enqueue(root, payload)
                flags = {"mailbox_inbox": mailbox_enabled, "signal_inbox": signal_enabled}
                self.assertEqual(wake.pending(root, cycle_class="interactive", **flags), bool(interactive))
                claimed = wake.claim(root, "chat", cycle_class="interactive", **flags)["events"]
                self.assertEqual([r["sequence"] for r in claimed], interactive)
                self.assertFalse(wake.pending(root, cycle_class="interactive", **flags))
                ordinary = wake.claim(root, "background", cycle_class="background", **flags)["events"]
                self.assertEqual([r["sequence"] for r in ordinary], background)
                self.assertEqual([r["sequence"] for r in wake.active_records(root)
                                  if r["status"] == "pending"],
                                 [n for n in (1, 2, 3, 4, 5) if n not in interactive + background])
                self.assertEqual([r["sequence"] for r in wake.claim(root, "generic")["events"][:2]],
                                 [n for n in (1, 2, 3, 4, 5) if n not in interactive + background][:2])
        with self.assertRaises(wake.Refusal):
            wake.pending(self.root, mailbox_inbox=1)
        with self.assertRaises(wake.Refusal):
            wake.claim(self.root, "bad-flags", signal_inbox="yes")

    def test_due_retry_sees_old_pending_below_watermark(self):
        wake.enqueue(self.root, event(1, source=wake.SIGNAL_SOURCE, kind=wake.SIGNAL_KIND))
        self.assertFalse(wake.pending(self.root, 100, cycle_class="interactive"))
        self.assertTrue(wake.pending(self.root, 100, cycle_class="interactive",
                                     include_unfinished=True))

    def test_legacy_sequence_floor_uses_both_lane_watermarks(self):
        (self.root / "events.json").write_text("[]")
        (self.root / "state.json").write_text(json.dumps({"event_after_sequence": 7,
                                                            "interactive": {"event_after_sequence": 40}}))
        self.assertEqual(wake.watermark(self.root), 40)
        wake.upgrade(self.root)
        self.assertEqual(wake.enqueue(self.root, event())["sequence"], 41)

    def test_upgrade_preserves_v3_records_and_retry_after_prepublication_failure(self):
        first = wake.enqueue(self.root, event())
        path = self.root / "events.json"
        prior = json.loads(path.read_text())
        prior["schema"] = wake.PREVIOUS_SCHEMA
        path.write_text(json.dumps(prior))
        prior_bytes = path.read_bytes()
        with patch.object(wake.os, "replace", side_effect=OSError("prepublication failure")):
            with self.assertRaises(OSError):
                wake.upgrade(self.root)
        self.assertEqual(path.read_bytes(), prior_bytes)
        wake.upgrade(self.root)
        current = json.loads(path.read_text())
        self.assertEqual(current["schema"], wake.SCHEMA)
        self.assertEqual(current["records"], [first])
        self.assertEqual(current["next_sequence"], 2)
        with patch.object(wake, "_write", side_effect=AssertionError("v4 retry rewrote queue")):
            wake.upgrade(self.root)
        self.assertEqual(wake.enqueue(self.root, event(2))["sequence"], 2)

    def test_current_fence_absent_and_deliberate_legacy_upgrade_preserves_floor(self):
        (self.root / "state.json").write_text(json.dumps({"event_after_sequence": 11,
                                                            "interactive": {"event_after_sequence": 25}}))
        wake.fence_current(self.root)
        self.assertEqual(json.loads((self.root / "events.json").read_text())["next_sequence"], 26)
        self.assertEqual(wake.enqueue(self.root, event())["sequence"], 26)
        record = wake.active_records(self.root)[0]
        (self.root / "events.json").write_text(json.dumps([record]))
        with self.assertRaisesRegex(wake.Refusal, "explicit queue upgrade"):
            wake.fence_current(self.root)
        wake.upgrade(self.root)
        state = json.loads((self.root / "events.json").read_text())
        self.assertEqual(state["schema"], wake.SCHEMA)
        self.assertEqual(state["next_sequence"], 27)
        self.assertEqual(state["records"], [record])

    def test_upgrade_crossed_before_directory_sync_is_synced_on_retry(self):
        wake.enqueue(self.root, event())
        path = self.root / "events.json"
        old = json.loads(path.read_text())
        old["schema"] = wake.PREVIOUS_SCHEMA
        path.write_text(json.dumps(old))
        original_fsync = wake.os.fsync
        calls = []
        def fail_second_sync(fd):
            calls.append(fd)
            if len(calls) == 2:
                raise OSError("post-replace directory sync failed")
            original_fsync(fd)
        with patch.object(wake.os, "fsync", side_effect=fail_second_sync):
            with self.assertRaises(OSError):
                wake.upgrade(self.root)
        self.assertEqual(json.loads(path.read_text())["schema"], wake.SCHEMA)
        with patch.object(wake, "_sync_dir", wraps=wake._sync_dir) as synced, \
             patch.object(wake, "_write", side_effect=AssertionError("must not rewrite")):
            wake.upgrade(self.root)
            synced.assert_called_once_with(self.root)

    def test_recovery_only_named_held_runs(self):
        wake.enqueue(self.root, event())
        wake.claim(self.root, "r")
        wake.recover(self.root, {"other"})
        self.assertEqual(wake.claim(self.root, "s", True)["events"], [])
        wake.recover(self.root, {"r"})
        self.assertEqual(wake.claim(self.root, "s", True)["events"][0]["status"], "held")

    def test_concurrent_enqueue(self):
        with multiprocessing.Pool(8) as pool:
            pool.starmap(_enqueue_worker, [(self.root, i) for i in range(32)])
        records = wake.active_records(self.root)
        self.assertEqual({r["event"]["event_id"] for r in records}, {str(i) for i in range(32)})
        self.assertEqual([r["sequence"] for r in records], list(range(1, 33)))

    def test_contended_lock_refuses_within_one_cycle(self):
        ready = multiprocessing.Event()
        holder = multiprocessing.Process(target=_hold_lock, args=(self.root, ready))
        holder.start()
        try:
            self.assertTrue(ready.wait(2))
            start = time.monotonic()
            with self.assertRaisesRegex(wake.Refusal, "busy"):
                wake.enqueue(self.root, event())
            self.assertLess(time.monotonic() - start, 1.5)
        finally:
            holder.terminate()
            holder.join(timeout=2)

    def test_full_corrupt_and_unsafe_paths_fail_closed(self):
        for i in range(64):
            wake.enqueue(self.root, event(i))
        with self.assertRaises(wake.Refusal):
            wake.enqueue(self.root, event(65))
        queue = self.root / "events.json"
        queue.write_text("not json")
        with self.assertRaises(wake.Refusal):
            wake.pending(self.root)
        with self.assertRaises(wake.Refusal):
            wake.enqueue(self.root, event(66))
        queue.unlink()
        target = self.root / "target"
        target.write_text("[]")
        queue.symlink_to(target)
        with self.assertRaises(wake.Refusal):
            wake.enqueue(self.root, event(66))
        self.assertEqual(target.read_text(), "[]")

    def test_encoded_byte_quota_and_symlink_lock(self):
        accepted = 0
        while True:
            try:
                wake.enqueue(self.root, event(accepted, body="🦋" * 8000))
            except wake.Refusal:
                break
            accepted += 1
        self.assertGreaterEqual(accepted, 6)
        self.assertLess(len((self.root / "events.json").read_bytes()), wake.MAX_BYTES)
        self.assertEqual(wake.watermark(self.root), accepted)
        first = wake.claim(self.root, "r" * 128)
        self.assertTrue(first["events"])
        wake.unfinished(self.root, "r" * 128, "\x00" * 512)
        self.assertLessEqual(len((self.root / "events.json").read_bytes()), wake.MAX_BYTES)
        with self.assertRaises(wake.Refusal):
            wake.enqueue(self.root, event(accepted, body="🦋" * 8000))
        lock = self.root / ".events.lock"
        lock.unlink()
        lock.symlink_to(self.root / "events.json")
        with self.assertRaises(wake.Refusal):
            wake.enqueue(self.root, event(9))

    def test_root_must_be_canonical(self):
        alias = self.root / "alias"
        alias.symlink_to(self.root, target_is_directory=True)
        with self.assertRaises(wake.Refusal):
            wake.pending(alias)
        with self.assertRaises(wake.Refusal):
            wake.enqueue(Path("relative"), event())
        wake.enqueue(self.root, event())
        with self.assertRaises(wake.Refusal):
            wake.claim(self.root, "../outside")

    def complete_run(self, run_id, records, replies=(), *, old=False):
        run = self.root / "runs" / run_id
        run.mkdir(parents=True)
        started = dt.datetime.now(dt.timezone.utc) - dt.timedelta(days=2 if old else 0)
        identities = [{"source": r["event"]["source"], "event_id": r["event"]["event_id"]}
                      for r in records]
        receipt = {"schema": "mneme.workshop.receipt.v2", "run_id": run_id,
                   "status": "completed", "started_at": started.isoformat(),
                   "event_count": len(identities), "event_ids": identities}
        (run / "receipt.json").write_text(json.dumps(receipt))
        (run / "result.json").write_text(json.dumps({"status": "rest", "summary": "done",
                                                      "artifacts": [], "next_step": "",
                                                      "memory_candidates": [],
                                                      "next_wake_seconds": 21600,
                                                      "replies": list(replies)}))
        return run

    def test_archive_completed_proof_retry_and_sparse_sequence(self):
        first = wake.enqueue(self.root, event())
        batch = wake.claim(self.root, "run-old")
        wake.delivered(self.root, "run-old")
        run = self.complete_run("run-old", batch["events"],
                                [{"source": "test", "event_id": "1", "text": "noted"}], old=True)
        with self.assertRaises(wake.Refusal):
            wake.archive(self.root)
        (self.root / "PAUSED").touch()
        result = wake.archive(self.root)
        self.assertEqual(result["archived_events"], 1)
        self.assertEqual(result["relocated_runs"], 1)
        self.assertFalse(run.exists())
        self.assertTrue((self.root / "run-archive" / "run-old").is_dir())
        archived = wake.lookup(self.root, "test", "1")
        self.assertEqual(archived["location"], "archive")
        self.assertEqual(archived["terminal"], {"status": "replied", "reply": "noted"})
        self.assertEqual(wake.enqueue(self.root, event()), first | {"status": "delivered", "claimed_run": "run-old"})
        with self.assertRaises(wake.Refusal):
            wake.enqueue(self.root, event(body="conflict"))
        self.assertEqual(wake.archive(self.root)["archived_events"], 0)
        later = wake.enqueue(self.root, event(2))
        self.assertEqual(later["sequence"], 2)
        self.assertEqual(wake.watermark(self.root), 2)

    def test_archive_requires_terminal_proof_and_preserves_today(self):
        wake.enqueue(self.root, event())
        batch = wake.claim(self.root, "run-today")
        wake.delivered(self.root, "run-today")
        run = self.complete_run("run-today", batch["events"])
        (self.root / "PAUSED").touch()
        self.assertEqual(wake.archive(self.root)["archived_events"], 1)
        self.assertTrue(run.exists())
        self.assertEqual(wake.lookup(self.root, "test", "1")["terminal"],
                         {"status": "completed_without_reply", "reply": None})

    def test_archive_refuses_invalid_completion_and_legacy_watermark(self):
        first = wake.enqueue(self.root, event())
        batch = wake.claim(self.root, "run-bad")
        wake.delivered(self.root, "run-bad")
        run = self.complete_run("run-bad", batch["events"])
        (run / "receipt.json").write_text(json.dumps({"schema": "mneme.workshop.receipt.v1"}))
        (self.root / "PAUSED").touch()
        with self.assertRaises(wake.Refusal):
            wake.archive(self.root)
        self.assertEqual(wake.active_records(self.root)[0]["event"], first["event"])
        (self.root / "events.json").write_text("[]")
        (self.root / "state.json").write_text(json.dumps({"event_after_sequence": 99}))
        self.assertEqual(wake.watermark(self.root), 99)
        wake.upgrade(self.root)
        self.assertEqual(wake.enqueue(self.root, event(2))["sequence"], 100)

    def test_archive_publication_crash_points_retry_safely(self):
        wake.enqueue(self.root, event())
        batch = wake.claim(self.root, "run-crash")
        wake.delivered(self.root, "run-crash")
        self.complete_run("run-crash", batch["events"])
        (self.root / "PAUSED").touch()
        with patch.object(wake.os, "rename", side_effect=OSError("simulated prepublication crash")):
            with self.assertRaises(OSError):
                wake.archive(self.root)
        self.assertIsNone(next((self.root / "event-archive").glob("*.json"), None))
        self.assertEqual(wake.lookup(self.root, "test", "1")["location"], "active")
        with patch.object(wake, "_write", side_effect=OSError("simulated postpublication crash")):
            with self.assertRaises(OSError):
                wake.archive(self.root)
        self.assertEqual(wake.lookup(self.root, "test", "1")["location"], "active")
        self.assertEqual(wake.archive(self.root)["archived_events"], 1)
        self.assertEqual(wake.lookup(self.root, "test", "1")["location"], "archive")

    def test_existing_archive_is_synced_before_retry_prunes(self):
        wake.enqueue(self.root, event())
        batch = wake.claim(self.root, "run-sync")
        wake.delivered(self.root, "run-sync")
        self.complete_run("run-sync", batch["events"])
        (self.root / "PAUSED").touch()
        original_sync = wake._sync_dir
        original_write = wake._write
        def fail_after_publish(path):
            if path.name == "event-archive":
                raise OSError("simulated directory sync failure")
            original_sync(path)
        with patch.object(wake, "_sync_dir", side_effect=fail_after_publish):
            with self.assertRaises(OSError):
                wake.archive(self.root)
        self.assertEqual(wake.lookup(self.root, "test", "1")["location"], "active")
        synced = []
        def observe_sync(path):
            if path.name == "event-archive":
                synced.append(True)
            original_sync(path)
        def assert_then_write(*args, **kwargs):
            self.assertTrue(synced)
            return original_write(*args, **kwargs)
        with patch.object(wake, "_sync_dir", side_effect=observe_sync), \
             patch.object(wake, "_write", side_effect=assert_then_write):
            self.assertEqual(wake.archive(self.root)["archived_events"], 1)

    def test_relocation_retry_flushes_both_directories(self):
        run = self.complete_run("run-relocate", [], old=True)
        (self.root / "PAUSED").touch()
        original_sync = wake._sync_dir
        def fail_after_rename(path):
            if path.name == "runs":
                raise OSError("simulated post-rename directory sync failure")
            original_sync(path)
        with patch.object(wake, "_sync_dir", side_effect=fail_after_rename):
            with self.assertRaises(OSError):
                wake.archive(self.root)
        self.assertFalse(run.exists())
        self.assertTrue((self.root / "run-archive" / "run-relocate").exists())
        synced = []
        def observe(path):
            synced.append(path.name)
            original_sync(path)
        with patch.object(wake, "_sync_dir", side_effect=observe):
            self.assertEqual(wake.archive(self.root)["relocated_runs"], 0)
        self.assertIn("runs", synced)
        self.assertIn("run-archive", synced)

    def test_archive_keeps_yesterday_run_in_rolling_hour(self):
        run = self.complete_run("run-midnight", [], old=True)
        receipt_path = run / "receipt.json"
        receipt = json.loads(receipt_path.read_text())
        receipt["started_at"] = "2026-09-24T23:45:00+00:00"
        receipt_path.write_text(json.dumps(receipt))
        (self.root / "PAUSED").touch()
        real_datetime = dt.datetime
        class JustAfterMidnight(real_datetime):
            @classmethod
            def now(cls, tz=None):
                return cls(2026, 9, 25, 0, 30, tzinfo=dt.timezone.utc)
        with patch.object(wake.dt, "datetime", JustAfterMidnight):
            self.assertEqual(wake.archive(self.root)["relocated_runs"], 0)
        self.assertTrue(run.exists())

    def test_full_queue_archive_then_new_send(self):
        admitted = 0
        while True:
            try:
                wake.enqueue(self.root, event(admitted))
            except wake.Refusal:
                break
            admitted += 1
        self.assertGreaterEqual(admitted, 40)
        for batch_number in range((admitted + 3) // 4):
            run_id = f"run-{batch_number}"
            claimed = wake.claim(self.root, run_id)
            self.complete_run(run_id, claimed["events"], old=True)
            wake.delivered(self.root, run_id)
        (self.root / "PAUSED").touch()
        self.assertEqual(wake.archive(self.root)["archived_events"], admitted)
        self.assertEqual(wake.active_records(self.root), [])
        self.assertEqual(wake.enqueue(self.root, event(admitted))["sequence"], admitted + 1)

    def test_archive_max_escaped_payload_and_reply(self):
        payload = event(source="s" * 64, event_id="i" * 128, kind="k" * 64,
                        summary="\x00" * 1000, body="\x00" * 8000,
                        reference="\x00" * 1000)
        wake.enqueue(self.root, payload)
        batch = wake.claim(self.root, "run-max")
        self.complete_run("run-max", batch["events"],
                          [{"source": payload["source"], "event_id": payload["event_id"],
                            "text": "\x00" * 4000}])
        wake.delivered(self.root, "run-max")
        (self.root / "PAUSED").touch()
        self.assertEqual(wake.archive(self.root)["archived_events"], 1)
        self.assertEqual(len(wake.lookup(self.root, payload["source"], payload["event_id"])
                             ["terminal"]["reply"]), 4000)

    def test_maintenance_recovers_future_size_stall_without_pause(self):
        # Real admission reserves future failure metadata, so a small on-disk
        # queue can already be full. Reproduce that rule rather than mocking it.
        admitted = 0
        while True:
            try:
                wake.enqueue(self.root, event(admitted, body="x" * 1200))
            except wake.Refusal:
                break
            admitted += 1
        for number in range((admitted + 3) // 4):
            run_id = f"run-maintain-{number}"
            claimed = wake.claim(self.root, run_id)
            self.complete_run(run_id, claimed["events"])
            wake.delivered(self.root, run_id)
        before, next_sequence = wake._load(self.root)
        new = {"event": event(admitted, body="x" * 1200), "status": "pending",
               "claimed_run": None, "detail": "", "sequence": next_sequence}
        self.assertLess((self.root / "events.json").stat().st_size, wake.MAX_BYTES // 2)
        self.assertGreater(wake._future_size(before + [new], next_sequence + 1), wake.MAX_BYTES)
        with self.assertRaisesRegex(wake.Refusal, "queue is full"):
            wake.enqueue(self.root, new["event"])
        result = wake.maintain(self.root)
        self.assertEqual(result, {"status": "maintained", "archived_events": admitted,
                                  "active_events": 0, "relocated_runs": 0,
                                  "watermark": admitted, "skipped_incomplete_events": 0})
        self.assertFalse((self.root / "PAUSED").exists())
        self.assertEqual(wake.enqueue(self.root, new["event"])["sequence"], admitted + 1)
        for record in before:
            old = record["event"]
            self.assertEqual(wake.lookup(self.root, old["source"], old["event_id"])["record"], record)
            self.assertEqual(wake.enqueue(self.root, old), record)
        with self.assertRaisesRegex(wake.Refusal, "conflicting archived payload"):
            wake.enqueue(self.root, event(0, body="changed"))

    def test_maintenance_skips_paused_and_both_busy_locks_without_waiting(self):
        pause = self.root / "PAUSED"
        pause.write_text("human pause\n")
        self.assertEqual(wake.maintain(self.root), {"status": "skipped", "reason": "paused"})
        self.assertEqual(pause.read_text(), "human pause\n")
        self.assertEqual(list(self.root.iterdir()), [pause])
        pause.unlink()
        for name in (".workshop.lock", ".events.lock"):
            with self.subTest(name=name):
                fd = os.open(self.root / name, os.O_RDWR | os.O_CREAT, 0o600)
                try:
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    start = time.monotonic()
                    self.assertEqual(wake.maintain(self.root),
                                     {"status": "skipped", "reason": "busy"})
                    self.assertLess(time.monotonic() - start, 0.5)
                finally:
                    os.close(fd)
        self.assertFalse((self.root / "events.json").exists())
        # All acquired leases were released, including the workshop lease when
        # the queue writer was busy.
        self.assertEqual(wake.maintain(self.root)["status"], "maintained")

    def test_maintenance_preserves_all_incomplete_lifecycles(self):
        pinned = []
        for number, status in enumerate(("running", "failed", "timeout", "interrupted",
                                         "invalid_result", "output_limit", "completed")):
            wake.enqueue(self.root, event(number))
            run_id = f"run-{status}"
            claimed = wake.claim(self.root, run_id)
            run = self.complete_run(run_id, claimed["events"], old=True)
            wake.delivered(self.root, run_id)
            if status != "completed":
                path = run / "receipt.json"
                receipt = json.loads(path.read_text())
                receipt["status"] = status
                path.write_text(json.dumps(receipt))
                (run / "result.json").unlink()  # No terminal result required to retain.
                pinned.append(run)
        wake.enqueue(self.root, event("unfinished"))
        claimed = wake.claim(self.root, "run-unfinished")
        pinned.append(self.complete_run("run-unfinished", claimed["events"], old=True))
        wake.unfinished(self.root, "run-unfinished", "keep this evidence")
        wake.enqueue(self.root, event("held"))
        claimed = wake.claim(self.root, "run-held")
        pinned.append(self.complete_run("run-held", claimed["events"], old=True))
        wake.enqueue(self.root, event("pending"))
        before = wake.active_records(self.root)
        expected = [r for r in before if r["claimed_run"] != "run-completed"]
        # The explicit operator command retains its stricter historical contract.
        (self.root / "PAUSED").touch()
        with self.assertRaisesRegex(wake.Refusal, "completed reply-capable receipt"):
            wake.archive(self.root)
        self.assertEqual(wake.active_records(self.root), before)
        (self.root / "PAUSED").unlink()
        result = wake.maintain(self.root)
        self.assertEqual(result["archived_events"], 1)
        self.assertEqual(result["skipped_incomplete_events"], 6)
        self.assertEqual(result["relocated_runs"], 1)
        self.assertEqual(wake.active_records(self.root), expected)
        self.assertTrue(all(run.exists() for run in pinned))
        self.assertTrue((self.root / "run-archive" / "run-completed").exists())

    def test_maintenance_refuses_unknown_or_malformed_receipt_without_pruning(self):
        for number in (1, 2):
            wake.enqueue(self.root, event(number))
            claimed = wake.claim(self.root, f"run-{number}")
            self.complete_run(f"run-{number}", claimed["events"])
            wake.delivered(self.root, f"run-{number}")
        bad = self.root / "runs" / "run-2" / "receipt.json"
        good_receipt = json.loads(bad.read_text())
        before = (self.root / "events.json").read_bytes()
        cases = [{"schema": "unknown"}, {"status": "mystery"}, {"status": []},
                 {"event_count": True}, {"event_ids": []}, {"started_at": "bad"},
                 {"run_id": "different"}]
        for changes in cases:
            with self.subTest(changes=changes):
                bad.write_text(json.dumps(good_receipt | changes))
                with self.assertRaises(wake.Refusal):
                    wake.maintain(self.root)
                self.assertEqual((self.root / "events.json").read_bytes(), before)
                self.assertFalse((self.root / "event-archive").exists())
        bad.write_text(json.dumps(good_receipt))
        result_path = bad.with_name("result.json")
        result_path.write_text("{broken")
        with self.assertRaises(wake.Refusal):
            wake.maintain(self.root)
        self.assertEqual((self.root / "events.json").read_bytes(), before)
        self.assertFalse((self.root / "event-archive").exists())

    def test_maintenance_failed_publication_exact_retry(self):
        wake.enqueue(self.root, event())
        claimed = wake.claim(self.root, "run-retry")
        self.complete_run("run-retry", claimed["events"])
        wake.delivered(self.root, "run-retry")
        before = (self.root / "events.json").read_bytes()
        with patch.object(wake, "_write", side_effect=OSError("crash before prune")):
            with self.assertRaises(OSError):
                wake.maintain(self.root)
        self.assertEqual((self.root / "events.json").read_bytes(), before)
        self.assertEqual(wake.maintain(self.root)["archived_events"], 1)
        self.assertEqual(wake.lookup(self.root, "test", "1")["location"], "archive")
        self.assertEqual(wake.maintain(self.root)["archived_events"], 0)
        self.assertEqual(wake.enqueue(self.root, event())["claimed_run"], "run-retry")

    def test_maintenance_keeps_quota_receipts_today_and_across_midnight(self):
        import heartbeat
        now = dt.datetime(2026, 9, 26, 0, 30, tzinfo=dt.timezone.utc)
        for number, started in enumerate(("2026-09-26T00:05:00+00:00",
                                         "2026-09-25T23:50:00+00:00",
                                         "2026-09-24T12:00:00+00:00")):
            run = self.complete_run(f"run-quota-{number}", [])
            receipt_path = run / "receipt.json"
            receipt = json.loads(receipt_path.read_text())
            receipt.update(schema=heartbeat.RECEIPT_SCHEMA, cycle_class="interactive",
                           started_at=started)
            receipt_path.write_text(json.dumps(receipt))
        before = heartbeat.quota_counts(heartbeat.ledger(self.root)[0], now)
        real_datetime = dt.datetime
        class FixedNow(real_datetime):
            @classmethod
            def now(cls, tz=None):
                return now
        with patch.object(wake.dt, "datetime", FixedNow):
            self.assertEqual(wake.maintain(self.root)["relocated_runs"], 1)
        self.assertEqual(heartbeat.quota_counts(heartbeat.ledger(self.root)[0], now), before)
        self.assertEqual(before["interactive_today"], 1)
        self.assertEqual(before["interactive_last_hour"], 2)

    def test_maintenance_cli_contract(self):
        command = [sys.executable, wake.__file__, "--root", str(self.root)]
        result = subprocess.run([*command, "maintain"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["status"], "maintained")
        (self.root / "PAUSED").touch()
        result = subprocess.run([*command, "maintain"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), {"status": "skipped", "reason": "paused"})
        refused = subprocess.run([*command, "maintain", "--summary", "no"],
                                 capture_output=True, text=True)
        self.assertEqual(refused.returncode, 1)
        self.assertIn("Event flags", json.loads(refused.stderr)["error"])
        help_result = subprocess.run([*command, "--help"], capture_output=True, text=True)
        self.assertIn("maintain", help_result.stdout)
        self.assertIn("archive requires PAUSED", help_result.stdout)
        (self.root / "PAUSED").unlink()
        run = self.complete_run("run-invalid-status", [], old=True)
        receipt_path = run / "receipt.json"
        receipt = json.loads(receipt_path.read_text())
        receipt["status"] = "unknown"
        receipt_path.write_text(json.dumps(receipt))
        result = subprocess.run([*command, "maintain"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn("Unknown run receipt", json.loads(result.stderr)["error"])
        self.assertEqual(json.loads(receipt_path.read_text()), receipt)

    def test_cli_bounded_and_status_read_only(self):
        script = Path(wake.__file__)
        command = [sys.executable, str(script), "--root", str(self.root)]
        status = subprocess.run([*command, "status"], capture_output=True, text=True)
        self.assertEqual(status.returncode, 0, status.stderr)
        self.assertEqual(json.loads(status.stdout)["pending"], 0)
        self.assertEqual(list(self.root.iterdir()), [])
        sent = subprocess.run([*command, "enqueue"], input=json.dumps(event()),
                              capture_output=True, text=True)
        self.assertEqual(sent.returncode, 0, sent.stderr)
        self.assertEqual(json.loads(sent.stdout)["sequence"], 1)
        shell = subprocess.run([*command, "enqueue", "--summary", "build failed",
                                "--id", "build-1"], capture_output=True, text=True)
        self.assertEqual(shell.returncode, 0, shell.stderr)
        self.assertEqual(json.loads(shell.stdout)["event"]["source"], "local-shell")
        self.assertEqual(json.loads(shell.stdout)["event"]["event_id"], "build-1")
        refused = subprocess.run([*command, "enqueue"], input=" " * 70000,
                                 capture_output=True, text=True)
        self.assertEqual(refused.returncode, 1)


if __name__ == "__main__":
    unittest.main()

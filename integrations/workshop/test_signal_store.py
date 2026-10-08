"""Isolated ledger tests: no Signal account, model, network, or workshop runner."""

import os
from pathlib import Path
import sqlite3
import tempfile
import unittest
from contextlib import closing
import importlib.util
import json

from signal_store import CapacityError, ConflictError, Store, StoreError, upgrade


class StoreTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name) / "signal.sqlite"
        with Store(self.path, owner_id="owner-aci", create=True):
            pass

    def open(self):
        return Store(self.path, owner_id="owner-aci")

    def test_explicit_create_owner_and_version_refusal_no_write(self):
        absent = Path(self.tmp.name) / "absent.sqlite"
        with self.assertRaises(StoreError):
            Store(absent, owner_id="owner-aci")
        self.assertFalse(absent.exists())
        with self.assertRaises(StoreError):
            Store(self.path, owner_id="owner-aci", create=True)
        before = self.path.read_bytes()
        with self.assertRaises(StoreError):
            Store(self.path, owner_id="someone-else")
        self.assertEqual(before, self.path.read_bytes())
        with closing(sqlite3.connect(self.path)) as conn:
            conn.execute("PRAGMA user_version=99")
        before = self.path.read_bytes()
        with self.assertRaises(StoreError):
            Store(self.path, owner_id="owner-aci")
        self.assertEqual(before, self.path.read_bytes())
        self.assertFalse(Path(str(self.path) + "-journal").exists())

    def test_unknown_and_symlink_refuse(self):
        unknown = Path(self.tmp.name) / "unknown.sqlite"
        unknown.write_bytes(b"not sqlite")
        with self.assertRaises(StoreError):
            Store(unknown, owner_id="owner-aci")
        link = Path(self.tmp.name) / "link.sqlite"
        link.symlink_to(self.path)
        with self.assertRaises(StoreError):
            Store(link, owner_id="owner-aci")
        hardlink = Path(self.tmp.name) / "hardlink.sqlite"
        os.link(self.path, hardlink)
        with self.assertRaises(StoreError):
            Store(hardlink, owner_id="owner-aci")

    def test_unknown_schema_object_refuses_before_write(self):
        with closing(sqlite3.connect(self.path)) as conn:
            conn.execute("CREATE TRIGGER unexpected AFTER INSERT ON messages BEGIN SELECT 1; END")
        before = self.path.read_bytes()
        with self.assertRaises(StoreError):
            self.open()
        self.assertEqual(before, self.path.read_bytes())

    def test_duplicate_conflict_and_validation_do_not_drain(self):
        with self.open() as store:
            self.assertEqual(store.ingest("m1", 1, "first", 0)["generation"], 1)
            pending = store.prepare_burst(10)
            self.assertEqual(store.ingest("m1", 1, "first", 11)["duplicate"], True)
            with self.assertRaises(ConflictError):
                store.ingest("m1", 1, "changed", 11)
            with self.assertRaises(StoreError):
                store.ingest("m2", True, "bad timestamp", 11)
            with self.assertRaises(StoreError):
                store.ingest("m2", 2, "", 11)
            self.assertEqual(store.status()["generation"], 1)
            self.assertEqual(store.pending_bursts(), [pending])
            self.assertEqual(store.status()["unanswered"], 1)

    def test_six_texts_one_quiet_burst_and_publication_restart(self):
        with self.open() as store:
            for i in range(6):
                store.ingest(f"m{i}", i, f"text {i}", i)
            self.assertIsNone(store.prepare_burst(14.9))
            burst = store.prepare_burst(15)
            self.assertEqual(burst["message_ids"], [f"m{i}" for i in range(6)])
            self.assertEqual(burst["body"].count("Owner: "), 6)
            self.assertEqual(store.prepare_burst(16), burst)
        with self.open() as store:
            self.assertEqual(store.pending_bursts(), [burst])
            self.assertEqual(store.prepare_burst(20), burst)
            store.mark_enqueued(burst["event_id"])
            store.mark_enqueued(burst["event_id"])
            self.assertEqual(store.pending_bursts(), [])
            self.assertEqual(store.awaiting_replies(), [burst])
            self.assertIsNone(store.prepare_burst(21))

    def test_max_latency_across_stream(self):
        with self.open() as store:
            store.ingest("m1", 1, "first", 0)
            for i in range(1, 5):
                store.ingest(f"m{i+1}", i+1, "next", i * 10)
            self.assertIsNone(store.prepare_burst(44))
            burst = store.prepare_burst(45)
            self.assertEqual(len(burst["message_ids"]), 5)

    def test_stale_reply_preserves_all_unanswered(self):
        with self.open() as store:
            store.ingest("m1", 1, "old", 0)
            old = store.prepare_burst(10)
            store.mark_enqueued(old["event_id"])
            store.ingest("m2", 2, "new", 11)
            result = store.queue_reply(old["event_id"], "run1", "old answer")
            self.assertEqual(result["status"], "superseded")
            self.assertEqual(store.pending_outbox(), [])
            new = store.prepare_burst(21)
            self.assertEqual(new["message_ids"], ["m1", "m2"])
            self.assertIn("old", new["body"])
            self.assertIn("new", new["body"])
            self.assertEqual(store.status()["unanswered"], 2)

    def test_new_message_supersedes_queued_send(self):
        with self.open() as store:
            store.ingest("m1", 1, "one", 0)
            burst = store.prepare_burst(10)
            reply = store.queue_reply(burst["event_id"], "run1", "answer")
            self.assertEqual(len(store.pending_outbox()), 1)
            store.ingest("m2", 2, "two", 11)
            self.assertFalse(store.begin_send(reply["id"]))
            self.assertEqual(store.pending_outbox(), [])
            self.assertEqual(store.status()["unanswered"], 2)

    def test_send_restart_is_unknown_not_auto_retry(self):
        with self.open() as store:
            store.ingest("m1", 1, "one", 0)
            burst = store.prepare_burst(10)
            reply = store.queue_reply(burst["event_id"], "run1", "answer")
            self.assertTrue(store.begin_send(reply["id"]))
            self.assertFalse(store.begin_send(reply["id"]))
        with self.open() as store:
            self.assertEqual(store.recover_uncertain_sends(), 1)
            self.assertEqual(store.recover_uncertain_sends(), 0)
            self.assertEqual(store.pending_outbox(), [])
            self.assertEqual(store.status()["unknown_outbox"], 1)
            self.assertFalse(store.begin_send(reply["id"]))
            self.assertEqual(store.status()["unanswered"], 1)

    def test_new_arrival_during_external_rpc_stays_unanswered(self):
        with self.open() as store:
            store.ingest("m1", 1, "first", 0)
            burst = store.prepare_burst(10)
            reply = store.queue_reply(burst["event_id"], "run1", "answer")
            self.assertTrue(store.begin_send(reply["id"]))
            store.ingest("m2", 2, "arrived during RPC", 11)
            store.finish_send(reply["id"], "accepted")
            self.assertEqual(store.status()["unanswered"], 1)
            followup = store.prepare_burst(21)
            self.assertEqual(followup["message_ids"], ["m2"])

    def test_accepted_answers_own_generation_and_short_context(self):
        with self.open() as store:
            store.ingest("m1", 1, "original", 0)
            burst = store.prepare_burst(10)
            store.mark_enqueued(burst["event_id"])
            reply = store.queue_reply(burst["event_id"], "run1", "the reply")
            self.assertTrue(store.begin_send(reply["id"]))
            store.finish_send(reply["id"], "accepted", timestamp_ms=2000)
            self.assertEqual(store.status()["unanswered"], 0)
            self.assertIsNone(store.prepare_burst(20))
            store.ingest("m2", 2, "followup", 20)
            next_burst = store.prepare_burst(30)
            self.assertEqual(next_burst["message_ids"], ["m2"])
            self.assertIn("original", next_burst["body"])
            self.assertIn("the reply", next_burst["body"])
            self.assertIn("followup", next_burst["body"])

    def test_two_events_can_have_same_completed_run(self):
        with self.open() as store:
            store.ingest("m1", 1, "old", 0)
            first = store.prepare_burst(10)
            store.mark_enqueued(first["event_id"])
            store.ingest("m2", 2, "new", 11)
            second = store.prepare_burst(21)
            store.mark_enqueued(second["event_id"])
            stale = store.queue_reply(first["event_id"], "same-run", "old reply")
            current = store.queue_reply(second["event_id"], "same-run", "new reply")
            self.assertEqual(stale["status"], "superseded")
            self.assertEqual(current["status"], "pending")
            self.assertEqual(store.pending_outbox(), [current])

    def test_empty_completed_reply_terminal_and_uniqueness(self):
        with self.open() as store:
            store.ingest("m1", 1, "question", 0)
            burst = store.prepare_burst(10)
            empty = store.queue_reply(burst["event_id"], "run1", "")
            self.assertEqual(empty["status"], "empty")
            self.assertEqual(empty, store.queue_reply(burst["event_id"], "run1", ""))
            with self.assertRaises(ConflictError):
                store.queue_reply(burst["event_id"], "run1", "oops")
            with self.assertRaises(ConflictError):
                store.queue_reply(burst["event_id"], "run2", "different")
            self.assertEqual(store.pending_outbox(), [])
            self.assertEqual(store.status()["unanswered"], 1)

    def test_unanswered_body_overflow_held_without_empty_turn(self):
        with self.open() as store:
            store.ingest("m1", 1, "x" * 4000, 0)
            store.ingest("m2", 2, "y" * 4000, 1)
            with self.assertRaises(CapacityError):
                store.prepare_burst(11)
            self.assertEqual(store.pending_bursts(), [])
            self.assertEqual(store.status()["generation"], 2)
            self.assertEqual(store.status()["unanswered"], 2)
            self.assertTrue(store.status()["body_capacity_blocked"])

    def test_row_capacity_refuses_without_mutating_generation(self):
        import signal_store
        old = signal_store.MAX_MESSAGES
        signal_store.MAX_MESSAGES = 1
        try:
            with self.open() as store:
                store.ingest("m1", 1, "one", 0)
                with self.assertRaises(CapacityError):
                    store.ingest("m2", 2, "two", 1)
                self.assertEqual(store.status()["generation"], 1)
                self.assertEqual(store.status()["messages"], 1)
        finally:
            signal_store.MAX_MESSAGES = old

    def test_physical_page_cap_refuses_transaction_and_reopens(self):
        import signal_store
        old = signal_store.MAX_DB_BYTES
        signal_store.MAX_DB_BYTES = 64 * 1024
        accepted = 0
        try:
            with self.open() as store:
                for i in range(100):
                    try:
                        store.ingest(f"m{i}", i, "x" * 4000, i)
                    except CapacityError:
                        break
                    accepted += 1
                else:
                    self.fail("physical capacity did not refuse")
                self.assertEqual(store.status()["generation"], accepted)
                self.assertEqual(store.status()["messages"], accepted)
            self.assertLessEqual(self.path.stat().st_size, signal_store.MAX_DB_BYTES)
            with self.open() as store:
                self.assertEqual(store.status()["generation"], accepted)
        finally:
            signal_store.MAX_DB_BYTES = old

    def test_direct_replay_conflict_and_proactive_send(self):
        with self.open() as store:
            queued = store.queue_direct("intent-1", "hi")
            self.assertEqual(queued["status"], "pending")
            self.assertIsNone(queued["generation"])
            self.assertEqual(store.queue_direct("intent-1", "hi"), queued)
            with self.assertRaises(ConflictError):
                store.queue_direct("intent-1", "different")
            with self.assertRaises(StoreError):
                store.queue_direct("intent-2", "")
            with self.assertRaises(StoreError):
                store.queue_direct("intent-2", "x" * 4001)
            self.assertTrue(store.begin_direct_send("intent-1"))
            self.assertFalse(store.begin_direct_send("intent-1"))
            store.finish_direct_send("intent-1", "accepted", 123)
            self.assertEqual(store.get_direct("intent-1")["timestamp_ms"], 123)
            self.assertEqual(store.queue_direct("intent-1", "hi")["status"], "accepted")
            self.assertFalse(store.begin_direct_send("intent-1"))
            with self.assertRaises(StoreError):
                store.get_direct("missing")

    def test_bound_direct_suppresses_legacy_and_answers_own_generation(self):
        with self.open() as store:
            store.ingest("m1", 1, "question", 0)
            burst = store.prepare_burst(10)
            store.mark_enqueued(burst["event_id"])
            old = store.queue_reply(burst["event_id"], "run1", "legacy")
            self.assertEqual(old["status"], "pending")
            direct = store.queue_direct("intent-1", "direct answer", burst["event_id"])
            self.assertEqual(direct["generation"], 1)
            self.assertEqual(store.pending_outbox(), [])
            self.assertEqual(store.awaiting_replies(), [])
            self.assertTrue(store.begin_direct_send("intent-1"))
            store.ingest("m2", 2, "during RPC", 11)
            store.finish_direct_send("intent-1", "accepted", 123)
            self.assertEqual(store.status()["unanswered"], 1)
            followup = store.prepare_burst(21)
            self.assertEqual(followup["message_ids"], ["m2"])
            self.assertIn("direct answer", followup["body"])
            self.assertNotIn("legacy", followup["body"])
            second = store.queue_direct("intent-2", "another", burst["event_id"])
            self.assertEqual(second["status"], "superseded")
            self.assertEqual(store.queue_direct("intent-2", "another", burst["event_id"]), second)

    def test_bound_direct_generation_fence_and_crash_uncertainty(self):
        with self.open() as store:
            store.ingest("m1", 1, "question", 0)
            burst = store.prepare_burst(10)
            store.mark_enqueued(burst["event_id"])
            store.queue_direct("stale", "answer", burst["event_id"])
            store.ingest("m2", 2, "new", 11)
            self.assertFalse(store.begin_direct_send("stale"))
            self.assertEqual(store.get_direct("stale")["status"], "superseded")
            store.queue_direct("proactive", "unbound")
            self.assertTrue(store.begin_direct_send("proactive"))
        with self.open() as store:
            self.assertEqual(store.recover_uncertain_sends(), 1)
            self.assertEqual(store.recover_uncertain_sends(), 0)
            self.assertEqual(store.get_direct("proactive")["status"], "unknown")
            self.assertFalse(store.begin_direct_send("proactive"))
            self.assertEqual(store.awaiting_replies(), [])

    def test_multiple_explicit_sends_per_event_and_failed_attempt_no_fallback(self):
        with self.open() as store:
            store.ingest("m1", 1, "question", 0)
            burst = store.prepare_burst(10)
            store.mark_enqueued(burst["event_id"])
            self.assertEqual(len(store.awaiting_replies()), 1)
            for request_id in ("first", "second"):
                store.queue_direct(request_id, request_id, burst["event_id"])
                self.assertTrue(store.begin_direct_send(request_id))
                store.finish_direct_send(request_id, "failed")
            self.assertEqual(store.status()["unanswered"], 1)
            self.assertEqual(store.awaiting_replies(), [])
            self.assertEqual(store.get_direct("first")["status"], "failed")
            self.assertEqual(store.get_direct("second")["status"], "failed")

    def test_direct_bounds(self):
        import signal_store
        prior = signal_store.MAX_DIRECT_OUTBOX
        signal_store.MAX_DIRECT_OUTBOX = 1
        try:
            with self.open() as store:
                store.queue_direct("one", "a")
                with self.assertRaises(CapacityError):
                    store.queue_direct("two", "b")
                self.assertEqual(store.status()["direct_outbox"], 1)
        finally:
            signal_store.MAX_DIRECT_OUTBOX = prior

    def test_read_history_three_origins_and_delivery_uncertainty(self):
        with self.open() as store:
            store.ingest("m1", 100, "hello", 0)
            burst = store.prepare_burst(10)
            legacy = store.queue_reply(burst["event_id"], "run1", "legacy answer")
            self.assertTrue(store.begin_send(legacy["id"]))
            store.finish_send(legacy["id"], "accepted", timestamp_ms=200)
            store.queue_direct("proactive", "proactive answer")
            self.assertTrue(store.begin_direct_send("proactive"))
            store.finish_direct_send("proactive", "accepted", timestamp_ms=300)
            store.queue_direct("uncertain", "maybe delivered")
            self.assertTrue(store.begin_direct_send("uncertain"))
            store.finish_direct_send("uncertain", "unknown")
            store.queue_direct("failed", "not delivered")
            self.assertTrue(store.begin_direct_send("failed"))
            store.finish_direct_send("failed", "failed")
            store.queue_direct("pending", "not attempted")
            before = (store.status(), store.conn.total_changes)
            history = store.read_history()
            self.assertEqual((store.status(), store.conn.total_changes), before)
            self.assertEqual([(item["id"], item["timestamp_ms"]) for item in history["entries"]],
                             [("m1", 100), (legacy["id"], 200), ("proactive", 300)])
            self.assertEqual([item["id"] for item in history["uncertain"]], ["uncertain"])
            self.assertEqual(history["uncertain"][0]["reason"], "delivery_unknown")
            self.assertFalse(history["truncated"])
            self.assertEqual([item["id"] for item in store.read_history(1)["entries"]],
                             ["proactive"])
            self.assertTrue(store.read_history(1)["truncated"])

    def test_read_history_untimed_accepted_and_utf8_budget(self):
        with self.open() as store:
            store.queue_direct("untimed", "accepted without transport timestamp")
            self.assertTrue(store.begin_direct_send("untimed"))
            store.finish_direct_send("untimed", "accepted")
            store.ingest("large", 10, "🦋" * 1000, 0)
            store.ingest("small", 20, "latest", 1)
            history = store.read_history()
            self.assertEqual([item["id"] for item in history["entries"]], ["small"])
            self.assertEqual(history["uncertain"][0]["id"], "untimed")
            self.assertEqual(history["uncertain"][0]["status"], "accepted")
            self.assertEqual(history["uncertain"][0]["reason"], "timestamp_unavailable")
            self.assertTrue(history["truncated"])
            self.assertLessEqual(len(json.dumps(history, ensure_ascii=False,
                                                separators=(",", ":")).encode("utf-8")), 4096)
            store.ingest("newest-oversize", 30, "🦋" * 1000, 2)
            newest = store.read_history(1)
            self.assertEqual(newest["entries"], [])
            self.assertTrue(newest["truncated"])
            for invalid in (0, 21, True, 1.0, "12"):
                with self.assertRaises(StoreError):
                    store.read_history(invalid)

    def test_explicit_v1_upgrade_and_real_old_reader_refusal(self):
        source = Path(os.environ.get("MNEME_SIGNAL_V1_ARTIFACT") or
                      (Path(__file__).resolve().parent / "fixtures/signal_store_v1.py"))
        self.assertTrue(source.is_file(), f"genuine v1 artifact unavailable: {source}")
        spec = importlib.util.spec_from_file_location("genuine_signal_v1", source)
        old = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(old)
        legacy_path = Path(self.tmp.name) / "legacy.sqlite"
        with old.Store(legacy_path, owner_id="owner-aci", create=True) as legacy:
            legacy.ingest("m1", 1, "retained", 0)
            burst = legacy.prepare_burst(10)
            legacy.mark_enqueued(burst["event_id"])
            legacy.queue_reply(burst["event_id"], "run1", "retained reply")
            before = legacy.status()
        with self.assertRaises(StoreError):
            Store(legacy_path, owner_id="owner-aci")
        self.assertTrue(upgrade(legacy_path, owner_id="owner-aci"))
        self.assertFalse(upgrade(legacy_path, owner_id="owner-aci"))
        with Store(legacy_path, owner_id="owner-aci") as current:
            after = current.status()
            for key in ("messages", "bursts", "outbox", "generation", "unanswered"):
                self.assertEqual(after[key], before[key])
            self.assertEqual(current.pending_outbox()[0]["text"], "retained reply")
            self.assertEqual(after["direct_outbox"], 0)
        data = legacy_path.read_bytes()
        with self.assertRaises(old.StoreError):
            old.Store(legacy_path, owner_id="owner-aci")
        self.assertEqual(legacy_path.read_bytes(), data)


if __name__ == "__main__":
    unittest.main()

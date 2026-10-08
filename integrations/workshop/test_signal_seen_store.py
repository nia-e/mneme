"""Exact seen receipts and deliberate ledger upgrades; no account or live state."""

import hashlib
import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

import signal_store
from signal_store import Store, StoreError, upgrade, upgrade_version


def prior(version):
    source = Path(__file__).parent / "fixtures" / f"signal_store_v{version}.py"
    if version == 2:
        assert hashlib.sha256(source.read_bytes()).hexdigest() == (
            "318f235248f1b1809876afaaa9693a6bf6f7b9bb5ff21e19edbffd1f28655c76")
    spec = importlib.util.spec_from_file_location(f"representative_seen_prior_v{version}", source)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class SeenTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "signal.sqlite"
        self.store = Store(self.path, owner_id="owner", create=True)
        self.addCleanup(self.store.close)

    def test_exact_seen_is_idempotent_and_independent_of_answered_and_history(self):
        store = self.store
        store.ingest("a", 9000, "arrived first with later clock", 0)
        store.ingest("b", 1000, "arrived later with earlier clock", 1)
        history = store.read_history(1)
        before = (store.conn.total_changes, store.status())
        self.assertEqual(store.read_history(1), history)
        self.assertEqual((store.conn.total_changes, store.status()), before)
        ids = [item["id"] for item in history["entries"] if item["direction"] == "incoming"]
        self.assertEqual(ids, ["a"])
        self.assertEqual(store.mark_seen(ids), 1)
        self.assertEqual(store.mark_seen(ids + ids), 0)
        self.assertEqual(store.status()["seen"], 1)
        self.assertEqual(store.status()["unanswered"], 2)
        self.assertEqual(store.read_history(1), history)
        self.assertEqual(store.prepare_burst(11)["message_ids"], ["b"])
        self.assertEqual(store.conn.execute("SELECT answered_event_id FROM messages").fetchall(), [(None,), (None,)])

    def test_seen_receipt_validation_atomic_unknown_and_bounded(self):
        self.store.ingest("a", 1, "first", 0)
        for invalid in ("a", ("a",), None, ["a", "unknown"], ["a", False], ["a"] * 21):
            with self.subTest(invalid=invalid), self.assertRaises(StoreError):
                self.store.mark_seen(invalid)
            self.assertEqual(self.store.status()["seen"], 0)
        self.assertEqual(self.store.mark_seen([]), 0)
        self.assertEqual(self.store.mark_seen(["a"]), 1)

    def test_cumulative_overlap_and_immutable_payload(self):
        store = self.store
        store.ingest("a", 1, "first", 0)
        first = store.prepare_burst(10)
        store.mark_enqueued(first["event_id"])
        store.ingest("b", 2, "second", 11)
        second = store.prepare_burst(21)
        self.assertEqual(second["message_ids"], ["a", "b"])
        raw = store.conn.execute("SELECT * FROM bursts ORDER BY generation").fetchall()
        store.mark_seen(["b"])
        self.assertEqual(store.fully_seen_bursts(), [])
        self.assertEqual(store.pending_bursts(), [second])
        self.assertEqual(store.prepare_burst(30), second)
        store.mark_seen(["a"])
        self.assertEqual(store.fully_seen_bursts(), [first, second])
        self.assertEqual(store.pending_bursts(), [])
        self.assertIsNone(store.prepare_burst(31))
        self.assertEqual(store.status()["pending_bursts"], 0)
        self.assertEqual(store.status()["unanswered"], 2)
        self.assertEqual(store.conn.execute("SELECT * FROM bursts ORDER BY generation").fetchall(), raw)

    def test_read_before_quiet_window_does_not_make_later_wake(self):
        store = self.store
        store.ingest("a", 1, "hello", 0)
        store.mark_seen(["a"])
        self.assertIsNone(store.prepare_burst(100))
        self.assertEqual(store.status()["bursts"], 0)
        store.ingest("b", 2, "next", 101)
        self.assertIsNone(store.prepare_burst(110))
        self.assertEqual(store.prepare_burst(111)["message_ids"], ["b"])

    def test_seen_arrivals_do_not_extend_quiet_window_for_unseen(self):
        store = self.store
        store.ingest("a", 1, "unseen", 0)
        store.ingest("b", 2, "seen later", 9)
        store.mark_seen(["b"])
        self.assertEqual(store.prepare_burst(10)["message_ids"], ["a"])

    def test_seen_prebuilt_burst_remains_skipped_after_reopen(self):
        store = self.store
        store.ingest("a", 1, "hello", 0)
        burst = store.prepare_burst(10)
        store.mark_seen(["a"])
        store.close()
        with Store(self.path, owner_id="owner") as reopened:
            self.assertEqual(reopened.fully_seen_bursts(), [burst])
            self.assertEqual(reopened.pending_bursts(), [])
            self.assertIsNone(reopened.prepare_burst(100))
            self.assertEqual(reopened.status()["generation"], 1)
            self.assertEqual(reopened.status()["unanswered"], 1)
            self.assertEqual(reopened.ingest("a", 1, "hello", 100)["duplicate"], True)

    def test_seen_unanswered_backlog_does_not_exhaust_new_burst_body(self):
        self.store.ingest("a", 1, "a" * 4000, 0)
        self.store.ingest("b", 2, "b" * 4000, 1)
        self.assertTrue(self.store.status()["body_capacity_blocked"])
        self.store.mark_seen(["a", "b"])
        self.store.ingest("c", 3, "fresh", 2)
        self.assertFalse(self.store.status()["body_capacity_blocked"])
        self.assertEqual(self.store.prepare_burst(12)["body"], "Owner: fresh")
        self.assertEqual(self.store.status()["unanswered"], 3)

    def test_bounded_history_outgoing_competition_and_whole_entry_omission(self):
        store = self.store
        store.ingest("large", 10, "🦋" * 1000, 0)
        store.ingest("small", 20, "latest incoming", 1)
        store.queue_direct("outgoing", "sent")
        store.begin_direct_send("outgoing")
        store.finish_direct_send("outgoing", "accepted", 30)
        newest = store.read_history(1)
        self.assertEqual(newest["entries"][0]["direction"], "outgoing")
        store.mark_seen([item["id"] for item in newest["entries"] if item["direction"] == "incoming"])
        self.assertEqual(store.status()["seen"], 0)
        history = store.read_history(20)
        ids = [item["id"] for item in history["entries"] if item["direction"] == "incoming"]
        self.assertEqual(ids, ["small"])
        self.assertTrue(history["truncated"])
        self.assertLessEqual(len(json.dumps(history, ensure_ascii=False, separators=(",", ":")).encode()), 4096)
        store.mark_seen(ids)
        self.assertEqual(store.status()["unseen"], 1)
        self.assertEqual(store.prepare_burst(11)["message_ids"], ["large"])

    def test_mark_seen_does_not_dispose_pending_reply(self):
        store = self.store
        store.ingest("a", 1, "question", 0)
        burst = store.prepare_burst(10)
        store.mark_enqueued(burst["event_id"])
        reply = store.queue_reply(burst["event_id"], "run", "answer")
        store.mark_seen(["a"])
        self.assertEqual(store.pending_outbox(), [reply])
        self.assertEqual(store.status()["unanswered"], 1)
        self.assertTrue(store.begin_send(reply["id"]))
        store.finish_send(reply["id"], "accepted", timestamp_ms=2)
        self.assertEqual(store.status()["unanswered"], 0)
        self.assertEqual(store.status()["seen"], 1)

    def test_write_rechecks_format_inside_transaction(self):
        self.store.ingest("a", 1, "hello", 0)
        with sqlite3.connect(self.path) as other:
            other.execute("PRAGMA user_version=99")
        before = self.path.read_bytes()
        with self.assertRaises(StoreError):
            self.store.mark_seen(["a"])
        self.assertEqual(self.path.read_bytes(), before)


class UpgradeSeenTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "signal.sqlite"

    def create_prior(self, version):
        old = prior(version)
        with old.Store(self.path, owner_id="owner", create=True) as store:
            store.ingest("a", 1, "keep question", 0)
            burst = store.prepare_burst(10)
            store.mark_enqueued(burst["event_id"])
            store.queue_reply(burst["event_id"], "run", "keep pending reply")
            if version == 2:
                store.queue_direct("unbound", "keep direct reply")
            before = store.status()
        return old, before

    def test_genuine_prior_upgrade_preserves_rows_old_reader_writer_refuses(self):
        for version in (1, 2):
            with self.subTest(version=version):
                if self.path.exists():
                    self.path.unlink()
                old, before = self.create_prior(version)
                raw = self.path.read_bytes()
                self.assertEqual(upgrade_version(self.path, owner_id="owner"), version)
                with self.assertRaises(StoreError):
                    Store(self.path, owner_id="owner")
                self.assertEqual(self.path.read_bytes(), raw)
                self.assertTrue(upgrade(self.path, owner_id="owner"))
                self.assertEqual(upgrade_version(self.path, owner_id="owner"), 3)
                after_bytes = self.path.read_bytes()
                self.assertFalse(upgrade(self.path, owner_id="owner"))
                self.assertEqual(self.path.read_bytes(), after_bytes)
                # Genuine old admission refuses the complete new schema before
                # it can run an old writer. The bridge lease also spans Store
                # lifetime, so an upgrader must wait for existing owners to exit.
                with self.assertRaises(old.StoreError):
                    with old.Store(self.path, owner_id="owner") as stale:
                        stale.ingest("stale", 2, "must not write", 11)
                self.assertEqual(self.path.read_bytes(), after_bytes)
                with Store(self.path, owner_id="owner") as store:
                    after = store.status()
                    for key in ("messages", "bursts", "outbox", "generation", "unanswered"):
                        self.assertEqual(after[key], before[key])
                    self.assertEqual(after["seen"], 0)
                    self.assertEqual(after["unseen"], 1)
                    self.assertEqual(store.pending_outbox()[0]["text"], "keep pending reply")
                    if version == 2:
                        self.assertEqual(store.get_direct("unbound")["text"], "keep direct reply")
                    store.mark_seen(["a"])
                    self.assertEqual(store.status()["seen"], 1)

    def test_unknown_or_owner_mismatch_refuses_without_side_effect(self):
        self.create_prior(2)
        before = self.path.read_bytes()
        for function in (upgrade_version, upgrade):
            with self.assertRaises(StoreError):
                function(self.path, owner_id="wrong")
            self.assertEqual(self.path.read_bytes(), before)
        with sqlite3.connect(self.path) as conn:
            conn.execute("PRAGMA user_version=99")
        before = self.path.read_bytes()
        for function in (upgrade_version, upgrade):
            with self.assertRaises(StoreError):
                function(self.path, owner_id="owner")
            self.assertEqual(self.path.read_bytes(), before)

    def test_upgrade_rollback_each_sqlite_boundary_and_commit_lost_ack(self):
        cutpoints = ("BEGIN IMMEDIATE", "ALTER TABLE messages", "UPDATE metadata", "PRAGMA user_version=3", "before COMMIT", "COMMIT")
        for cutpoint in cutpoints:
            with self.subTest(cutpoint=cutpoint):
                if self.path.exists():
                    self.path.unlink()
                old, _ = self.create_prior(2)
                before = self.path.read_bytes()
                fired = []

                class CutConnection(sqlite3.Connection):
                    def execute(self, sql, parameters=()):
                        if cutpoint == "before COMMIT" and sql == "COMMIT" and not fired:
                            fired.append(True)
                            raise RuntimeError("injected before commit")
                        result = super().execute(sql, parameters)
                        if sql.startswith(cutpoint) and not fired:
                            fired.append(True)
                            raise RuntimeError("injected after boundary")
                        return result

                def connect(store, mode):
                    return sqlite3.connect(store._uri(mode), uri=True, isolation_level=None,
                                           timeout=0, factory=CutConnection)

                with patch.object(Store, "_connect", connect), self.assertRaises(RuntimeError):
                    upgrade(self.path, owner_id="owner")
                self.assertEqual(fired, [True])
                if cutpoint == "COMMIT":
                    self.assertEqual(upgrade_version(self.path, owner_id="owner"), 3)
                    self.assertFalse(upgrade(self.path, owner_id="owner"))
                else:
                    self.assertEqual(self.path.read_bytes(), before)
                    self.assertEqual(upgrade_version(self.path, owner_id="owner"), 2)
                    with old.Store(self.path, owner_id="owner") as valid_old:
                        self.assertEqual(valid_old.status()["messages"], 1)
                    self.assertTrue(upgrade(self.path, owner_id="owner"))
                with Store(self.path, owner_id="owner") as current:
                    self.assertEqual(current.status()["unseen"], 1)
                    self.assertEqual(current.status()["seen"], 0)


if __name__ == "__main__":
    unittest.main()

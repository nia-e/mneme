"""Disposable persistent accounting tests; no provider or enrolled state access."""
from __future__ import annotations

from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import tempfile
from types import SimpleNamespace
import unittest

import packet_accounting as accounting


def prompt_digest(prompt):
    return hashlib.sha256(prompt.encode()).hexdigest()


def result(input_tokens=10, output_tokens=2, *, known=True):
    return {"provider_attempt": True,
            "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens,
                      "cached_input_tokens": 3, "reasoning_output_tokens": 1} if known else None}


class PacketAccountingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.path = self.root / "reader.accounting.sqlite3"
        self.budget = SimpleNamespace(attempts=2, input_tokens=100, output_tokens=20)
        self.ledger = None
        self.addCleanup(self.close)

    def open(self, *, path=None, host="codex", session="s1", data=None, legacy_raw=None, fresh=False):
        self.close()
        self.ledger = accounting.Ledger(path or self.path, host, session,
            accounting.empty_usage() if data is None else data, legacy_raw=legacy_raw, fresh=fresh)
        return self.ledger

    def close(self):
        if self.ledger is not None:
            self.ledger.close()
            self.ledger = None

    def fresh(self):
        return self.open(fresh=True)

    def bind(self, kind="user_message", turn="t1", prompt="first task"):
        return self.ledger.bind(kind, turn, prompt_digest(prompt))[0]

    def test_identity_binding_survives_more_than_128_new_packets_and_reopen(self):
        ledger = self.fresh()
        first, created = ledger.bind("user_message", "t1", prompt_digest("first task"))
        self.assertTrue(created)
        self.assertEqual(ledger.bind("user_message", "t1", prompt_digest("first task")), (first, False))
        for index in range(2, 260):
            ledger.bind("user_message", f"t{index}", prompt_digest(str(index)))
        ledger.commit()
        ledger = self.open()
        self.assertEqual(ledger.bind("user_message", "t1", prompt_digest("first task")), (first, False))
        with self.assertRaisesRegex(ValueError, "packet_prompt_changed"):
            ledger.bind("user_message", "t1", prompt_digest("changed task"))
        self.assertEqual(ledger.usage(first), accounting.empty_usage())

    def test_orphan_sqlite_sidecar_refuses_fresh_creation(self):
        for suffix in ("-journal", "-wal", "-shm"):
            with self.subTest(suffix=suffix):
                path = self.root / (suffix + ".sqlite3")
                sidecar = Path(str(path) + suffix)
                sidecar.write_bytes(b"unknown partial state")
                with self.assertRaisesRegex(ValueError, "sidecar_without_database"):
                    self.open(path=path, fresh=True)
                self.assertFalse(path.exists())
                self.assertEqual(sidecar.read_bytes(), b"unknown partial state")

    def test_settlement_receipt_retains_only_bounded_usage_fields(self):
        ledger = self.fresh()
        packet = self.bind()
        self.assertTrue(ledger.reserve(packet, "bounded", {}, self.budget))
        value = result()
        value["usage"].update(unrelated="not accounting" * 1000, cached_input_tokens={"bad": "type"})
        self.assertTrue(ledger.settle("bounded", value))
        receipt = ledger.db.execute("SELECT receipt FROM calls WHERE id='bounded'").fetchone()[0]
        self.assertLess(len(receipt), 200)
        self.assertNotIn("unrelated", receipt)
        self.assertIsNone(ledger.usage(packet)["cached_input_tokens"])

    def test_event_kind_is_independent_but_digest_cannot_split_an_existing_kind(self):
        ledger = self.fresh()
        user = self.bind()
        ended = self.bind("turn_end")
        self.assertNotEqual(user, ended)
        self.assertEqual(ledger.latest_end(), ended)
        with self.assertRaisesRegex(ValueError, "packet_prompt_changed"):
            ledger.bind("turn_end", "t1", prompt_digest("changed"))
        later = self.bind("turn_end", "t2")
        self.assertEqual(ledger.latest_end(), later)
        # An old replay is not a new idle-maintenance packet.
        self.assertEqual(self.bind("turn_end"), ended)
        self.assertEqual(ledger.latest_end(), later)

    def test_exhausted_user_packet_does_not_exhaust_its_end_or_next_user_packet(self):
        ledger = self.fresh()
        user = self.bind()
        for index in range(self.budget.attempts):
            self.assertTrue(ledger.reserve(user, f"call-{index}", {"turn": "t1"}, self.budget))
            self.assertTrue(ledger.settle(f"call-{index}", result()))
        self.assertFalse(ledger.allowed(user, self.budget))
        self.assertFalse(ledger.reserve(user, "excess", {}, self.budget))
        ended, next_user = self.bind("turn_end"), self.bind("user_message", "t2")
        self.assertTrue(ledger.allowed(ended, self.budget))
        self.assertTrue(ledger.allowed(next_user, self.budget))
        mirrored = {}
        ledger.mirror(mirrored)
        self.assertEqual(mirrored["attempts"], self.budget.attempts)
        self.assertEqual(mirrored["input_tokens"], 20)
        ledger.commit()
        ledger = self.open()
        self.assertFalse(ledger.allowed(user, self.budget))
        self.assertTrue(ledger.allowed(ended, self.budget))
        self.assertTrue(ledger.allowed(next_user, self.budget))

    def test_all_packet_token_caps_and_optional_room_are_pre_call_gates(self):
        for field, amount in (("input_tokens", 100), ("output_tokens", 20)):
            with self.subTest(field=field):
                ledger = self.open(path=self.root / f"{field}.sqlite3", fresh=True)
                packet = self.bind()
                self.assertTrue(ledger.reserve(packet, "one", {}, self.budget))
                usage = result(input_tokens=amount if field == "input_tokens" else 10,
                               output_tokens=amount if field == "output_tokens" else 2)
                self.assertTrue(ledger.settle("one", usage))
                self.assertFalse(ledger.allowed(packet, self.budget))
                self.assertFalse(ledger.reserve(packet, "two", {}, self.budget))
        ledger = self.open(path=self.root / "room.sqlite3", fresh=True)
        packet = self.bind()
        self.assertFalse(ledger.reserve(packet, "too-many", {}, self.budget, calls=3))
        self.assertFalse(ledger.reserve(packet, "no-input-room", {}, self.budget, input_room=100))
        self.assertFalse(ledger.reserve(packet, "no-output-room", {}, self.budget, output_room=20))
        self.assertEqual(ledger.usage(packet)["attempts"], 0)

    def test_routing_and_selector_share_user_packet_and_single_reservation(self):
        ledger = self.fresh()
        user, ended = self.bind(), self.bind("turn_end")
        self.assertTrue(ledger.reserve(user, "routing", {"routing": "t1"}, self.budget,
                                       stage="routing", calls=2))
        self.assertFalse(ledger.reserve(ended, "recording", {}, self.budget))
        self.assertTrue(ledger.settle("routing", result(12, 3)))
        self.assertFalse(ledger.reserve(user, "routing-again", {}, self.budget, stage="routing", calls=2))
        self.assertTrue(ledger.reserve(user, "selector", {"turn": "t1"}, self.budget))
        self.assertTrue(ledger.settle("selector", result(10, 2)))
        mirrored = {}
        ledger.mirror(mirrored)
        self.assertEqual(mirrored["attempts"], 2)
        self.assertEqual(mirrored["routing_usage"]["attempts"], 1)
        self.assertEqual(mirrored["routing_usage"]["input_tokens"], 12)
        self.assertEqual(ledger.usage(user)["input_tokens"], 22)
        self.assertFalse(ledger.allowed(user, self.budget))
        self.assertTrue(ledger.allowed(ended, self.budget))

    def test_unknown_usage_poison_is_packet_local_but_aggregate_stays_sticky(self):
        ledger = self.fresh()
        original = self.bind()
        self.assertTrue(ledger.reserve(original, "unknown", {}, self.budget, stage="routing"))
        self.assertTrue(ledger.settle("unknown", result(known=False)))
        self.assertFalse(ledger.allowed(original, self.budget))
        later = self.bind("user_message", "t2")
        self.assertTrue(ledger.reserve(later, "known", {}, self.budget))
        self.assertTrue(ledger.settle("known", result()))
        mirrored = {}
        ledger.mirror(mirrored)
        self.assertTrue(mirrored["unknown_usage"])
        self.assertTrue(mirrored["routing_usage"]["unknown_usage"])
        self.assertIsNone(mirrored["cached_input_tokens"])
        self.assertIsNone(mirrored["reasoning_output_tokens"])
        self.assertFalse(ledger.usage(later)["unknown_usage"])
        self.assertTrue(ledger.allowed(later, self.budget))

    def test_late_settlement_charges_original_packet_without_clearing_new_reservation(self):
        ledger = self.fresh()
        original = self.bind()
        self.assertTrue(ledger.reserve(original, "old-call", {"turn": "t1"}, self.budget))
        self.assertTrue(ledger.abandon())
        self.assertFalse(ledger.abandon())
        later = self.bind("user_message", "t2")
        self.assertTrue(ledger.reserve(later, "new-call", {"turn": "t2"}, self.budget))
        self.assertTrue(ledger.settle("old-call", result(12, 3)))
        self.assertEqual(ledger.usage(original)["input_tokens"], 12)
        self.assertTrue(ledger.usage(original)["unknown_usage"])
        self.assertEqual(ledger.usage(later)["input_tokens"], 0)
        mirrored = {}
        ledger.mirror(mirrored)
        self.assertEqual(mirrored["reservation"], {"turn": "t2"})
        self.assertEqual(mirrored["reservation_call_id"], "new-call")
        self.assertTrue(ledger.settle("new-call", result(7, 2)))
        ledger.commit()
        ledger = self.open()
        self.assertTrue(ledger.settle("old-call", result(12, 3)))
        self.assertTrue(ledger.settle("new-call", result(7, 2)))
        self.assertEqual(ledger.usage(original)["input_tokens"], 12)
        self.assertEqual(ledger.usage(later)["input_tokens"], 7)
        ledger.mirror(mirrored)
        self.assertEqual(mirrored["input_tokens"], 19)
        self.assertEqual(mirrored["attempts"], 2)

    def test_durable_receipt_rejects_conflicting_duplicate_and_reused_call_id(self):
        ledger = self.fresh()
        packet = self.bind()
        self.assertTrue(ledger.reserve(packet, "call", {}, self.budget))
        self.assertTrue(ledger.settle("call", result()))
        ledger.commit()
        ledger = self.open()
        self.assertTrue(ledger.settle("call", result()))
        self.assertFalse(ledger.settle("call", result(11, 2)))
        self.assertFalse(ledger.reserve(packet, "call", {}, self.budget))
        self.assertFalse(ledger.settle("not-a-call", result()))
        self.assertEqual(ledger.usage(packet)["input_tokens"], 10)
        self.assertEqual(ledger.usage(packet)["attempts"], 1)

    def test_cancelled_before_provider_still_consumes_reserved_attempt(self):
        ledger = self.fresh()
        packet = self.bind()
        self.assertTrue(ledger.reserve(packet, "cancelled", {}, self.budget))
        self.assertTrue(ledger.settle("cancelled", {"provider_attempt": False}))
        self.assertEqual(ledger.usage(packet), {**accounting.empty_usage(), "attempts": 1})
        self.assertFalse(ledger.settle("cancelled", result()))

    def test_json_mirror_is_one_way_and_does_not_restore_delivery_state(self):
        ledger = self.fresh()
        packet = self.bind()
        self.assertTrue(ledger.reserve(packet, "call", {"turn": "t1"}, self.budget))
        self.assertTrue(ledger.settle("call", result()))
        ledger.commit()
        # A stale JSON snapshot is diagnostic input, never an accounting reset.
        old_json = {**accounting.empty_usage(), "pending": {"turn": "new"}, "ready": None,
                    "fence": "new-fence", "routing_usage": {"attempts": 1000}}
        ledger = self.open(data=old_json)
        ledger.mirror(old_json)
        self.assertEqual(old_json["attempts"], 1)
        self.assertEqual(old_json["input_tokens"], 10)
        self.assertEqual(old_json["pending"], {"turn": "new"})
        self.assertEqual(old_json["fence"], "new-fence")
        self.assertIsNone(old_json["ready"])
        self.assertNotIn("routing_usage", old_json)
        self.assertIsNone(old_json["reservation"])

    def test_transaction_rollback_and_commit_cut_preserve_reservation_identity(self):
        ledger = self.fresh()
        packet = self.bind()
        ledger.commit()
        ledger = self.open()
        self.assertTrue(ledger.reserve(packet, "rolled-back", {}, self.budget))
        ledger = self.open()  # close without commit: no paid admission survived
        self.assertEqual(ledger.usage(packet)["attempts"], 0)
        self.assertTrue(ledger.reserve(packet, "committed", {"turn": "t1"}, self.budget))
        ledger.commit()
        ledger = self.open()
        mirrored = {}
        ledger.mirror(mirrored)
        self.assertEqual(mirrored["reservation_call_id"], "committed")
        self.assertEqual(mirrored["attempts"], 1)
        self.assertFalse(ledger.reserve(packet, "after-restart", {}, self.budget))

    def test_legacy_exact_bytes_and_aggregate_baseline_are_preserved_on_retry(self):
        legacy = {**accounting.empty_usage(), "attempts": 37, "input_tokens": 3400,
                  "output_tokens": 290, "unknown_usage": True,
                  "cached_input_tokens": None, "reasoning_output_tokens": None,
                  "schema": "mneme.codex-reader.state.v2",
                  "routing_usage": {**accounting.empty_usage(), "attempts": 4, "input_tokens": 330}}
        raw = (json.dumps(legacy, indent=2) + "\n").encode()
        ledger = self.open(data=legacy, legacy_raw=raw)
        ledger.commit()
        ledger = self.open(data=legacy, legacy_raw=raw)
        stored = ledger.db.execute("SELECT legacy_hash,legacy_raw FROM meta WHERE id=1").fetchone()
        self.assertEqual(stored, (hashlib.sha256(raw).hexdigest(), raw))
        mirrored = {}
        ledger.mirror(mirrored)
        for key in accounting.FIELDS:
            self.assertEqual(mirrored[key], legacy[key])
        self.assertEqual(mirrored["routing_usage"], legacy["routing_usage"])
        # Unattributed historical spend is telemetry, not a new packet's cap.
        packet = self.bind("user_message", "new-turn")
        self.assertTrue(ledger.allowed(packet, self.budget))
        ledger.commit()
        self.close()
        before = self.path.read_bytes()
        with self.assertRaisesRegex(ValueError, "legacy_accounting_preimage_changed"):
            self.open(data=legacy, legacy_raw=raw.rstrip())
        self.assertEqual(self.path.read_bytes(), before)
        ledger = self.open(data=legacy, legacy_raw=raw)
        self.assertEqual(ledger.lookup("user_message", "new-turn", prompt_digest("first task")), packet)

    def test_known_legacy_active_packet_is_tombstoned_not_refilled(self):
        for unknown in (False, True):
            with self.subTest(legacy_unknown=unknown):
                legacy = {**accounting.empty_usage(), "attempts": 37, "input_tokens": 3400,
                          "output_tokens": 290, "unknown_usage": unknown,
                          "active": {"turn": "retained-turn", "request": prompt_digest("old task")}}
                raw = json.dumps(legacy).encode()
                ledger = self.open(path=self.root / f"legacy-{unknown}.sqlite3", data=legacy, legacy_raw=raw)
                retained, created = ledger.bind("user_message", "retained-turn", prompt_digest("old task"))
                self.assertFalse(created)
                self.assertFalse(ledger.allowed(retained, self.budget))
                self.assertTrue(ledger.usage(retained)["unknown_usage"])
                with self.assertRaisesRegex(ValueError, "packet_prompt_changed"):
                    ledger.bind("user_message", "retained-turn", prompt_digest("changed old task"))
                new_packet = self.bind("user_message", "new-turn")
                self.assertTrue(ledger.allowed(new_packet, self.budget))
                mirrored = {}
                ledger.mirror(mirrored)
                for key in accounting.FIELDS:
                    self.assertEqual(mirrored[key], legacy[key])
                self.assertIsNone(ledger.latest_end())  # Migration cannot invent an end event.
                ledger.commit()
                ledger = self.open(path=self.root / f"legacy-{unknown}.sqlite3", data=legacy, legacy_raw=raw)
                self.assertEqual(ledger.bind("user_message", "retained-turn", prompt_digest("old task")),
                                 (retained, False))
                self.assertFalse(ledger.allowed(retained, self.budget))

    def test_packet_identity_includes_host_and_session_domains(self):
        packets = []
        for index, (host, session) in enumerate((("codex", "s1"), ("codex", "s2"),
                                                ("claude_code", "s1"))):
            ledger = self.open(path=self.root / f"owner-{index}.sqlite3", host=host, session=session, fresh=True)
            packets.append(self.bind())
            ledger.commit()
        self.assertEqual(len(set(packets)), len(packets))

    def test_interrupted_initial_schema_creation_is_not_silently_recreated(self):
        # A bad legacy identity trips validation after schema preparation. Its
        # rolled-back file remains ambiguous evidence, not an empty-store permit.
        legacy = {**accounting.empty_usage(), "active": {"turn": None, "request": prompt_digest("old")}}
        raw = json.dumps(legacy).encode()
        with self.assertRaisesRegex(ValueError, "invalid_packet_identity"):
            self.open(data=legacy, legacy_raw=raw)
        self.assertTrue(self.path.exists())
        before = self.path.read_bytes()
        with self.assertRaises(sqlite3.DatabaseError):
            self.open(data=legacy, legacy_raw=raw)
        self.assertEqual(self.path.read_bytes(), before)
        with self.assertRaises(sqlite3.DatabaseError):
            self.open(fresh=True)
        self.assertEqual(self.path.read_bytes(), before)

    def test_current_json_cannot_silently_create_missing_accounting(self):
        with self.assertRaisesRegex(ValueError, "current_reader_missing_accounting"):
            self.open()
        self.assertFalse(self.path.exists())
        ledger = self.fresh()
        ledger.commit()
        self.close()
        before = self.path.read_bytes()
        with self.assertRaisesRegex(ValueError, "accounting_without_reader_state"):
            self.open(fresh=True)
        self.assertEqual(self.path.read_bytes(), before)

    def test_wrong_owner_host_and_unknown_schema_refuse_without_reset(self):
        ledger = self.fresh()
        packet = self.bind()
        self.assertTrue(ledger.reserve(packet, "call", {}, self.budget))
        ledger.commit()
        self.close()
        before = self.path.read_bytes()
        for owner in ({"session": "other"}, {"host": "claude_code"}):
            with self.subTest(owner=owner):
                with self.assertRaisesRegex(ValueError, "accounting_owner_or_schema_changed"):
                    self.open(**owner)
                self.assertEqual(self.path.read_bytes(), before)
        with closing(sqlite3.connect(self.path)) as db:
            db.execute("UPDATE meta SET schema='future-schema' WHERE id=1")
            db.commit()
        before = self.path.read_bytes()
        with self.assertRaisesRegex(ValueError, "accounting_owner_or_schema_changed"):
            self.open()
        self.assertEqual(self.path.read_bytes(), before)

    def test_existing_unrecognized_database_is_never_initialized(self):
        self.path.write_bytes(b"not an accounting database")
        before = self.path.read_bytes()
        with self.assertRaises(sqlite3.DatabaseError):
            self.open(fresh=True)
        self.assertEqual(self.path.read_bytes(), before)

    def test_identity_validation_does_not_mint_turnless_or_digestless_packets(self):
        ledger = self.fresh()
        for args in (("idle", "t1", prompt_digest("x")), ("turn_end", None, prompt_digest("x")),
                     ("turn_end", "", prompt_digest("x")), ("user_message", "t1", "")):
            with self.subTest(args=args), self.assertRaisesRegex(ValueError, "invalid_packet_identity"):
                ledger.bind(*args)
        self.assertIsNone(ledger.latest_end())
        self.assertEqual(ledger.db.execute("SELECT count(*) FROM packets").fetchone()[0], 0)

    @unittest.skipIf(os.name == "nt", "Unix symlink/hardlink construction; Windows admission has separate host tests")
    def test_database_and_sqlite_sidecar_aliases_are_refused(self):
        target = self.root / "target"
        target.write_bytes(b"unrelated bytes")
        for suffix in ("", "-journal", "-wal", "-shm"):
            with self.subTest(suffix=suffix):
                alias = Path(str(self.path) + suffix)
                alias.symlink_to(target)
                with self.assertRaises((OSError, ValueError)):
                    self.open(fresh=True)
                self.assertEqual(target.read_bytes(), b"unrelated bytes")
                if suffix:
                    self.assertFalse(self.path.exists(), "unsafe sidecar must refuse before DB creation")
                alias.unlink()
                if self.path.exists():
                    self.path.unlink()
        os.link(target, self.path)
        with self.assertRaises((OSError, ValueError)):
            self.open(fresh=True)
        self.assertEqual(target.read_bytes(), b"unrelated bytes")


if __name__ == "__main__":
    unittest.main()

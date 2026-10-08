"""No-provider contract tests for the evaluation-only reader runner."""
from __future__ import annotations

import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import reflexive_reader_probe as probe


class ReaderProbeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.document = json.loads(probe.CASES.read_text(encoding="utf-8"))

    def test_frozen_inventory_and_explicit_packets(self):
        development = probe.inventory(self.document, "development")
        holdout = probe.inventory(self.document, "holdout")
        self.assertEqual((len(development), len(holdout)), (5, 5))
        self.assertTrue(all(len(c["stages"]) == 1 for c in development + holdout))
        self.assertTrue(all(len(c["stages"][0]["host"]["selector_packet"]) == 2
                            for c in development + holdout))

    def test_packet_projection_and_repeat_control(self):
        case = probe.inventory(self.document, "development")[0]
        lookup = {c["id"]: c for c in self.document["cards"]}
        seen = []
        def fake_reader(cue, cards, ledger, *, live, key_file):
            seen.append((cue, cards))
            self.assertNotIn("expected_retention", repr(cards))
            self.assertNotIn("accepted_actions", repr(cards))
            return {"selected_ids": [cards[0]["id"]], "provider_attempt": False,
                    "cache_hit": len(seen) == 2, "usage": None,
                    "estimated_usd": 0, "reserved_usd": 0}
        with patch.object(probe.reader, "select", side_effect=fake_reader):
            row = probe.packet_case(case, lookup, Path("/tmp/unused-ledger"), False, None)
        self.assertEqual(len(seen), 3)
        self.assertEqual(seen[0], seen[1])
        self.assertEqual([c["id"] for c in seen[2][1]], list(reversed(row["packet_ids"])))
        self.assertTrue(row["retention"]["exact_definite"])

    def test_oracle_intersection_does_not_penalize_missing_native_card(self):
        labels = {"keep": ["a", "b"], "drop": ["c"], "allow": ["d"]}
        result = probe.retention(["a"], ["a", "c"], labels)
        self.assertEqual(result["available_keep"], ["a"])
        self.assertTrue(result["exact_definite"])
        self.assertEqual(result["correct_drop"], ["c"])

    def test_native_seam_preserves_collector_metadata_and_ok_abstention(self):
        original = {"outcome": "ok", "cards": [
            {"id": "a", "summary": "first", "fingerprint": "fa"},
            {"id": "b", "summary": "second", "fingerprint": "fb"}],
            "observation": {"schema": 1, "learning": "disabled", "cards": [
                {"node_id": "a", "graph_path": None},
                {"node_id": "b", "graph_path": None}]}}
        selected_input = []
        def fake_select(cue, cards, ledger, live, key_file):
            selected_input.extend(cards)
            return {"selected_ids": [], "provider_attempt": False}
        native = []
        with patch.object(probe.transfer.hook_recall, "_collect", return_value=original.copy()), \
             patch.object(probe, "select", side_effect=fake_select):
            with probe.recall_override(Path("/tmp/service"), Path("/tmp/project"),
                                       "reader", native, Path("/tmp/ledger"), False, None):
                result = probe.transfer.hooks._recall_cards({}, "task cue")
        self.assertEqual(result["outcome"], "ok")
        self.assertEqual(result["cards"], [])
        self.assertEqual(result["observation"]["cards"], [])
        self.assertEqual([c["fingerprint"] for c in selected_input], ["fa", "fb"])
        self.assertTrue(native[-1]["collector"]["selector_abstained"])
        self.assertFalse(native[-1]["collector"]["native_empty"])

    def test_totals_count_only_actual_attempts_not_cached_usage(self):
        result = {"provider_attempt": True, "cache_hit": False,
                  "reserved_usd": .002753, "estimated_usd": .0001,
                  "usage": {"input_tokens": 2}}
        cached = {**result, "provider_attempt": False, "cache_hit": True,
                  "reserved_usd": 0}
        total = probe.totals([{"first": result, "repeat": cached, "reverse": cached}])
        self.assertEqual(total["reader_provider_attempts"], 1)
        self.assertEqual(total["reader_cache_hits"], 2)
        self.assertEqual(total["reader_estimated_usd_known"], .0001)

    def test_reservation_before_network_is_counted(self):
        reserved_no_network = {"provider_attempt": False, "cache_hit": False,
                               "reserved_usd": .002753, "usage": None,
                               "estimated_usd": None}
        total = probe.totals([{"first": reserved_no_network,
                               "repeat": {"provider_attempt": False,
                                          "cache_hit": False, "reserved_usd": 0},
                               "reverse": {"provider_attempt": False,
                                           "cache_hit": False, "reserved_usd": 0}}])
        self.assertEqual(total["reader_provider_attempts"], 0)
        self.assertEqual(total["reader_reservations_usd"], .002753)

    def test_prepare_never_reads_key_or_overwrites_output(self):
        with tempfile.TemporaryDirectory() as name:
            output = Path(name) / "receipt.json"
            argv = ["reader", "--cases", str(probe.CASES), "--key-file",
                    str(Path(name) / "nonexistent-secret"), "--output", str(output)]
            with patch.object(sys, "argv", argv):
                probe.main()
            self.assertEqual(json.loads(output.read_text())["status"], "prepared")
            with patch.object(sys, "argv", argv), self.assertRaisesRegex(RuntimeError, "overwrite"):
                probe.main()

    def test_holdout_native_preflight_refused_before_binary_checks(self):
        with tempfile.TemporaryDirectory() as name:
            with patch.object(sys, "argv", ["reader", "--phase", "native", "--split", "holdout",
                                            "--output", str(Path(name) / "receipt.json")]), \
                 self.assertRaisesRegex(RuntimeError, "preflight forbidden"):
                probe.main()


if __name__ == "__main__":
    unittest.main()

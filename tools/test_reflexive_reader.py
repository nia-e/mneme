"""Offline contract tests: no TypeSafe connection is made here."""

import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from tools import reflexive_reader as reader


TASK = "Decide the safe order for the current packing task before dispatching this item."


def card(identifier, summary, **extra):
    return {"id": identifier, "summary": summary, "source": "fixture:synthetic", **extra}


class Provider:
    def __init__(self, probabilities, *, model=reader.MODEL, usage=None, status=200, error=None):
        self.probabilities = probabilities
        self.model = model
        self.usage = usage or {"input_tokens": 100, "output_tokens": 5}
        self.status = status
        self.error = error
        self.calls = []

    def __call__(self, body, key):
        self.calls.append((json.loads(body), key))
        response = {"model": self.model, "answers": {
            f"q{i}": {"type": "noul", "noul": p}
            for i, p in enumerate(self.probabilities)}, "usage": self.usage}
        return self.status, response, self.error, 1.25


class ReaderTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.ledger = self.root / "shared-ledger.json"
        self.key = self.root / "key"
        self.key.write_text("very-secret-key\n", encoding="ascii")
        self.cards = [card("a", "At this station, equalize a vented capsule before pressing it.", fingerprint="f1"),
                      card("b", "The shelf label used a different color last year.", fingerprint="f2")]

    def live(self, cue=TASK, cards=None, provider=None, **kw):
        return reader.select(cue, self.cards if cards is None else cards,
                             self.ledger, live=True, key_file=self.key,
                             provider=provider, **kw)

    def test_no_network_for_empty_trivial_offline_seen_and_graph_packet(self):
        provider = Provider([1, 1])
        self.assertEqual(self.live("", provider=provider)["selected_ids"], ["a", "b"])
        self.assertEqual(self.live("thanks!", provider=provider)["reason"], "nonsubstantive")
        self.assertEqual(self.live(provider=provider, seen=(("a", "f1"), ("b", "f2")))["selected_ids"], [])
        self.assertEqual(self.live(provider=provider, seen=(("a", "wrong"),))["selected_ids"], [])
        # That last request *does* call the provider: the wrong fingerprint does
        # not suppress a card. The provider vetoes both with p=1.
        self.assertEqual(len(provider.calls), 1)
        graph = [dict(self.cards[0], native={"graph_path": [{"edge": "synthetic"}]}), self.cards[1]]
        self.assertEqual(self.live(cards=graph, provider=provider)["selected_ids"], ["a", "b"])
        # A seen graph card still protects its companion: no partial packet
        # judgment after the deterministic seen filter.
        self.assertEqual(self.live(cards=graph, provider=provider,
                                   seen=(("a", "f1"),))["selected_ids"], ["b"])
        self.assertEqual(len(provider.calls), 1)
        self.assertEqual(reader.select(TASK, self.cards, self.ledger, provider=provider)["reason"], "offline")
        self.assertEqual(len(provider.calls), 1)

    def test_exact_duplicate_and_narrow_scope_are_separate_noul_vetoes(self):
        cue = ("Decide the safe packing order. The current task card already says "
               "equalize this vented capsule before pressing it; use this live rule.")
        cards = [card("same", "Equalize this vented capsule before pressing it."),
                 card("scope", "At a different press, sealed capsules bypass equalization; "
                      "that exception was verified only for sealed capsules.")]
        provider = Provider([reader.VETO_THRESHOLD, reader.VETO_THRESHOLD - .000001])
        result = self.live(cue, cards, provider)
        self.assertEqual(result["selected_ids"], ["scope"])
        self.assertEqual(result["reason"], "veto")
        request = provider.calls[0][0]
        self.assertEqual(request["model"], reader.MODEL)
        self.assertEqual(request["state"], {"task": cue})
        self.assertEqual(set(request["questions"]), {"q0", "q1"})
        self.assertEqual(request["questions"]["q0"]["instructions"]["card"], cards[0]["summary"])
        self.assertEqual(request["questions"]["q1"]["instructions"]["card"], cards[1]["summary"])
        self.assertIn("already", request["questions"]["q0"]["criteria"]["true"])
        self.assertIn("uncertain", request["questions"]["q0"]["criteria"]["false"])
        self.assertEqual(result["usage"], {"input_tokens": 100, "output_tokens": 5})
        self.assertEqual(result["estimated_usd"], 100 * 42 / 1_000_000_000)
        self.assertEqual(result["reserved_usd"], .002753)

    def test_success_cache_and_identity_changes(self):
        provider = Provider([.99, .1])
        first = self.live(provider=provider)
        self.assertEqual(first["selected_ids"], ["b"])
        again = self.live(provider=provider)
        self.assertTrue(again["cache_hit"])
        self.assertFalse(again["provider_attempt"])
        self.assertEqual(again["reserved_usd"], 0)
        self.assertEqual(len(provider.calls), 1)
        self.live(TASK + " Use the current item.", provider=provider)
        changed = [dict(self.cards[0], summary=self.cards[0]["summary"] + " Again."), self.cards[1]]
        self.live(cards=changed, provider=provider)
        with patch.object(reader, "PROMPT_VERSION", "narrow-veto-v2"):
            self.live(provider=provider)
        with patch.object(reader, "VETO_THRESHOLD", reader.VETO_THRESHOLD + .01):
            self.live(provider=provider)
        with patch.object(reader, "MODEL", "jev-1.14.0"):
            self.live(provider=Provider([.99, .1], model="jev-1.14.0"))
        self.assertEqual(len(json.loads(self.ledger.read_text())["intents"]), 6)
        self.assertEqual(len(provider.calls), 5)

    def test_failed_or_ambiguous_attempt_is_not_replayed(self):
        provider = Provider([.99, .99], error="HTTP 502")
        first = self.live(provider=provider)
        self.assertEqual(first["selected_ids"], ["a", "b"])
        self.assertTrue(first["provider_attempt"])
        self.assertIsNone(first["estimated_usd"])
        second = self.live(provider=provider)
        self.assertEqual(second["reason"], "prior_attempt")
        self.assertEqual(len(provider.calls), 1)
        self.assertEqual(len(json.loads(self.ledger.read_text())["intents"]), 1)

    def test_reserved_crash_marker_never_retries(self):
        # Simulate a process dying after its durable reservation, before network.
        first = self.live(provider=Provider([.1, .1]))
        self.assertTrue(first["provider_attempt"])
        data = json.loads(self.ledger.read_text())
        data["intents"][0] = {k: v for k, v in data["intents"][0].items()
                              if k in ("identity", "ids", "reserved_micro_usd")}
        data["intents"][0]["status"] = "reserved"
        self.ledger.write_text(json.dumps(data))
        provider = Provider([.99, .99])
        self.assertEqual(self.live(provider=provider)["reason"], "prior_attempt")
        self.assertEqual(provider.calls, [])

    def test_invalid_key_reserves_but_never_invokes_provider(self):
        self.key.write_text("bad key with spaces", encoding="ascii")
        provider = Provider([1, 1])
        result = self.live(provider=provider)
        self.assertFalse(result["provider_attempt"])
        self.assertIsNone(result["estimated_usd"])
        self.assertEqual(result["reserved_usd"], .002753)
        self.assertEqual(provider.calls, [])
        self.key.write_text("valid-secret", encoding="ascii")
        self.assertEqual(self.live(provider=provider)["reason"], "prior_attempt")
        self.assertEqual(provider.calls, [])

    def test_invalid_responses_fail_open_and_consume_reservation(self):
        bad = [Provider([float("nan"), .99]), Provider([True, .99]),
               Provider([.99], model="different"),
               Provider([.99, .99], usage={"input_tokens": 65537, "output_tokens": 0})]
        for index, provider in enumerate(bad):
            with self.subTest(index=index):
                result = self.live(TASK + f" Variant {index}.", provider=provider)
                self.assertEqual(result["selected_ids"], ["a", "b"])
                self.assertEqual(result["reason"], "provider_error")
                self.assertTrue(result["provider_attempt"])
        huge = "x" * (reader.MAX_RESPONSE_BYTES + 1)
        cards = [card("large", "A bounded source-backed lesson.")]
        class HugeProvider:
            def __call__(self, body, key):
                return 200, {"model": reader.MODEL, "answers": {"q0": {"type": "noul", "noul": .99}},
                             "usage": {"input_tokens": 1, "output_tokens": 1}, "junk": huge}, None, 1
        self.assertEqual(self.live(TASK + " More.", cards, HugeProvider())["reason"], "provider_error")

    def test_request_cap_budget_cap_and_invalid_inputs_never_call(self):
        provider = Provider([1, 1])
        oversized = "Decide " + "🦋" * 201
        self.assertEqual(self.live(oversized, provider=provider)["reason"], "invalid_input")
        bad_summary = [dict(self.cards[0], summary="🦋" * 176), self.cards[1]]
        self.assertEqual(self.live(cards=bad_summary, provider=provider)["reason"], "invalid_input")
        self.assertEqual(provider.calls, [])
        # A valid cue and two bounded cards can still exceed the serialized
        # request cap if the rubric grows; reject before reserving/network.
        with patch.object(reader, "MAX_REQUEST_BYTES", 10):
            self.assertEqual(self.live(provider=provider)["reason"], "request_cap")
        self.assertFalse(self.ledger.exists())
        with patch.object(reader, "MAX_CALLS", 0):
            self.assertEqual(self.live(provider=provider)["reason"], "budget_cap")
        with patch.object(reader, "MAX_SPEND_MICRO_USD", 0):
            self.assertEqual(self.live(provider=provider)["reason"], "budget_cap")
        self.assertEqual(provider.calls, [])

    def test_ledger_contains_no_key_cue_or_card_text(self):
        self.live(provider=Provider([.1, .1]))
        raw = self.ledger.read_text()
        self.assertNotIn("very-secret-key", raw)
        self.assertNotIn(TASK, raw)
        self.assertNotIn(self.cards[0]["summary"], raw)
        self.assertIn('"status": "success"', raw)


if __name__ == "__main__":
    unittest.main()

"""Offline selector/ledger tests. All provider calls here are injected fakes."""

import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from tools import codex_reader as reader


TASK = "Decide how to finish the present console task without repeating the earlier failed action."


def card(identifier, text, **extra):
    return {"id": identifier, "summary": text, "source": "fixture:synthetic", **extra}


class Provider:
    def __init__(self, selected, *, status="answered", usage=None):
        self.selected = selected
        self.status = status
        self.usage = usage if usage is not None else {
            "input_tokens": 100, "cached_input_tokens": 40,
            "cache_write_input_tokens": 5, "output_tokens": 20,
            "reasoning_output_tokens": 12}
        self.calls = []

    def __call__(self, prompt, schema, **kwargs):
        self.calls.append((prompt, schema, kwargs))
        return {"status": self.status, "answer": {"selected_ids": self.selected},
                "usage": self.usage, "elapsed_ms": 1.5}


class ReaderTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.ledger = self.root / "shared.json"
        self.home = self.root / "home"
        self.home.mkdir()
        self.workdir = self.root / "empty"
        self.workdir.mkdir()
        self.cards = [card("a", "A past check froze the manifest before assignment.", fingerprint="f1"),
                      card("b", "A station sign changed color but not behavior.", fingerprint="f2"),
                      card("c", "A different setup used a wait-for-receipt protocol.", fingerprint="f3")]

    def select(self, dialogue=None, cards=None, provider=None, *, now=100.0, scope="s", **kwargs):
        return reader.select([{"role": "user", "text": TASK}] if dialogue is None else dialogue,
                             self.cards if cards is None else cards, self.ledger, live=True,
                             codex=self.root / "fake-codex", home=self.home,
                             workdir=self.workdir, env={}, provider=provider,
                             now=now, scope=scope, **kwargs)

    def test_offline_empty_trivial_seen_and_bounds_no_call(self):
        provider = Provider(["a"])
        self.assertEqual(reader.select([{"role": "user", "text": TASK}], self.cards,
                                       self.ledger)["selected_ids"], ["a", "b"])
        self.assertEqual(self.select([], provider=provider)["reason"], "nonsubstantive")
        self.assertEqual(self.select([{"role": "user", "text": "thanks"}],
                                     provider=provider)["reason"], "nonsubstantive")
        self.assertEqual(self.select(provider=provider, seen=(("a", "f1"), ("b", "f2"),
                                                          ("c", "f3")))["selected_ids"], [])
        # Native baseline admits first two before seen suppression; no rank-3
        # backfill on an offline/failure path.
        self.assertEqual(reader.select([{"role": "user", "text": TASK}], self.cards,
                                       self.ledger, seen=(("a", "f1"),
                                                          ("b", "f2")))["selected_ids"], [])
        self.assertEqual(self.select(cards=[dict(self.cards[0], summary="🦋" * 176)],
                                     provider=provider)["reason"], "invalid_input")
        self.assertEqual(provider.calls, [])
        with self.assertRaises(ValueError):
            self.select(cards=self.cards + self.cards + [self.cards[0]], provider=provider)
        with self.assertRaises(ValueError):
            self.select(cards=[card("a", "x"), card("a", "y")], provider=provider)
        with self.assertRaises(ValueError):
            self.select(cards=[card("", "x")], provider=provider)

    def test_window_is_recent_bounded_and_payload_is_last(self):
        dialogue = [{"role": "user", "text": "old " + "x" * 5000}]
        dialogue += [{"role": "assistant", "text": "older response"}] * 5
        dialogue += [{"role": "user", "text": TASK + " New detail."}]
        provider = Provider(["c", "a"])
        result = self.select(dialogue, provider=provider)
        self.assertEqual(result["selected_ids"], ["a", "c"])
        prompt, schema, _ = provider.calls[0]
        self.assertEqual(schema, reader.OUTPUT_SCHEMA)
        self.assertLessEqual(len(prompt.encode()), reader.MAX_PROMPT_BYTES)
        self.assertTrue(prompt.startswith(reader.PROMPT_PREFIX))
        payload = json.loads(prompt[len(reader.PROMPT_PREFIX):])
        self.assertLessEqual(len(payload["dialogue"]), 6)
        self.assertLessEqual(len(reader._encode(payload["dialogue"])), 4000)
        self.assertEqual(payload["dialogue"][-1]["text"], TASK + " New detail.")
        self.assertEqual([c["id"] for c in payload["cards"]], ["a", "b", "c"])
        self.assertNotIn("old " + "x" * 5000, prompt)

    def test_cache_identity_and_seen_fingerprint(self):
        provider = Provider(["c"])
        first = self.select(provider=provider)
        self.assertTrue(first["provider_attempt"])
        self.assertEqual(first["selected_ids"], ["c"])
        cached = self.select(provider=provider, now=101)
        self.assertTrue(cached["cache_hit"])
        self.assertFalse(cached["provider_attempt"])
        self.assertIsNone(cached["usage"])
        self.assertEqual(len(provider.calls), 1)
        self.select(provider=provider, now=103, scope="different")
        self.select(provider=provider, now=106, dialogue=[{"role": "user", "text": TASK + " Now revised."}])
        changed = [dict(self.cards[0], fingerprint="new"), *self.cards[1:]]
        self.select(provider=provider, now=109, cards=changed)
        path = [dict(self.cards[0], native={"graph_path": [{"edge": "bridge"}]}), *self.cards[1:]]
        self.select(provider=provider, now=112, cards=path)
        self.select(provider=provider, now=115, model="gpt-5.6-terra")
        self.select(provider=provider, now=118, effort="medium")
        self.select(provider=provider, now=121, seen=(("a", "f1"),))
        with patch.object(reader, "BASE_INSTRUCTIONS", reader.BASE_INSTRUCTIONS + "\nRevision."):
            self.select(provider=provider, now=124)
        self.assertEqual(len(provider.calls), 9)
        self.assertEqual(len(json.loads(self.ledger.read_text())["intents"]), 9)

    def test_one_active_per_scope_debounce_and_other_scope(self):
        provider = Provider(["a"])
        self.assertEqual(self.select(provider=provider)["selected_ids"], ["a"])
        changed = [{"role": "user", "text": TASK + " Different."}]
        self.assertEqual(self.select(changed, provider=provider, now=101)["reason"], "debounce")
        self.assertFalse(self.select(changed, provider=provider, now=101)["provider_attempt"])
        self.assertEqual(self.select(changed, provider=provider, now=101,
                                     scope="other")["reason"], "selected")
        # A durable reservation from an in-flight call blocks just its scope.
        data = json.loads(self.ledger.read_text())
        data["intents"][0]["status"] = "reserved"
        self.ledger.write_text(json.dumps(data))
        self.assertEqual(self.select(changed, provider=provider, now=103)["reason"], "busy")
        self.assertEqual(len(provider.calls), 2)

    def test_failure_no_retry_missing_usage_stops_new_calls(self):
        provider = Provider([], status="error", usage={})
        failed = self.select(provider=provider)
        self.assertEqual(failed["selected_ids"], ["a", "b"])
        self.assertEqual(failed["usage"]["input_tokens"], None)
        self.assertEqual(self.select(provider=provider)["reason"], "prior_attempt")
        changed = [{"role": "user", "text": TASK + " Changed."}]
        self.assertEqual(self.select(changed, provider=provider, now=103)["reason"], "usage_unknown")
        self.assertEqual(len(provider.calls), 1)

    def test_observed_usage_and_soft_stops_no_double_reasoning(self):
        provider = Provider(["a"], usage={"input_tokens": 240000,
            "cached_input_tokens": 100000, "cache_write_input_tokens": 5000,
            "output_tokens": 19000, "reasoning_output_tokens": 18000})
        result = self.select(provider=provider)
        self.assertEqual(result["usage"]["uncached_input_tokens"], 140000)
        self.assertEqual(result["usage"]["reasoning_output_tokens"], 18000)
        changed = [{"role": "user", "text": TASK + " Changed."}]
        with patch.object(reader, "SOFT_INPUT_TOKENS", 200000):
            self.assertEqual(self.select(changed, provider=provider, now=103)["reason"], "token_stop")
        with patch.object(reader, "SOFT_OUTPUT_TOKENS", 18000):
            self.assertEqual(self.select(changed, provider=provider, now=103)["reason"], "token_stop")
        self.assertEqual(len(provider.calls), 1)

    def test_command_is_tool_free_and_event_usage_survives_invalid_tool_event(self):
        argv = reader._command(Path("/bin/codex"), self.home, self.workdir,
                               "gpt-5.6-sol", "low", self.root / "schema",
                               self.root / "answer", self.root / "instructions")
        self.assertIn("--ignore-user-config", argv)
        self.assertIn("--ignore-rules", argv)
        self.assertIn("--ephemeral", argv)
        self.assertIn("read-only", argv)
        self.assertIn("shell_tool", argv)
        self.assertIn("multi_agent", argv)
        self.assertTrue(any("model_instructions_file=" in arg for arg in argv))
        self.assertEqual(argv[-1], "-")
        events = b'\n'.join([json.dumps({"type": "item.completed", "item": {"type": "function_call"}}).encode(),
            json.dumps({"type": "turn.completed", "usage": {"input_tokens": 50,
                "output_tokens": 8, "reasoning_output_tokens": 5}}).encode()])
        usage, completed, valid = reader._events(events)
        self.assertTrue(completed)
        self.assertFalse(valid)
        self.assertEqual(usage["input_tokens"], 50)
        self.assertIsNone(usage["cached_input_tokens"])


if __name__ == "__main__":
    unittest.main()

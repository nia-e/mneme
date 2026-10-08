"""Read-only optional preference lane, using disposable owners and fake selectors."""
import hashlib
import json
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

import hook_recall
import hooks
import reader_contract
import reader_worker
from fixture_recall_owner import FakeClient, RecallOwnerFixture, ID1, ID2, ID3, ID7
from librarian_policy import LibrarianBudget
from target_policy import (TargetPolicy, GLOBAL_PREFERENCE_TAG, GLOBAL_PREFERENCE_NAMESPACE,
                           GLOBAL_PREFERENCE_CUE)
from turn_observer import validate_delivery_packet, DELIVERY_SCHEMA


class PreferenceOwnerTests(unittest.TestCase, RecallOwnerFixture):
    def setUp(self):
        RecallOwnerFixture.__init__(self, self)
        self._connect_config(database_name="user", database_path="/owner/preferences.db")
        self.policy = TargetPolicy("global_preference", "user", "/owner/preferences.db", ID7,
                                   str(self.root), str(self.config))
        self.catalog = [{"db": "user", "name": "user", "state": "open",
                         "configured_path": "/owner/preferences.db", "db_id": ID7}]
        self.context["primary"] = [{"id": ID1}, {"id": ID2}, {"id": ID3}]
        self.context["expansions"] = []
        self.context["observation"] = {"schema": 1, "learning": "disabled", "cards": [
            {"node_id": id, "lane": "primary", "card_sha256": "a" * 64, "graph_path": None}
            for id in (ID1, ID2, ID3)]}
        for node in self.nodes.values():
            node["tags"] = [GLOBAL_PREFERENCE_TAG]
            node["provenance"]["source"]["namespace"] = GLOBAL_PREFERENCE_NAMESPACE

    def test_fixed_remote_cue_depth_tag_and_native_get_scope_filter(self):
        self.nodes[ID2]["provenance"]["source"]["namespace"] = "private-unrelated"
        self.nodes[ID3]["tags"] = ["private-unrelated"]
        client = FakeClient(self.catalog, self.context, self.nodes)
        shared = {"deadline": time.monotonic() + 10, "decoded_bytes": 123}
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall.collect_reader(self.config, GLOBAL_PREFERENCE_CUE, self.root,
                store_target=self.policy, shared_work=shared)
        self.assertEqual(result["outcome"], "ok")
        self.assertEqual([c["id"] for c in result["cards"]], [ID1])
        self.assertEqual(result["cards"][0]["db_id"], ID7)
        self.assertEqual(result["cards"][0]["scope"], "global_preference")
        request = client.calls[1][1]
        self.assertEqual(request["text"], GLOBAL_PREFERENCE_CUE)
        self.assertEqual(request["depth"], 0)
        self.assertEqual(request["tags"], [GLOBAL_PREFERENCE_TAG])
        self.assertNotIn("routing_hints", request)
        self.assertTrue(all(args.get("db") == "user" for name, args in client.calls if name != "databases"))
        expected_bytes = 123 + 2 * hook_recall._bytes(self.catalog) + hook_recall._bytes(self.context)
        expected_bytes += sum(hook_recall._bytes(n) for n in self.nodes.values())
        self.assertEqual(shared["decoded_bytes"], expected_bytes)
        self.assertEqual(result["native_work"]["decoded_bytes"], expected_bytes)

    def test_private_cue_is_rejected_before_contact(self):
        with patch("hook_recall.McpClient") as client:
            result = hook_recall.collect_reader(self.config, "secret project task", self.root,
                                                store_target=self.policy)
        self.assertEqual(result["outcome"], "unavailable")
        client.assert_not_called()

    def test_native_budget_not_reset_and_failure_remains_charged(self):
        budget = LibrarianBudget()
        shared = {"deadline": time.monotonic() + 10, "decoded_bytes": budget.native_read_bytes}
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall.collect_reader(self.config, GLOBAL_PREFERENCE_CUE, self.root,
                store_target=self.policy, shared_work=shared)
        self.assertEqual(result["outcome"], "timeout")
        self.assertEqual(client.calls, [])
        self.assertEqual(shared["decoded_bytes"], budget.native_read_bytes)
        self.catalog[0]["db_id"] = ID2
        shared["decoded_bytes"] = 5
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall.collect_reader(self.config, GLOBAL_PREFERENCE_CUE, self.root,
                store_target=self.policy, shared_work=shared)
        self.assertEqual(result["outcome"], "unavailable")
        self.assertEqual(shared["decoded_bytes"], 5 + hook_recall._bytes(self.catalog))
        self.assertEqual([name for name, _ in client.calls], ["databases"])


class PreferenceSelectorTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.config = {"project_root": self.root, "service_config": self.root / "project.json",
                       "state_dir": self.root / "state", "reader_model": "gpt-6.1-sol",
                       "librarian_effort": "medium"}
        self.config["service_config"].write_text("{}")
        self.policy = TargetPolicy("global_preference", "user", "/owner/preferences.db", ID7,
                                   str(self.root), str(self.root / "global.json"))
        self.project = {"id": ID1, "summary": "Project observation", "status": "active",
                        "source": "codex:project", "fingerprint": "a" * 64}
        self.global_card = {**self.project, "summary": "A collaboration preference", "db_id": ID7,
                            "source": GLOBAL_PREFERENCE_NAMESPACE + ":preference", "scope": "global_preference"}
        self.calls = []
        self.runtime = self

    def select(self, dialogue, cards, **kwargs):
        self.calls.append((dialogue, cards))
        return {"reason": "selected", "selected_ids": [c["id"] for c in cards],
                "provider_attempt": True, "usage": {"input_tokens": 10, "output_tokens": 3},
                "concerns": [{"kind": "disagreement", "left_id": cards[0]["id"],
                              "right_id": cards[1]["id"], "caveat": "scope differs", "missing_fact": "current applicability"}]
                             if len(cards) > 1 else []}

    def execute(self, global_result=None, project=None, emitted=None):
        project = {"outcome": "ok", "cards": [self.project], "db_id": ID2} if project is None else project
        results = [project, global_result or {"outcome": "ok", "cards": [self.global_card]}]
        with patch("reader_worker.global_preferences_policy", return_value=self.policy), \
             patch("hook_recall.collect_reader", side_effect=results) as collect:
            result = reader_worker._execute(self.config, "secret project task", self.runtime, lambda: True,
                                            set() if emitted is None else emitted)
        return result, collect

    def test_duplicate_ids_single_selector_native_delivery_exact_binding_and_no_concern(self):
        result, collect = self.execute()
        self.assertEqual(result["outcome"], "selected")
        self.assertEqual(len(self.calls), 1)
        self.assertEqual([c["id"] for c in self.calls[0][1]], [ID1, "global:" + ID1])
        self.assertEqual([c["id"] for c in result["cards"]], [ID1, ID1])
        self.assertEqual(result["concern_nominations"], [])
        self.assertEqual(collect.call_args_list[1].args[1], GLOBAL_PREFERENCE_CUE)
        self.assertIs(collect.call_args_list[0].kwargs["shared_work"], collect.call_args_list[1].kwargs["shared_work"])
        packed = hooks._pack_async_delivery(result["cards"], "s1", "t1", result["db_id"])
        self.assertEqual([c["db_id"] for c in packed["displayed"]], [ID2, ID7])
        self.assertEqual([c["displayed_view"]["id"] for c in packed["displayed"]], [ID1, ID1])
        self.assertNotIn("global:" + ID1, packed["context"])
        packet = validate_delivery_packet({"schema": DELIVERY_SCHEMA, "session_id": "s1", "turn_id": "t1",
            "rendered_text": packed["context"], "rendered_sha256": hashlib.sha256(packed["context"].encode()).hexdigest(),
            "displayed": packed["displayed"], "concerns": packed["concerns"]})
        self.assertEqual(packet["rendered_text"].encode(), packed["context"].encode())
        self.assertLessEqual(len(packed["context"].encode()), hooks.MAX_CONTEXT_BYTES)
        diagnostic = reader_worker._selection_diagnostic(result, {}, True)
        self.assertNotIn("omitted", diagnostic)
        self.assertEqual(diagnostic["selected_keys"], [[ID2, ID1], [ID7, ID1]])

    def test_optional_unavailable_preserves_project_and_project_unavailable_preserves_global(self):
        result, _ = self.execute(global_result={"outcome": "unavailable", "cards": []})
        self.assertEqual([c["summary"] for c in result["cards"]], [self.project["summary"]])
        self.assertEqual(result["selection"]["global_preference_outcome"], "unavailable")
        self.calls.clear()
        result, _ = self.execute(project={"outcome": "timeout", "cards": []})
        self.assertEqual(result["cards"], [self.global_card])

    def test_scoped_emission_does_not_hide_independent_store(self):
        result, _ = self.execute(emitted={(ID2, ID1, self.project["fingerprint"])})
        self.assertEqual(result["cards"], [self.global_card])
        result, _ = self.execute(emitted={(ID1, self.project["fingerprint"])})
        self.assertEqual(result["cards"], [self.global_card])

    def test_no_opt_in_retains_one_collector_and_legacy_ids(self):
        with patch("hook_recall.collect_reader", return_value={"outcome": "ok", "cards": [self.project]}) as collect:
            result = reader_worker._execute(self.config, "secret project task", self.runtime, lambda: True, set())
        self.assertEqual(collect.call_count, 1)
        self.assertNotIn("shared_work", collect.call_args.kwargs)
        self.assertNotIn("db_id", result["cards"][0])
        self.assertEqual(self.calls[0][1][0]["id"], ID1)

    def test_shared_work_exhaustion_skips_global_without_new_collector(self):
        def exhausted(*args, **kwargs):
            kwargs["shared_work"]["decoded_bytes"] = kwargs["budget"].native_read_bytes
            return {"outcome": "ok", "cards": [self.project], "db_id": ID2}
        with patch("reader_worker.global_preferences_policy", return_value=self.policy), \
             patch("hook_recall.collect_reader", side_effect=exhausted) as collect:
            result = reader_worker._execute(self.config, "secret project task", self.runtime, lambda: True, set())
        self.assertEqual(collect.call_count, 1)
        self.assertEqual(result["selection"]["global_preference_outcome"], "budget")
        self.assertEqual(result["cards"][0]["summary"], self.project["summary"])
        self.assertTrue(result["selection"]["native_work"]["read_allowance_exhausted"])

    def test_consume_ledger_scopes_identical_native_id_and_fingerprint(self):
        event = {"session_id": "s1", "turn_id": "t1", "prompt": "Implement a useful feature"}
        reader_worker.notice(self.config, event, True)
        def ready(data):
            data["ready"] = {**data["pending"], "fence": data["fence"], "db_id": ID2,
                             "cards": [{**self.project, "db_id": ID2}, self.global_card]}
            return None, True
        reader_worker._state(self.config, "s1", ready, create=False)
        result = reader_worker.consume(self.config, event)
        self.assertEqual([c["db_id"] for c in result["cards"]], [ID2, ID7])
        path, _ = reader_worker._paths(self.config, "s1")
        ledger = json.loads(path.read_text())["emitted"]
        self.assertEqual(ledger, [[ID2, ID1, "a" * 64], [ID7, ID1, "a" * 64]])
        reader_worker._state(self.config, "s1", ready, create=False)
        self.assertEqual(reader_worker.consume(self.config, event)["outcome"], "duplicate")

    def test_scope_projection_checked_and_visible_to_single_selector(self):
        prompt, offered = reader_contract.prepare([{"role": "user", "text": "Implement a useful feature"}], [self.global_card])
        self.assertIn('"scope":"global_preference"', prompt)
        self.assertIn('"db_id":"' + ID7 + '"', prompt)
        bad = dict(self.global_card, kind="episode")
        self.assertEqual(reader_contract.prepare([{"role": "user", "text": "Implement a useful feature"}], [bad]), (None, "invalid_input"))


if __name__ == "__main__":
    unittest.main()

"""Named-owner read composition: fake native owners, no model or live stores."""
from copy import deepcopy
import hashlib
import json
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch

import hooks
import hook_recall
import reader_contract
import reader_worker
import store_registry
from turn_observer import validate_delivery_packet, DELIVERY_SCHEMA

NODE = "01K123456789ABCDEFGHJKMNPQ"
NODE2 = "01K123456789ABCDEFGHJKMNPR"
NODE3 = "01K123456789ABCDEFGHJKMNPS"
OWNERS = ["01ARZ3NDEKTSV4RRFFQ69G5FA" + suffix for suffix in "VWX"]


class NamedReaderTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.cwd = self.root / "work"
        self.cwd.mkdir()
        self.path = self.root / "stores.json"
        self.entries = []
        for index, name in enumerate(("project-not-special", "personal", "company")):
            service = self.root / (name + ".json")
            service.write_text(json.dumps({"mode": "connect", "url": f"http://127.0.0.1:{18766+index}/",
                "database_name": "user", "database_path": f"/owner/{name}.db"}))
            self.entries.append({"name": name, "service_config": str(service), "db_alias": "user",
                "database_path": f"/owner/{name}.db", "db_id": OWNERS[index],
                "purpose": f"Source-bound {name} material.", "read": {"enabled": True},
                "maintenance": {"enabled": index != 1}})
        self.config = {"project_root": self.cwd, "state_dir": self.root / "state",
                       "reader_model": "gpt-6.1-sol", "librarian_effort": "medium",
                       "recording_mode": "automatic", "memory_mode": "async",
                       "memory_scope": "registry", "selection_cwd": self.cwd}
        self.freeze()
        self.calls = []
        self.responses = [{"outcome": "ok", "db_id": owner, "cards": [self.card()]}
                          for owner in OWNERS]
        self.runtime = self
        self.selected_concerns = []

    def card(self, identifier=NODE):
        return {"id": identifier, "summary": "A bounded historical observation", "status": "active",
                "source": "fixture", "fingerprint": "a" * 64}

    def freeze(self):
        self.path.write_text(json.dumps({"schema": store_registry.SCHEMA, "stores": self.entries}))
        selected = store_registry.resolve(self.cwd, registry_path=self.path)
        self.config["frozen_selection"] = selected.to_dict()
        self.config.pop("_reader_config_pin", None)
        return selected

    def select(self, dialogue, cards, **kwargs):
        self.calls.append((dialogue, deepcopy(cards), deepcopy(kwargs)))
        return {"reason": "selected", "selected_ids": [c["id"] for c in cards],
                "concerns": self.selected_concerns, "provider_attempt": True,
                "usage": {"input_tokens": 10, "output_tokens": 2}, "elapsed_ms": 1}

    def execute(self, *, emitted=None, effect=None, start_owner=None):
        responses = deepcopy(self.responses)
        def collect(path, cue, root, **kwargs):
            index = next(i for i, entry in enumerate(self.entries) if entry["service_config"] == str(path))
            kwargs["shared_work"]["decoded_bytes"] += 200
            return responses[index]
        with patch("hook_recall.collect_reader", side_effect=effect or collect) as native:
            result = reader_worker._execute(self.config, "Investigate the current integration design",
                self.runtime, lambda: True, emitted or set(), start_owner=start_owner)
        return result, native

    def test_three_duplicate_node_ids_one_selector_exact_owner_pins(self):
        result, native = self.execute()
        self.assertEqual(result["outcome"], "selected")
        self.assertEqual(len(self.calls), 1)
        self.assertEqual(native.call_count, 3)
        self.assertEqual([c["id"] for c in self.calls[0][1]], [owner + ":" + NODE for owner in OWNERS])
        self.assertEqual([c["id"] for c in result["cards"]], [NODE] * 3)
        self.assertEqual([c["db_id"] for c in result["cards"]], OWNERS)
        self.assertTrue(all(c["scope"] == "named" for c in result["cards"]))
        self.assertEqual(result["read_coverage"], {"selected": 3, "attempted": 3, "unavailable": 0,
                                                 "budget_omitted": 0, "complete": True})
        shared = native.call_args_list[0].kwargs["shared_work"]
        self.assertTrue(all(call.kwargs["shared_work"] is shared for call in native.call_args_list))
        self.assertEqual(result["selection"]["native_work"]["decoded_bytes"], 600)
        packed = hooks._pack_async_delivery(result["cards"], "s1", "t1", read_coverage=result["read_coverage"])
        self.assertNotIn(OWNERS[0] + ":" + NODE, packed["context"])
        self.assertEqual([c["db_id"] for c in packed["displayed"]], OWNERS)
        self.assertLessEqual(len(packed["context"].encode()), hooks.MAX_CONTEXT_BYTES)
        packet = validate_delivery_packet({"schema": DELIVERY_SCHEMA, "session_id": "s1", "turn_id": "t1",
            "rendered_text": packed["context"], "rendered_sha256": hashlib.sha256(packed["context"].encode()).hexdigest(),
            "displayed": packed["displayed"], "concerns": packed["concerns"]})
        self.assertEqual(packet["rendered_text"], packed["context"])
        diagnostic = reader_worker._selection_diagnostic(result, {}, True)
        self.assertNotIn("omitted", diagnostic)
        self.assertEqual(diagnostic["selected_keys"], [[owner, NODE] for owner in OWNERS])

    def test_disabled_owner_no_contact_fixed_cue_has_no_project_text(self):
        self.entries[1]["read"] = {"enabled": False}
        self.entries[2]["read"]["cue"] = "Standing company conventions."
        self.freeze()
        result, native = self.execute()
        self.assertEqual([str(call.args[0]) for call in native.call_args_list],
                         [self.entries[0]["service_config"], self.entries[2]["service_config"]])
        self.assertEqual(native.call_args_list[1].args[1], "Standing company conventions.")
        self.assertEqual([c["db_id"] for c in result["cards"]], [OWNERS[0], OWNERS[2]])

    def test_unavailable_owner_is_partial_not_empty_or_fallback(self):
        self.responses[1] = {"outcome": "unavailable", "cards": []}
        result, native = self.execute()
        self.assertEqual(native.call_count, 3)
        self.assertEqual([c["db_id"] for c in result["cards"]], [OWNERS[0], OWNERS[2]])
        self.assertFalse(result["read_coverage"]["complete"])
        self.assertEqual(result["read_coverage"]["unavailable"], 1)
        packed = hooks._pack_async_delivery(result["cards"], "s", "t", read_coverage=result["read_coverage"])
        self.assertIn("coverage", packed["context"])
        self.assertIn("unavailable", packed["context"])

    def test_aggregate_native_allowance_stops_next_owner_without_contact(self):
        def collect(path, cue, root, **kwargs):
            kwargs["shared_work"]["decoded_bytes"] = kwargs["budget"].native_read_bytes
            return deepcopy(self.responses[0])
        result, native = self.execute(effect=collect)
        self.assertEqual(native.call_count, 1)
        self.assertEqual(len(self.calls), 1)
        self.assertEqual(result["read_coverage"]["budget_omitted"], 2)
        self.assertEqual(len(result["cards"]), 1)

    def test_content_overflow_stops_later_owners_and_smaller_cards(self):
        first = {**self.card(), "summary": "a" * 200}
        overflow = {**self.card(NODE2), "summary": "b" * 800}
        smaller = self.card(NODE3)
        def named(card):
            return {**card, "db_id": OWNERS[0], "scope": "named",
                    "store_name": self.entries[0]["name"], "store_purpose": self.entries[0]["purpose"]}
        # The final card would fit; finishing promptly deliberately wins over
        # looking for smaller cards after an actual content overflow.
        limit = hook_recall._bytes({"cards": [named(first), named(smaller)]}) + 16
        self.assertGreater(hook_recall._bytes({"cards": [named(first), named(overflow)]}), limit)
        self.responses[0]["cards"] = [first, deepcopy(first), overflow, smaller, deepcopy(smaller)]
        with patch("hook_recall.MAX_READER_BYTES", limit):
            result, native = self.execute()
        self.assertEqual(native.call_count, 1)
        self.assertEqual(len(self.calls), 1)
        self.assertEqual([card["id"] for card in result["cards"]], [NODE])
        self.assertEqual(result["selection"]["content_omitted_count"], 2)
        self.assertEqual(result["selection"]["postpack_count"], 1)
        self.assertEqual(result["read_coverage"], {"selected": 3, "attempted": 1, "unavailable": 0,
                                                  "budget_omitted": 2, "complete": False})
        self.assertEqual([owner["outcome"] for owner in result["selection"]["owners"]],
                         ["ok", "budget", "budget"])
        self.assertEqual(result["selection"]["native_work"]["decoded_bytes"], 200)
        self.assertFalse(result["selection"]["native_work"]["read_allowance_exhausted"])
        diagnostic = reader_worker._selection_diagnostic(result, {}, True)
        self.assertNotIn("omitted", diagnostic)
        self.assertEqual(diagnostic["content_omitted_count"], 2)

    def test_seen_only_owner_does_not_fill_content_or_starve_fresh_owners(self):
        seen = {**self.card(), "summary": "already delivered " * 100}
        self.responses[0]["cards"] = [seen, deepcopy(seen)]
        with patch("hook_recall.MAX_READER_BYTES", 1000):
            result, native = self.execute(emitted={(OWNERS[0], NODE, "a" * 64)})
        self.assertEqual(native.call_count, 3)
        self.assertEqual(len(self.calls), 1)
        self.assertEqual([card["db_id"] for card in result["cards"]], OWNERS[1:])
        self.assertEqual(result["selection"]["seen_count"], 1)
        self.assertEqual(result["selection"]["postpack_count"], 2)
        self.assertNotIn("content_omitted_count", result["selection"])
        self.assertTrue(result["read_coverage"]["complete"])

    def test_no_card_outcomes_distinguish_seen_invalid_empty_and_omitted(self):
        cases = (("seen", "empty"), ("invalid", "unavailable"),
                 ("empty", "empty"), ("overflow", "omitted"))
        for case, outcome in cases:
            with self.subTest(case=case):
                for response in self.responses:
                    response["cards"] = ([] if case == "empty" else
                                         [{**self.card(), "status": "deleted"}, None] if case == "invalid"
                                         else [self.card()])
                emitted = {(owner, NODE, "a" * 64) for owner in OWNERS} if case == "seen" else set()
                with patch("hook_recall.MAX_READER_BYTES", 1 if case == "overflow" else 8192):
                    result, native = self.execute(emitted=emitted)
                self.assertEqual(result["outcome"], outcome)
                self.assertEqual(result["cards"], [])
                self.assertFalse(result["provider_attempt"])
                self.assertEqual(native.call_count, 1 if case == "overflow" else 3)
                self.assertEqual(result["selection"]["seen_count"], 3 if case == "seen" else 0)
                self.assertEqual(result["selection"].get("invalid_count", 0), 6 if case == "invalid" else 0)
        self.assertEqual(self.calls, [])

    def test_invalid_or_mismatched_owner_never_contributes_content(self):
        for wrong_owner in (None, "not-a-db-id", OWNERS[0]):
            with self.subTest(owner=wrong_owner):
                self.responses[1]["db_id"] = wrong_owner
                self.responses[1]["cards"] = [{**self.card(), "summary": "x" * 10000}]
                result, native = self.execute()
                self.assertEqual(native.call_count, 3)
                self.assertEqual([card["db_id"] for card in result["cards"]], [OWNERS[0], OWNERS[2]])
                self.assertEqual(result["read_coverage"]["unavailable"], 1)
                self.assertEqual(result["selection"]["owners"][1]["outcome"], "unavailable")
                self.assertNotIn("content_omitted_count", result["selection"])

    def test_duplicate_owner_discovered_by_unpinned_project_is_not_packed_twice(self):
        project = self.entries[0]
        binding = store_registry.project_store(project["service_config"], self.cwd,
            database_path=project["database_path"], name=project["name"], db_alias="user",
            purpose=project["purpose"], read={"enabled": True})
        self.path.write_text(json.dumps({"schema": store_registry.SCHEMA, "stores": self.entries[1:]}))
        selected = store_registry.resolve(self.cwd, registry_path=self.path, project_binding=binding)
        self.config["frozen_selection"] = selected.to_dict()
        self.responses[0]["db_id"] = OWNERS[1]
        self.responses[1]["cards"] = [{**self.card(), "summary": "x" * 10000}]
        result, native = self.execute()
        self.assertEqual(native.call_count, 3)
        self.assertEqual([card["db_id"] for card in result["cards"]], OWNERS[1:])
        self.assertEqual(result["read_coverage"]["unavailable"], 1)
        self.assertEqual(result["selection"]["owners"][1]["outcome"], "duplicate_owner")
        self.assertNotIn("content_omitted_count", result["selection"])

    def test_owner_cache_does_not_collapse_alias_or_ownerless_history(self):
        result, _ = self.execute(emitted={(OWNERS[0], NODE, "a" * 64), (NODE, "a" * 64)})
        self.assertEqual([c["db_id"] for c in result["cards"]], OWNERS[1:])
        self.assertEqual(result["selection"]["seen_count"], 1)

    def test_service_change_before_read_or_during_selector_fences_without_refunding(self):
        Path(self.entries[1]["service_config"]).write_text("{}")
        result, native = self.execute()
        native.assert_not_called()
        self.assertFalse(result["provider_attempt"])
        entry = self.entries[1]
        Path(entry["service_config"]).write_text(json.dumps({"mode": "connect", "url": "http://127.0.0.1:18767/",
            "database_name": "user", "database_path": entry["database_path"]}))
        self.freeze()
        def select(dialogue, cards, **kwargs):
            self.path.write_text(self.path.read_text() + " ")
            return {"reason": "selected", "selected_ids": [c["id"] for c in cards], "concerns": [],
                    "provider_attempt": True, "usage": {"input_tokens": 12, "output_tokens": 3}}
        with patch.object(self, "select", side_effect=select):
            result, native = self.execute()
        self.assertEqual(result["cards"], [])
        self.assertTrue(result["provider_attempt"])
        self.assertEqual(result["usage"], {"input_tokens": 12, "output_tokens": 3})

    def test_durable_fair_read_and_maintenance_cursors_share_one_session(self):
        reader_worker.notice(self.config, {"session_id": "s", "turn_id": "t", "prompt": "Investigate owner sequencing"}, True)
        names = [reader_worker._registry_read_start(self.config, "s") for _ in range(4)]
        self.assertEqual(names, [e["name"] for e in self.entries] + [self.entries[0]["name"]])
        result, native = self.execute(start_owner=names[2])
        self.assertEqual(str(native.call_args_list[0].args[0]), self.entries[2]["service_config"])
        projected = [reader_worker._maintenance_config(self.config, "s") for _ in range(3)]
        self.assertEqual([c["selected_store"] for c in projected],
                         [self.entries[i]["name"] for i in (0, 2, 0)])
        self.assertTrue(all(c["state_dir"] == self.config["state_dir"] for c in projected))

    def test_same_session_config_change_preserves_spending_and_cursors(self):
        event = {"session_id": "s", "turn_id": "t", "prompt": "Investigate owner configuration"}
        reader_worker.notice(self.config, event, True)
        def paid_original_packet(data):
            accounting = data["_accounting"]
            packet = reader_worker._packet(data, data["active"])
            for index in range(3):
                call = f"fixture-original-packet-{index}"
                self.assertTrue(accounting.reserve(packet, call, {}, reader_worker.resolve(self.config)))
                self.assertTrue(accounting.settle(call, {"provider_attempt": index == 0,
                    "usage": {"input_tokens": 123, "output_tokens": 17}}))
            data.update(read_cursor=OWNERS[0], maintenance_cursor=OWNERS[2])
            return None, True
        self.assertTrue(reader_worker._state(self.config, "s", paid_original_packet, create=False)[0])
        self.entries[1]["purpose"] = "Revised description is not a new session allowance."
        self.freeze()
        reader_worker.notice(self.config, {**event, "turn_id": "t2"}, True)
        path, _ = reader_worker._paths(self.config, "s")
        data = json.loads(path.read_text())
        self.assertEqual((data["attempts"], data["input_tokens"], data["output_tokens"]), (3, 123, 17))
        self.assertEqual(data["read_cursor"], OWNERS[0])
        self.assertEqual(data["maintenance_cursor"], OWNERS[2])

    def test_prompt_omission_not_falsely_reported_source_empty(self):
        with patch("registry_reader.prepare", return_value=(None, "prompt_cap")):
            result, native = self.execute()
        self.assertEqual(result["outcome"], "omitted")
        self.assertEqual(result["selection"]["selector_reason"], "prompt_cap")
        self.assertEqual(result["selection"]["postpack_count"], 3)
        self.assertFalse(self.calls)

    def test_exact_episode_edition_survives_selector_aliases_and_delivery(self):
        episode = {**self.card(), "kind": "episode", "episode_id": NODE2,
            "edition_id": NODE, "revision": 1, "current_edition_id": NODE2,
            "occurred": {"kind": "unknown"}, "recorded_at": 1, "edition_recorded_at": 2,
            "thread": None, "recording_session": "exact-recording", "origins": [{"kind": "lexical"}]}
        for response in self.responses:
            response["cards"] = [deepcopy(episode)]
        result, _ = self.execute()
        self.assertEqual(result["outcome"], "selected")
        self.assertEqual(len(result["cards"]), 3)
        prompt, _ = reader_contract.prepare(self.calls[0][0], self.calls[0][1])
        projected = json.loads(prompt[len(reader_contract.PROMPT_PREFIX):])["cards"]
        for owner, card in zip(OWNERS, projected):
            self.assertEqual(card["id"], owner + ":" + NODE)
            self.assertEqual(card["edition_id"], NODE)
            self.assertEqual(card["current_edition_id"], NODE2)
            self.assertEqual(card["recording_session"], "exact-recording")
        packed = hooks._pack_async_delivery(result["cards"], "s", "t")
        self.assertTrue(all(c["edition_id"] == NODE for c in packed["cards"]))
        forged = deepcopy(self.calls[0][1])
        forged[0]["native_id"] = NODE2
        self.assertEqual(reader_contract.prepare(self.calls[0][0], forged), (None, "invalid_input"))

    def test_conditional_witness_never_borrows_binding_from_other_owner(self):
        for owner, response in zip(OWNERS, self.responses):
            binding = {"db_id": owner, "route": {"previous": NODE2, "target": NODE,
                "from": NODE2, "to": NODE, "previous_fingerprint": "a" * 64,
                "target_fingerprint": "b" * 64, "edge_fingerprint": "c" * 64}}
            response["observation"] = {"schema": 1, "learning": "disabled", "cards": [
                {"node_id": NODE, "entry_kind": "conditional", "graph_path": None,
                 "conditional_binding": binding}]}
        self.responses[1]["observation"]["cards"][0]["conditional_binding"]["db_id"] = OWNERS[0]
        result, _ = self.execute()
        self.assertEqual([c["entry_kind"] for c in result["cards"]], ["conditional"] * 3)
        self.assertEqual(result["cards"][0]["conditional_binding"]["db_id"], OWNERS[0])
        self.assertNotIn("conditional_binding", result["cards"][1])
        self.assertEqual(result["cards"][2]["conditional_binding"]["db_id"], OWNERS[2])
        self.assertTrue(all("conditional_binding" not in c for c in self.calls[0][1]))
        packed = hooks._pack_async_delivery(result["cards"], "s", "t")
        self.assertEqual(packed["displayed"][2]["conditional_binding"]["db_id"], OWNERS[2])

    def test_same_owner_concerns_are_warning_only_not_maintenance_authority(self):
        for response in self.responses:
            response["cards"].append(self.card(NODE2))
        self.selected_concerns = [
            {"kind": "disagreement", "left_id": owner + ":" + NODE,
             "right_id": owner + ":" + NODE2, "caveat": "The accounts differ.",
             "missing_fact": "Which task applied?"} for owner in OWNERS]
        self.selected_concerns.append({"kind": "disagreement", "left_id": OWNERS[0] + ":" + NODE,
            "right_id": OWNERS[1] + ":" + NODE, "caveat": "Different owners.", "missing_fact": "Scope?"})
        with patch("hook_recall.register_reader_concerns") as write:
            result, _ = self.execute()
        write.assert_not_called()
        warnings = result["concerns"]
        self.assertEqual([warning["db_id"] for warning in warnings], OWNERS)
        self.assertTrue(all(c["expected_row"] is None for c in warnings))
        self.assertNotIn("concern_nominations", result)
        self.assertEqual(result["routing_mode"], "native_only")
        self.assertEqual(result["selection"]["concern_mode"], "warning_only")
        packed = hooks._pack_async_delivery(result["cards"], "s", "t", concerns=warnings)
        self.assertTrue(all(c["db_id"] in OWNERS for c in packed["concerns"]))

    def test_unknown_failed_native_read_fences_remaining_work_without_fabricating_count(self):
        def collect(path, cue, root, **kwargs):
            kwargs["shared_work"].update(decoded_bytes=137, accounting_unknown=True)
            return {"outcome": "unavailable", "cards": []}
        result, native = self.execute(effect=collect)
        self.assertEqual(native.call_count, 1)
        self.assertEqual(result["read_coverage"]["budget_omitted"], 2)
        work = result["selection"]["native_work"]
        self.assertIsNone(work["decoded_bytes"])
        self.assertEqual(work["known_decoded_bytes"], 137)
        self.assertTrue(work["accounting_unknown"])

    def test_owner_qualified_existing_concern_rows_reach_prompt_without_id_collision(self):
        for owner, response in zip(OWNERS, self.responses):
            response["cards"].append(self.card(NODE2))
            row = {"notice": {"binding": {"key": {"lo": NODE, "hi": NODE2, "kind": "disagreement"},
                    "endpoints": [{"id": NODE, "meaning": "a" * 64}, {"id": NODE2, "meaning": "b" * 64}]},
                "concern": "Account differs for " + owner, "missing_fact": "Which context?"}, "finding": None}
            response["concern_rows"] = [row]
        result, _ = self.execute(emitted={(OWNERS[1], NODE, "a" * 64)})
        prompt, _ = reader_contract.prepare(self.calls[0][0], self.calls[0][1], **self.calls[0][2])
        projected = json.loads(prompt[len(reader_contract.PROMPT_PREFIX):])
        self.assertEqual([row["endpoint_ids"] for row in projected["concern_rows"]],
                         [[owner + ":" + NODE, owner + ":" + NODE2] for owner in OWNERS])
        self.assertEqual(len(result["cards"]), 6)
        self.assertEqual(result["selection"]["seen_count"], 0)

    def worker_delivery(self):
        from reader_test_support import PublicationHandoff
        event = {"session_id": "s", "turn_id": "t", "prompt": "Investigate across named owners"}
        reader_worker.notice(self.config, event, True)
        def collect(path, cue, root, **kwargs):
            index = next(i for i, entry in enumerate(self.entries) if entry["service_config"] == str(path))
            kwargs["shared_work"]["decoded_bytes"] += 200
            return deepcopy(self.responses[index])
        self.close = lambda: None
        with (patch("hook_recall.collect_reader", side_effect=collect),
              patch("recording_jobs.has_work", return_value=False),
              patch("recording_jobs.close_turn"), patch("stewardship.step"),
              PublicationHandoff(reader_worker, "s") as handoff):
            worker = threading.Thread(target=lambda: reader_worker.serve(self.config, "s",
                runtime_factory=lambda *_: self, idle_seconds=10))
            worker.start()
            try:
                handoff.wait()
                result = reader_worker.consume(self.config, event)
                path, _ = reader_worker._paths(self.config, "s")
                state = json.loads(path.read_text())
            finally:
                reader_worker.end_session(self.config, "s")
                handoff.finish()
                worker.join(2)
            self.assertFalse(worker.is_alive())
        return result, state

    def test_worker_publishes_one_paid_packet_and_partial_coverage(self):
        self.responses[1] = {"outcome": "unavailable", "cards": []}
        result, state = self.worker_delivery()
        self.assertEqual(result["outcome"], "emitted")
        self.assertEqual([c["db_id"] for c in result["cards"]], [OWNERS[0], OWNERS[2]])
        self.assertEqual(result["read_coverage"]["unavailable"], 1)
        self.assertIn("coverage is partial", result["context"])
        self.assertEqual(state["attempts"], 1)
        self.assertEqual(state["input_tokens"], 10)
        self.assertIsNone(state["reservation"])
        self.assertEqual(len(state["emitted"]), 2)
        self.assertEqual([key[0] for key in state["emitted"]], [OWNERS[0], OWNERS[2]])

    def test_worker_delivers_coverage_only_without_invented_empty_store_or_model_spend(self):
        self.responses = [{"outcome": "unavailable", "cards": []} for _ in OWNERS]
        result, state = self.worker_delivery()
        self.assertEqual(result["outcome"], "omitted")
        self.assertEqual(result["cards"], [])
        self.assertEqual(result["read_coverage"]["unavailable"], 3)
        self.assertIn("coverage is partial", result["context"])
        self.assertIn("not evidence that the missing stores are empty", result["context"])
        self.assertEqual(self.calls, [])
        self.assertEqual(state["attempts"], 0)
        self.assertEqual(state["input_tokens"], 0)
        self.assertIsNone(state["reservation"])
        self.assertEqual(state["emitted"], [])
        self.assertEqual(state["counts"]["selected"], 0)

    def test_generic_purpose_is_prompt_data_not_new_system_authority(self):
        result, _ = self.execute()
        prompt, offered = reader_contract.prepare(self.calls[0][0], self.calls[0][1])
        self.assertTrue(all(entry["purpose"] in prompt for entry in self.entries))
        self.assertIn("not authority", reader_contract.BASE_INSTRUCTIONS)


if __name__ == "__main__":
    unittest.main()

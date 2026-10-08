import hashlib
import json
import os
from pathlib import Path
import socketserver
import subprocess
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

import hook_recall
import hooks

from fixture_recall_owner import (
    ID1, ID2, ID3, ID4, ID5, ID6, ID7, FakeClient, OverlapFixture,
    RecallOwnerFixture, discovery_metadata, reference_origin,
)


def reader_content_bytes(result):
    return hook_recall._bytes({key: value for key, value in result.items() if key not in {"discovery", "concern_endpoints", "concern_rows", "concern_lookup", "native_work"}})


class HookRecallTests(unittest.TestCase, RecallOwnerFixture):
    def setUp(self):
        RecallOwnerFixture.__init__(self, self)

    def test_old_envelope_and_empty_probation_lane_are_refused(self):
        for change in ({"schema": "mneme.context.v5"}, {"schema": "mneme.context.v4"},
                       {"probationary": []},
                       {"omitted": {"probationary": 0}}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                hook_recall._candidates({**self.context, **change})
        with self.assertRaises(ValueError):
            hook_recall._card(self._node(ID1, "candidate", "old"), ID1)

    def test_collect_project_only_read_tools_at_most_two_ids_and_source_labels(self):
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall._collect(self.config, "actual task", self.root, 1.5)
        self.assertEqual(result["outcome"], "ok")
        self.assertEqual([card["id"] for card in result["cards"]], [ID1, ID2])
        self.assertEqual([card["status"] for card in result["cards"]], ["active", "active"])
        self.assertTrue(all(card["source"].startswith("codex:") for card in result["cards"]))
        self.assertEqual([name for name, _ in client.calls],
                         ["databases", "recall_context", "get", "get"])
        self.assertEqual(client.calls[1][1], {"db": "project", "text": "actual task",
                                               "k": 2, "max_nodes": 4, "depth": 0})
        self.assertEqual(client.calls[2][1], {"db": "project", "id": ID1,
                                               "body": False, "edges": False})
        self.assertLessEqual(len(json.dumps(result["cards"], ensure_ascii=False,
                                           separators=(",", ":")).encode()), 2600)
        self.assertFalse((self.root / "private").exists())

    def test_reader_carries_only_guarded_project_identity_and_never_retries_failed_guard(self):
        self.catalog[0]["db_id"] = ID7
        self.context["primary"] = [{"id": ID1}]
        self.context["expansions"] = []
        self.context["observation"] = {"schema": 1, "learning": "disabled", "cards": [
            {"node_id": ID1, "lane": "primary", "card_sha256": "a" * 64, "graph_path": None}]}
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall.collect_reader(self.config, "actual task", self.root)
        self.assertEqual(result["db_id"], ID7)
        self.assertEqual(client.calls[-1][1]["expected_db_id"], ID7)
        self.assertEqual(result["cards"][0]["fingerprint"],
                         hook_recall._card(self.nodes[ID1], ID1)["fingerprint"])
        def fail_guard(name, args):
            client.calls.append((name, args))
            if name == "databases": return self.catalog
            if name == "recall_context": return self.context
            raise hook_recall.McpError("guard refused")
        client.calls.clear()
        client.call_tool = fail_guard
        with patch("hook_recall.McpClient", return_value=client):
            self.assertEqual(hook_recall.collect_reader(self.config, "actual task", self.root)["outcome"],
                             "unavailable")
        self.assertEqual([name for name, _ in client.calls], ["databases", "recall_context", "get"])

    def test_catalog_without_canonical_db_id_preserves_recall_without_delivery_binding(self):
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall._collect(self.config, "actual task", self.root, 1.5)
        self.assertEqual(result["outcome"], "ok")
        self.assertNotIn("db_id", result)
        self.assertNotIn("expected_db_id", client.calls[2][1])

    def test_ordinary_nonreader_recall_does_not_acquire_optional_reader_guard(self):
        self.catalog[0]["db_id"] = ID7
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall._collect(self.config, "actual task", self.root, 1.5)
        self.assertEqual(result["outcome"], "ok")
        self.assertNotIn("db_id", result)
        self.assertTrue(all("expected_db_id" not in args for name, args in client.calls if name == "get"))

    def test_opt_in_observation_is_metadata_not_feedback_and_filters_to_kept_cards(self):
        self.context["observation"] = {"schema": 1, "learning": "disabled", "cards": [
            {"node_id": ID1, "lane": "primary", "card_sha256": "a" * 64, "graph_path": None},
            {"node_id": ID2, "lane": "primary", "card_sha256": "b" * 64,
             "graph_path": [{"previous": ID1, "target": ID2, "from": ID1,
                             "to": ID2, "kind": "supports", "anchor": None}]},
            {"node_id": ID3, "lane": "expansions", "card_sha256": "c" * 64, "graph_path": None}]}
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall._collect(self.config, "actual task", self.root, 1.5, observe=True)
        self.assertEqual(client.calls[1][1]["observe"], True)
        self.assertEqual(client.calls[1][1]["depth"], 2)
        self.assertEqual([c["node_id"] for c in result["observation"]["cards"]], [ID1, ID2])
        self.assertEqual(result["observation"]["learning"], "disabled")
        self.assertEqual(result["observation"]["cards"][1]["graph_path"][0]["target"], ID2)
        self.assertFalse(any(name in ("reflect", "capture", "episode") for name, _ in client.calls))
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            ordinary = hook_recall._collect(self.config, "actual task", self.root, 1.5)
        self.assertNotIn("observe", client.calls[1][1])
        self.assertEqual(client.calls[1][1]["depth"], 0)
        self.assertNotIn("observation", ordinary)
        episode = {"id": ID3}
        side = {"observation": {"schema": 1, "learning": "disabled", "cards": [
            {"node_id": ID3, "lane": "episodic", "card_sha256": "d" * 64, "graph_path": None}]}}
        self.assertEqual(hook_recall._observed_cards(side, [episode])["cards"][0]["lane"], "episodic")

    def test_archived_readback_is_skipped_without_extra_get_and_empty_is_distinct(self):
        self.nodes[ID1]["status"] = "archived"
        self.nodes[ID2]["status"] = "archived"
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall._collect(self.config, "task", self.root, 1.5)
        self.assertEqual(result, {"outcome": "empty", "cards": [], "elapsed_ms": 0})
        self.assertEqual([args["id"] for name, args in client.calls if name == "get"], [ID1, ID2])

    def test_core_cards_cannot_starve_native_task_lanes(self):
        self.context["core"] = [{"id": ID1}, {"id": ID2}]
        self.context["primary"] = [{"id": ID3}, {"id": ID2}]
        self.context["expansions"] = []
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall._collect(self.config, "actual task", self.root, 1.5)
        self.assertEqual([card["id"] for card in result["cards"]], [ID3, ID2])
        self.assertEqual([args["id"] for name, args in client.calls if name == "get"], [ID3, ID2])
        self.context["primary"] = []
        self.context["expansions"] = []
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            self.assertEqual(hook_recall._collect(self.config, "task", self.root, 1.5)["outcome"],
                             "empty")
        self.assertFalse(any(name == "get" for name, _ in client.calls))

    def _episode(self, identifier=ID3, **changes):
        entry = {"id": identifier, "kind": "episode", "episode_id": ID2,
                 "edition_id": identifier, "revision": 1,
                 "current_edition_id": identifier, "occurred": {"kind": "point", "at": 12},
                 "recorded_at": 15, "edition_recorded_at": 20, "thread": "workshop",
                 "recording_session": None, "origins": [{"kind": "lexical"}]}
        entry.update(changes)
        node = self._node(identifier, "active", "Removed the workshop deadline")
        node["created"] = entry["edition_recorded_at"]
        node["memory_kind"] = {"kind": "episode", "episode": {
            key: entry[key] for key in ("episode_id", "revision", "occurred", "recorded_at", "thread")}}
        if entry["recording_session"] is not None:
            node["provenance"]["source"]["session"] = entry["recording_session"]
        node["memory_kind"]["episode"].update(revises=ID2, edit_reason="clarify")
        if "occurrence_contexts" in entry:
            node["memory_kind"]["episode"]["occurrence_contexts"] = entry["occurrence_contexts"]
        self.nodes[identifier] = node
        return entry

    def test_v6_reserves_one_card_for_each_kind_and_preserves_episode_readback(self):
        episode = self._episode()
        self.context.update(schema="mneme.context.v6", episodes=[episode])
        # A valid typed result cannot duplicate an edition in semantic lanes.
        self.context["expansions"] = []
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall._collect(self.config, "workshop timeout", self.root, 1.5)
        self.assertEqual([card["id"] for card in result["cards"]], [ID1, ID3])
        self.assertEqual([card["kind"] for card in result["cards"]], ["semantic", "episode"])
        card = result["cards"][1]
        for key in hook_recall.EPISODE_FIELDS:
            self.assertEqual(card[key], episode[key])
        self.assertEqual([name for name, _ in client.calls], ["databases", "recall_context", "get", "get"])
        self.assertLessEqual(len(json.dumps(result["cards"], ensure_ascii=False, separators=(",", ":")).encode()),
                             hook_recall.MAX_CARDS_BYTES)

    def test_v6_episodic_only_and_empty_lanes(self):
        episodes = [self._episode(ID1), self._episode(ID3)]
        self.context.update(schema="mneme.context.v6", primary=[], expansions=[], episodes=episodes)
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall._collect(self.config, "deadline", self.root, 1.5)
        self.assertEqual([card["id"] for card in result["cards"]], [ID1, ID3])
        self.assertTrue(all(card["kind"] == "episode" for card in result["cards"]))
        self.context["episodes"] = []
        self.assertEqual(hook_recall._candidates(self.context), [])

    def test_v6_readback_cannot_silently_flatten_or_change_an_episode(self):
        episode = self._episode()
        candidate = dict(episode)
        for change in ({"memory_kind": {"kind": "semantic"}}, {"created": 21},
                       {"memory_kind": {"kind": "episode", "episode": {}}}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                hook_recall._card({**self.nodes[ID3], **change}, ID3, candidate)
        changed = {**self.nodes[ID3], "memory_kind": {"kind": "episode", "episode": {
            **self.nodes[ID3]["memory_kind"]["episode"], "thread": "other"}}}
        with self.assertRaisesRegex(ValueError, "episode readback mismatch"):
            hook_recall._card(changed, ID3, candidate)
        self.context.update(schema="mneme.context.v6", expansions=[], episodes=[{**episode, "edition_id": ID1}])
        with self.assertRaisesRegex(ValueError, "episode edition identity mismatch"):
            hook_recall._candidates(self.context)

    def test_episode_metadata_participates_in_fingerprint(self):
        episode = self._episode()
        original = hook_recall._card(self.nodes[ID3], ID3, episode)
        episode = self._episode(thread="other-work")
        changed = hook_recall._card(self.nodes[ID3], ID3, episode)
        self.assertEqual(original["summary"], changed["summary"])
        self.assertNotEqual(original["fingerprint"], changed["fingerprint"])

    def test_episode_account_fingerprint_binds_full_exact_immutable_readback(self):
        episode = self._episode()
        self.nodes[ID3]["summary"] = "x" * 800
        first = hook_recall._card(self.nodes[ID3], ID3, episode)
        self.nodes[ID3]["summary"] += "undisplayed tail"
        changed = hook_recall._card(self.nodes[ID3], ID3, episode)
        self.assertEqual(first["summary"], changed["summary"])
        self.assertNotEqual(first["fingerprint"], changed["fingerprint"])
        self.nodes[ID3]["memory_kind"]["episode"]["edit_reason"] = "Different exact editorial reason"
        edited = hook_recall._card(self.nodes[ID3], ID3, episode)
        self.assertNotEqual(changed["fingerprint"], edited["fingerprint"])
        self.nodes[ID3]["provenance"]["source"]["digest"] = "d" * 64
        sourced = hook_recall._card(self.nodes[ID3], ID3, episode)
        self.assertEqual(edited["source"], sourced["source"])
        self.assertNotEqual(edited["fingerprint"], sourced["fingerprint"])
        for malformed in ({"revision": True}, {"occurred": {"kind": "point", "at": True}}):
            self.nodes[ID3]["memory_kind"]["episode"].update(malformed)
            with self.assertRaisesRegex(ValueError, "episode readback mismatch"):
                hook_recall._card(self.nodes[ID3], ID3, episode)
            self.nodes[ID3]["memory_kind"]["episode"].update({key: episode[key] for key in malformed})

    def test_occurrence_contexts_are_exact_readback_and_fingerprint_data(self):
        original = self._episode()
        old_card = hook_recall._card(self.nodes[ID3], ID3, original)
        refs = [{"namespace": "session", "key": "pi", "label": "Earlier work"}]
        episode = self._episode(occurrence_contexts=refs)
        candidate = hook_recall._candidate(episode, "episodes")
        card = hook_recall._card(self.nodes[ID3], ID3, candidate)
        self.assertEqual(card["occurrence_contexts"], refs)
        self.assertNotEqual(old_card["fingerprint"], card["fingerprint"])
        for changes in ({}, {"occurrence_contexts": None},
                        {"occurrence_contexts": [{"namespace": "session", "key": "mac"}]}):
            facet = {key: value for key, value in self.nodes[ID3]["memory_kind"]["episode"].items()
                     if key != "occurrence_contexts"}
            facet.update(changes)
            node = {**self.nodes[ID3], "memory_kind": {"kind": "episode", "episode": facet}}
            with self.subTest(changes=changes), self.assertRaisesRegex(ValueError, "context readback mismatch"):
                hook_recall._card(node, ID3, candidate)
        # A new field appearing only in the exact readback is also a mismatch.
        with self.assertRaisesRegex(ValueError, "context readback mismatch"):
            hook_recall._card(self.nodes[ID3], ID3, original)

    def test_v6_mixed_unicode_cards_remain_byte_bounded(self):
        episode = self._episode(thread="é" * 64)
        self.context.update(schema="mneme.context.v6", expansions=[], episodes=[episode])
        for node in self.nodes.values():
            node["summary"] = "é" * 1000
            node["provenance"] = {"type": "web", "url": "https://example.test/" + "é" * 128}
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall._collect(self.config, "workshop", self.root, 1.5)
        self.assertLessEqual(len(json.dumps(result["cards"], ensure_ascii=False, separators=(",", ":")).encode()),
                             hook_recall.MAX_CARDS_BYTES)
        self.assertEqual([card["kind"] for card in result["cards"]], ["semantic", "episode"])
        self.assertTrue(all(len(card["summary"].encode()) <= hook_recall.MAX_SUMMARY_BYTES
                            for card in result["cards"]))

    def test_reader_candidate_ninth_survives_without_count_ceiling(self):
        ids = ["01ARZ3NDEKTSV4RRFFQ69G5FA" + str(i) for i in range(9)]
        context = {"schema":"mneme.context.v6", "core":[],
                   "primary":[{"id":i} for i in ids], "expansions":[], "episodes":[]}
        self.assertEqual([c["id"] for c in hook_recall._candidates(context, reader=True)], ids)

    def test_reader_wider_pool_keeps_lane_diversity_and_native_paths(self):
        episode = self._episode(ID7)
        for identifier in (ID4, ID5, ID6):
            self.nodes[identifier] = self._node(identifier, "active", "é" * 1000)
        self.context.update(schema="mneme.context.v6",
                            primary=[{"id": identifier} for identifier in (ID1, ID2, ID3, ID4, ID5)],
                            expansions=[{"id": ID6}],
                            episodes=[episode])
        path = [{"previous": ID1, "target": ID6, "from": ID1,
                 "to": ID6, "kind": "supports", "anchor": "real edge"}]
        self.context["observation"] = {"schema": 1, "learning": "disabled", "cards": [
            {"node_id": identifier, "lane": lane, "card_sha256": "a" * 64,
             "graph_path": path if identifier == ID6 else None}
            for identifier, lane in ((ID1, "primary"), (ID2, "primary"),
                                     (ID3, "primary"), (ID4, "primary"),
                                     (ID5, "primary"), (ID6, "expansions"),
                                     (ID7, "episodic"))]}
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall.collect_reader(self.config, "decision about workshop", self.root)
        self.assertEqual(result["outcome"], "ok")
        self.assertEqual(client.calls[1][1], {"db": "project", "text": "decision about workshop",
                                              "k": hook_recall.reader_plan([{ "role": "user", "text": "decision about workshop"}])[0]["k"], "max_nodes": hook_recall.reader_plan([{ "role": "user", "text": "decision about workshop"}])[0]["max_nodes"], "depth": 2, "observe": True})
        self.assertEqual([card["id"] for card in result["cards"]],
                         [ID1, ID6, ID2, ID7, ID3, ID4, ID5])
        self.assertEqual(len([name for name, _ in client.calls if name == "get"]), 7)
        self.assertTrue(all(hook_recall._bytes(card) <= hook_recall.MAX_READER_CARD_BYTES
                            for card in result["cards"]))
        self.assertTrue(all(hooks._valid_card(card) for card in result["cards"]))
        self.assertLessEqual(reader_content_bytes(result), hook_recall.MAX_READER_BYTES)
        self.assertLessEqual(hook_recall._bytes(result), hook_recall.MAX_READER_RESULT_BYTES)
        self.assertEqual(result["cards"][3]["kind"], "episode")
        self.assertEqual(result["cards"][3]["episode_id"], episode["episode_id"])
        observed = {row["node_id"]: row for row in result["observation"]["cards"]}
        self.assertEqual(observed[ID6]["graph_path"], path)
        self.assertIsNone(observed[ID1]["graph_path"])
        self.assertIn(ID5, observed)
        bounded_client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=bounded_client), \
             patch("hook_recall.MAX_READER_BYTES", 2400):
            bounded = hook_recall.collect_reader(self.config, "decision about workshop", self.root)
        self.assertEqual(bounded["outcome"], "ok")
        self.assertTrue(bounded["cards"])
        self.assertLess(len(bounded["cards"]), 7)
        self.assertLessEqual(reader_content_bytes(bounded), 2400)
        self.assertLessEqual(hook_recall._bytes(bounded), 2400 + hook_recall.MAX_READER_CONTROL_BYTES)
        self.assertEqual(bounded_client.calls[1][1]["k"], client.calls[1][1]["k"])

    def test_reader_long_source_episode_keeps_meaningful_summary(self):
        episode = self._episode(ID3, thread="é" * 64)
        self.nodes[ID3]["summary"] = "é" * 500
        self.nodes[ID3]["provenance"] = {"type": "web", "url": "https://example.test/" + "é" * 150}
        self.context.update(schema="mneme.context.v6", primary=[], expansions=[], episodes=[episode],
                            observation={"schema": 1, "learning": "disabled", "cards": [
                                {"node_id": ID3, "lane": "episodic", "card_sha256": "a" * 64,
                                 "graph_path": None}]})
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall.collect_reader(self.config, "workshop", self.root)
        self.assertEqual(result["outcome"], "ok")
        card = result["cards"][0]
        # Added v6 origins/session coordinates cost real bytes; only the
        # presentation summary may shrink, never the complete metadata.
        self.assertLessEqual(len(card["summary"].encode("utf-8")), 700)
        self.assertGreater(len(card["summary"].encode("utf-8")), 600)
        self.assertEqual(card["summary"], "é" * (len(card["summary"].encode("utf-8")) // 2))
        self.assertLessEqual(hook_recall._bytes(card), hook_recall.MAX_READER_CARD_BYTES)
        self.assertEqual(card["thread"], episode["thread"])
        self.assertTrue(hooks._valid_card(card))

    def test_v6_historical_reference_head_movement_changes_view_not_account(self):
        episode = self._episode(ID3)
        self.catalog[0]["db_id"] = ID7
        self.context.update(discovery_metadata(), primary=[], expansions=[], episodes=[episode],
                            observation={"schema": 1, "learning": "disabled", "cards": [
                                {"node_id": ID3, "lane": "episodic", "card_sha256": "a" * 64,
                                 "graph_path": None}]})
        client = FakeClient(self.catalog, self.context, self.nodes)
        original_read = client.call_tool

        def move_head_after_recall(name, args):
            value = original_read(name, args)
            if name == "get":
                self.context["episodes"][0]["current_edition_id"] = ID4
                self.context["episodes"][0]["origins"] = [reference_origin(ID3)]
            return value

        client.call_tool = move_head_after_recall
        with patch("hook_recall.McpClient", return_value=client):
            before = hook_recall._collect(self.config, "workshop scene", self.root, 1.5, reader=True)
            after = hook_recall._collect(self.config, "linked scene", self.root, 1.5, reader=True)
        first, second = before["cards"][0], after["cards"][0]
        self.assertEqual(first["current_edition_id"], ID3)  # Recall-time observation, not fresh head.
        self.assertEqual(second["current_edition_id"], ID4)
        self.assertEqual(second["edition_id"], ID3)  # Exact old account, never substitute the head.
        self.assertEqual(first["fingerprint"], second["fingerprint"])
        self.assertNotEqual(first, second)
        self.assertNotEqual(hashlib.sha256(json.dumps(first, sort_keys=True).encode()).hexdigest(),
                            hashlib.sha256(json.dumps(second, sort_keys=True).encode()).hexdigest())
        self.assertEqual([args["id"] for name, args in client.calls if name == "get"], [ID3, ID3])
        self.assertTrue(all(args["expected_db_id"] == ID7 for name, args in client.calls if name == "get"))
        self.assertEqual(set(name for name, _ in client.calls), {"databases", "recall_context", "get"})

    def test_v6_multiple_origins_and_exact_nullable_recording_session(self):
        origins = [{"kind": "lexical"}, reference_origin(ID3),
                   reference_origin(ID3, episode_anchor=True)]
        episode = self._episode(ID3, origins=origins, recording_session="late recap 🐙")
        candidate = hook_recall._candidate(episode, "episodes")
        card = hook_recall._card(self.nodes[ID3], ID3, candidate)
        self.assertEqual(card["origins"], origins)
        self.assertEqual(card["recording_session"], "late recap 🐙")
        self.assertNotIn(card["recording_session"], card["source"])
        fingerprint = card["fingerprint"]
        for session in (None, "late recap 🐙!"):
            changed = self._episode(ID3, origins=origins, recording_session=session)
            changed_card = hook_recall._card(self.nodes[ID3], ID3, changed)
            self.assertEqual(changed_card["recording_session"], session)
            self.assertNotEqual(changed_card["fingerprint"], fingerprint)
        changed["recording_session"] = "forged recording coordinate"
        with self.assertRaisesRegex(ValueError, "recording session readback mismatch"):
            hook_recall._card(self.nodes[ID3], ID3, changed)
        # Unverified coordinates cannot be salvaged as a semantic card.
        for missing in ("recording_session", "origins"):
            with self.subTest(missing=missing), self.assertRaises(ValueError):
                hook_recall._candidate({k: v for k, v in episode.items() if k != missing}, "episodes")

    def test_episode_origin_metadata_overflow_omits_whole_card_not_origins(self):
        episode = self._episode(ID3, recording_session="x" * 512,
                                origins=[reference_origin(ID3, anchor_id=f"anchor-{i}" * 10)
                                         for i in range(14)])
        card = hook_recall._card(self.nodes[ID3], ID3, episode)
        original_origins = json.loads(json.dumps(card["origins"]))
        self.assertIsNone(hook_recall._reader_card(card))
        self.assertEqual(card["origins"], original_origins)
        self.assertEqual(card["recording_session"], "x" * 512)
        later = hook_recall._card(self.nodes[ID1], ID1)
        self.assertIsNotNone(hook_recall._reader_card(later))

    def test_lexical_reference_and_presentation_coverage_are_independent(self):
        metadata = discovery_metadata()
        metadata["episodic_retrieval"]["state"] = "not_searched_tag_filter"
        references = metadata["episode_reference_retrieval"]
        references.update(anchors_total=3, anchors_examined=2, raw_edges_scanned=8,
                          raw_edge_limit=8, endpoint_reads=3, endpoint_read_limit=4,
                          indexed_seeks=5, edge_point_reads=8, body_anchor_point_reads=2,
                          missing=1, non_episode=1, cache_hits=2, unread_anchors=1,
                          further_tail_unknown=True, stop_reason="budget")
        metadata["omitted"]["episodic"]["bounded_window_budget"] = 2
        self.context.update(metadata)
        adapter = dict.fromkeys(hook_recall._ADAPTER_COUNTS, 0)
        adapter.update(byte_budget_omitted=1)
        projected = hook_recall._discovery(self.context, adapter)
        self.assertEqual(projected["native"]["episodic_retrieval"]["state"], "not_searched_tag_filter")
        self.assertEqual(projected["native"]["episode_reference_retrieval"], references)
        self.assertEqual(projected["native"]["omitted"]["episodic"]["bounded_window_budget"], 2)
        self.assertEqual(projected["adapter"]["byte_budget_omitted"], 1)
        self.assertNotIn("stamp", json.dumps(projected))

    def test_optional_routing_metadata_never_displaces_ordinary_cards(self):
        self.context.update(discovery_metadata())
        self.catalog[0]["db_id"] = ID4
        rows = [{"node_id": identifier, "lane": "primary", "card_sha256": "a" * 64,
                 "graph_path": [{"previous": ID7, "target": identifier, "from": ID7,
                                 "to": identifier, "kind": "associative", "anchor": None}]}
                for identifier in (ID1, ID2, ID3)]
        self.context["observation"] = {"schema": 1, "learning": "disabled", "cards": rows}
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            baseline = hook_recall._collect(self.config, "task", self.root, 1.5, reader=True)
        for row in rows:
            row["routing_binding"] = {"db_id": ID4, "route": {
                **{key: row["graph_path"][-1][key] for key in ("previous", "target", "from", "to")},
                "previous_fingerprint": "b" * 64, "target_fingerprint": "c" * 64,
                "edge_fingerprint": "d" * 64}}
        with patch("hook_recall.McpClient", return_value=client), \
                patch("hook_recall.MAX_READER_BYTES", reader_content_bytes(baseline) + 16):
            result = hook_recall._collect(self.config, "task", self.root, 1.5, reader=True)
        self.assertEqual({k:v for k,v in result.items() if k != "native_work"},
                         {k:v for k,v in baseline.items() if k != "native_work"})
        self.assertEqual(result["discovery"]["adapter"]["byte_budget_omitted"], 0)

    def test_conditional_origin_is_non_direct_even_without_feedback_binding(self):
        self.reader_context([ID1, ID2, ID3])
        rows = self.context["observation"]["cards"]
        rows[0]["entry_kind"] = "conditional"
        rows[0]["conditional_binding"] = {"malformed": "x" * 3000}
        order = [c["id"] for c in hook_recall._candidates(self.context, reader=True)]
        self.assertEqual(order, [ID2, ID1, ID3])
        for observation in (None, {"schema": 9}):
            self.context["observation"] = observation
            self.assertEqual([c["id"] for c in hook_recall._candidates(self.context, reader=True)],
                             [ID1, ID2, ID3])

    def test_conditional_binding_exact_target_db_and_path_exclusivity(self):
        from test_routing_memory import binding, TARGET, DB
        self.context["db_id"] = DB
        route = binding()
        row = {"node_id": TARGET, "lane": "primary", "card_sha256": "a" * 64,
               "graph_path": None, "entry_kind": "conditional", "conditional_binding": route}
        self.context["observation"] = {"schema": 1, "learning": "disabled", "cards": [row]}
        def observed():
            return hook_recall._observed_cards(self.context, [{"id": TARGET}], reader=True)["cards"][0]
        self.assertEqual(observed()["conditional_binding"], route)
        for changes in ({"graph_path": [{"forged": "hop"}]}, {"graph_path": {}},
                        {"routing_binding": route}, {"conditional_binding": None},
                        {"conditional_binding": {"x": "y" * 2000}},
                        {"conditional_binding": {**route, "db_id": ID4}}):
            with self.subTest(changes=changes):
                original = dict(row)
                row.update(changes)
                clean = observed()
                self.assertEqual(clean["entry_kind"], "conditional")
                self.assertIsNone(clean["graph_path"])
                self.assertNotIn("conditional_binding", clean)
                self.assertNotIn("routing_binding", clean)
                row.clear(); row.update(original)
        row.pop("entry_kind")
        self.assertNotIn("conditional_binding", observed())

    def test_conditional_metadata_sheds_before_any_useful_readback_card(self):
        self.reader_context([ID1, ID2, ID3])
        self.catalog[0]["db_id"] = ID4
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            baseline = hook_recall._collect(self.config, "task", self.root, 1.5, reader=True)
        for row in self.context["observation"]["cards"]:
            row.update(entry_kind="conditional", conditional_binding={"db_id": ID4, "route": {
                "previous": ID7, "target": row["node_id"], "from": ID7, "to": row["node_id"],
                "previous_fingerprint": "b" * 64, "target_fingerprint": "c" * 64,
                "edge_fingerprint": "d" * 64}})
        with patch("hook_recall.McpClient", return_value=client), patch(
                "hook_recall.MAX_READER_BYTES", reader_content_bytes(baseline) + 16):
            result = hook_recall._collect(self.config, "task", self.root, 1.5, reader=True)
        self.assertEqual(result["cards"], baseline["cards"])
        self.assertTrue(all("conditional_binding" not in r and "entry_kind" not in r
                            for r in result["observation"]["cards"]))
        self.assertEqual(result["discovery"]["adapter"]["byte_budget_omitted"], 0)

    def test_conflicting_conditional_path_is_not_graph_discovery(self):
        self.reader_context([ID1])
        self.context["observation"]["cards"][0].update(entry_kind="conditional",
            graph_path=[{"forged": "hop"}])
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall._collect(self.config, "task", self.root, 1.5, reader=True)
        self.assertEqual(result["cards"][0]["id"], ID1)
        self.assertIsNone(result["observation"]["cards"][0]["graph_path"])
        adapter = result["discovery"]["adapter"]
        self.assertEqual(adapter["graph_readback_omitted"], 0)
        self.assertEqual(adapter["graph_byte_omitted"], 0)
        self.nodes[ID1]["status"] = "archived"
        with patch("hook_recall.McpClient", return_value=client):
            unavailable = hook_recall._collect(self.config, "task", self.root, 1.5, reader=True)
        self.assertEqual(unavailable["cards"], [])
        self.assertEqual(unavailable["discovery"]["adapter"]["readback_skipped"], 1)
        self.assertEqual(unavailable["discovery"]["adapter"]["graph_readback_omitted"], 0)

    def test_unknown_or_orphan_origin_preserves_order_without_direct_or_graph_salvage(self):
        from test_routing_memory import binding
        for changes in ({"entry_kind": "future"}, {"entry_kind": None},
                        {"conditional_binding": binding()}):
            for graph in (False, True):
                with self.subTest(changes=changes, graph=graph):
                    self.reader_context([ID1, ID2, ID3])
                    row = self.context["observation"]["cards"][0]
                    row.update(changes)
                    if graph:
                        row.update(graph_path=[{"previous": ID7, "target": ID1, "from": ID7,
                            "to": ID1, "kind": "associative", "anchor": None}],
                            routing_binding={"db_id": ID4, "route": {"previous": ID7, "target": ID1,
                                "from": ID7, "to": ID1, "previous_fingerprint": "b" * 64,
                                "target_fingerprint": "c" * 64, "edge_fingerprint": "d" * 64}})
                    # A known conditional later card would otherwise be interleaved
                    # before ID2; unknown origin instead keeps the native ordering.
                    self.context["observation"]["cards"][2]["entry_kind"] = "conditional"
                    self.assertEqual([c["id"] for c in hook_recall._candidates(self.context, reader=True)],
                                     [ID1, ID2, ID3])
                    clean = hook_recall._observed_cards(self.context, [{"id": ID1}], reader=True)["cards"][0]
                    self.assertIsNone(clean["graph_path"])
                    for field in ("routing_binding", "conditional_binding", "entry_kind"):
                        self.assertNotIn(field, clean)

    def reader_context(self, identifiers):
        self.context.update(discovery_metadata())
        self.context.update(primary=[{"id": identifier} for identifier in identifiers], expansions=[], episodes=[],
                            observation={"schema": 1, "learning": "disabled", "cards": [
                                {"node_id": identifier, "lane": "primary", "card_sha256": "a" * 64,
                                 "graph_path": None} for identifier in identifiers]})

    def test_native_omissions_window_cut_readback_skip_and_byte_cut_are_distinct(self):
        ids = ["0" * 25 + str(i) for i in range(10)]
        self.reader_context(ids)
        self.nodes = {identifier: self._node(identifier, "active", "é" * 350) for identifier in ids}
        self.nodes[ids[0]]["status"] = "archived"
        self.context["omitted"]["primary"] = {"bounded_window_budget": 3, "further_tail_unknown": True}
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client), patch("hook_recall.MAX_READER_BYTES", 2400):
            result = hook_recall.collect_reader(self.config, "task", self.root)
        discovery = result["discovery"]
        self.assertEqual(discovery["scope"], "bounded_native_window")
        self.assertEqual(discovery["continuation"], "unavailable")
        self.assertFalse(discovery["native"]["partial"])
        self.assertEqual(discovery["native"]["omitted"]["primary"], self.context["omitted"]["primary"])
        adapter = discovery["adapter"]
        self.assertEqual(adapter["observed_unique_window"], 10)
        self.assertEqual(adapter["readbacks_attempted"], 10)
        self.assertEqual(adapter["readback_skipped"], 1)
        self.assertGreater(adapter["byte_budget_omitted"], 0)
        self.assertEqual(adapter["returned"], len(result["cards"]))
        self.assertEqual(sum(adapter[k] for k in ("work_budget_unread", "readback_skipped", "byte_budget_omitted", "returned")), 10)
        self.assertEqual([name for name, _ in client.calls].count("recall_context"), 1)
        self.assertEqual([name for name, _ in client.calls].count("get"), 10)
        self.assertLessEqual(reader_content_bytes(result), 2400)
        self.assertNotIn("secret", json.dumps(discovery))
        self.assertNotIn("corpus", json.dumps(discovery))

    def test_more_than_sixteen_short_native_cards_and_proven_minima(self):
        import reader_contract
        ids = ["01ARZ3NDEKTSV4RRFFQ69G5F" + f"{i:02}" for i in range(20)]
        self.reader_context(ids)
        self.nodes = {i:self._node(i, "active", "x") for i in ids}
        client = FakeClient(self.catalog,self.context,self.nodes)
        with patch("hook_recall.McpClient",return_value=client):
            result = hook_recall.collect_reader(self.config,"Investigate useful warning",self.root)
        self.assertGreater(len(result["cards"]),8)
        self.assertGreater(len(result["cards"]),16)
        self.assertEqual(sum(name=="recall_context" for name,_ in client.calls),1)
        self.assertLessEqual(reader_content_bytes(result),8192)
        self.assertLessEqual(hook_recall._bytes(result),10240)
        prompt,ctx = reader_contract.prepare([{ "role":"user","text":"Investigate useful warning"}],result["cards"])
        projected = json.loads(prompt.split("PAYLOAD:\n",1)[1])["cards"]
        for card,item in zip(result["cards"],projected):
            self.assertGreaterEqual(len(reader_contract._encode(card)),reader_contract.MIN_VERIFIED_CARD_BYTES)
            self.assertGreaterEqual(len(reader_contract._encode(item)),reader_contract.MIN_PROJECTED_CARD_BYTES)
        self.assertEqual(len(ctx["ids"]),len(result["cards"]))

    def test_one_response_read_overshoot_stops_later_work_not_kept_card(self):
        from librarian_policy import LibrarianBudget
        self.reader_context([ID1,ID2,ID3])
        self.nodes[ID1] = self._node(ID1,"active","x"*35000)
        client = FakeClient(self.catalog,self.context,self.nodes)
        with patch("hook_recall.McpClient",return_value=client):
            result = hook_recall.collect_reader(self.config,"Investigate warning",self.root,
                                               budget=LibrarianBudget(effort="low"))
        self.assertEqual([c["id"] for c in result["cards"]],[ID1])
        self.assertEqual(sum(name=="get" for name,_ in client.calls),1)
        self.assertGreater(result["native_work"]["decoded_bytes"],32768)
        self.assertTrue(result["native_work"]["read_allowance_exhausted"])
        self.assertEqual(result["discovery"]["adapter"]["work_budget_unread"],2)
        self.assertEqual(result["discovery"]["adapter"]["readback_skipped"],0)
        self.assertLessEqual(hook_recall._bytes(result),10240)

    def test_absent_or_malformed_native_coverage_does_not_drop_valid_cards(self):
        self.reader_context([ID1, ID2])
        baseline_context = dict(self.context)
        for change in ({"partial": []}, {"retrieval": None}, {"omitted": {}}, {"episodic_retrieval": {"state": "invented"}}):
            self.context = {**baseline_context, **change}
            client = FakeClient(self.catalog, self.context, self.nodes)
            with patch("hook_recall.McpClient", return_value=client):
                result = hook_recall.collect_reader(self.config, "task", self.root)
            self.assertEqual([c["id"] for c in result["cards"]], [ID1, ID2])
            self.assertEqual(result["discovery"]["native"]["state"], "unknown")
        self.context = {k: v for k, v in baseline_context.items() if k not in discovery_metadata()}
        with patch("hook_recall.McpClient", return_value=FakeClient(self.catalog, self.context, self.nodes)):
            result = hook_recall.collect_reader(self.config, "task", self.root)
        self.assertEqual(result["discovery"]["native"], {"state": "unknown", "reason": "absent"})
        self.assertEqual(len(result["cards"]), 2)

    def test_empty_window_and_episodic_nonsearch_states_never_mean_corpus_complete(self):
        self.reader_context([])
        for state, reason in (("not_searched", None), ("not_searched_tag_filter", None),
                              ("unavailable", "adapter_unsupported"), ("unavailable", "store_not_upgraded")):
            self.context["episodic_retrieval"] = {"state": state, "mode": "lexical",
                                                 "cue_normalized": False, "cue_truncated": False}
            if reason:
                self.context["episodic_retrieval"]["unavailable_reason"] = reason
            with patch("hook_recall.McpClient", return_value=FakeClient(self.catalog, self.context, self.nodes)):
                result = hook_recall.collect_reader(self.config, "task", self.root)
            self.assertEqual(result["outcome"], "empty")
            self.assertEqual(result["discovery"]["native"]["episodic_retrieval"], self.context["episodic_retrieval"])
            self.assertEqual(result["discovery"]["adapter"]["returned"], 0)
            self.assertEqual(result["discovery"]["continuation"], "unavailable")
            self.assertNotIn("complete", json.dumps(result["discovery"]))

    def test_tagged_coverage_is_exact_source_projection_or_bounded_unknown_marker(self):
        self.reader_context([ID1])
        retrieval = self.context["retrieval"]
        retrieval.update(mode="tagged", partial=True,
                         work={key: i for i, key in enumerate(hook_recall._TAGGED_WORK_FIELDS)})
        seed = {"strategy": "lifecycle_hnsw_and_hashed_tag_sample_postfilter", "exceeded_limit": "raw_memberships",
                "raw_memberships": 12, "canonical_candidates_checked": 2, "matching_candidates": 1,
                "physical": [{"status": "active", "hnsw_quota": 4, "hnsw_inspected": 4, "sample_pivot": -7,
                              "tag_samples": [{"query_tag_index": 0, "quota": 4, "inspected": 2}]}]}
        retrieval["lanes"]["primary"]["seed_coverage"] = seed
        with patch("hook_recall.McpClient", return_value=FakeClient(self.catalog, self.context, self.nodes)):
            baseline = hook_recall.collect_reader(self.config, "task", self.root)
        native = baseline["discovery"]["native"]
        self.assertEqual(native["retrieval"]["work"], retrieval["work"])
        self.assertEqual(native["retrieval"]["lanes"]["primary"]["seed_coverage"], seed)
        self.assertTrue(native["retrieval"]["partial"])
        self.assertNotIn("stamp", native["retrieval"])
        seed["physical"][0]["tag_samples"] = [{"query_tag_index": i, "quota": 4, "inspected": 2} for i in range(32)]
        with patch("hook_recall.McpClient", return_value=FakeClient(self.catalog, self.context, self.nodes)):
            oversized = hook_recall.collect_reader(self.config, "task", self.root)
        self.assertEqual(oversized["cards"], baseline["cards"])
        self.assertEqual(oversized["observation"], baseline["observation"])
        self.assertEqual(oversized["discovery"]["native"], {"state": "unknown", "reason": "byte_cap"})
        self.assertLessEqual(hook_recall._bytes({"discovery": oversized["discovery"]}), hook_recall.MAX_READER_CONTROL_BYTES)

    def test_fatal_readback_error_is_not_a_successful_skipped_card_count(self):
        self.reader_context([ID1])
        client = FakeClient(self.catalog, self.context, self.nodes)
        original = client.call_tool
        def fail_get(name, args):
            if name == "get":
                raise hook_recall.McpError("guard refused")
            return original(name, args)
        client.call_tool = fail_get
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall.collect_reader(self.config, "task", self.root)
        self.assertEqual(result["outcome"], "unavailable")
        self.assertEqual(result["cards"], [])
        self.assertIsNone(result["discovery"]["adapter"])
        self.assertEqual(result["discovery"]["native"]["state"], "unknown")

    def test_reader_malformed_observation_and_readback_fail_closed(self):
        self.context["observation"] = {"schema": 1, "learning": "disabled", "cards": []}
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall.collect_reader(self.config, "task", self.root)
        self.assertEqual(result["outcome"], "unavailable")
        self.nodes[ID1]["summary_truncated"] = True
        self.context["observation"]["cards"] = [
            {"node_id": identifier, "lane": "primary", "card_sha256": "a" * 64,
             "graph_path": None} for identifier in (ID1, ID2)]
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            self.assertEqual(hook_recall.collect_reader(self.config, "task", self.root)["outcome"],
                             "unavailable")

    def test_reader_whole_deadline_and_invalid_input_skip(self):
        self.context["observation"] = {"schema": 1, "learning": "disabled", "cards": []}
        class SlowClient(FakeClient):
            def call_tool(self, name, args):
                if name == "recall_context":
                    time.sleep(0.02)
                return super().call_tool(name, args)
        client = SlowClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall.collect_reader(self.config, "task", self.root, timeout=0.01)
        self.assertEqual(result["outcome"], "timeout")
        self.assertFalse(any(name == "get" for name, _ in client.calls))
        with patch("hook_recall.McpClient") as mcp:
            self.assertEqual(hook_recall.collect_reader(self.config, " ", self.root)["outcome"],
                             "skipped")
            mcp.assert_not_called()

    def test_catalog_and_store_boundary_refuse_without_recall(self):
        for catalog in (self.catalog + [{"db": "user"}],
                        [{**self.catalog[0], "configured_path": str(self.store) + ".other"}],
                        [{**self.catalog[0], "state": "maintenance"}]):
            client = FakeClient(catalog, self.context, self.nodes)
            with self.subTest(catalog=catalog), patch("hook_recall.McpClient", return_value=client):
                with self.assertRaisesRegex(ValueError, "unexpected project catalog"):
                    hook_recall._collect(self.config, "task", self.root, 1.5)
                self.assertEqual([name for name, _ in client.calls], ["databases"])
        other_root = self.root / "other"
        other_root.mkdir()
        with patch("hook_recall.McpClient") as mcp:
            with self.assertRaisesRegex(ValueError, "project store boundary"):
                hook_recall._collect(self.config, "task", other_root, 1.5)
            mcp.assert_not_called()

    def test_connect_remote_path_needs_no_local_store_and_stays_read_only(self):
        data = self._connect_config()
        remote = Path(data["database_path"])
        self.store.unlink()
        self.assertFalse(remote.exists())
        catalog = [{**self.catalog[0], "configured_path": str(remote)}]
        client = FakeClient(catalog, self.context, self.nodes)
        before = set(self.root.rglob("*"))
        with patch("hook_recall.McpClient", return_value=client) as mcp, \
             patch("service.ensure_ready", side_effect=AssertionError("must not ensure service")), \
             patch("service.start", side_effect=AssertionError("must not start service")), \
             patch("subprocess.Popen", side_effect=AssertionError("must not spawn service")), \
             patch.object(Path, "mkdir", side_effect=AssertionError("must not provision directories")), \
             patch.object(Path, "write_bytes", side_effect=AssertionError("must not provision store")), \
             patch.object(Path, "write_text", side_effect=AssertionError("must not write config")):
            result = hook_recall._collect(self.config, "actual remote task", self.root, 1.5)
        self.assertEqual(result["outcome"], "ok")
        self.assertEqual([card["id"] for card in result["cards"]], [ID1, ID2])
        mcp.assert_called_once_with(data["url"], token=None, timeout=1.5)
        self.assertEqual([name for name, _ in client.calls],
                         ["databases", "recall_context", "get", "get"])
        self.assertEqual(set(self.root.rglob("*")), before)
        self.assertFalse(remote.exists())
        self.assertFalse(self.store.exists())

    def test_connect_reader_preserves_native_observation_and_graph_provenance(self):
        data = self._connect_config()
        self.store.unlink()
        catalog = [{**self.catalog[0], "configured_path": data["database_path"]}]
        path = [{"previous": ID1, "target": ID2, "from": ID1, "to": ID2,
                 "kind": "supports", "anchor": "verified graph edge"}]
        self.context["observation"] = {"schema": 1, "learning": "disabled", "cards": [
            {"node_id": identifier, "lane": lane, "card_sha256": digest * 64,
             "graph_path": path if identifier == ID2 else None}
            for identifier, lane, digest in ((ID1, "primary", "a"),
                                            (ID2, "primary", "b"),
                                            (ID3, "expansions", "c"))]}
        client = FakeClient(catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall.collect_reader(self.config, "actual remote task", self.root)
        self.assertEqual(result["outcome"], "ok")
        self.assertEqual([card["id"] for card in result["cards"]], [ID1, ID2, ID3])
        self.assertEqual(client.calls[1], ("recall_context", {
            "db": "project", "text": "actual remote task", "k": hook_recall.reader_plan([{ "role": "user", "text": "actual remote task"}])[0]["k"],
            "max_nodes": hook_recall.reader_plan([{ "role": "user", "text": "actual remote task"}])[0]["max_nodes"], "depth": 2, "observe": True}))
        self.assertEqual(result["observation"]["learning"], "disabled")
        observed = {row["node_id"]: row for row in result["observation"]["cards"]}
        self.assertEqual(observed[ID2]["graph_path"], path)
        self.assertEqual([name for name, _ in client.calls],
                         ["databases", "recall_context", "get", "get", "get"])

    def test_connect_catalog_must_match_exact_single_project_before_recall(self):
        data = self._connect_config()
        entry = {**self.catalog[0], "configured_path": data["database_path"]}
        for catalog in ([{**entry, "configured_path": str(self.store)}],
                        [entry, {"db": "user"}], [{**entry, "db": "user"}],
                        [{**entry, "name": "other-project"}],
                        [{**entry, "state": "maintenance"}], []):
            client = FakeClient(catalog, self.context, self.nodes)
            with self.subTest(catalog=catalog), patch("hook_recall.McpClient", return_value=client):
                with self.assertRaisesRegex(ValueError, "unexpected project catalog"):
                    hook_recall._collect(self.config, "task", self.root, 1.5)
            self.assertEqual([name for name, _ in client.calls], ["databases"])

    def test_connect_user_store_is_rejected_before_client_creation(self):
        self._connect_config(database_name="user")
        with patch("hook_recall.McpClient") as mcp:
            with self.assertRaisesRegex(ValueError, "project store boundary"):
                hook_recall._collect(self.config, "task", self.root, 1.5)
            mcp.assert_not_called()

    def test_local_user_store_is_rejected_before_client_creation(self):
        data = json.loads(self.config.read_text())
        data["database_name"] = "user"
        data["database_path"] = data.pop("project_db")
        self.config.write_text(json.dumps(data))
        with patch("hook_recall.McpClient") as mcp:
            with self.assertRaisesRegex(ValueError, "project store boundary"):
                hook_recall._collect(self.config, "task", self.root, 1.5)
            mcp.assert_not_called()

    def test_connect_nonloopback_endpoints_are_rejected_before_client_creation(self):
        for url in ("http://host.docker.internal:18765/", "http://192.0.2.1:18765/",
                    "http://localhost:18765/", "https://127.0.0.1:18765/"):
            self._connect_config(url=url)
            with self.subTest(url=url), patch("hook_recall.McpClient") as mcp:
                with self.assertRaisesRegex(ValueError, "numeric loopback HTTP"):
                    hook_recall._collect(self.config, "task", self.root, 1.5)
                mcp.assert_not_called()

    def test_missing_local_project_store_still_refuses_without_client_or_provision(self):
        self.store.unlink()
        with patch("hook_recall.McpClient") as mcp, \
             patch("service.ensure_ready", side_effect=AssertionError("must not ensure service")), \
             patch("service.start", side_effect=AssertionError("must not start service")):
            with self.assertRaisesRegex(ValueError, "project store is unavailable"):
                hook_recall._collect(self.config, "task", self.root, 1.5)
            self.assertEqual(hook_recall.collect_reader(self.config, "task", self.root)["outcome"],
                             "unavailable")
            mcp.assert_not_called()
        self.assertFalse(self.store.exists())
        self.assertFalse((self.root / "private").exists())

    def test_get_fingerprint_uses_full_summary_provenance_not_status(self):
        node = self._node(ID1, "active", "é" * 500)
        card = hook_recall._card(node, ID1)
        self.assertLessEqual(len(card["summary"].encode()), 700)
        self.assertEqual(len(card["fingerprint"]), 64)
        changed = {**node, "summary": node["summary"] + "tail"}
        self.assertEqual(card["summary"], hook_recall._card(changed, ID1)["summary"])
        self.assertNotEqual(card["fingerprint"], hook_recall._card(changed, ID1)["fingerprint"])
        changed = {**node, "status": "archived"}
        self.assertIsNone(hook_recall._card(changed, ID1))
        changed = {**node, "provenance": {"type": "web", "url": "https://example.test"}}
        self.assertNotEqual(card["fingerprint"], hook_recall._card(changed, ID1)["fingerprint"])

    def test_native_source_codec_is_accepted_as_provenance_not_forged_for_display(self):
        node = self._node(ID1, "active", "A sourced lesson")
        node["provenance"]["source"]["request_codec"] = "capture_v2"
        card = hook_recall._card(node, ID1)
        self.assertTrue(card["source"].startswith("codex:"))
        assert_without_codec = self._node(ID1, "active", "A sourced lesson")
        self.assertNotEqual(card["fingerprint"],
                            hook_recall._card(assert_without_codec, ID1)["fingerprint"])

    def test_malformed_get_is_not_presented(self):
        for change in ({"id": ID2}, {"summary_truncated": True}, {"status": "unexpected"},
                       {"provenance": None}):
            node = {**self.nodes[ID1], **change}
            with self.subTest(change=change), self.assertRaises(ValueError):
                hook_recall._card(node, ID1)

    def test_parent_prompt_only_on_stdin_and_oversized_query_skips(self):
        completed = subprocess.CompletedProcess(args=[], returncode=0,
                                                stdout=b'{"outcome":"empty","cards":[]}', stderr=b"")
        with patch("hook_recall.subprocess.run", return_value=completed) as run:
            result = hook_recall.recall_cards(self.config, "private prompt", self.root)
            self.assertEqual(result["outcome"], "empty")
            args = run.call_args.args[0]
            self.assertEqual(args[-1], "--collect")
            self.assertNotIn("private prompt", " ".join(args))
            self.assertEqual(json.loads(run.call_args.kwargs["input"])["prompt"], "private prompt")
        with patch("hook_recall.subprocess.run") as run:
            result = hook_recall.recall_cards(self.config, "é" * 4097, self.root)
            self.assertEqual(result["outcome"], "skipped")
            run.assert_not_called()

    def test_parent_deadline_kills_slow_http_child(self):
        binary = os.environ.get("MNEME_CLIENT_BINARY")
        if not binary or not Path(binary).is_file():
            self.skipTest("set MNEME_CLIENT_BINARY to a built compatible mnemed")
        class SlowHandler(socketserver.BaseRequestHandler):
            def handle(self):
                self.request.recv(4096)
                time.sleep(0.8)

        class Server(socketserver.ThreadingTCPServer):
            allow_reuse_address = True
            daemon_threads = True

        with Server(("127.0.0.1", 0), SlowHandler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            data = json.loads(self.config.read_text())
            data["port"] = server.server_address[1]
            self.config.write_text(json.dumps(data))
            started = time.monotonic()
            result = hook_recall.recall_cards(self.config, "actual task", self.root, timeout=0.2)
            elapsed = time.monotonic() - started
            server.shutdown()
        self.assertEqual(result["outcome"], "timeout")
        self.assertLess(elapsed, 0.7)
        self.assertFalse((self.root / "private").exists())

    def test_malformed_collector_reply_is_unavailable(self):
        completed = subprocess.CompletedProcess(args=[], returncode=0, stdout=b"not json", stderr=b"")
        with patch("hook_recall.subprocess.run", return_value=completed):
            self.assertEqual(hook_recall.recall_cards(self.config, "task", self.root)["outcome"],
                             "unavailable")


class OverlapTests(unittest.TestCase, OverlapFixture):
    """Offline guarded-overlap fixtures; the native guard remains client-owned."""

    def setUp(self):
        OverlapFixture.__init__(self, self)

    def test_background_slow_read_uses_remaining_whole_budget_without_retry(self):
        from mcp_client import McpTransportError, McpToolError
        self.context.update(primary=[{"id": ID1}], expansions=[], observation={
            "schema": 1, "learning": "disabled", "cards": [
                {"node_id": ID1, "lane": "primary", "card_sha256": "a" * 64,
                 "graph_path": None}]})
        for collector in ("reader", "overlap"):
            for duration, expected in ((2.64, "ok"), (16, "timeout")):
                with self.subTest(collector=collector, duration=duration):
                    clock = [100.0]
                    client = FakeClient(self.catalog, self.context, self.nodes)
                    original = client.call_tool
                    startup = []
                    timeouts = []
                    def construct(*_args, **kwargs):
                        startup.append(kwargs["timeout"])
                        client.timeout = kwargs["timeout"]
                        return client
                    def read(name, args):
                        timeouts.append((name, client.timeout))
                        elapsed = duration if name == "recall_context" else .1
                        if elapsed > min(startup[0], client.timeout):
                            client.calls.append((name, args))
                            clock[0] += min(startup[0], client.timeout)
                            raise McpTransportError("fixture whole-call timed out; not retried")
                        clock[0] += elapsed
                        if name == "concern":
                            raise McpToolError("fixture optional lookup unsupported")
                        return original(name, args)
                    client.call_tool = read
                    with patch("hook_recall.time.monotonic", side_effect=lambda: clock[0]), \
                         patch("hook_recall.McpClient", side_effect=construct):
                        if collector == "reader":
                            result = hook_recall.collect_reader(self.config, "task", self.root)
                        else:
                            result = hook_recall.collect_overlap(self.config, "task", self.root,
                                                                expected_db_id=self.db_id)
                    self.assertEqual(result["outcome"], expected, result)
                    self.assertEqual(startup, [15])
                    recall_timeout = next(seconds for name, seconds in timeouts if name == "recall_context")
                    self.assertGreater(recall_timeout, 14)
                    self.assertEqual(sum(name == "recall_context" for name, _ in client.calls), 1)
                    self.assertLessEqual(clock[0], 115)
                    if expected == "ok":
                        self.assertEqual(result["cards"][0]["id"], ID1)
                    else:
                        self.assertEqual(result["cards"], [])

    def test_background_latency_cannot_widen_effort_limit_or_foreground_rpc(self):
        from librarian_policy import LibrarianBudget
        with patch("hook_recall.McpClient") as client:
            result = hook_recall.collect_reader(self.config, "task", self.root,
                timeout=11, budget=LibrarianBudget(effort="low"))
            self.assertEqual(result["outcome"], "skipped")
            client.assert_not_called()
            result = hook_recall.resolve_project_identity(self.config, self.root, timeout=3)
            self.assertEqual(result["outcome"], "unavailable")
            client.assert_not_called()
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client) as factory:
            hook_recall._collect(self.config, "task", self.root, 5)
        self.assertEqual(factory.call_args.kwargs["timeout"], 1.5)

    def test_initial_identity_resolution_reads_only_native_catalog(self):
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall.resolve_project_identity(self.config, self.root)
        self.assertEqual(result["outcome"], "ok")
        self.assertEqual(result["db_id"], self.db_id)
        self.assertEqual(result["cards"], [])
        self.assertEqual(client.calls, [("databases", {})])
        self.assertFalse((self.root / "private").exists())

    def test_only_guarded_readback_supplies_window_beyond_four_summary_hints(self):
        for identifier in (ID4, ID5, ID6):
            self.nodes[identifier] = self._node(identifier, "active", "é" * 1000)
        self.context["primary"] = [{"id": i, "summary": "UNGUARDED_DISCOVERY"}
                                   for i in (ID1, ID2, ID3, ID4, ID5, ID6)]
        self.context["expansions"] = []
        self.nodes[ID1]["summary"] = "é" * 1000
        self.nodes[ID1]["body"] = "UNDISPLAYED_BODY"
        client = FakeClient(self.catalog, self.context, self.nodes)
        result = self.overlap(client)
        self.assertEqual(result["outcome"], "ok")
        self.assertEqual(result["db_id"], self.db_id)
        self.assertEqual(len(result["cards"]), 6)
        self.assertEqual([name for name, _ in client.calls],
                         ["databases", "recall_context", *(["get"] * 6), "databases"])
        from librarian_policy import LibrarianBudget
        self.assertEqual(client.calls[1][1], {"db": "project", "text": "bounded task cue",
                                            **LibrarianBudget().overlap_window(12288)})
        for name, args in client.calls:
            if name == "get":
                self.assertEqual(args, {"db": "project", "id": args["id"], "body": False,
                                        "edges": False, "expected_db_id": self.db_id})
        for card in result["cards"]:
            self.assertEqual(set(card), {"id", "summary", "kind"})
            self.assertEqual(card["kind"], "semantic")
            self.assertLessEqual(len(card["summary"].encode()), 512)
        self.assertNotIn("UNGUARDED_DISCOVERY", json.dumps(result))
        self.assertNotIn("UNDISPLAYED_BODY", json.dumps(result))
        # FakeClient deliberately has no connect_result or capability metadata.

    def test_successful_empty_discovery_still_checks_both_catalogs(self):
        self.context.update(primary=[], expansions=[])
        client = FakeClient(self.catalog, self.context, self.nodes)
        result = self.overlap(client)
        self.assertEqual(result["outcome"], "empty")
        self.assertEqual(result["db_id"], self.db_id)
        self.assertEqual(result["cards"], [])
        self.assertEqual([name for name, _ in client.calls],
                         ["databases", "recall_context", "databases"])

    def test_optional_routing_reads_only_labelled_witness_bodies_and_fails_open(self):
        import routing_memory as routing
        from test_routing_memory import node, body, binding
        route = binding(); route['db_id'] = self.db_id
        witness = node(body(binding=route))
        witness.update(id=ID1, db_id=self.db_id, summary='Earlier routing judgment.', summary_truncated=False)
        witness['provenance']['source']['key'] = 'earlier-judgment'
        self.nodes[ID1] = witness
        self.context.update(primary=[{'id': ID1}, {'id': ID2}], expansions=[])
        client = FakeClient(self.catalog, self.context, self.nodes)
        result = self.overlap(client, include_routing=True)
        self.assertEqual(result['outcome'], 'ok')
        self.assertEqual(result['cards'][0]['routing_witness']['witness']['binding'], route)
        self.assertNotIn('routing_witness', result['cards'][1])
        body_calls = [args for name, args in client.calls if name == 'get' and args['body']]
        self.assertEqual(body_calls, [{'db':'project', 'id':ID1, 'body':True, 'edges':False,
                                      'max_body_bytes':routing.MAX_BODY_BYTES, 'expected_db_id':self.db_id}])

        class MissingBody(FakeClient):
            def call_tool(self, name, args):
                if name == 'get' and args['body']:
                    raise TimeoutError('Optional body is unavailable')
                return super().call_tool(name, args)

        result = self.overlap(MissingBody(self.catalog, self.context, self.nodes), include_routing=True)
        self.assertEqual(result['outcome'], 'ok')
        self.assertEqual(len(result['cards']), 2)
        self.assertNotIn('routing_witness', result['cards'][0])
        baseline = self.overlap(FakeClient(self.catalog, self.context, self.nodes))
        self.assertEqual(result['cards'], baseline['cards'])

    def routing_fixture(self, count=10, effort="high", invalid=False):
        from test_routing_memory import node, body, binding
        from librarian_policy import LibrarianBudget
        route = binding(); route['db_id'] = self.db_id
        self.nodes = {}
        for i in range(count):
            identifier = '0' * 24 + f'{i:02}'
            value = node(body(binding=route))
            value.update(id=identifier, db_id=self.db_id, summary='Scoped opinion.', summary_truncated=False)
            value['provenance']['source']['key'] = str(i)
            if invalid:
                value['provenance']['source']['reference'] = 'authored://invalid'
            self.nodes[identifier] = value
        self.context.update(primary=[{'id': ident} for ident in self.nodes], expansions=[])
        return LibrarianBudget(effort=effort)

    def routing_read(self, client, budget, **kwargs):
        with patch('hook_recall.McpClient', return_value=client):
            return hook_recall.collect_routing_witnesses(self.config, 'A task cue', self.root,
                current=[{'source':'cue','text':'A task cue'}], budget=budget, **kwargs)

    def test_signed_discovery_many_witnesses_one_route_not_hint_count_capped(self):
        from routing_contract import discovery_plan
        budget = self.routing_fixture(36)
        client = FakeClient(self.catalog, self.context, self.nodes)
        result = self.routing_read(client, budget)
        self.assertEqual(result['outcome'], 'ok')
        self.assertEqual(len(result['witnesses']), 36)
        self.assertNotIn('cards', result)
        plan, _ = discovery_plan([{'source':'cue','text':'A task cue'}], budget=budget)
        self.assertEqual(client.calls[1], ('recall_context', {'db':'project', 'text':'A task cue',
            **plan, 'depth':0, 'tags':['routing-judgment']}))
        gets = [args for name,args in client.calls if name=='get']
        self.assertEqual(len(gets), 36)
        self.assertTrue(all(g['expected_db_id']==self.db_id and g['body'] and not g['edges']
                            and g['max_body_bytes']==8192 for g in gets))
        self.assertEqual(client.calls[-1], ('databases', {}))
        self.assertEqual(result['routing_discovery']['discovery']['native']['state'], 'unknown')

    def test_effort_window_and_read_work_preserve_final_guard(self):
        for effort in ('low', 'medium', 'high'):
            budget = self.routing_fixture(36, effort=effort)
            client = FakeClient(self.catalog, self.context, self.nodes)
            result = self.routing_read(client, budget)
            diagnostic = result['routing_discovery']
            self.assertEqual(result['outcome'], 'ok')
            self.assertEqual(diagnostic['native_work']['read_allowance_bytes'], budget.native_read_bytes)
            self.assertEqual(client.calls[-1], ('databases', {}))
            if effort == 'low':
                self.assertGreater(diagnostic['hydration']['work_budget_unread'], 0)
                self.assertTrue(diagnostic['native_work']['read_allowance_exhausted'])
            self.assertLessEqual(diagnostic['native_work']['decoded_bytes'], budget.native_read_bytes)

    def test_rejected_witnesses_charge_every_response(self):
        budget = self.routing_fixture(12, invalid=True)
        client = FakeClient(self.catalog, self.context, self.nodes)
        result = self.routing_read(client, budget)
        self.assertEqual(result['outcome'], 'empty')
        diagnostic = result['routing_discovery']
        self.assertEqual(diagnostic['hydration']['rejected'], 12)
        expected = 2 * hook_recall._bytes(self.catalog) + hook_recall._bytes(self.context)
        expected += sum(hook_recall._bytes(n) for n in self.nodes.values())
        self.assertEqual(diagnostic['native_work']['decoded_bytes'], expected)

    def test_routing_partial_prefix_discarded_on_final_identity_or_config_drift(self):
        budget = self.routing_fixture(36, effort='low')
        class ChangedFinal(FakeClient):
            def call_tool(inner, name, args):
                value = super().call_tool(name, args)
                if name == 'databases' and len(inner.calls) > 1:
                    return [{**self.catalog[0], 'db_id': ID6}]
                return value
        result = self.routing_read(ChangedFinal(self.catalog, self.context, self.nodes), budget)
        self.assertEqual(result['outcome'], 'identity_mismatch')
        self.assertEqual(result['witnesses'], [])
        self.assertEqual(result['routing_discovery']['hydration']['returned'], 0)
        class ChangedConfig(FakeClient):
            def call_tool(inner, name, args):
                value = super().call_tool(name, args)
                if name == 'databases' and len(inner.calls) > 1:
                    self.config.write_text(self.config.read_text() + ' ')
                return value
        result = self.routing_read(ChangedConfig(self.catalog, self.context, self.nodes), budget)
        self.assertEqual(result['outcome'], 'identity_mismatch')
        self.assertEqual(result['witnesses'], [])

    def test_failed_final_catalog_discards_routing_prefix(self):
        budget = self.routing_fixture(2)
        class TimedFinal(FakeClient):
            def call_tool(inner, name, args):
                if name == 'databases' and inner.calls:
                    inner.calls.append((name,args)); raise TimeoutError('final guard')
                return super().call_tool(name,args)
        result = self.routing_read(TimedFinal(self.catalog,self.context,self.nodes), budget)
        self.assertEqual(result['outcome'],'timeout')
        self.assertEqual(result['witnesses'],[])

    def test_hydration_time_reserve_leaves_final_identity_check(self):
        budget = self.routing_fixture(3, effort='low')
        clock = [0.0]
        class SlowBody(FakeClient):
            def call_tool(inner, name, args):
                value = super().call_tool(name,args)
                if name == 'get':
                    clock[0] = .91
                return value
        client = SlowBody(self.catalog,self.context,self.nodes)
        with patch('hook_recall.time.monotonic', side_effect=lambda: clock[0]):
            result = self.routing_read(client,budget,timeout=1)
        self.assertEqual(result['outcome'],'ok')
        self.assertEqual(len(result['witnesses']),1)
        self.assertEqual(client.calls[-1],('databases',{}))
        self.assertEqual(result['routing_discovery']['hydration']['work_budget_unread'],2)
        self.assertTrue(result['routing_discovery']['native_work']['read_allowance_exhausted'])

    def test_initial_oversized_catalog_does_not_admit_discovery(self):
        budget = self.routing_fixture(2,effort='low')
        oversized = [{**self.catalog[0], 'extra': 'x' * budget.native_read_bytes}]
        client = FakeClient(oversized,self.context,self.nodes)
        result = self.routing_read(client,budget)
        self.assertEqual(result['outcome'],'unavailable')
        self.assertEqual(result['witnesses'],[])
        self.assertEqual(client.calls,[('databases',{})])
        self.assertEqual(result['routing_discovery']['native_work']['decoded_bytes'],hook_recall._bytes(oversized))
        self.assertTrue(result['routing_discovery']['native_work']['read_allowance_exhausted'])
        self.assertEqual(result['routing_discovery']['discovery']['native']['state'],'unknown')

    def test_actual_final_catalog_bytes_are_charged_not_reserved_estimate(self):
        budget = self.routing_fixture(2,effort='low')
        class EnlargedFinal(FakeClient):
            def call_tool(inner,name,args):
                value = super().call_tool(name,args)
                if name == 'databases' and len(inner.calls) > 1:
                    return [{**self.catalog[0],'extra':'x' * budget.native_read_bytes}]
                return value
        result = self.routing_read(EnlargedFinal(self.catalog,self.context,self.nodes),budget)
        self.assertEqual(result['outcome'],'unavailable')
        self.assertEqual(result['witnesses'],[])
        self.assertGreater(result['routing_discovery']['native_work']['decoded_bytes'],budget.native_read_bytes)

    def test_routing_discovery_known_partial_and_unknown_tail_stay_explicit(self):
        budget = self.routing_fixture(2)
        self.context.update(discovery_metadata())
        self.context['partial'] = True
        self.context['retrieval']['mode'] = 'tagged'
        self.context['omitted']['primary']['further_tail_unknown'] = True
        result = self.routing_read(FakeClient(self.catalog,self.context,self.nodes), budget)
        native = result['routing_discovery']['discovery']['native']
        self.assertTrue(native['partial'])
        self.assertTrue(native['omitted']['primary']['further_tail_unknown'])
        self.assertEqual(result['routing_discovery']['discovery']['continuation'],'unavailable')

    def test_whole_hint_array_byte_boundary_and_absent_empty_distinction(self):
        from routing_memory import hint, encoded, MAX_HINT_BYTES
        from test_routing_memory import binding
        row = hint(binding(), 'weaken')
        count = (MAX_HINT_BYTES - 1) // (len(encoded(row)) + 1)
        accepted = [row] * count
        self.assertLessEqual(len(encoded(accepted)), MAX_HINT_BYTES)
        self.assertGreater(len(encoded(accepted + [row])), MAX_HINT_BYTES)
        for supplied, expected in ((None, None), ([], []), (accepted, accepted), (accepted + [row], []),
                                   (accepted + [{'bad':'row'}], [])):
            client = FakeClient(self.catalog,self.context,self.nodes)
            with patch('hook_recall.McpClient',return_value=client):
                hook_recall.collect_reader(self.config,'A task cue',self.root,routing_hints=supplied)
            request = client.calls[1][1]
            if expected is None:
                self.assertNotIn('routing_hints',request)
            else:
                self.assertEqual(request['routing_hints'],expected)

    def test_routing_sidecar_failure_keeps_ordinary_guarded_cards(self):
        self.context.update(primary=[{'id':ID1}], expansions=[])
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch('hook_recall.McpClient', return_value=client):
            result = hook_recall.collect_reader(self.config, 'A task cue', self.root, routing_hints=[])
        self.assertEqual(result['outcome'], 'ok')
        self.assertEqual(result['cards'][0]['id'], ID1)
        self.assertNotIn('observation', result)
        self.assertEqual(client.calls[1][1]['routing_hints'], [])
        self.assertTrue(client.calls[1][1]['observe'])
        self.assertEqual(client.calls[2][1]['expected_db_id'], self.db_id)

    def test_association_recheck_is_one_guarded_get_of_exact_seen_summary(self):
        target = hook_recall.AssociationTarget("overlap001", ID1, "Evidence one", "semantic")
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            outcome = hook_recall.check_association_target(
                self.config, self.root, target, expected_db_id=self.db_id, timeout=1)
        self.assertEqual(outcome, "kept")
        self.assertEqual(client.calls, [("databases", {}),
                                        ("get", {"db": "project", "id": ID1, "body": False,
                                                 "edges": False, "expected_db_id": self.db_id})])
        self.assertLessEqual(client.timeout, 1.5)
        self.assertFalse((self.root / "private").exists())

    def test_delivered_target_recheck_compares_full_get_fingerprint_and_project_db(self):
        fingerprint = hook_recall._card(self.nodes[ID1], ID1)["fingerprint"]
        target = hook_recall.AssociationTarget("shown001", ID1, "Evidence one", "semantic",
                                              origin="delivery", db_id=self.db_id,
                                              full_get_fingerprint=fingerprint)
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=client):
            self.assertEqual(hook_recall.check_association_target(
                self.config, self.root, target, expected_db_id=self.db_id, timeout=1), "kept")
        changed = {**self.nodes[ID1], "summary": "Evidence one, but edited beyond display"}
        client = FakeClient(self.catalog, self.context, {**self.nodes, ID1: changed})
        with patch("hook_recall.McpClient", return_value=client):
            self.assertEqual(hook_recall.check_association_target(
                self.config, self.root, target, expected_db_id=self.db_id, timeout=1), "stale")
        wrong_db = hook_recall.AssociationTarget("shown001", ID1, "Evidence one", "semantic",
                                                origin="delivery", db_id=ID6,
                                                full_get_fingerprint=fingerprint)
        with patch("hook_recall.McpClient", side_effect=AssertionError("foreign DB read")):
            self.assertEqual(hook_recall.check_association_target(
                self.config, self.root, wrong_db, expected_db_id=self.db_id, timeout=1), "unavailable")
        long_node = self._node(ID1, "active", "é" * 450)
        shown = hooks._limited_text(long_node["summary"], 800)
        self.assertEqual(len(shown.encode()), 803)  # Existing display appends ellipsis.
        target = hook_recall.AssociationTarget("shown001", ID1, shown, "semantic",
                                              origin="delivery", db_id=self.db_id,
                                              full_get_fingerprint=hook_recall._card(long_node, ID1)["fingerprint"])
        client = FakeClient(self.catalog, self.context, {**self.nodes, ID1: long_node})
        with patch("hook_recall.McpClient", return_value=client):
            self.assertEqual(hook_recall.check_association_target(
                self.config, self.root, target, expected_db_id=self.db_id, timeout=1), "kept")

    def test_association_recheck_drops_changed_archived_or_episode_target(self):
        target = hook_recall.AssociationTarget("overlap001", ID1, "Evidence one", "semantic")
        for changed, expected in (({"summary": "Changed"}, "stale"),
                                  ({"status": "archived"}, "stale"),
                                  ({"memory_kind": {"kind": "episode"}}, "unavailable")):
            with self.subTest(changed=changed):
                client = FakeClient(self.catalog, self.context,
                                    {**self.nodes, ID1: {**self.nodes[ID1], **changed}})
                with patch("hook_recall.McpClient", return_value=client):
                    self.assertEqual(hook_recall.check_association_target(
                        self.config, self.root, target, expected_db_id=self.db_id, timeout=1), expected)
                self.assertEqual([name for name, _ in client.calls], ["databases", "get"])

    def test_association_recheck_compares_only_bounded_summary_assessor_saw(self):
        prefix = "s" * hook_recall.MAX_OVERLAP_SUMMARY_BYTES
        target = hook_recall.AssociationTarget("overlap001", ID1, prefix, "semantic")
        client = FakeClient(self.catalog, self.context,
                            {**self.nodes, ID1: self._node(ID1, "active", prefix + "new hidden suffix")})
        with patch("hook_recall.McpClient", return_value=client):
            self.assertEqual(hook_recall.check_association_target(
                self.config, self.root, target, expected_db_id=self.db_id, timeout=1), "kept")

    def test_association_recheck_never_reads_episode_or_wrong_db_and_never_retries_guard(self):
        for target, db in ((hook_recall.AssociationTarget("overlap001", ID1, "Evidence one", "episode"), self.db_id),
                           (hook_recall.AssociationTarget("overlap001", ID1, "Evidence one", "semantic"), ID6)):
            client = FakeClient(self.catalog, self.context, self.nodes)
            with patch("hook_recall.McpClient", return_value=client):
                self.assertEqual(hook_recall.check_association_target(
                    self.config, self.root, target, expected_db_id=db, timeout=1), "unavailable")
            self.assertFalse(any(name == "get" for name, _ in client.calls))
        client = FakeClient(self.catalog, self.context, self.nodes)
        def guarded_failure(name, args):
            client.calls.append((name, args))
            if name == "databases":
                return self.catalog
            raise hook_recall.McpError("expected_db_id mismatch")
        client.call_tool = guarded_failure
        with patch("hook_recall.McpClient", return_value=client):
            self.assertEqual(hook_recall.check_association_target(
                self.config, self.root,
                hook_recall.AssociationTarget("overlap001", ID1, "Evidence one", "semantic"),
                expected_db_id=self.db_id, timeout=1), "unavailable")
        self.assertEqual([name for name, _ in client.calls], ["databases", "get"])

    def test_initial_catalog_identity_mismatch_stops_before_discovery(self):
        for changes in ({"db_id": ID6}, {"configured_path": str(self.store) + ".other"},
                        {"name": "other"}, {"state": "maintenance"}):
            client = FakeClient([{**self.catalog[0], **changes}], self.context, self.nodes)
            with self.subTest(changes=changes):
                result = self.overlap(client)
                self.assertEqual(result["outcome"], "identity_mismatch")
                self.assertEqual(result["cards"], [])
                self.assertNotIn("db_id", result)
                self.assertEqual(client.calls, [("databases", {})])

    def test_missing_or_noncanonical_catalog_identity_is_not_empty(self):
        for invalid in (None, True, "not-an-id", ID7.lower()):
            client = FakeClient([{**self.catalog[0], "db_id": invalid}], self.context, self.nodes)
            with self.subTest(invalid=invalid):
                result = self.overlap(client)
                self.assertEqual(result["outcome"], "unavailable")
                self.assertNotIn("db_id", result)
                self.assertEqual(client.calls, [("databases", {})])

    def test_catalog_drift_after_readback_discards_every_hint(self):
        for changes in ({"db_id": ID6}, {"configured_path": str(self.store) + ".other"},
                        {"db": "other"}, {"state": "maintenance"}):
            client = FakeClient(self.catalog, self.context, self.nodes)
            original = client.call_tool
            catalogs = 0

            def read(name, args):
                nonlocal catalogs
                value = original(name, args)
                if name == "databases":
                    catalogs += 1
                    if catalogs == 2:
                        return [{**value[0], **changes}]
                return value

            client.call_tool = read
            with self.subTest(changes=changes):
                result = self.overlap(client)
                self.assertEqual(result["outcome"], "identity_mismatch")
                self.assertEqual(result["cards"], [])
                self.assertNotIn("db_id", result)

    def test_service_configuration_change_discards_every_hint(self):
        client = FakeClient(self.catalog, self.context, self.nodes)
        original = client.call_tool

        def read(name, args):
            value = original(name, args)
            if name == "recall_context":
                self.config.write_text(self.config.read_text() + "\n")
            return value

        client.call_tool = read
        result = self.overlap(client)
        self.assertEqual(result["outcome"], "identity_mismatch")
        self.assertEqual(result["cards"], [])
        self.assertNotIn("db_id", result)

    def test_native_guard_refusal_is_never_an_unguarded_retry(self):
        for message, expected in (("native client does not support expected_db_id", "unavailable"),
                                  ("expected_db_id mismatch for database project", "identity_mismatch"),
                                  ("native client response timed out; request was not retried", "timeout")):
            client = FakeClient(self.catalog, self.context, self.nodes)
            original = client.call_tool

            def read(name, args):
                value = original(name, args)
                if name == "get":
                    raise hook_recall.McpError(message)
                return value

            client.call_tool = read
            with self.subTest(message=message):
                result = self.overlap(client)
                self.assertEqual(result["outcome"], expected)
                self.assertEqual(result["cards"], [])
                self.assertNotIn("db_id", result)
                self.assertEqual(len([name for name, _ in client.calls if name == "get"]), 1)
                self.assertEqual(client.calls[-1][1]["expected_db_id"], self.db_id)

    def test_malformed_discovery_or_readback_never_becomes_empty_overlap(self):
        self.context["primary"] = [{"id": "bad"}]
        self.context["expansions"] = []
        client = FakeClient(self.catalog, self.context, self.nodes)
        self.assertEqual(self.overlap(client)["outcome"], "unavailable")
        self.assertFalse(any(name == "get" for name, _ in client.calls))
        self.context["primary"] = [{"id": ID1}, {"id": ID2}]
        for change in ({"summary_truncated": True}, {"id": ID3}, {"summary": ""},
                       {"status": "archived"}, {"provenance": {}}):
            client = FakeClient(self.catalog, self.context,
                                {**self.nodes, ID2: {**self.nodes[ID2], **change}})
            with self.subTest(change=change):
                result = self.overlap(client)
                self.assertEqual(result["outcome"], "unavailable")
                self.assertEqual(result["cards"], [])

    def test_remote_owner_path_is_not_opened_or_provisioned(self):
        data = self._connect_config()
        self.store.unlink()
        self.catalog[0]["configured_path"] = data["database_path"]
        client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("service.ensure_ready", side_effect=AssertionError("no service start")), \
             patch("service.start", side_effect=AssertionError("no service start")):
            result = self.overlap(client)
        self.assertEqual(result["outcome"], "ok")
        self.assertFalse(Path(data["database_path"]).exists())
        self.assertFalse(self.store.exists())

    def test_overlap_deadline_and_invalid_input_do_not_produce_hints(self):
        client = FakeClient(self.catalog, self.context, self.nodes)
        original = client.call_tool

        def read(name, args):
            value = original(name, args)
            if name == "recall_context":
                time.sleep(0.02)
            return value

        client.call_tool = read
        result = self.overlap(client, timeout=0.01)
        self.assertEqual(result["outcome"], "timeout")
        self.assertFalse(any(name == "get" for name, _ in client.calls))
        for cue, identifier, timeout in ((" ", self.db_id, 2), (None, self.db_id, 2),
                                         ("task", "bad", 2), ("task", self.db_id, 16),
                                         ("task", self.db_id, float("nan"))):
            with self.subTest(cue=cue, identifier=identifier, timeout=timeout), \
                 patch("hook_recall.McpClient") as mcp:
                result = hook_recall.collect_overlap(self.config, cue, self.root,
                                                      expected_db_id=identifier, timeout=timeout)
                self.assertEqual(result["outcome"], "unavailable")
                mcp.assert_not_called()


if __name__ == "__main__":
    unittest.main()


class ReaderConcernNativeTests(unittest.TestCase, RecallOwnerFixture):
    def setUp(self):
        RecallOwnerFixture.__init__(self, self)

    def setup_client(self):
        self.catalog[0]["db_id"] = ID7
        self.context["primary"] = [{"id": ID1}, {"id": ID2}]
        self.context["expansions"] = []
        self.context["observation"] = {"schema": 1, "learning": "disabled", "cards": [
            {"node_id": i, "lane": "primary", "card_sha256": "a" * 64, "graph_path": None} for i in (ID1, ID2)]}
        from test_turn_observer import concern_row
        self.row = concern_row(ID1, ID2)
        for e in self.row["notice"]["binding"]["endpoints"]:
            self.nodes[e["id"]]["concern_endpoint"] = e
        return FakeClient(self.catalog, self.context, self.nodes)

    def test_native_endpoint_pages_and_optional_failure_preserve_cards(self):
        client = self.setup_client()
        calls = []
        def checked(db, payload, **kwargs):
            calls.append(payload)
            return {"db": "project", "db_id": ID7, "action": "list", "page": {"items": [self.row], "next": None}}
        client.concern_checked = checked
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall.collect_reader(self.config, "actual task", self.root)
        self.assertEqual(result["concern_rows"], [self.row])
        self.assertEqual(result["concern_endpoints"][ID1]["meaning"], "a" * 64)
        self.assertTrue(all("limit" not in c for c in calls))
        def unavailable(*args, **kwargs):
            raise ValueError("unsupported")
        client.concern_checked = unavailable
        with patch("hook_recall.McpClient", return_value=client):
            missed = hook_recall.collect_reader(self.config, "actual task", self.root)
        self.assertEqual(missed["cards"], result["cards"])
        self.assertEqual(missed["concern_lookup"], "unknown")

    def test_notice_retains_atomic_prior_row_and_refusal_keeps_readonly_warning(self):
        client = self.setup_client()
        nomination = {"kind": "disagreement", "left_id": ID1, "right_id": ID2,
                      "caveat": "Fresh question", "missing_fact": "Fresh uncertainty"}
        endpoints = {e["id"]: e for e in self.row["notice"]["binding"]["endpoints"]}
        def checked(*args, **kwargs):
            return {"db": "project", "db_id": ID7, "action": "notice",
                    "outcome": {"status": "unchanged", "row": self.row}}
        client.concern_checked = checked
        with patch("hook_recall.McpClient", return_value=client):
            warnings = hook_recall.register_reader_concerns(self.config, self.root, ID7, [nomination], endpoints)
        self.assertEqual(warnings[0]["expected_row"], self.row)
        self.assertIn("Fresh question", warnings[0]["shown_text"])
        with patch("hook_recall.McpClient", return_value=client):
            warnings = hook_recall.register_reader_concerns(self.config, self.root, ID7, [nomination], endpoints, allowed=lambda: False)
        self.assertIsNone(warnings[0]["expected_row"])

    def test_native_lookup_cursor_and_budget_omission_keep_content(self):
        client = self.setup_client()
        calls = []
        def checked(db, payload, **kwargs):
            calls.append(payload)
            page = {"items": [], "next": {"opaque": "native-cursor"}} if "after" not in payload else {"items": [self.row], "next": None}
            return {"db": "project", "db_id": ID7, "action": "list", "page": page}
        client.concern_checked = checked
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall.collect_reader(self.config, "actual task", self.root)
        self.assertEqual(result["concern_rows"], [self.row])
        self.assertEqual(calls[1]["after"], {"opaque": "native-cursor"})
        def large(db, payload, **kwargs):
            return {"db": "project", "db_id": ID7, "action": "list",
                    "page": {"items": [self.row] * 8, "next": {"opaque": "tail"}}}
        client.concern_checked = large
        with patch("hook_recall.McpClient", return_value=client):
            capped = hook_recall.collect_reader(self.config, "actual task", self.root)
        self.assertEqual(capped["cards"], result["cards"])
        self.assertEqual(capped["concern_lookup"], "unknown")
        self.assertLessEqual(hook_recall._bytes(capped), hook_recall.MAX_READER_RESULT_BYTES)

    def test_joined_fake_reader_observer_assessor_and_later_scoped_finding(self):
        """Compose real contracts around a fake native owner; no provider or CAS claim."""
        import reader_contract
        import reader_runtime
        import reader_worker
        import recording_contract
        import recording_jobs
        import turn_observer
        from fixture_source_turn import (SourceTurnFixture, SESSION, TURN, opening, call,
                                        output, delivered, assistant, event)
        client = self.setup_client()
        durable = {"row": None}
        counts = {"selector": 0, "assessor": 0, "notice": 0, "finding": 0}
        def checked(db, payload, **kwargs):
            self.assertEqual(kwargs["expected_db_id"], ID7)
            action = payload["action"]
            if action == "list":
                return {"db": db, "db_id": ID7, "action": action,
                        "page": {"items": [durable["row"]] if durable["row"] else [], "next": None}}
            if action == "notice":
                counts["notice"] += 1
                if durable["row"] is None:
                    durable["row"] = {"notice": payload["notice"], "finding": None}
                    status = "applied"
                else:
                    status = "unchanged"
            else:
                counts["finding"] += 1
                self.assertEqual(payload["expected"], durable["row"])
                durable["row"] = {**durable["row"], "finding": payload["finding"]}
                status = "applied"
            return {"db": db, "db_id": ID7, "action": action,
                    "outcome": {"status": status, "row": durable["row"]}}
        client.concern_checked = checked
        prompts = []
        class Selector:
            def select(inner, dialogue, cards, *, concern_rows=None):
                counts["selector"] += 1
                prompt, context = reader_contract.prepare(dialogue, cards, concern_rows=concern_rows)
                prompts.append(prompt)
                answer = reader_contract.validate_answer({"selected_ids": [ID1, ID2], "concerns": [{
                    "kind": "disagreement", "left_id": ID1, "right_id": ID2,
                    "caveat": "These settings may apply in different scopes.",
                    "missing_fact": "What does the local tool report?"}]}, context)
                return {**answer, "reason": "selected", "provider_attempt": True,
                        "usage": {"input_tokens": 10, "output_tokens": 2}}
        config = {"service_config": self.config, "project_root": self.root,
                  "reader_model": "gpt-6.1-sol", "librarian_effort": "medium"}
        with patch("hook_recall.McpClient", return_value=client):
            selected = reader_worker._execute(config, "Investigate this repository setting", Selector(), lambda: True, set())
            warnings = hook_recall.register_reader_concerns(self.config, self.root, ID7,
                        selected["concern_nominations"], selected["concern_endpoints"], allowed=lambda: True)
        expected = warnings[0]["expected_row"]
        packed = hooks._pack_async_delivery(selected["cards"], SESSION, TURN, ID7, concerns=warnings)
        packet = {"schema": turn_observer.DELIVERY_SCHEMA, "session_id": SESSION, "turn_id": TURN,
                  "rendered_text": packed["context"], "rendered_sha256": hashlib.sha256(packed["context"].encode()).hexdigest(),
                  "displayed": packed["displayed"], "concerns": packed["concerns"]}
        source = SourceTurnFixture(self)
        source.write(opening() + [call(), delivered(packet), output(), assistant(), event("task_complete")])
        observation = source.observe(delivery=packet)
        contexts = []
        def fresh(preparation, **kwargs):
            counts["assessor"] += 1
            prompt, context = preparation()
            contexts.append(context)
            answer = {"proposal": None, "maintenance": [{"target": "case001", "scope": "This inspected local setting",
                      "observation": "The tool reported setting=5; other settings may apply elsewhere.",
                      "evidence": [e.evidence_id for e in context.bindings if e.kind in ("memory_delivery", "tool_result")]}]}
            checked_answer = kwargs["validator"](answer, context)
            return {"assessment": checked_answer, "reason": kwargs["success"](checked_answer),
                    "usage": {"input_tokens": 20, "output_tokens": 3}, "provider_attempt": True, "elapsed_ms": 1}
        runtime = object.__new__(reader_runtime.ReaderRuntime)
        with patch.object(runtime, "_fresh_assessment", side_effect=fresh):
            from librarian_policy import LibrarianBudget
            runtime.budget = LibrarianBudget()
            assessment = runtime.assess(observation)
        self.assertIsNone(assessment["proposal"])
        frozen, omitted = recording_jobs._freeze_maintenance(assessment["maintenance"], contexts[0],
                                        db_id=ID7, session=SESSION, turn=TURN)
        self.assertEqual(omitted, 0)
        self.assertEqual(frozen[0]["payload"]["expected"], expected)
        receipt = client.concern_checked("project", frozen[0]["payload"], expected_db_id=ID7)
        recording_jobs._checked_maintenance_result(receipt, {"db_id": ID7}, frozen[0])
        with patch("hook_recall.McpClient", return_value=client):
            later = reader_worker._execute(config, "Investigate this setting on another task", Selector(), lambda: True, set())
            repeated = hook_recall.register_reader_concerns(self.config, self.root, ID7,
                       later["concern_nominations"], later["concern_endpoints"], allowed=lambda: True)
        self.assertIn("The tool reported setting=5", prompts[-1])
        self.assertEqual(repeated[0]["expected_row"], durable["row"])
        self.assertEqual(durable["row"]["notice"], expected["notice"])
        self.assertEqual(counts, {"selector": 2, "assessor": 1, "notice": 2, "finding": 1})

    def test_populated_finding_borrows_unused_total_headroom_and_deduplicates_pages(self):
        client = self.setup_client()
        self.context.update(discovery_metadata())
        self.row["finding"] = {"scope": "This inspected deployment", "observation": "Tool reported local settings.",
            "evidence": [{"source_ref": "source-turn:tool-result:" + str(i), "digest": "c" * 64} for i in range(3)]}
        padding = 1116 - hook_recall._bytes(self.row)
        self.assertGreaterEqual(padding, 0)
        self.row["finding"]["observation"] += "x" * padding
        self.assertEqual(hook_recall._bytes(self.row), 1116)
        pages = []
        def checked(db, payload, **kwargs):
            page = {"items": [self.row], "next": None}
            pages.append(page)
            return {"db": "project", "db_id": ID7, "action": "list", "page": page}
        client.concern_checked = checked
        with patch("hook_recall.McpClient", return_value=client):
            result = hook_recall.collect_reader(self.config, "actual task", self.root)
        self.assertEqual(result["outcome"], "ok")
        self.assertEqual(len(result["cards"]), 2)
        self.assertEqual(result["concern_rows"], [self.row])
        self.assertEqual(result["concern_lookup"], "bounded_pages_complete")
        self.assertEqual(len(pages), 2)
        self.assertGreater(sum(hook_recall._bytes(page) for page in pages), hook_recall.MAX_READER_CONTROL_BYTES)
        control = {k: result[k] for k in ("discovery", "concern_endpoints", "concern_rows", "concern_lookup")}
        self.assertGreater(hook_recall._bytes(control), hook_recall.MAX_READER_CONTROL_BYTES)
        self.assertLessEqual(hook_recall._bytes(result), hook_recall.MAX_READER_RESULT_BYTES)
        self.assertLessEqual(reader_content_bytes(result), hook_recall.MAX_READER_BYTES)

        # Maximal ordinary content consumes the spare headroom. Optional lookup
        # remains unknown rather than removing a card or enlarging the envelope.
        ids = [ID1, ID2, ID3, ID4, ID5, ID6, ID7, "01ARZ3NDEKTSV4RRFFQ69G5FB2"]
        self.context["primary"] = [{"id": i} for i in ids]
        self.context["observation"]["cards"] = [{"node_id": i, "lane": "primary",
            "card_sha256": "a" * 64, "graph_path": None} for i in ids]
        for i in ids:
            self.nodes[i] = self._node(i, "active", '"' * 700)
            self.nodes[i]["concern_endpoint"] = {"id": i, "meaning": "a" * 64 if i == ID1 else "b" * 64}
        baseline_client = FakeClient(self.catalog, self.context, self.nodes)
        with patch("hook_recall.McpClient", return_value=baseline_client):
            baseline = hook_recall.collect_reader(self.config, "actual task", self.root)
        pages.clear()
        with patch("hook_recall.McpClient", return_value=client):
            pressured = hook_recall.collect_reader(self.config, "actual task", self.root)
        self.assertEqual(pressured["cards"], baseline["cards"])
        self.assertEqual(pressured["observation"], baseline["observation"])
        self.assertEqual(pressured["concern_lookup"], "unknown")
        self.assertLessEqual(hook_recall._bytes(pressured), hook_recall.MAX_READER_RESULT_BYTES)
        self.assertLessEqual(reader_content_bytes(pressured), hook_recall.MAX_READER_BYTES)

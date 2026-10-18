"""Offline semantic creation-date propagation, exact delivery and old-v7 skew."""
import copy
import hashlib
import json
import unittest
from unittest.mock import patch

import core_hook
import hook_recall
import hooks
import reader_contract
import turn_observer as observer
from fixture_recall_owner import ID1, ID2, ID7, FakeClient, RecallOwnerFixture
from fixture_source_turn import (SourceTurnFixture, SESSION, TURN, opening, delivered,
                                 assistant, event)
from librarian_policy import LibrarianBudget
from fixture_model_dates import epoch_second_display


class SemanticCreatedTests(unittest.TestCase):
    dialogue = [{"role": "user", "text": "Investigate this repository design"}]
    maximum = (1 << 128) - 1

    def setUp(self):
        self.owner = RecallOwnerFixture(self)

    def card(self, **changes):
        return {"id": ID1, "summary": "A scoped historical observation", "kind": "semantic",
                "status": "active", "source": "fixture", "fingerprint": "a" * 64, **changes}

    def packet(self, card):
        packed = hooks._pack_async_delivery([card], SESSION, TURN, ID7)
        return {"schema": observer.DELIVERY_SCHEMA, "session_id": SESSION, "turn_id": TURN,
                "rendered_text": packed["context"],
                "rendered_sha256": hashlib.sha256(packed["context"].encode()).hexdigest(),
                "displayed": packed["displayed"], "concerns": []}

    def test_old_v7_missing_and_new_creation_use_same_read_path(self):
        # A pre-F4 v7 envelope has no creation field; do not synthesize zero/null.
        context = {**self.owner.context, "schema": "mneme.context.v7",
                   "touchstone_retrieval": {"searched": False, "read_limit": 0,
                    "catalog_reads": 0, "record_reads": 0, "referrer_page_reads": 0,
                    "target_reads": 0, "anchors_total": 0, "anchors_examined": 0,
                    "owners_discovered": 0, "further_tail_unknown": False, "stop_reason": None}}
        prior_calls = None
        for created in (None, 0, 1729212345678, self.maximum):
            with self.subTest(created=created):
                source_context = copy.deepcopy(context)
                nodes = copy.deepcopy(self.owner.nodes)
                if created is not None:
                    for entry in source_context["primary"]:
                        entry["created"] = created
                    for node in nodes.values():
                        node["created"] = created
                client = FakeClient(self.owner.catalog, source_context, nodes)
                with patch("hook_recall.McpClient", return_value=client):
                    result = hook_recall._collect(self.owner.config, "actual task", self.owner.root, 1.5)
                self.assertEqual(result["outcome"], "ok")
                self.assertEqual([c["id"] for c in result["cards"]], [ID1, ID2])
                calls = [name for name, _ in client.calls]
                if prior_calls is not None:
                    self.assertEqual(calls, prior_calls)
                prior_calls = calls
                self.assertEqual(calls, ["databases", "recall_context", "get", "get"])
                prompt, _ = reader_contract.prepare(self.dialogue, result["cards"])
                projected = json.loads(prompt.split("PAYLOAD:\n", 1)[1])["cards"]
                for card, projection in zip(result["cards"], projected):
                    self.assertTrue(hooks._valid_card(card))
                    shown = observer.validate_delivery_packet(self.packet(card))["displayed"][0]["displayed_view"]
                    if created is None:
                        self.assertNotIn("created", card)
                    else:
                        self.assertEqual(card["created"], created)
                    for view in (projection, shown):
                        self.assertNotIn("created", view)
                        if created is None:
                            self.assertNotIn("created_utc", view)
                        else:
                            self.assertEqual(view["created_utc"], reader_contract.format_timestamp_millis(created))
                self.assertLessEqual(hook_recall._bytes(result["cards"]), hook_recall.MAX_CARDS_BYTES)

    def test_creation_is_validated_at_each_boundary_without_coercion(self):
        for bad in (True, False, -1, 1 << 128, None, 1.0, "1729212345678"):
            with self.subTest(bad=bad):
                card = self.card(created=bad)
                with self.assertRaises(ValueError):
                    hook_recall._candidate({"id": ID1, "created": bad}, "primary")
                with self.assertRaises(ValueError):
                    hook_recall._card({**self.owner.nodes[ID1], "created": bad}, ID1)
                self.assertFalse(hooks._valid_card(card))
                self.assertEqual(reader_contract.prepare(self.dialogue, [card]), (None, "invalid_input"))
                with self.assertRaises(ValueError):
                    observer.validate_displayed_view({k: v for k, v in card.items() if k != "fingerprint"})
        for node in (self.owner.nodes[ID1], {**self.owner.nodes[ID1], "created": 21}):
            with self.assertRaisesRegex(ValueError, "timestamp mismatch"):
                hook_recall._card(node, ID1, {"id": ID1, "kind": "semantic", "created": 20})

    def test_get_creation_survives_legacy_header_without_an_extra_read(self):
        candidate = hook_recall._candidate({"id": ID1}, "primary")
        card = hook_recall._card({**self.owner.nodes[ID1], "created": 20}, ID1, candidate)
        self.assertEqual(card["created"], 20)

    def test_touchstone_keeps_its_authored_view_alongside_creation(self):
        touchstone = {"schema": "mneme.touchstone-view.v1", "subject": "Remembered warmth",
                      "coverage": "summary_only", "references": [], "references_omitted": 0,
                      "origins": [{"kind": "direct"}]}
        card = self.card(created=20, touchstone=touchstone)
        prompt, _ = reader_contract.prepare(self.dialogue, [card])
        projected = json.loads(prompt.split("PAYLOAD:\n", 1)[1])["cards"][0]
        shown = observer.validate_delivery_packet(self.packet(card))["displayed"][0]["displayed_view"]
        for view in (projected, shown):
            self.assertEqual(view["touchstone"], touchstone)
            self.assertEqual(view["created_utc"], "1970-01-01T00:00:00Z")
            self.assertNotIn("created", view)

    def test_exact_visible_view_hash_and_transcript_replay_bind_creation(self):
        packet = self.packet(self.card(created=1729212345678))
        checked = observer.validate_delivery_packet(packet)
        shown = checked["displayed"][0]
        self.assertEqual(shown["displayed_view_sha256"],
                         hashlib.sha256(observer.encoded(shown["displayed_view"])).hexdigest())
        self.assertIn("not freshness, verification or last update", packet["rendered_text"])
        self.assertIn("verification or last-updated claim", reader_contract.BASE_INSTRUCTIONS)
        self.assertEqual(shown["displayed_view"]["created_utc"], "2024-10-18T00:45:45Z")
        source = SourceTurnFixture(self)
        source.write(opening() + [delivered(packet), assistant(), event("task_complete")])
        observed = source.observe(delivery=packet)
        self.assertEqual(observed["status"], "complete", observed)
        self.assertEqual(observed["coverage"]["memory_delivery"], "admitted_retained")
        self.assertEqual(source.observe(delivery=copy.deepcopy(packet)), observed)
        import recording_contract
        prompt, context = recording_contract.prepare(observed)
        self.assertIsInstance(prompt, str, context)
        evidence = json.loads(prompt.split("PAYLOAD:\n", 1)[1])["evidence"]
        delivery = next(item["text"] for item in evidence if item["kind"] == "memory_delivery")
        self.assertEqual(delivery, packet["rendered_text"])
        self.assertIn('"created_utc":"2024-10-18T00:45:45Z"', delivery)
        self.assertNotIn('"created":', delivery)
        for repair_digest in (False, True):
            altered = copy.deepcopy(packet)
            altered["displayed"][0]["displayed_view"]["created_utc"] = "2024-10-18T00:45:46Z"
            if repair_digest:
                altered["displayed"][0]["displayed_view_sha256"] = hashlib.sha256(
                    observer.encoded(altered["displayed"][0]["displayed_view"])).hexdigest()
            with self.assertRaises(ValueError):
                observer.validate_delivery_packet(altered)
        for replacement in ('"created_utc":true', '"created_utc":-1',
                            '"created_utc":"2024-10-18T00:45:46Z"'):
            altered = copy.deepcopy(packet)
            altered["rendered_text"] = altered["rendered_text"].replace('"created_utc":"2024-10-18T00:45:45Z"', replacement)
            altered["rendered_sha256"] = hashlib.sha256(altered["rendered_text"].encode()).hexdigest()
            with self.assertRaises(ValueError):
                observer.validate_delivery_packet(altered)

    def test_exact_serialized_date_bytes_count_for_selector_and_delivery(self):
        card = self.card(created=self.maximum)
        full_prompt, _ = reader_contract.prepare(self.dialogue, [card])
        size = len(full_prompt.encode())
        class ExactBudget(LibrarianBudget):
            selector_prompt_bytes = property(lambda self: size)
        class ShortBudget(LibrarianBudget):
            selector_prompt_bytes = property(lambda self: size - 1)
        self.assertEqual(reader_contract.prepare(self.dialogue, [card], budget=ExactBudget())[0], full_prompt)
        self.assertEqual(reader_contract.prepare(self.dialogue, [card], budget=ShortBudget()), (None, "prompt_cap"))
        packed = hooks._pack_async_delivery([card], SESSION, TURN, ID7)
        size = len(packed["context"].encode())
        with patch.object(hooks, "MAX_CONTEXT_BYTES", size):
            self.assertEqual(hooks._pack_async_delivery([card], SESSION, TURN, ID7), packed)
        with patch.object(hooks, "MAX_CONTEXT_BYTES", size - 1):
            omitted = hooks._pack_async_delivery([card], SESSION, TURN, ID7)
        self.assertEqual(omitted["cards"], [])
        self.assertEqual(omitted["displayed"], [])
        self.assertEqual(omitted["budget_omitted_count"], 1)
        self.assertEqual(hooks._pack_async_delivery(packed["cards"], SESSION, TURN, ID7), packed)

    def test_utc_whole_seconds_and_out_of_calendar_range_are_exact(self):
        for millis, expected in (
                (0, "1970-01-01T00:00:00Z"), (999, "1970-01-01T00:00:00Z"),
                (1000, "1970-01-01T00:00:01Z"),
                (1729212345678, "2024-10-18T00:45:45Z"),
                (253402300799999, "9999-12-31T23:59:59Z"),
                (253402300800000, "out of calendar range (253402300800000 Unix epoch ms)"),
                (self.maximum, f"out of calendar range ({self.maximum} Unix epoch ms)")):
            with self.subTest(millis=millis):
                self.assertEqual(reader_contract.format_timestamp_millis(millis), expected)
                card = self.card(created=millis)
                original = copy.deepcopy(card)
                prompt, _ = reader_contract.prepare(self.dialogue, [card])
                projected = json.loads(prompt.split("PAYLOAD:\n", 1)[1])["cards"][0]
                packet = self.packet(card)
                for view in (projected, packet["displayed"][0]["displayed_view"]):
                    self.assertNotIn("created", view)
                    self.assertEqual(view["created_utc"], expected)
                self.assertEqual(card, original)

    def test_display_validation_rejects_noncanonical_dates_and_mixed_epochs(self):
        card = self.packet(self.card(created=1729212345678))["displayed"][0]["displayed_view"]
        for date in (True, 1729212345678, "2024-10-18T00:45:45.678Z", "2024-10-18T00:45:45+00:00",
                     "2024-02-30T00:00:00Z", "2024-1-18T00:45:45Z", "1969-12-31T23:59:59Z",
                     "out of calendar range (0 Unix epoch ms)",
                     f"out of calendar range ({1 << 128} Unix epoch ms)"):
            with self.subTest(date=date), self.assertRaises(ValueError):
                observer.validate_displayed_view({**card, "created_utc": date})
        with self.assertRaises(ValueError):
            observer.validate_displayed_view({**card, "created": 1729212345678})

    def test_retained_integer_date_packets_replay_without_rewriting(self):
        episode = self.card(kind="episode", episode_id=ID2, edition_id=ID1, revision=1,
                            current_edition_id=ID1, occurred={"kind": "point", "at": 10},
                            recorded_at=15, edition_recorded_at=20, thread=None,
                            recording_session=None, origins=[{"kind": "lexical"}])
        for card in (self.card(created=1729212345678), episode):
            with self.subTest(kind=card["kind"]):
                packet = self.packet(card)
                old_view = {key: value for key, value in card.items() if key != "fingerprint"}
                packet["displayed"][0]["displayed_view"] = old_view
                packet["displayed"][0]["displayed_view_sha256"] = hashlib.sha256(observer.encoded(old_view)).hexdigest()
                packet["rendered_text"] = observer.DISPLAY_PREFIX + json.dumps(
                    [old_view], ensure_ascii=False, separators=(",", ":"))
                packet["rendered_sha256"] = hashlib.sha256(packet["rendered_text"].encode()).hexdigest()
                original = copy.deepcopy(packet)
                self.assertEqual(observer.validate_delivery_packet(packet), original)
                source = SourceTurnFixture(self)
                source.write(opening() + [delivered(packet), assistant(), event("task_complete")])
                observed = source.observe(delivery=packet)
                self.assertEqual(observed["coverage"]["memory_delivery"], "admitted_retained")
                retained = next(item["packet"] for item in observed["evidence"]
                                if item["kind"] == "memory_delivery")
                self.assertEqual(retained, original)
                self.assertEqual(packet, original)

    def test_episode_unknown_range_and_separate_account_times_render_readably(self):
        card = self.card(kind="episode", episode_id=ID2, edition_id=ID1, revision=1,
                         current_edition_id=ID1, occurred={"kind": "range", "start": 1001, "end": 2002},
                         recorded_at=3003, edition_recorded_at=4004, thread=None,
                         recording_session=None, origins=[{"kind": "lexical"}])
        for occurred, expected in ((card["occurred"], {"kind": "range", "start_utc": "1970-01-01T00:00:01Z",
                                                     "end_utc": "1970-01-01T00:00:02Z"}),
                                   ({"kind": "unknown"}, {"kind": "unknown"})):
            original = {**card, "occurred": occurred}
            prompt, _ = reader_contract.prepare(self.dialogue, [original])
            projected = json.loads(prompt.split("PAYLOAD:\n", 1)[1])["cards"][0]
            shown = observer.validate_delivery_packet(self.packet(original))["displayed"][0]["displayed_view"]
            for view in (projected, shown):
                self.assertEqual(view["occurred_utc"], expected)
                self.assertEqual(view["recorded_at_utc"], "1970-01-01T00:00:03Z")
                self.assertEqual(view["edition_recorded_at_utc"], "1970-01-01T00:00:04Z")
                self.assertTrue(set(view).isdisjoint({"occurred", "recorded_at", "edition_recorded_at", "created"}))
            self.assertEqual(original["recorded_at"], 3003)

    def test_core_creation_is_readable_before_exact_whole_card_packing(self):
        target = {"scope": "project", "db": "project"}
        node = {**self.owner.nodes[ID1], "created": 1729212345678, "tags": ["core"],
                "body": "Remembered context", "body_truncated": False}
        native = {"nodes": [node], "total": 1, "truncated": False}
        original = copy.deepcopy(native)
        section = core_hook._pack(target, native)
        self.assertEqual(section["cards"][0]["created_utc"], "2024-10-18T00:45:45Z")
        self.assertNotIn("created", section["cards"][0])
        self.assertEqual(native, original)
        text = core_hook._context([section])["hookSpecificOutput"]["additionalContext"]
        self.assertIn('"created_utc":"2024-10-18T00:45:45Z"', text)
        self.assertNotIn('"created":', text)
        # Core's existing packer reserves its longer provisional partial status.
        size = len(core_hook._json({**section, "outcome": "partial"}).encode())
        self.assertEqual(core_hook._pack(target, native, limit=size), section)
        self.assertEqual(core_hook._pack(target, native, limit=size - 1)["cards"], [])
        legacy = copy.deepcopy(native)
        del legacy["nodes"][0]["created"]
        self.assertNotIn("created_utc", core_hook._pack(target, legacy)["cards"][0])
        for bad in (True, -1, None, 1 << 128):
            malformed = {**native, "nodes": [{**node, "created": bad}]}
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                core_hook._pack(target, malformed)

    def test_episode_dates_remain_distinct_no_duplicate_semantic_field(self):
        card = self.card(kind="episode", episode_id=ID2, edition_id=ID1, revision=1,
                         current_edition_id=ID1, occurred={"kind": "point", "at": 10},
                         recorded_at=15, edition_recorded_at=20, thread=None,
                         recording_session=None, origins=[{"kind": "lexical"}])
        prompt, _ = reader_contract.prepare(self.dialogue, [card])
        projected = json.loads(prompt.split("PAYLOAD:\n", 1)[1])["cards"][0]
        displayed = self.packet(card)["displayed"][0]["displayed_view"]
        native = {**self.owner.nodes[ID1], "created": 20, "memory_kind": {
            "kind": "episode", "episode": {key: card[key] for key in
                ("episode_id", "revision", "occurred", "recorded_at", "thread")}}}
        collected = hook_recall._card(native, ID1, card)
        for key in ("occurred", "recorded_at", "edition_recorded_at"):
            self.assertEqual(collected[key], card[key])
        expected = epoch_second_display(card)
        for view in (projected, displayed):
            self.assertNotIn("created", view)
            self.assertNotIn("created_utc", view)
            for key in ("occurred", "recorded_at", "edition_recorded_at"):
                self.assertNotIn(key, view)
                self.assertEqual(view[key + "_utc"], expected[key + "_utc"])
        self.assertFalse(hooks._valid_card({**card, "created": 20}))
        self.assertEqual(reader_contract.prepare(self.dialogue, [{**card, "created": 20}]),
                         (None, "invalid_input"))
        with self.assertRaises(ValueError):
            observer.validate_displayed_view({**displayed, "created": 20})

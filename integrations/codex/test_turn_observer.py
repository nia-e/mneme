"""Synthetic source-turn admission/observation; no providers or real transcripts."""
from __future__ import annotations

from dataclasses import FrozenInstanceError, replace
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import turn_observer as observer
from fixture_source_turn import (
    META, PROMPT, SESSION, TURN, SourceTurnFixture, assistant, call, complete_turn,
    context, delivered, delivery_packet, encoded, event, header, opening, output,
    refresh_delivery_packet, row, sha, start, user,
)


class TurnObserverTests(unittest.TestCase, SourceTurnFixture):
    def setUp(self):
        SourceTurnFixture.__init__(self, self)

    def test_root_session_sources_preserve_desktop_and_legacy_admission(self):
        for fields in ({}, {"source": "vscode", "originator": "codex-tui"},
                       {"source": "vscode", "originator": "Codex Desktop"},
                       {"source": "cli"}, {"source": "exec"}):
            with self.subTest(fields=fields):
                rows = complete_turn()
                rows[0]["payload"].update(fields)
                self.write(rows)
                self.assertEqual(self.observe()["status"], "complete")

    def test_explicit_subagent_session_metadata_is_rejected_before_turn_admission(self):
        # Header-only local inspection found this thread_spawn source envelope;
        # originator is the same as a desktop root, not a role discriminator.
        spawned = {"subagent": {"thread_spawn": {
            "parent_thread_id": "synthetic-parent", "depth": 1,
            "agent_path": "/root/reviewer", "agent_role": "explorer",
            "agent_nickname": "Synthetic"}}}
        for fields in ({"source": spawned, "parent_thread_id": "synthetic-parent",
                        "originator": "codex-tui"},
                       {"source": "subagent"}, {"source": {"subagent": None}},
                       {"agent_id": "child"}, {"agent_type": "worker"}):
            with self.subTest(fields=fields):
                rows = complete_turn()
                rows[0]["payload"].update(fields)
                self.write(rows)
                result = self.admit()
                self.assertEqual(result["status"], "deferred", result)
                self.assertEqual(result["reason"], "subagent_session_source")
                self.assertIsNone(result["admission"])

    def test_direct_observation_cannot_bypass_subagent_header_check(self):
        self.write(complete_turn())
        pin = self.admission()
        rows = complete_turn()
        rows[0]["payload"]["source"] = {"subagent": {"thread_spawn": {"depth": 1}}}
        self.write(rows)
        # Supply correct replacement anchors explicitly, as a supporting-API
        # caller could. A valid hash binds bytes, not root-session authority.
        header_bytes, start_bytes = encoded(rows[:1]), encoded(rows[1:2])
        pin = replace(pin,
                      session_record=observer.RecordAnchor(0, len(header_bytes), sha(header_bytes)),
                      start_record=observer.RecordAnchor(len(header_bytes), len(start_bytes), sha(start_bytes)))
        result = self.observe(pin)
        self.assertEqual(result["status"], "deferred", result)
        self.assertEqual(result["reason"], "subagent_session_source")
        self.assertEqual(result["evidence"], [])

    def test_unknown_session_source_shapes_do_not_establish_root_scope(self):
        for source in (None, False, [], {}, {"future_agent": {}}, "future-source"):
            with self.subTest(source=source):
                rows = complete_turn()
                rows[0]["payload"]["source"] = source
                self.write(rows)
                result = self.admit()
                self.assertEqual(result["status"], "deferred", result)
                self.assertEqual(result["reason"], "unsupported_session_source")

    @staticmethod
    def collaboration(*, identifier="agent-message", turn=TURN, encrypted=False):
        # Actual Codex compact envelope, with synthetic content only. MESSAGE
        # may include opaque encrypted content; FINAL_ANSWER may be text-only.
        content = [{"type": "input_text", "text": "UNTRUSTED_AGENT_TEXT"}]
        if encrypted:
            content.append({"type": "encrypted_content", "encrypted_content": "OPAQUE_AGENT_PAYLOAD"})
        return row("response_item", {"type": "agent_message", "id": identifier,
                   "author": "/root/reviewer", "recipient": "/root", "content": content,
                   META: {"turn_id": turn, "create_time": 1790867655.506312}})

    def test_collaboration_is_omitted_and_later_user_preference_survives(self):
        self.write(opening() + [call(), self.collaboration(), output(),
                   self.collaboration(identifier="encrypted-agent", encrypted=True),
                   user("Prefer Sol subagents for cross-review.", identifier="preference"),
                   assistant(), event("task_complete")])
        before = self.path.read_bytes()
        result = self.observe()
        self.assertEqual(result["status"], "complete", result)
        self.assertEqual(result["coverage"]["omissions"]["collaboration_input"], 2)
        text = json.dumps(result)
        self.assertIn("Prefer Sol subagents", text)
        self.assertNotIn("UNTRUSTED_AGENT_TEXT", text)
        self.assertNotIn("OPAQUE_AGENT_PAYLOAD", text)
        self.assertEqual([i["kind"] for i in result["evidence"]],
                         ["user_statement", "tool_call", "tool_result", "user_statement",
                          "assistant_assertion"])
        self.assertEqual(self.path.read_bytes(), before)

    def test_collaboration_requires_same_turn_and_unique_item_identity(self):
        for message, reason in ((self.collaboration(turn="other"), "agent_message_identity_mismatch"),
                                (self.collaboration(identifier=""), "agent_message_identity_mismatch"),
                                (self.collaboration(identifier="prompt"), "repeated_response_item_id")):
            with self.subTest(reason=reason):
                self.write(opening() + [message, assistant(), event("task_complete")])
                result = self.observe()
                self.assertEqual(result["status"], "deferred", result)
                self.assertEqual(result["reason"], reason)

    def test_many_collaboration_deliveries_do_not_consume_evidence_slots(self):
        self.write(opening() + [self.collaboration(identifier=f"agent-{i}", encrypted=i % 2 == 0)
                   for i in range(40)] + [
                   user("Use Sol workers.", identifier="preference"),
                   assistant("I adopted the returned work packet."), event("task_complete")])
        result = self.observe(limits=observer.Limits(items=3))
        self.assertEqual(result["status"], "complete", result)
        self.assertEqual(result["coverage"]["omissions"]["collaboration_input"], 40)
        self.assertEqual(result["coverage"]["public_evidence"]["observed_records"], 3)
        self.assertEqual(result["coverage"]["public_evidence"]["omitted_records"], 0)
        self.assertEqual(result["evidence"][-1]["content"][0]["text"],
                         "I adopted the returned work packet.")

    def test_collaboration_does_not_hide_interruption_or_missing_closure(self):
        for boundary, reason in (([event("context_compacted")], "source_task_interrupted"),
                                 ([event("turn_aborted")], "source_task_interrupted"),
                                 ([], "source_turn_not_closed")):
            with self.subTest(reason=reason):
                self.write(opening() + [self.collaboration(encrypted=True), *boundary,
                           user("Use Sol workers.", identifier="preference"), assistant()])
                result = self.observe()
                self.assertEqual(result["status"], "deferred", result)
                self.assertEqual(result["reason"], reason)

    def test_malformed_or_future_collaboration_shapes_defer(self):
        changes = [{"author": None}, {"author": "/root/\nuser"}, {"recipient": "user"},
                   {"content": None}, {"content": []}, {"new_field": "future"},
                   {"content": [{"type": "input_text", "text": 5}]},
                   {"content": [{"type": "encrypted_content", "encrypted_content": []}]},
                   {"content": [{"type": "future_content", "text": "unknown"}]},
                   {"content": [{"type": [], "text": "unknown"}]},
                   {"content": [{"type": "input_text", "text": "x", "authority": "user"}]},
                   {"content": ["not-a-slot"]},
                   {"content": [{"type": "input_text", "text": "x"}] * 33}]
        for change in changes:
            with self.subTest(change=change):
                message = self.collaboration()
                message["payload"].update(change)
                self.write(opening() + [message, assistant(), event("task_complete")])
                result = self.observe()
                self.assertEqual(result["status"], "deferred", result)
                self.assertEqual(result["reason"], "unsupported_agent_message")
        message = self.collaboration()
        message["payload"]["type"] = "future_agent_message"
        self.write(opening() + [message, assistant(), event("task_complete")])
        self.assertEqual(self.observe()["reason"], "unsupported_response_item")

    def test_omitted_collaboration_still_consumes_byte_and_event_allowances(self):
        for encrypted in (False, True):
            message = self.collaboration(encrypted=encrypted)
            slot = message["payload"]["content"][-1]
            slot["encrypted_content" if encrypted else "text"] = "x" * 1024
            self.write(opening() + [message, assistant(), event("task_complete")])
            result = self.observe(limits=observer.Limits(item_bytes=1024))
            self.assertEqual(result["status"], "deferred", result)
            self.assertEqual(result["reason"], "agent_message_bytes_exceeded")
        self.write(opening() + [self.collaboration(identifier=f"agent-{i}") for i in range(10)]
                   + [assistant(), event("task_complete")])
        result = self.observe(self.admission(), limits=observer.Limits(events=6))
        self.assertEqual(result["status"], "deferred", result)
        self.assertEqual(result["reason"], "event_limit_exceeded")

    def test_delivery_packet_is_bounded_copied_and_identity_bound(self):
        packet = delivery_packet()
        packet["displayed"][0]["shown_summary"] = "x" * 800 + "…"
        refresh_delivery_packet(packet)
        copied = observer.validate_delivery_packet(packet, session_id=SESSION, turn_id=TURN)
        copied["displayed"][0]["shown_summary"] = "changed"
        self.assertNotEqual(copied, packet)
        for change in ({"rendered_sha256": "b" * 64}, {"turn_id": "wrong"},
                       {"rendered_text": "x" * 4097}, {"displayed": []}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                observer.validate_delivery_packet({**packet, **change}, turn_id=TURN)

    def test_conditional_delivery_binding_is_optional_exclusive_and_never_graph(self):
        from test_routing_memory import binding, DB, TARGET, PREVIOUS
        packet = delivery_packet()
        card = packet["displayed"][0]
        card.update(db_id=DB, node_id=TARGET, entry_kind="conditional", conditional_binding=binding())
        refresh_delivery_packet(packet)
        copied = observer.validate_delivery_packet(packet)
        self.assertEqual(copied, packet)
        for change in ({"conditional_binding": None}, {"node_id": PREVIOUS},
                       {"db_id": PREVIOUS}, {"routing_binding": binding()}):
            with self.subTest(change=change):
                altered = json.loads(json.dumps(packet))
                altered["displayed"][0].update(change)
                refresh_delivery_packet(altered)
                cleaned = observer.validate_delivery_packet(altered)["displayed"][0]
                self.assertEqual(cleaned["entry_kind"], "conditional")
                self.assertNotIn("routing_binding", cleaned)
                self.assertNotIn("conditional_binding", cleaned)
                self.assertEqual(cleaned["shown_summary"], card["shown_summary"])
        for change in ({"entry_kind": "unknown", "routing_binding": binding()},
                       {"routing_binding": binding()}):
            altered = json.loads(json.dumps(packet))
            altered["displayed"][0].pop("entry_kind")
            altered["displayed"][0].update(change)
            cleaned = observer.validate_delivery_packet(altered)["displayed"][0]
            self.assertNotIn("routing_binding", cleaned)
            self.assertNotIn("conditional_binding", cleaned)

    def test_conditional_packet_byte_pressure_sheds_binding_not_advice_origin(self):
        from test_routing_memory import binding, DB, TARGET
        packet = delivery_packet()
        card = packet["displayed"][0]
        card.update(db_id=DB, node_id=TARGET, entry_kind="conditional", conditional_binding=binding())
        refresh_delivery_packet(packet)
        core = json.loads(json.dumps(packet))
        core["displayed"][0].pop("conditional_binding")
        with patch.object(observer, "MAX_DELIVERY_BYTES", len(observer.encoded(core))):
            self.assertEqual(observer.validate_delivery_packet(packet), core)
        advice_only = json.loads(json.dumps(core))
        advice_only["displayed"][0].pop("entry_kind")
        with patch.object(observer, "MAX_DELIVERY_BYTES", len(observer.encoded(advice_only))):
            self.assertEqual(observer.validate_delivery_packet(packet), advice_only)

    def test_exact_delivery_marker_survives_with_source_binding(self):
        packet = delivery_packet()
        self.write(opening() + [call(), delivered(packet), output(), assistant(), event("task_complete")])
        observed = self.observe(delivery=packet)
        self.assertEqual(observed["status"], "complete")
        self.assertEqual(observed["coverage"]["memory_delivery"], "admitted_retained")
        marker = next(i for i in observed["evidence"] if i["kind"] == "memory_delivery")
        self.assertEqual(marker["packet"], packet)
        self.assertEqual(marker["content_index"], 0)
        self.assertEqual(marker["ref"]["item_id"], "memory")

    def test_variable_card_delivery_flows_into_recording_contract_unchanged(self):
        import hooks
        import recording_contract
        cards = [{"id": "01ARZ3NDEKTSV4RRFFQ69G5FA" + str(i), "summary": "Useful lesson " + str(i),
                  "status": "active", "source": "test", "fingerprint": str(i) * 64}
                 for i in range(8)]
        packed = hooks._pack_async_delivery(cards, SESSION, TURN, "0" * 26)
        packet = {"schema": observer.DELIVERY_SCHEMA, "session_id": SESSION, "turn_id": TURN,
                  "rendered_text": packed["context"], "rendered_sha256": sha(packed["context"].encode()),
                  "displayed": packed["displayed"], "concerns": []}
        self.assertEqual(observer.validate_delivery_packet(packet), packet)
        self.write(opening() + [call(), delivered(packet), output(), assistant(), event("task_complete")])
        observed = self.observe(delivery=packet)
        self.assertEqual(observed["status"], "complete")
        marker = next(i for i in observed["evidence"] if i["kind"] == "memory_delivery")
        self.assertEqual(len(marker["packet"]["displayed"]), 8)
        prompt, contract = recording_contract.prepare(observed)
        self.assertIsNotNone(prompt, contract)
        payload = json.loads(prompt.split("\nPAYLOAD:\n", 1)[1])
        self.assertEqual(len(payload["delivered_cards"]), 8)
        self.assertEqual([c["summary"] for c in payload["delivered_cards"]], [c["summary"] for c in cards])

    def test_variable_card_packet_byte_cap_uniqueness_and_foreign_identity(self):
        packet = delivery_packet()
        base = packet["displayed"][0]
        packet["displayed"] = [{**base, "node_id": "0" * 25 + str(i)} for i in range(8)]
        refresh_delivery_packet(packet)
        self.assertEqual(len(observer.validate_delivery_packet(packet)["displayed"]), 8)
        for displayed in (packet["displayed"] + [packet["displayed"][0]],
                          [{**base, "node_id": "foreign"}],
                          [{**base, "node_id": "0" * 24 + f"{i:02}", "shown_summary": "x" * 800}
                           for i in range(12)]):
            with self.subTest(displayed=displayed), self.assertRaises(ValueError):
                observer.validate_delivery_packet({**packet, "displayed": displayed})

    def test_missing_or_wrong_optional_delivery_does_not_refuse_ordinary_turn(self):
        packet = delivery_packet()
        for message, expected in ((None, "not_admitted"), (delivered(packet, role="user"), "not_admitted"),
                                  (delivered(packet, tag="other"), "ambiguous"),
                                  (delivered(packet, turn="other"), "ambiguous")):
            with self.subTest(message=message):
                self.write(opening() + ([message] if message else []) +
                           [call(), output(), assistant(), event("task_complete")])
                observed = self.observe(delivery=packet)
                self.assertEqual(observed["status"], "complete", observed)
                self.assertEqual(observed["coverage"]["memory_delivery"], expected)

    def test_stale_delivery_schemas_are_omitted_without_rewriting_history(self):
        for schema in (observer.LEGACY_DELIVERY_SCHEMA, "mneme.codex-memory-delivery.v2", "mneme.codex-memory-delivery.v3"):
            with self.subTest(schema=schema):
                packet = delivery_packet()
                packet["schema"] = schema
                for card in packet["displayed"]:
                    card.pop("displayed_view")
                    card.pop("displayed_view_sha256")
                if schema == observer.LEGACY_DELIVERY_SCHEMA:
                    packet.pop("concerns")
                self.write(opening() + [call(), delivered(packet), output(), assistant(), event("task_complete")])
                before = self.path.read_bytes()
                observed = self.observe(delivery=packet)
                self.assertEqual(observed["status"], "complete")
                self.assertEqual(observed["coverage"]["memory_delivery"], "invalid_packet")
                self.assertFalse(any(item["kind"] == "memory_delivery" for item in observed["evidence"]))
                self.assertEqual(self.path.read_bytes(), before)
                self.assertFalse(any(i["kind"] == "memory_delivery" for i in observed["evidence"]))
        self.write(complete_turn())
        observed = self.observe(delivery={**packet, "rendered_sha256": "b" * 64})
        self.assertEqual(observed["status"], "complete")
        self.assertEqual(observed["coverage"]["memory_delivery"], "invalid_packet")

    def test_duplicate_admission_drops_marker_not_ordinary_evidence(self):
        packet = delivery_packet()
        self.write(opening() + [delivered(packet), call(), output(),
                   delivered(packet, identifier="memory-again"), assistant(), event("task_complete")])
        observed = self.observe(delivery=packet, limits=observer.Limits(items=4))
        self.assertEqual(observed["status"], "complete", observed)
        self.assertEqual(observed["coverage"]["memory_delivery"], "ambiguous")
        self.assertEqual([i["kind"] for i in observed["evidence"]],
                         ["user_statement", "tool_call", "tool_result", "assistant_assertion"])
        self.assertEqual(observed["coverage"]["public_evidence"]["observed_records"], 4)

    def test_ambiguous_matching_slots_invalidate_delivery_in_either_order(self):
        packet = delivery_packet()
        bad = delivered(packet, identifier="ambiguous")
        bad["payload"]["content"] *= 2
        bad["payload"][META]["content_item_kinds"] *= 2
        for messages in ([delivered(packet), bad], [bad, delivered(packet)], [bad]):
            with self.subTest(messages=messages):
                self.write(opening() + messages + [call(), output(), assistant(), event("task_complete")])
                observed = self.observe(delivery=packet)
                self.assertEqual(observed["status"], "complete", observed)
                self.assertEqual(observed["coverage"]["memory_delivery"], "ambiguous")
                self.assertEqual(observed["coverage"]["public_evidence"]["observed_records"], 4)
                self.assertFalse(any(i["kind"] == "memory_delivery" for i in observed["evidence"]))

    def test_marker_reserves_room_before_optional_pair_at_exact_item_capacity(self):
        packet = delivery_packet()
        self.write(opening() + [call(), delivered(packet), output(), assistant(), event("task_complete")])
        observed = self.observe(delivery=packet, limits=observer.Limits(items=4))
        self.assertEqual(observed["status"], "complete", observed)
        self.assertEqual(observed["coverage"]["memory_delivery"], "admitted_retained")
        self.assertEqual([i["kind"] for i in observed["evidence"]],
                         ["user_statement", "memory_delivery", "assistant_assertion"])
        self.assertEqual(observed["coverage"]["public_evidence"], {
            "mode": "selected_suffix", "observed_records": 5,
            "selected_records": 3, "omitted_records": 2})

    def test_delivery_anchor_survives_large_intervening_block_without_rescuing_outcomes(self):
        packet = delivery_packet()
        for large_block, limits in (
                ([call("large"), delivered(packet),
                  output("large", text="OMITTED_FAILURE:" + "x" * 33000)], observer.Limits()),
                ([call("large"), delivered(packet),
                  assistant("OMITTED_DETAIL:" + "x" * 20000, identifier="middle", phase="commentary"),
                  call("interleaved"), output("large", text="OMITTED_FAILURE:" + "x" * 20000),
                  output("interleaved")], observer.Limits(evidence_bytes=12000))):
            with self.subTest(limits=limits):
                self.write(opening() + [assistant("OLDER_SUCCESS", identifier="old", phase="commentary")]
                           + large_block + [call("recent"), output("recent", text="later observation"),
                                            assistant(), event("task_complete")])
                ordinary = self.observe(limits=limits)
                full = self.observe(delivery=packet)
                selected = self.observe(delivery=packet, limits=limits)
                self.assertEqual(selected["status"], "complete", selected)
                self.assertEqual(selected["coverage"]["memory_delivery"], "admitted_retained")
                self.assertEqual([item for item in selected["evidence"] if item["kind"] != "memory_delivery"],
                                 ordinary["evidence"])
                marker = next(item for item in selected["evidence"] if item["kind"] == "memory_delivery")
                self.assertEqual(marker["packet"], packet)
                self.assertEqual(marker["ref"]["item_id"], "memory")
                self.assertEqual(marker["content_index"], 0)
                if full["coverage"]["memory_delivery"] == "admitted_retained":
                    self.assertEqual(marker, next(item for item in full["evidence"]
                                                  if item["kind"] == "memory_delivery"))
                self.assertNotIn("OLDER_SUCCESS", json.dumps(selected["evidence"]))
                self.assertNotIn("OMITTED_FAILURE", json.dumps(selected["evidence"]))
                self.assertNotIn("OMITTED_DETAIL", json.dumps(selected["evidence"]))
                self.assertEqual([item["call_id"] for item in selected["evidence"]
                                  if item["kind"] in ("tool_call", "tool_result")], ["recent", "recent"])
                self.assertEqual([item["ref"]["ordinal"] for item in selected["evidence"]],
                                 sorted(item["ref"]["ordinal"] for item in selected["evidence"]))
                selection = selected["coverage"]["public_evidence"]
                self.assertEqual(selection["observed_records"],
                                 ordinary["coverage"]["public_evidence"]["observed_records"] + 1)
                self.assertEqual(selection["omitted_records"],
                                 ordinary["coverage"]["public_evidence"]["omitted_records"])
                raw_source = self.path.read_bytes()
                for item in selected["evidence"]:
                    ref = item["ref"]
                    self.assertEqual(ref["raw_line_sha256"], sha(raw_source[
                        ref["byte_offset"]:ref["byte_offset"] + ref["line_bytes"]]))

    def test_delivery_anchor_is_optional_at_exact_byte_capacity(self):
        packet = delivery_packet()
        self.write(opening() + [call(), delivered(packet), output(), assistant(), event("task_complete")])
        ordinary = self.observe()
        bound = len(observer.encoded(ordinary["evidence"]))
        selected = self.observe(delivery=packet, limits=observer.Limits(evidence_bytes=bound))
        self.assertEqual(selected["status"], "complete", selected)
        self.assertEqual(selected["evidence"], ordinary["evidence"])
        self.assertEqual(selected["coverage"]["memory_delivery"], "admitted_omitted")
        self.assertEqual(selected["work"]["evidence_bytes"], bound)

    def test_oversized_delivery_marker_does_not_poison_ordinary_component(self):
        packet = delivery_packet(text="Memory: " + "x" * 4000)
        self.write(opening() + [call(), delivered(packet), output(), assistant(), event("task_complete")])
        ordinary = self.observe()
        bound = max(len(observer.encoded(item)) for item in ordinary["evidence"])
        selected = self.observe(delivery=packet, limits=observer.Limits(item_bytes=bound))
        self.assertEqual(selected["status"], "complete", selected)
        self.assertEqual(selected["evidence"], ordinary["evidence"])
        self.assertEqual(selected["coverage"]["memory_delivery"], "admitted_omitted")
        self.assertEqual(selected["coverage"]["public_evidence"]["observed_records"], 5)

    def test_shared_selector_omit_and_duplicate_cannot_restore_delivery_anchor(self):
        packet = delivery_packet()
        self.write(opening() + [call(), delivered(packet), output(), assistant(), event("task_complete")])
        observed = self.observe(delivery=packet)
        marker = next(item for item in observed["evidence"] if item["kind"] == "memory_delivery")
        ordinary = [item for item in observed["evidence"] if item["kind"] != "memory_delivery"]
        fits = lambda items: len(items) <= 5
        selector = observer.PublicEvidenceSelector(items=5, item_bytes=32768, evidence_bytes=131072)
        for item in observed["evidence"]:
            selector.add(item)
        self.assertEqual(selector.select(fits), observed["evidence"])
        self.assertEqual(selector.select(fits, omit_kinds=("memory_delivery",)), ordinary)
        selector.add({**marker, "ref": {**marker["ref"], "ordinal": 999}})
        self.assertEqual(selector.select(fits), ordinary)
        self.assertEqual(selector.observed_records, 6)

    def test_delivery_reservation_keeps_whole_blocks_under_item_pressure(self):
        packet = delivery_packet()
        self.write(opening() + [call("a"), delivered(packet),
                   assistant("intervening", identifier="middle", phase="commentary"),
                   call("b"), output("a"), output("b"), call("recent"), output("recent"),
                   assistant(), event("task_complete")])
        for count in (2, 3, 4, 5, 8, 9, 10):
            with self.subTest(items=count):
                limits = observer.Limits(items=count)
                ordinary = self.observe(limits=limits)
                selected = self.observe(delivery=packet, limits=limits)
                self.assertEqual(selected["status"], ordinary["status"])
                self.assertLessEqual(len(selected["evidence"]), count)
                if count == 2:
                    self.assertEqual(selected["evidence"], ordinary["evidence"])
                    self.assertEqual(selected["coverage"]["memory_delivery"], "admitted_omitted")
                else:
                    self.assertEqual(selected["coverage"]["memory_delivery"], "admitted_retained")
                calls = [i["call_id"] for i in selected["evidence"] if i["kind"] == "tool_call"]
                results = [i["call_id"] for i in selected["evidence"] if i["kind"] == "tool_result"]
                self.assertEqual(calls, results)
                self.assertEqual(selected["evidence"][0]["kind"], "user_statement")
                self.assertEqual(selected["evidence"][-1]["phase"], "final_answer")

    def test_reserved_early_multicard_delivery_survives_saturating_byte_suffix(self):
        packet = delivery_packet(text="Memory: " + "x" * 1200)
        packet["displayed"] = [{**packet["displayed"][0], "node_id": "0" * 25 + str(i),
                                "shown_summary": "Exact historical card " + str(i)} for i in (1, 2)]
        refresh_delivery_packet(packet)
        recent = []
        for i in range(6):
            name = "recent" if i == 5 else "older" + str(i)
            recent += [call(name), output(name, text="Independent check " + "x" * 500)]
        self.write(opening() + [delivered(packet), call("large"),
            output("large", text="OMITTED_FAILURE:" + "x" * 33000)] + recent +
            [assistant(), event("task_complete")])
        full = self.observe(delivery=packet)
        desired = [i for i in full["evidence"] if i["kind"] in
                   ("user_statement", "memory_delivery", "assistant_assertion")
                   or i.get("call_id") == "recent"]
        cap = len(observer.encoded(desired))
        limits = observer.Limits(evidence_bytes=cap)
        ordinary = self.observe(limits=limits)
        observed = self.observe(delivery=packet, limits=limits)
        self.assertEqual(observed["status"], "complete", observed)
        self.assertEqual(observed["evidence"], desired)
        self.assertEqual(observed["work"]["evidence_bytes"], cap)
        self.assertGreater(len(ordinary["evidence"]), len(desired) - 1)
        self.assertEqual(observed["coverage"]["memory_delivery"], "admitted_retained")
        self.assertEqual(observed["coverage"]["public_evidence"]["omitted_records"], 12)
        self.assertNotIn("OMITTED_FAILURE", json.dumps(observed["evidence"]))
        marker = next(i for i in observed["evidence"] if i["kind"] == "memory_delivery")
        self.assertEqual(marker["packet"], packet)
        self.assertEqual(marker["ref"]["item_id"], "memory")
        self.assertEqual(marker["content_index"], 0)
        raw = self.path.read_bytes()
        for item in observed["evidence"]:
            ref = item["ref"]
            self.assertEqual(ref["raw_line_sha256"], sha(raw[ref["byte_offset"]:
                ref["byte_offset"] + ref["line_bytes"]]))

    def test_default_caps_balance_exact_delivery_and_saturating_work_at_both_stages(self):
        import recording_contract as contract
        packet = delivery_packet(text="Memory: " + "x" * 3800)
        packet["displayed"] = [{**packet["displayed"][0], "node_id": "0" * 25 + str(i),
                                "shown_summary": "Exact card " + str(i)} for i in (1, 2)]
        refresh_delivery_packet(packet)
        rows = opening() + [delivered(packet), call("large"),
                            output("large", text="OMITTED_FAILURE:" + "x" * 33000)]
        for i in range(40):
            rows += [call("pair" + str(i)), output("pair" + str(i), text="Current check: " + "x" * 3000)]
        self.write(rows + [assistant(), event("task_complete")])
        observed = self.observe(delivery=packet)
        self.assertEqual(observed["status"], "complete", observed)
        self.assertEqual(observed["coverage"]["memory_delivery"], "admitted_retained")
        self.assertGreater(observed["coverage"]["public_evidence"]["omitted_records"], 0)
        self.assertLessEqual(len(observed["evidence"]), observer.Limits().items)
        self.assertLessEqual(observed["work"]["evidence_bytes"], observer.Limits().evidence_bytes)
        self.assertTrue(all(len(observer.encoded(i)) <= observer.Limits().item_bytes for i in observed["evidence"]))
        marker = next(i for i in observed["evidence"] if i["kind"] == "memory_delivery")
        self.assertEqual(marker["packet"], packet)
        before = json.loads(json.dumps(observed))
        prompt, context = contract.prepare(observed)
        self.assertIsNotNone(prompt, context)
        self.assertEqual(observed, before)
        payload = json.loads(prompt[len(contract.PROMPT_PREFIX):])
        self.assertEqual(payload["coverage"]["memory_delivery"], "admitted_retained")
        self.assertGreater(payload["coverage"]["public_evidence"]["omitted_records"],
                           observed["coverage"]["public_evidence"]["omitted_records"])
        self.assertLessEqual(context.authored_bytes, contract.MAX_AUTHORED_BYTES)
        bound = next(b for b in context.bindings if b.kind == "memory_delivery")
        self.assertEqual(json.loads(bound.source_ref_json), marker["ref"])
        self.assertEqual(bound.text, packet["rendered_text"])
        self.assertEqual([t.native_id for t in context.association_bindings],
                         [c["node_id"] for c in packet["displayed"]])
        self.assertTrue(all(t.origin == "delivery" and t.routing_binding_json is None
                            for t in context.association_bindings))
        by_id = {b.evidence_id: b for b in context.bindings}
        latest_call, latest_result = context.tool_pairs[-1]
        self.assertEqual(json.loads(by_id[latest_call].source_ref_json)["item_id"], "call-pair39")
        self.assertEqual(json.loads(by_id[latest_result].source_ref_json)["item_id"], "result-pair39")
        timing = {e["id"]: e["relative_to_memory"] for e in payload["evidence"]}
        self.assertEqual((timing[latest_call], timing[latest_result]), ("after_admission", "after_admission"))
        self.assertNotIn("OMITTED_FAILURE", prompt)
        self.assertIsNone(contract.validate_answer({"proposal": None}, context)["proposal"])
        only_memory = {"proposal": {"kind": "lesson", "summary": "Scoped note", "body": "Scoped experience",
            "evidence_ids": [bound.evidence_id], "associate_with": "shown001"}}
        with self.assertRaisesRegex(ValueError, "delivery_source_evidence_missing"):
            contract.validate_answer(only_memory, context)

    def test_delivery_before_verified_prompt_is_not_attached(self):
        packet = delivery_packet()
        self.write([header(), start(), delivered(packet), user(), assistant(), event("task_complete")])
        observed = self.observe(delivery=packet)
        self.assertEqual(observed["status"], "complete", observed)
        self.assertEqual(observed["coverage"]["memory_delivery"], "not_admitted")

    def deferred(self, result):
        self.assertEqual(result["status"], "deferred", result)
        self.assertIsInstance(result["reason"], str)
        self.assertTrue(result["reason"])
        self.assertIn("coverage", result)
        for key in ("bytes_read", "records_parsed"):
            self.assertIsInstance(result["work"][key], int)
            self.assertGreaterEqual(result["work"][key], 0)

    def assert_work(self, result, *, bound):
        self.assertGreaterEqual(result["work"]["bytes_read"], 0)
        self.assertLessEqual(result["work"]["bytes_read"], bound)
        self.assertGreaterEqual(result["work"]["records_parsed"], 0)

    def test_exact_source_turn_without_any_memory_packet(self):
        self.write(complete_turn())
        before = self.path.read_bytes()
        pin = self.admission()
        result = self.observe(pin)
        self.assertEqual(result["status"], "complete", result)
        self.assertEqual([item["kind"] for item in result["evidence"]],
                         ["user_statement", "tool_call", "tool_result", "assistant_assertion"])
        self.assertEqual([item["call_id"] for item in result["evidence"]
                          if item["kind"] in ("tool_call", "tool_result")], ["read", "read"])
        self.assertEqual(self.path.read_bytes(), before)
        for item in result["evidence"]:
            reference = item["ref"]
            self.assertEqual(reference["ordinal_scope"], "source_turn")
            raw = before[reference["byte_offset"]:
                         reference["byte_offset"] + reference["line_bytes"]]
            self.assertEqual(reference["raw_line_sha256"], sha(raw))

    def test_admission_and_record_anchors_are_frozen(self):
        self.write(complete_turn())
        pin = self.admission()
        self.assertIsInstance(pin, observer.TurnAdmission)
        self.assertEqual(pin.session_id, SESSION)
        self.assertEqual(pin.turn_id, TURN)
        self.assertEqual(pin.expected_prompt_sha256, sha(PROMPT.encode()))
        self.assertEqual(pin.device, self.path.stat().st_dev)
        self.assertEqual(pin.inode, self.path.stat().st_ino)
        with self.assertRaises(FrozenInstanceError):
            pin.turn_id = "another"
        with self.assertRaises(FrozenInstanceError):
            pin.start_record.byte_offset = 0
        raw = self.path.read_bytes()
        for anchor in (pin.session_record, pin.start_record):
            self.assertEqual(anchor.raw_line_sha256,
                             sha(raw[anchor.byte_offset:anchor.byte_offset + anchor.line_bytes]))

    def test_turn_context_is_optional_but_must_match_when_present(self):
        for contexts, expected in (([], "complete"), ([context()], "complete"),
                                   ([context("another-turn")], "deferred")):
            with self.subTest(contexts=contexts):
                self.write([header(), start(), *contexts, user(), assistant(),
                            event("task_complete")])
                result = self.observe()
                self.assertEqual(result["status"], expected, result)
                if expected == "deferred":
                    self.assertEqual(result["reason"], "turn_changed")

    def test_no_tool_final_and_short_user_correction_are_observable(self):
        rows = opening() + [user("no, R5", identifier="correction"),
                            assistant("R5, corrected."), event("task_complete")]
        self.write(rows)
        result = self.observe()
        self.assertEqual(result["status"], "complete", result)
        self.assertEqual([item["kind"] for item in result["evidence"]],
                         ["user_statement", "user_statement", "assistant_assertion"])
        self.assertIn("no, R5", json.dumps(result["evidence"]))

    def test_user_text_is_not_environment_or_normalized_projection(self):
        environment = user("ENVIRONMENT_ONLY", identifier="environment",
                           kind="environments.environment_context")
        projection = row("event_msg", {"type": "item_completed", "turn_id": TURN,
                         "item": {"type": "UserMessage", "text": "PROJECTION_ONLY"}})
        self.write([header(), start(), context(), environment, user(), projection,
                    assistant(), event("task_complete")])
        result = self.observe()
        self.assertEqual(result["status"], "complete", result)
        text = json.dumps(result["evidence"])
        self.assertNotIn("ENVIRONMENT_ONLY", text)
        self.assertNotIn("PROJECTION_ONLY", text)
        self.assertEqual(sum(item["kind"] == "user_statement" for item in result["evidence"]), 1)

    def test_exact_prompt_bytes_include_unicode_newlines_and_whitespace(self):
        prompt = "café ☃\r\nLeave this trailing space: "
        self.write(opening(prompt) + [assistant(), event("task_complete")])
        pin = self.admission(prompt=prompt)
        self.assertEqual(self.observe(pin)["status"], "complete")
        self.deferred(self.observe(self.admission(prompt=prompt.rstrip())))

    def test_wrong_session_or_turn_cannot_admit_and_wrong_prompt_cannot_complete(self):
        self.write(complete_turn())
        for args in ({"session": "foreign-session"}, {"turn": "foreign-turn"}):
            with self.subTest(args=args):
                self.deferred(self.admit(**args))
        self.deferred(self.observe(self.admission(prompt="Different task")))

    def test_multislot_task_prompt_is_not_concatenated(self):
        prompt = user()
        prompt["payload"]["content"] = [{"type": "input_text", "text": PROMPT[:15]},
                                           {"type": "input_text", "text": PROMPT[15:]}]
        prompt["payload"][META]["content_item_kinds"] = ["user.text", "user.text"]
        self.write([header(), start(), context(), prompt, event("task_complete")])
        self.deferred(self.observe())

    def test_aligned_environment_slot_does_not_enter_prompt_digest(self):
        prompt = user()
        prompt["payload"]["content"].insert(0, {"type": "input_text", "text": "ENV_ONLY"})
        prompt["payload"][META]["content_item_kinds"].insert(0, "environments.environment_context")
        self.write([header(), start(), context(), prompt, assistant(), event("task_complete")])
        result = self.observe()
        self.assertEqual(result["status"], "complete", result)
        self.assertNotIn("ENV_ONLY", json.dumps(result["evidence"]))

    def test_prompt_marker_and_metadata_must_align(self):
        for mutate in (
            lambda p: p.pop(META),
            lambda p: p[META].update(turn_id="wrong-turn"),
            lambda p: p[META].update(content_item_kinds=["environments.environment_context"]),
            lambda p: p[META].update(content_item_kinds=[]),
        ):
            with self.subTest(mutate=mutate):
                rows = complete_turn()
                mutate(rows[3]["payload"])
                self.write(rows)
                self.deferred(self.observe())

    def test_first_source_prompt_cannot_be_replaced_by_later_matching_text(self):
        self.write([header(), start(), context(), user("A different source task"),
                    user(PROMPT, identifier="later-matching-input"), event("task_complete")])
        self.deferred(self.observe())

    def test_admission_can_precede_prompt_flush_but_observation_cannot(self):
        self.write([header(), start(), context()])
        pin = self.admission()
        self.deferred(self.observe(pin))
        prompt_record = encoded([user()])
        with self.path.open("ab") as stream:
            stream.write(prompt_record[:-1])
        self.deferred(self.observe(pin))
        with self.path.open("ab") as stream:
            stream.write(b"\n" + encoded([assistant(), event("task_complete")]))
        self.assertEqual(self.observe(pin)["status"], "complete")

    def test_completion_requires_closed_flushed_record_not_eof_or_final_answer(self):
        self.write(opening() + [assistant()])
        pin = self.admission()
        self.deferred(self.observe(pin))
        closing = encoded([event("task_complete")])
        with self.path.open("ab") as stream:
            stream.write(closing[:-1])
        self.deferred(self.observe(pin))
        with self.path.open("ab") as stream:
            stream.write(b"\n")
        self.assertEqual(self.observe(pin)["status"], "complete")

    def test_large_preceding_history_uses_bounded_locator_and_absolute_refs(self):
        history = [row("event_msg", {"type": "token_count", "padding": "x" * 4096})
                   for _ in range(260)]
        rows = [header(), *history, *complete_turn()[1:]]
        self.write(rows)
        result = self.admit()
        self.assertEqual(result["status"], "admitted", result)
        limits = observer.Limits()
        self.assert_work(result, bound=limits.header_bytes + limits.tail_bytes + 2 * limits.line_bytes + 2)
        observed = self.observe(result["admission"])
        self.assertEqual(observed["status"], "complete", observed)
        self.assertGreater(observed["evidence"][0]["ref"]["byte_offset"], 1024 * 1024)
        self.assertLess(observed["work"]["bytes_read"], 70 * 1024)

    def test_tail_miss_does_not_scan_unbounded_history(self):
        self.write(opening() + [assistant("x" * 4096, identifier=f"later-{i}")
                                for i in range(8)])
        limits = observer.Limits(header_bytes=256, tail_bytes=512)
        result = self.admit(limits=limits)
        self.deferred(result)
        self.assert_work(result, bound=limits.header_bytes + limits.tail_bytes + 2 * limits.line_bytes + 2)

    def test_old_completed_turn_can_be_observed_before_newer_turn(self):
        self.write(complete_turn())
        pin = self.admission()
        suffix = [start("new-turn"), context("new-turn"),
                  user("NEW_TASK_NOT_EVIDENCE", identifier="new-prompt", turn="new-turn")]
        suffix += [row("event_msg", {"type": "token_count", "padding": "z" * 4096})
                   for _ in range(260)]
        with self.path.open("ab") as stream:
            stream.write(encoded(suffix))
        result = self.observe(pin)
        self.assertEqual(result["status"], "complete", result)
        self.assertNotIn("NEW_TASK_NOT_EVIDENCE", json.dumps(result))
        self.assertLess(result["work"]["bytes_read"], 70 * 1024)

    def test_turn_switch_before_close_is_not_completion(self):
        self.write(opening())
        pin = self.admission()
        for fence in (start("other-turn"), context("other-turn")):
            with self.subTest(fence=fence):
                self.write(opening() + [fence, event("task_complete")])
                self.deferred(self.observe(pin))

    def test_abort_and_compaction_do_not_reconstruct_missing_history(self):
        self.write(opening())
        pin = self.admission()
        for fence in (event("turn_aborted"), event("task_aborted"),
                      event("context_compacted"), row("compacted", {})):
            with self.subTest(fence=fence):
                self.write(opening() + [fence, assistant(), event("task_complete")])
                self.deferred(self.observe(pin))

    def test_duplicate_task_start_and_response_identity_are_deferred(self):
        self.write(opening())
        pin = self.admission()
        for extra in ([start()], [assistant(identifier="prompt")],
                      [call(), call()], [call(), output(), output()]):
            with self.subTest(extra=extra):
                self.write(opening() + extra + [event("task_complete")])
                self.deferred(self.observe(pin))

    def test_repeated_call_or_result_identity_is_not_hidden_by_distinct_item_id(self):
        self.write(opening())
        pin = self.admission()
        duplicate_call = call()
        duplicate_call["payload"]["id"] = "different-call-item"
        duplicate_result = output()
        duplicate_result["payload"]["id"] = "different-result-item"
        for extra in ([call(), duplicate_call, output()],
                      [call(), output(), duplicate_result]):
            with self.subTest(extra=extra):
                self.write(opening() + extra + [event("task_complete")])
                self.deferred(self.observe(pin))

    def test_missing_or_repeated_task_start_cannot_admit(self):
        for rows in ([header(), context(), user(), event("task_complete")],
                     [header(), start(), start(), context(), user(), event("task_complete")]):
            with self.subTest(rows=rows):
                self.write(rows)
                self.deferred(self.admit())

    def test_missing_or_wrongly_paired_results_are_deferred(self):
        self.write(opening())
        pin = self.admission()
        for actions in ([call()], [output()],
                        [call(), output(typ="function_call_output")]):
            with self.subTest(actions=actions):
                self.write(opening() + actions + [event("task_complete")])
                self.deferred(self.observe(pin))

    def test_function_call_pairs_remain_unexecuted_evidence(self):
        self.write(opening() + [call(typ="function_call", text='{"command":"false"}'),
                               output(typ="function_call_output", text={"exit_code": 1}),
                               event("task_complete")])
        result = self.observe()
        self.assertEqual(result["status"], "complete", result)
        self.assertEqual([item["kind"] for item in result["evidence"]],
                         ["user_statement", "tool_call", "tool_result"])

    def test_same_inode_pinned_header_or_start_drift_is_rejected(self):
        raw = encoded(complete_turn())
        for old, new in ((SESSION.encode(), b"s" * len(SESSION)),
                         (b'"task_started"', b'"task_altered"')):
            with self.subTest(old=old):
                self.path.write_bytes(raw)
                pin = self.admission()
                changed = raw.replace(old, new, 1)
                self.assertEqual(len(changed), len(raw))
                self.path.write_bytes(changed)
                self.assertEqual(self.path.stat().st_ino, pin.inode)
                self.deferred(self.observe(pin))

    def test_atomic_rotation_does_not_repin_identical_looking_file(self):
        self.write(complete_turn())
        pin = self.admission()
        replacement = self.sessions / "replacement.jsonl"
        replacement.write_bytes(self.path.read_bytes())
        replacement.replace(self.path)
        self.assertNotEqual(self.path.stat().st_ino, pin.inode)
        self.deferred(self.observe(pin))

    def test_truncation_and_source_prompt_rewrite_are_rejected(self):
        original = encoded(complete_turn())
        for changed in (encoded([header()]), original.replace(PROMPT.encode(), b"q" * len(PROMPT))):
            with self.subTest(size=len(changed)):
                self.path.write_bytes(original)
                pin = self.admission()
                self.path.write_bytes(changed)
                self.deferred(self.observe(pin))

    def test_explicit_path_must_be_regular_inside_sessions_root(self):
        self.write(complete_turn())
        outside = self.root / "outside.jsonl"
        outside.write_bytes(self.path.read_bytes())
        link = self.sessions / "linked.jsonl"
        link.symlink_to(self.path)
        for path in (outside, link, self.sessions, self.sessions / "missing.jsonl"):
            with self.subTest(path=path.name):
                self.deferred(self.admit(path=path))

    def test_malformed_unterminated_and_duplicate_key_records_do_not_complete(self):
        self.write(opening())
        pin = self.admission()
        for broken in (b'{bad}\n', b'{"type":"event_msg","type":"response_item","payload":{}}\n',
                       b'{"type":"event_msg","payload":', b'\xff\n'):
            with self.subTest(broken=broken):
                self.path.write_bytes(encoded(opening()) + broken)
                self.deferred(self.observe(pin))

    def test_no_private_analysis_reasoning_or_undisplayed_body_leaks(self):
        private = assistant("PRIVATE_ANALYSIS", identifier="private-analysis", phase="analysis")
        reasoning = row("response_item", {"type": "reasoning", "id": "reasoning",
                         "summary": "PRIVATE_REASONING", "encrypted_content": "PRIVATE_ENCRYPTED"})
        message = user()
        message["payload"]["body"] = "UNDISPLAYED_BODY"
        self.write([header(), start(), context(), message, private, reasoning,
                    assistant(), event("task_complete")])
        result = self.observe()
        self.assertEqual(result["status"], "complete", result)
        serialized = json.dumps(result)
        self.assertNotIn("PRIVATE_", serialized)
        self.assertNotIn("UNDISPLAYED_BODY", serialized)

    def test_closed_long_turn_selects_recent_suffix_without_clipping(self):
        rows = opening() + [assistant(f"old-{i}:" + "x" * 20000,
                                      identifier=f"comment-{i}", phase="commentary")
                            for i in range(8)]
        rows += [assistant("Latest final.", identifier="last-final"), event("task_complete")]
        self.write(rows)
        result = self.observe()
        self.assertEqual(result["status"], "complete", result)
        self.assertEqual(result["schema"], observer.OBSERVATION_SCHEMA)
        selection = result["coverage"]["public_evidence"]
        self.assertEqual(selection["mode"], "selected_suffix")
        self.assertEqual(selection["observed_records"], 10)
        self.assertGreater(selection["omitted_records"], 0)
        self.assertEqual(selection["selected_records"], len(result["evidence"]))
        self.assertEqual(result["evidence"][0]["content"][0]["text"], PROMPT)
        self.assertEqual(result["evidence"][-1]["content"][0]["text"], "Latest final.")
        self.assertNotIn("old-0:", json.dumps(result["evidence"]))
        self.assertLessEqual(result["work"]["evidence_bytes"], observer.Limits().evidence_bytes)
        original = self.path.read_bytes()
        for item in result["evidence"]:
            ref = item["ref"]
            raw = original[ref["byte_offset"]:ref["byte_offset"] + ref["line_bytes"]]
            self.assertEqual(ref["raw_line_sha256"], sha(raw))

    def test_item_count_limit_selects_suffix_after_complete_scan(self):
        rows = opening() + [assistant(f"optional-{index}", identifier=f"comment-{index}",
                                      phase="commentary") for index in range(100)]
        rows += [assistant("Last final.", identifier="last"), event("task_complete")]
        self.write(rows)
        result = self.observe()
        self.assertEqual(result["status"], "complete", result)
        self.assertEqual(result["coverage"]["public_evidence"]["observed_records"], 102)
        self.assertLessEqual(len(result["evidence"]), observer.Limits().items)
        self.assertNotIn("optional-0", json.dumps(result["evidence"]))
        self.assertEqual(result["evidence"][-1]["content"][0]["text"], "Last final.")

    def test_interleaved_component_omits_all_intervening_records_but_keeps_mandatory(self):
        rows = opening() + [assistant("EARLIER_PASS", identifier="old", phase="commentary"),
                            call("a"), assistant("INTERVENING", identifier="middle", phase="commentary"),
                            call("b"), output("a"), user("LATE_CORRECTION", identifier="correction"),
                            output("b"), assistant("Final caveat.", identifier="final"),
                            event("task_complete")]
        self.write(rows)
        full = self.observe()
        self.assertEqual(full["status"], "complete", full)
        mandatory = [item for item in full["evidence"]
                     if item["kind"] == "user_statement"
                     or (item["kind"] == "assistant_assertion" and item["phase"] == "final_answer")]
        bound = len(observer.encoded(mandatory)) + 8
        selected = self.observe(limits=observer.Limits(evidence_bytes=bound))
        self.assertEqual(selected["status"], "complete", selected)
        self.assertEqual([item["kind"] for item in selected["evidence"]],
                         ["user_statement", "user_statement", "assistant_assertion"])
        self.assertNotIn("EARLIER_PASS", json.dumps(selected["evidence"]))
        self.assertNotIn("INTERVENING", json.dumps(selected["evidence"]))
        self.assertEqual(selected["coverage"]["public_evidence"]["omitted_records"], 6)

    def test_oversized_later_block_blocks_older_success_but_allows_later_pair(self):
        rows = opening() + [assistant("EARLIER_PASS", identifier="old", phase="commentary"),
                            call("large"), output("large", text="FAILURE_EDIT:" + "x" * 33000),
                            call("recent"), output("recent", text="later observation"),
                            assistant("Final is an assertion.", identifier="final"),
                            event("task_complete")]
        self.write(rows)
        result = self.observe()
        self.assertEqual(result["status"], "complete", result)
        serialized = json.dumps(result["evidence"])
        self.assertNotIn("EARLIER_PASS", serialized)
        self.assertNotIn("FAILURE_EDIT", serialized)
        self.assertEqual([item["call_id"] for item in result["evidence"]
                          if item["kind"] in ("tool_call", "tool_result")], ["recent", "recent"])
        self.assertEqual(result["coverage"]["public_evidence"]["omitted_records"], 3)

    def test_mandatory_overflow_defers_after_verified_closure(self):
        self.write(opening() + [user("late:" + "y" * 4000, identifier="late"),
                                assistant("Final.", identifier="final"), event("task_complete")])
        result = self.observe(limits=observer.Limits(evidence_bytes=3000))
        self.deferred(result)
        self.assertEqual(result["reason"], "mandatory_evidence_limit_exceeded")
        self.assertIsNotNone(result["boundary"])
        self.assertEqual(result["coverage"]["missing_results"], 0)

    def test_dangling_call_still_defers_when_optional_block_exceeds_selection_cap(self):
        self.write(opening() + [call("unfinished", text="x" * 33000),
                                assistant("Final.", identifier="final"), event("task_complete")])
        result = self.observe()
        self.deferred(result)
        self.assertEqual(result["reason"], "action_result_missing")

    def test_oversized_provisional_final_can_be_replaced_by_small_last_final(self):
        self.write(opening() + [assistant("x" * 33000, identifier="provisional"),
                                assistant("corrected final", identifier="last"),
                                event("task_complete")])
        result = self.observe()
        self.assertEqual(result["status"], "complete", result)
        self.assertEqual([item["content"][0]["text"] for item in result["evidence"]],
                         [PROMPT, "corrected final"])

    def test_observation_resource_caps_never_silently_drop_evidence(self):
        self.write(complete_turn())
        pin = self.admission()
        for limits in (observer.Limits(scan_bytes=100), observer.Limits(line_bytes=80),
                       observer.Limits(events=2), observer.Limits(items=1),
                       observer.Limits(item_bytes=20), observer.Limits(evidence_bytes=20)):
            with self.subTest(limits=limits):
                self.deferred(self.observe(pin, limits=limits))

    def test_prompt_cap_defers_observation_and_invalid_limits_do_not_admit(self):
        self.write(complete_turn())
        self.deferred(self.observe(limits=observer.Limits(prompt_bytes=2)))
        for limits in (observer.Limits(events=0),
                       observer.Limits(scan_bytes=observer.Limits().scan_bytes + 1),
                       observer.Limits(items=True)):
            with self.subTest(limits=limits):
                self.deferred(self.admit(limits=limits))

    def test_admission_rejects_malformed_expected_prompt_digest(self):
        self.write(complete_turn())
        for invalid in ("", "x" * 64, "0" * 63, None, True):
            with self.subTest(invalid=invalid):
                result = observer.admit_source_turn(
                    self.path, sessions_root=self.sessions, session_id=SESSION,
                    turn_id=TURN, expected_prompt_sha256=invalid)
                self.deferred(result)
                self.assertEqual(result["work"]["bytes_read"], 0)

    def test_oversized_unterminated_source_record_is_bounded(self):
        self.write(opening())
        pin = self.admission()
        with self.path.open("ab") as stream:
            stream.write(b"x" * 32768)
        limits = observer.Limits(scan_bytes=8192, line_bytes=1024)
        result = self.observe(pin, limits=limits)
        self.deferred(result)
        self.assert_work(result, bound=limits.scan_bytes + limits.header_bytes + limits.line_bytes + 2)

    def anchored_append(self, records):
        self.write(opening())
        pin = self.admission()
        with self.path.open("ab") as stream:
            stream.write(records if isinstance(records, bytes) else encoded(records))
        return pin

    def test_large_optional_result_and_event_omit_component_keep_late_fact(self):
        large = "LARGE_OPTIONAL:" + "x" * (600 * 1024)
        pin = self.anchored_append([
            call("old"), output("old", text="OLD_SUCCESS"),
            call("large"), output("large", text=large),
            row("event_msg", {"type":"item_completed", "item":large}),
            call("late"), output("late", text="DECISIVE_LATE_FACT"),
            assistant("Latest final."), event("task_complete")])
        result = self.observe(pin)
        self.assertEqual(result["status"], "complete", result)
        text = json.dumps(result["evidence"])
        self.assertIn("DECISIVE_LATE_FACT", text)
        self.assertNotIn("LARGE_OPTIONAL", text)
        self.assertNotIn("OLD_SUCCESS", text)
        self.assertEqual(result["coverage"]["missing_results"], 0)
        self.assertEqual(result["coverage"]["public_evidence"]["omitted_records"], 4)
        ceiling = observer.observation_byte_ceiling(pin)
        self.assertEqual(result["work"]["byte_ceiling"], ceiling)
        self.assertLessEqual(result["work"]["bytes_read"], ceiling)
        self.assertLessEqual(3 * ceiling, 32 * 1024 * 1024)
        self.assertLessEqual(result["work"]["evidence_bytes"], observer.Limits().evidence_bytes)

    def test_large_mandatory_user_or_final_still_deferred_after_closure(self):
        for record in (user("x" * (600 * 1024), identifier="later-user"),
                       assistant("x" * (600 * 1024), identifier="large-final")):
            with self.subTest(kind=record["payload"]["role"]):
                pin = self.anchored_append([record, event("task_complete")])
                result = self.observe(pin)
                self.assertEqual(result["reason"], "mandatory_evidence_limit_exceeded")
                self.assertIsNotNone(result["boundary"])
                self.assertEqual(result["coverage"]["missing_results"], 0)

    def test_large_records_never_skip_control_or_strict_json_validation(self):
        padding = "x" * (600 * 1024)
        interrupted = event("task_aborted"); interrupted["payload"]["padding"] = padding
        switched = context("another-turn"); switched["payload"]["padding"] = padding
        duplicate = encoded([row("event_msg", {"type":"item_completed", "padding":padding})])
        duplicate = duplicate.replace(b'"type": "event_msg"', b'"type":"event_msg","type":"event_msg"', 1)
        malformed = b'{"type":"event_msg","payload":{"padding":"' + padding.encode() + b'",BAD}}\n'
        cases = [(encoded([interrupted]), "source_task_interrupted"),
                 (encoded([switched]), "turn_changed"),
                 (duplicate, "malformed_record"), (malformed, "malformed_record"),
                 (encoded([output("read", turn="wrong", text=padding)]), "action_turn_metadata_mismatch"),
                 (encoded([output("read", text=padding)])[:-1], "unflushed_record")]
        for records, reason in cases:
            with self.subTest(reason=reason):
                pin = self.anchored_append(encoded([call()]) + records)
                result = self.observe(pin)
                self.assertEqual(result["status"], "deferred", result)
                self.assertEqual(result["reason"], reason)
                self.assertLessEqual(result["work"]["bytes_read"], observer.observation_byte_ceiling(pin))

    def test_explicit_small_raw_record_limit_remains_strict(self):
        pin = self.anchored_append([call(), output(text="x" * (600 * 1024)),
                                    assistant(), event("task_complete")])
        result = self.observe(pin, limits=observer.Limits(line_bytes=512 * 1024))
        self.assertEqual(result["reason"], "line_bytes_exceeded")
        self.assertLessEqual(result["work"]["source_bytes_read"], 512 * 1024 + 1 + len(encoded([start(),context(),user(),call()])))

    def test_ceiling_strictly_validates_structural_anchors_before_io(self):
        self.write(complete_turn()); pin = self.admission()
        invalid = [None, replace(pin, session_record=None),
                   replace(pin, session_record=replace(pin.session_record, byte_offset=1)),
                   replace(pin, session_record=replace(pin.session_record, line_bytes=observer.Limits().header_bytes+1)),
                   replace(pin, start_record=replace(pin.start_record, line_bytes=True)),
                   replace(pin, start_record=replace(pin.start_record, byte_offset=-1)),
                   replace(pin, start_record=replace(pin.start_record, raw_line_sha256="invalid")),
                   replace(pin, start_record=replace(pin.start_record, line_bytes=observer.Limits().line_bytes+1))]
        for bad in invalid:
            with self.subTest(admission=bad), patch.object(observer, "_read") as read:
                with self.assertRaises(ValueError): observer.observation_byte_ceiling(bad)
                self.assertEqual(self.observe(bad if bad is not None else replace(pin,start_record=None))["status"], "deferred")
                read.assert_not_called()
        for limits in (None, observer.Limits(scan_bytes=0), observer.Limits(line_bytes=True)):
            with self.assertRaises(ValueError): observer.observation_byte_ceiling(pin, limits)
        original = observer.observation_byte_ceiling(pin)
        larger = replace(pin,start_record=replace(pin.start_record,line_bytes=pin.start_record.line_bytes+100))
        self.assertEqual(observer.observation_byte_ceiling(larger), original+200)
        # The shape ceiling does not grant source authority: changed anchors fail
        # their existing exact reread/hash checks.
        changed = replace(pin,start_record=replace(pin.start_record,raw_line_sha256="f"*64))
        self.assertEqual(self.observe(changed)["reason"], "anchor_changed")

    def test_admission_buffer_ceiling_does_not_scale_to_raw_source_record_limit(self):
        self.write(complete_turn())
        result = self.admit()
        self.assertEqual(result["status"], "admitted")
        limits = observer.Limits()
        self.assertLess(result["work"]["byte_ceiling"], 2 * 1024 * 1024)
        self.assertLessEqual(result["work"]["bytes_read"], result["work"]["byte_ceiling"])


if __name__ == "__main__":
    unittest.main()


def concern_row(a="1" * 26, b="2" * 26, *, kind="disagreement", finding=None):
    return {"notice": {"binding": {"key": {"lo": a, "hi": b, "kind": kind},
                    "endpoints": [{"id": a, "meaning": "a" * 64}, {"id": b, "meaning": "b" * 64}]},
            "concern": "The advice differs", "missing_fact": "Which scope applies?"}, "finding": finding}


class ConcernDeliveryTests(unittest.TestCase):
    def packet(self):
        packet = delivery_packet('Memory cards. Exact caveat 🐙 "escaped".')
        packet["schema"] = observer.DELIVERY_SCHEMA
        packet["displayed"].append({**packet["displayed"][0], "node_id": "2" * 26})
        packet["concerns"] = [{"shown_text": 'Exact caveat 🐙 "escaped".',
                               "displayed_endpoint_ids": ["2" * 26, "1" * 26],
                               "expected_row": concern_row()}]
        return refresh_delivery_packet(packet)

    def test_v3_exact_case_and_stale_packets_are_not_reauthorized(self):
        packet = self.packet()
        self.assertEqual(observer.validate_delivery_packet(packet), packet)
        for schema in (observer.LEGACY_DELIVERY_SCHEMA, "mneme.codex-memory-delivery.v2", "mneme.codex-memory-delivery.v3"):
            with self.subTest(schema=schema), self.assertRaises(ValueError):
                observer.validate_delivery_packet({**delivery_packet(), "schema": schema})
        packet["concerns"][0]["expected_row"] = None
        self.assertIsNone(observer.validate_delivery_packet(packet)["concerns"][0]["expected_row"])

    def test_caveat_substring_without_exact_pair_label_is_not_admitted(self):
        packet = self.packet()
        packet["rendered_text"] = packet["rendered_text"].replace(
            observer.render_delivery_concern(packet["concerns"][0]), packet["concerns"][0]["shown_text"])
        packet["rendered_sha256"] = sha(packet["rendered_text"].encode())
        with self.assertRaisesRegex(ValueError, "invalid_delivery_concerns"):
            observer.validate_delivery_packet(packet)

    def test_foreign_pair_missing_text_malformed_native_meanings_rejected(self):
        import copy
        for mutate in (lambda p: p["concerns"][0]["displayed_endpoint_ids"].__setitem__(0, "3" * 26),
                       lambda p: p["concerns"][0].__setitem__("shown_text", "Not emitted"),
                       lambda p: p["concerns"][0]["expected_row"]["notice"]["binding"]["endpoints"][0].__setitem__("meaning", "invented"),
                       lambda p: p["concerns"].append(copy.deepcopy(p["concerns"][0]))):
            p = self.packet()
            mutate(p)
            with self.assertRaises(ValueError):
                observer.validate_delivery_packet(p)


class EpisodeDisplayBindingTests(unittest.TestCase):
    def card(self):
        return {"id": "1" * 26, "kind": "episode", "summary": "Earlier repair attempt",
                "status": "active", "source": "codex:later-recap", "fingerprint": "a" * 64,
                "episode_id": "0" * 26, "edition_id": "1" * 26, "revision": 1,
                "current_edition_id": "2" * 26, "occurred": {"kind": "unknown"},
                "recorded_at": 15, "edition_recorded_at": 20, "thread": None,
                "recording_session": "opaque recorder/session",
                "occurrence_contexts": [{"namespace": "session", "key": "pi", "label": "Earlier work"}],
                "origins": [{"kind": "reference", "anchor": {"kind": "semantic", "node_id": "3" * 26},
                             "from": "3" * 26, "to": "1" * 26,
                             "edge_kind": "Associative", "body_anchor": None}]}

    def packet(self, card=None):
        import hooks
        packed = hooks._pack_async_delivery([self.card() if card is None else card], SESSION, TURN, "0" * 26)
        return {"schema": observer.DELIVERY_SCHEMA, "session_id": SESSION, "turn_id": TURN,
                "rendered_text": packed["context"], "rendered_sha256": sha(packed["context"].encode()),
                "displayed": packed["displayed"], "concerns": []}

    def test_exact_historical_view_and_stable_account_are_distinct(self):
        card = self.card()
        packet = self.packet(card)
        copied = observer.validate_delivery_packet(packet)
        shown = copied["displayed"][0]
        self.assertEqual(shown["displayed_view"], {k: v for k, v in card.items() if k != "fingerprint"})
        self.assertFalse(any(k in shown for k in ("routing_binding", "entry_kind", "conditional_binding")))
        for changes in ({"current_edition_id": "4" * 26}, {"origins": [{"kind": "lexical"}]},
                        {"source": "another display preview"}):
            with self.subTest(changes=changes):
                other = observer.validate_delivery_packet(self.packet({**card, **changes}))["displayed"][0]
                self.assertEqual(other["full_get_fingerprint"], shown["full_get_fingerprint"])
                self.assertNotEqual(other["displayed_view_sha256"], shown["displayed_view_sha256"])
        copied["displayed"][0]["displayed_view"]["origins"][0]["edge_kind"] = "Bridge"
        self.assertEqual(packet["displayed"][0]["displayed_view"]["origins"][0]["edge_kind"], "Associative")

    def test_tampered_facets_digest_and_rehashed_batch_are_refused(self):
        import copy
        packet = self.packet()
        for changes in ({"current_edition_id": "4" * 26}, {"recording_session": None},
                        {"origins": [{"kind": "lexical"}]}, {"thread": "different"},
                        {"source": "different preview"}, {"occurred": {"kind": "point", "at": 12}}):
            for repair_digest in (False, True):
                with self.subTest(changes=changes, repair_digest=repair_digest):
                    altered = copy.deepcopy(packet)
                    shown = altered["displayed"][0]
                    shown["displayed_view"].update(changes)
                    if repair_digest:
                        shown["displayed_view_sha256"] = sha(observer.encoded(shown["displayed_view"]))
                    with self.assertRaises(ValueError):
                        observer.validate_delivery_packet(altered)
        altered = copy.deepcopy(packet)
        altered["displayed"][0]["displayed_view_sha256"] = "b" * 64
        with self.assertRaises(ValueError):
            observer.validate_delivery_packet(altered)
        altered = copy.deepcopy(packet)
        altered["rendered_text"] = altered["rendered_text"].replace('"current_edition_id":"' + "2" * 26,
                                                                   '"current_edition_id":"' + "4" * 26)
        altered["rendered_sha256"] = sha(altered["rendered_text"].encode())
        with self.assertRaises(ValueError):
            observer.validate_delivery_packet(altered)
        # Python equality would incorrectly equate true/1 and 1.0/1. The exact
        # displayed wire view must bind numeric types as well as values.
        for replacement in ('"revision":true', '"revision":1.0'):
            altered = copy.deepcopy(packet)
            altered["rendered_text"] = altered["rendered_text"].replace('"revision":1', replacement)
            altered["rendered_sha256"] = sha(altered["rendered_text"].encode())
            with self.subTest(replacement=replacement), self.assertRaises(ValueError):
                observer.validate_delivery_packet(altered)
        altered = copy.deepcopy(packet)
        altered["displayed"][0]["entry_kind"] = "conditional"
        with self.assertRaises(ValueError):
            observer.validate_delivery_packet(altered)


class ConcernObservedEvidenceTests(unittest.TestCase, SourceTurnFixture):
    def setUp(self):
        SourceTurnFixture.__init__(self, self)

    def test_exact_v3_pair_and_caveat_survive_source_observation(self):
        packet = ConcernDeliveryTests().packet()
        self.write(opening() + [call(), delivered(packet), output(), assistant(), event("task_complete")])
        observed = self.observe(delivery=packet)
        self.assertEqual(observed["status"], "complete")
        marker = next(i for i in observed["evidence"] if i["kind"] == "memory_delivery")
        self.assertEqual(marker["packet"]["concerns"], packet["concerns"])
        self.assertEqual(marker["packet"]["rendered_text"], packet["rendered_text"])

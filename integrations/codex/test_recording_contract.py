"""Pure synthetic recording-contract checks; no transcripts, providers or stores."""
from __future__ import annotations

import copy
from dataclasses import FrozenInstanceError
import hashlib
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import recording_contract as contract
from test_turn_observer import refresh_delivery_packet


def encoded(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True,
                      separators=(",", ":"), allow_nan=False).encode("utf-8")


def reference(ordinal, *, item=True):
    result = {"ordinal": ordinal, "line": ordinal + 1, "byte_offset": ordinal * 256,
              "line_bytes": 128, "raw_line_sha256": hashlib.sha256(f"record-{ordinal}".encode()).hexdigest(),
              "ordinal_scope": "source_turn"}
    if item:
        result["item_id"] = f"host-only-item-{ordinal}"
    return result


def statement(text, *, kind="user_statement", ordinal=1, slots=None, phase="final_answer"):
    item = {"kind": kind, "ref": reference(ordinal),
            "content": [{"content_index": index, "text": value}
                        for index, value in enumerate(slots if slots is not None else [text])]}
    if kind == "assistant_assertion":
        item["phase"] = phase
    return item


def tool_call(*, ordinal=2, call_id="host-only-call-A", typ="custom_tool_call", name="read", text="inspect()"):
    return {"kind": "tool_call", "ref": reference(ordinal), "type": typ,
            "call_id": call_id, "name": name, "input": text}


def tool_result(*, ordinal=3, call_id="host-only-call-A", typ="custom_tool_call_output", output="setting=5"):
    return {"kind": "tool_result", "ref": reference(ordinal), "type": typ,
            "call_id": call_id, "output": output}


def memory_marker(ordinal=2):
    from test_turn_observer import delivery_packet
    return {"kind": "memory_delivery", "ref": reference(ordinal), "content_index": 0,
            "packet": delivery_packet()}


def observation(items=None):
    items = items if items is not None else [statement("no, R5"),
             statement("R5, corrected.", kind="assistant_assertion", ordinal=2)]
    boundary_ordinal = max((item["ref"]["ordinal"] for item in items), default=0) + 1
    return {"schema": "mneme.codex-turn-observation.v3", "status": "complete",
            "reason": "source_turn_complete", "evidence": items,
            "coverage": {"source_turn": "closed_verified", "prompt": "verified",
                         "earlier_session_context": "omitted", "other_host_inputs": "omitted",
                         "private_reasoning": "excluded", "sensitivity_review": "not_performed",
                         "missing_results": 0, "omissions": {},
                         "memory_delivery": "admitted_retained" if any(i.get("kind") == "memory_delivery" for i in items) else "not_recorded",
                         "public_evidence": {"mode": "all", "observed_records": len(items),
                                             "selected_records": len(items), "omitted_records": 0}},
            "boundary": reference(boundary_ordinal, item=False),
            "work": {"bytes_read": 1000, "records_parsed": boundary_ordinal + 1,
                     "source_bytes_read": 1000, "evidence_bytes": len(encoded(items)),
                     "byte_ceiling": 10485760},
            "limits": {"header_bytes": 524288, "tail_bytes": 262144, "scan_bytes": 8388608,
                       "line_bytes": 524288, "events": 4096, "items": 96, "item_bytes": 32768,
                       "evidence_bytes": 131072, "prompt_bytes": 8192}}


def proposal(context, *, index=0, kind="episode"):
    binding = context.bindings[index]
    value = {"kind": kind, "summary": "A bounded observation.",
             "body": "The user corrected the value to R5.",
             "evidence_ids": [binding.evidence_id]}
    if kind == "lesson":
        value["associate_with"] = None
    return {"proposal": value}


def validate_proposal(answer, context):
    """Existing proposal-only assertions exercise the named historical branch."""
    result = contract.validate_answer(answer, context)
    assert result["maintenance"] == []
    return result["proposal"]


class TopicTagContractTests(unittest.TestCase):
    def prepared(self, *, tags_context=None, **kwargs):
        from test_tag_context import context, DB
        return contract.prepare(observation(), expected_db_id=DB,
                                tag_context=context(["people", "rust"]) if tags_context is None else tags_context,
                                **kwargs)

    def test_one_existing_proposal_schema_requires_ordinary_tags_and_freezes_policy(self):
        prompt, context = self.prepared()
        packet = json.loads(prompt.split("PAYLOAD:\n", 1)[1])
        self.assertIn("tag_context", packet)
        self.assertIn("future", contract.instructions_for(context))
        for branch in contract.schema_for(context)["properties"]["proposal"]["anyOf"][1:]:
            self.assertIn("tags", branch["required"])
        answer = proposal(context, kind="lesson")
        with self.assertRaisesRegex(ValueError, "invalid_proposal_shape"):
            validate_proposal(answer, context)
        answer["proposal"]["tags"] = ["rust", "new-supported-distinction"]
        selected = validate_proposal(answer, context)
        self.assertEqual(selected.tags, ("new-supported-distinction", "rust"))
        self.assertEqual(selected.tag_context_json, context.tag_context_json)
        self.assertEqual(json.loads(selected.tag_context_json)["vocabulary_partial"], False)

    def test_special_duplicate_controls_and_byte_bounds_are_rejected(self):
        from tag_context import PROTECTED_TAGS
        _, context = self.prepared()
        for tags in ([[tag] for tag in PROTECTED_TAGS]):
            answer = proposal(context)
            answer["proposal"]["tags"] = tags
            with self.subTest(tags=tags), self.assertRaisesRegex(ValueError, "invalid_topic_tags"):
                validate_proposal(answer, context)
        for tags in (["rust", "rust"], [" é"], ["x\x85y"], ["é" * 129], "rust"):
            answer = proposal(context)
            answer["proposal"]["tags"] = tags
            with self.subTest(tags=tags), self.assertRaisesRegex(ValueError, "invalid_topic_tags"):
                validate_proposal(answer, context)

    def test_unavailable_malformed_or_wrong_owner_context_disables_tags_not_recording(self):
        from test_tag_context import context, DB, OTHER
        from tag_context import disabled
        for candidate in (disabled(DB, "guide_unavailable"), object(),
                          __import__("dataclasses").replace(context(), db_id=OTHER)):
            prompt, prepared = self.prepared(tags_context=candidate)
            self.assertIsNotNone(prompt)
            self.assertIsNone(prepared.tag_context_json)
            self.assertNotIn("tag_context", json.loads(prompt.split("PAYLOAD:\n", 1)[1]))
            self.assertIsNotNone(validate_proposal(proposal(prepared), prepared))

    def test_optional_tag_context_does_not_evict_evidence_to_fit(self):
        plain_prompt, plain = contract.prepare(observation())
        # Existing selected source fits; extra optional classification context
        # does not. Retain the exact source and historical proposal branch.
        with patch.object(contract, "MAX_AUTHORED_BYTES", plain.authored_bytes):
            prompt, tagged = self.prepared()
        self.assertEqual(prompt, plain_prompt)
        self.assertEqual(tagged.bindings, plain.bindings)
        self.assertIsNone(tagged.tag_context_json)

    def test_global_preference_cannot_export_topic_tags_or_guide(self):
        _, context = self.prepared(global_preferences_enabled=True)
        answer = proposal(context, kind="lesson")
        answer["proposal"].update(destination="global_preference", tags=["rust"])
        with self.assertRaisesRegex(ValueError, "global_preference_operation"):
            validate_proposal(answer, context)

    def test_possibility_reserves_room_for_its_host_category(self):
        _, context = self.prepared()
        answer = proposal(context, kind="possibility")
        answer["proposal"]["tags"] = [f"topic-{i}" for i in range(32)]
        with self.assertRaisesRegex(ValueError, "invalid_topic_tags"):
            validate_proposal(answer, context)


class PossibilityContractTests(unittest.TestCase):
    def test_proposal_is_bounded_sourced_and_has_no_extra_authority(self):
        from dataclasses import replace
        _, context = contract.prepare(observation())
        answer = proposal(context, kind="possibility")
        answer["proposal"].update(summary="Open question: could R5 work?", body="Proposal, not a verified result.")
        checked = validate_proposal(answer, context)
        self.assertEqual(checked.kind, "possibility")
        self.assertEqual(checked.evidence[0].evidence_id, "e001")
        self.assertIsNone(checked.associate_with)
        self.assertIsNone(checked.routing_judgment)
        for field, value in (("associate_with", None), ("routing_judgment", None),
                             ("tags", ["pursuing"]), ("evidence_ids", ["unknown"]),
                             ("body", "x" * (contract.MAX_BODY_BYTES + 1))):
            invalid = copy.deepcopy(answer)
            invalid["proposal"][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                validate_proposal(invalid, context)
        with self.assertRaises(ValueError):
            validate_proposal(answer, replace(context, recording_scope="workshop"))
        global_answer = copy.deepcopy(answer)
        global_answer["proposal"]["destination"] = "global_preference"
        with self.assertRaises(ValueError):
            validate_proposal(global_answer, replace(context, global_preferences_enabled=True))
        global_answer["proposal"]["destination"] = "project"
        self.assertEqual(validate_proposal(global_answer, replace(context, global_preferences_enabled=True)).kind,
                         "possibility")
        self.assertIn("possibility", contract.OUTPUT_SCHEMA["properties"]["proposal"]["anyOf"][1]["properties"]["kind"]["enum"])


class RecordingPrepareTests(unittest.TestCase):
    def test_voice_guidance_fits_all_static_prompt_variants(self):
        from dataclasses import replace
        from itertools import product

        _, base = self.prepared()
        for routing, maintenance, workshop, preferences in product((False, True), repeat=4):
            with self.subTest(routing=routing, maintenance=maintenance,
                              workshop=workshop, preferences=preferences):
                context = replace(base, routing_enabled=routing,
                                  concern_bindings=(None,) if maintenance else (),
                                  recording_scope="workshop" if workshop else "project",
                                  global_preferences_enabled=preferences)
                instructions = contract.instructions_for(context)
                self.assertIn("source assistant's voice", instructions)
                self.assertIn("Attribute user/other-agent actions", instructions)
                self.assertIn("technical facts can stay impersonal", instructions)
                self.assertIn("Do not invent experience or feelings", instructions)
                static_bytes = (len(instructions.encode())
                                + len(encoded(contract.schema_for(context)))
                                + len(contract.PROMPT_PREFIX.encode()))
                limit = contract.MAX_STATIC_BYTES
                if maintenance:
                    limit += (len(contract.MAINTENANCE_INSTRUCTIONS.encode())
                              + len(encoded(contract._MAINTENANCE_SCHEMA)) + 64)
                if preferences:
                    limit += len(contract.GLOBAL_PREFERENCE_INSTRUCTIONS.encode()) + 512
                self.assertLessEqual(static_bytes, limit)

    def test_conditional_advice_keeps_origin_and_exact_binding_without_graph_claim(self):
        from test_routing_memory import binding, DB, TARGET
        marker = memory_marker()
        marker["packet"]["displayed"][0].update(db_id=DB, node_id=TARGET,
            entry_kind="conditional", conditional_binding=binding())
        refresh_delivery_packet(marker["packet"])
        source = observation([statement("Independent task constraint."), marker,
                              statement("Observed later result.", ordinal=3)])
        prompt, context = self.prepared(source)
        payload = json.loads(prompt[len(contract.PROMPT_PREFIX):])
        target, = context.association_bindings
        self.assertEqual(target.entry_kind, "conditional")
        self.assertEqual(json.loads(target.routing_binding_json), binding())
        self.assertEqual(payload["delivered_cards"][0]["entry_kind"], "conditional")
        self.assertTrue(context.routing_enabled)
        self.assertIn("NOT a walked route", contract.instructions_for(context))
        self.assertNotIn("graph_path", prompt)
        self.assertEqual(contract.schema_for(context), contract._schema_for(True, False))
        # Missing optional binding retains exact advice/origin, but cannot offer feedback.
        marker["packet"]["displayed"][0]["conditional_binding"] = None
        prompt, unbound = self.prepared(source)
        self.assertFalse(unbound.routing_enabled)
        self.assertIsNone(unbound.association_bindings[0].routing_binding_json)
        self.assertEqual(unbound.association_bindings[0].entry_kind, "conditional")
        self.assertIn('"entry_kind":"conditional"', prompt)
        # Final prompt pressure must not remove ordinary source evidence to retain optional feedback.
        marker["packet"]["displayed"][0]["conditional_binding"] = binding()
        with patch.object(contract, "MAX_AUTHORED_BYTES", unbound.authored_bytes):
            _, packed = self.prepared(source)
        self.assertEqual(packed.bindings, unbound.bindings)
        self.assertFalse(packed.routing_enabled)
        self.assertEqual(packed.association_bindings[0].entry_kind, "conditional")

    def prepared(self, source=None, cards=None):
        prompt, context = contract.prepare(observation() if source is None else source, cards)
        self.assertIsInstance(prompt, str, context)
        self.assertTrue(prompt)
        self.assertIsInstance(context, contract.ValidationContext)
        return prompt, context

    def refused(self, source, cards=None):
        prompt, reason = contract.prepare(source, cards)
        self.assertIsNone(prompt)
        self.assertIsInstance(reason, str)
        self.assertTrue(reason)

    def test_admitted_memory_is_distinct_from_prior_and_inflight_evidence(self):
        source = observation([statement("Investigate q7."), tool_call(ordinal=2), memory_marker(3),
                 tool_result(ordinal=4), tool_call(ordinal=5, call_id="later"),
                 tool_result(ordinal=6, call_id="later"),
                 statement("A scoped result.", kind="assistant_assertion", ordinal=7)])
        before = copy.deepcopy(source)
        prompt, context = self.prepared(source)
        self.assertEqual(contract.prepare(source, []), (prompt, context))
        self.assertEqual(source, before)
        packet = json.loads(prompt[len(contract.PROMPT_PREFIX):])
        timing = {e["id"]: e["relative_to_memory"] for e in packet["evidence"]}
        self.assertEqual(timing["t001_call"], "prior_or_inflight")
        self.assertEqual(timing["t001_result"], "prior_or_inflight")
        self.assertEqual(timing["t002_call"], "after_admission")
        self.assertEqual(timing["t002_result"], "after_admission")
        self.assertEqual(timing["e003"], "admitted_memory")
        self.assertEqual(packet["delivered_cards"][0]["id"], "shown001")
        self.assertEqual(packet["delivered_cards"][0]["evidence_id"], "e003")
        target, = context.association_bindings
        self.assertEqual((target.origin, target.db_id, target.native_id),
                         ("delivery", "0" * 26, "1" * 26))
        self.assertEqual(target.full_get_fingerprint, "a" * 64)

    def test_delivered_association_needs_memory_and_separate_source(self):
        source = observation([statement("No: q7 uses UTC."), memory_marker(),
                  statement("Noted.", kind="assistant_assertion", ordinal=3)])
        _, context = self.prepared(source)
        answer = proposal(context, kind="lesson")
        answer["proposal"].update(associate_with="shown001", evidence_ids=["e001", "e002"])
        checked = validate_proposal(answer, context)
        self.assertEqual(checked.associate_with.origin, "delivery")
        for ids in (["e001"], ["e002"]):
            answer["proposal"]["evidence_ids"] = ids
            with self.subTest(ids=ids), self.assertRaisesRegex(ValueError, "delivery_source_evidence_missing"):
                validate_proposal(answer, context)

    def test_same_node_overlap_cannot_downgrade_delivery_binding(self):
        source = observation([statement("No: q7 uses UTC."), memory_marker(),
                  statement("Noted.", kind="assistant_assertion", ordinal=3)])
        overlap = [{"id": "1" * 26, "kind": "semantic", "summary": "Fresh overlap alias."}]
        prompt, context = self.prepared(source, overlap)
        packet = json.loads(prompt[len(contract.PROMPT_PREFIX):])
        self.assertEqual(packet["overlap_cards"], [])
        self.assertEqual(packet["omitted_overlap_cards"], 1)
        self.assertEqual(context.dropped_overlap_cards, 1)
        target, = context.association_bindings
        self.assertEqual((target.opaque_id, target.origin), ("shown001", "delivery"))
        self.assertEqual(target.full_get_fingerprint, "a" * 64)
        answer = proposal(context, kind="lesson")
        answer["proposal"].update(associate_with="overlap001", evidence_ids=["e001", "e002"])
        with self.assertRaisesRegex(ValueError, "invalid_association_reference"):
            validate_proposal(answer, context)
        for kind in ("lesson", "episode"):
            answer = proposal(context, index=1, kind=kind)
            with self.subTest(kind=kind), self.assertRaisesRegex(ValueError, "delivery_source_evidence_missing"):
                validate_proposal(answer, context)

    def test_final_packing_reserves_marker_before_optional_tool_pair(self):
        items = [statement("Inspect q7."), memory_marker(), tool_call(ordinal=3),
                 tool_result(ordinal=4, output="x" * 5000),
                 statement("Reported checks.", kind="assistant_assertion", ordinal=5)]
        _, baseline = self.prepared(observation([items[0], *items[2:]]))
        with patch.object(contract, "MAX_AUTHORED_BYTES", baseline.authored_bytes + 100):
            prompt, context = self.prepared(observation(items))
        packet = json.loads(prompt[len(contract.PROMPT_PREFIX):])
        self.assertEqual(packet["delivered_cards"][0]["id"], "shown001")
        self.assertEqual(len(context.association_bindings), 1)
        self.assertEqual(packet["coverage"]["memory_delivery"], "admitted_retained")
        self.assertEqual([b.kind for b in context.bindings],
                         ["user_statement", "memory_delivery", "assistant_assertion"])
        self.assertEqual(context.tool_pairs, ())

    def test_mandatory_anchors_force_marker_and_targets_to_yield_under_prompt_pressure(self):
        user = statement("Mandatory current task constraint.")
        final = statement("Mandatory final report.", kind="assistant_assertion", ordinal=5)
        _, mandatory = self.prepared(observation([user, final]))
        marker = memory_marker()
        items = [user, marker, tool_call(ordinal=3), tool_result(ordinal=4, output="Independent check"), final]
        original = copy.deepcopy(items)
        with patch.object(contract, "MAX_AUTHORED_BYTES", mandatory.authored_bytes + 100):
            prompt, context = self.prepared(observation(items))
        payload = json.loads(prompt[len(contract.PROMPT_PREFIX):])
        self.assertEqual(items, original)
        self.assertEqual([b.kind for b in context.bindings], ["user_statement", "assistant_assertion"])
        self.assertEqual(context.association_bindings, ())
        self.assertNotIn("delivered_cards", payload)
        self.assertEqual(payload["coverage"]["memory_delivery"], "admitted_omitted")
        self.assertFalse(context.routing_enabled)
        self.assertLessEqual(context.authored_bytes, mandatory.authored_bytes + 100)

    def test_final_packing_restores_exact_delivery_without_its_old_tool_block(self):
        items = [statement("Inspect q7 under the current compatibility constraint."),
                 tool_call(ordinal=2), memory_marker(3),
                 tool_result(ordinal=4, output="OMITTED earlier result " + "x" * 12000),
                 tool_call(ordinal=5, call_id="later"),
                 tool_result(ordinal=6, call_id="later", output="Current check: R5 required."),
                 statement("Report only the retained current check.",
                           kind="assistant_assertion", ordinal=7)]
        retained = [items[0], items[2], *items[4:]]
        _, baseline = self.prepared(observation(retained))
        original = copy.deepcopy(items)
        with patch.object(contract, "MAX_AUTHORED_BYTES", baseline.authored_bytes + 100):
            prompt, context = self.prepared(observation(items))
        packet = json.loads(prompt[len(contract.PROMPT_PREFIX):])
        self.assertEqual(items, original)
        self.assertEqual([json.loads(b.source_ref_json)["ordinal"] for b in context.bindings],
                         [1, 3, 5, 6, 7])
        marker = next(b for b in context.bindings if b.kind == "memory_delivery")
        self.assertEqual(marker.source_ref_json, encoded(items[2]["ref"]).decode())
        self.assertEqual((marker.text, marker.source_field, marker.rendering),
                         (items[2]["packet"]["rendered_text"], "content[0].text", "exact_text"))
        self.assertNotIn("OMITTED earlier result", prompt)
        self.assertEqual(packet["coverage"]["public_evidence"], {
            "mode": "selected_suffix", "observed_records": 7,
            "selected_records": 5, "omitted_records": 2})
        self.assertEqual(packet["coverage"]["memory_delivery"], "admitted_retained")
        self.assertEqual([e["relative_to_memory"] for e in packet["evidence"]],
                         ["prior_or_inflight", "admitted_memory", "after_admission",
                          "after_admission", "after_admission"])
        target, = context.association_bindings
        self.assertEqual((target.origin, target.full_get_fingerprint), ("delivery", "a" * 64))
        self.assertLessEqual(context.authored_bytes, baseline.authored_bytes + 100)
        # Availability is not a usefulness judgment; abstention remains legal.
        self.assertIsNone(validate_proposal({"proposal": None}, context))
        only_memory = proposal(context, index=1, kind="lesson")
        only_memory["proposal"]["associate_with"] = "shown001"
        with self.assertRaisesRegex(ValueError, "delivery_source_evidence_missing"):
            validate_proposal(only_memory, context)

    def test_episode_delivery_and_unbound_references_cannot_be_targets(self):
        marker = memory_marker()
        from test_turn_observer import EpisodeDisplayBindingTests
        card = EpisodeDisplayBindingTests().card()
        marker["packet"]["displayed"][0].update(kind="episode", node_id=card["id"],
            displayed_view={key: value for key, value in card.items() if key != "fingerprint"})
        refresh_delivery_packet(marker["packet"])
        _, context = self.prepared(observation([statement("Inspect q7."), marker,
                           statement("Reported checks.", kind="assistant_assertion", ordinal=3)]))
        answer = proposal(context, kind="lesson")
        answer["proposal"].update(associate_with="shown001", evidence_ids=["e001", "e002"])
        with self.assertRaisesRegex(ValueError, "invalid_association_kind"):
            validate_proposal(answer, context)
        answer["proposal"]["associate_with"] = "shown002"
        with self.assertRaisesRegex(ValueError, "invalid_association_reference"):
            validate_proposal(answer, context)

    def test_marker_coverage_and_packet_shape_are_checked_before_selection(self):
        for change in ("coverage", "packet", "duplicate"):
            source = observation([statement("Inspect q7."), memory_marker(),
                                 statement("Done.", kind="assistant_assertion", ordinal=4)])
            if change == "coverage":
                source["coverage"]["memory_delivery"] = "not_admitted"
            elif change == "packet":
                source["evidence"][1]["packet"]["rendered_sha256"] = "b" * 64
            else:
                source = observation([statement("Inspect q7."), memory_marker(2), memory_marker(3),
                                      statement("Done.", kind="assistant_assertion", ordinal=4)])
            with self.subTest(change=change):
                self.refused(source)

    def test_no_tool_correction_is_eligible_with_no_overlap(self):
        for cards in (None, []):
            with self.subTest(cards=cards):
                prompt, context = self.prepared(cards=cards)
                self.assertEqual([binding.text for binding in context.bindings], ["no, R5", "R5, corrected."])
                self.assertEqual([binding.kind for binding in context.bindings], ["user_statement", "assistant_assertion"])
                self.assertEqual(context.dropped_overlap_cards, 0)
                self.assertIn("no, R5", prompt)

    def test_message_slots_remain_separate_and_ordered(self):
        source = observation([statement("first"), statement("", kind="assistant_assertion", ordinal=2,
                                                           slots=["alpha", "beta", "gamma"]),
                              statement("last correction", ordinal=3)])
        _, context = self.prepared(source)
        self.assertEqual([binding.text for binding in context.bindings],
                         ["first", "alpha", "beta", "gamma", "last correction"])
        self.assertEqual(len({binding.evidence_id for binding in context.bindings}), 5)
        for binding in context.bindings:
            self.assertRegex(binding.evidence_id, r"^e[0-9]{3}$")
        self.assertEqual(len({binding.source_field for binding in context.bindings[1:4]}), 3)

    def test_tool_typing_pairing_and_canonical_rendering(self):
        source = observation([statement("inspect"),
                              tool_call(text='{"b":2,"a":"café"}'),
                              tool_call(ordinal=3, call_id="host-only-call-B", typ="function_call", name="exec", text="echo done"),
                              tool_result(ordinal=4, call_id="host-only-call-B", typ="function_call_output", output="done\r\n "),
                              tool_result(ordinal=5, output={"z": [2, True], "a": "café"})])
        _, context = self.prepared(source)
        self.assertEqual([binding.kind for binding in context.bindings],
                         ["user_statement", "tool_call", "tool_call", "tool_result", "tool_result"])
        first_call, second_call, second_result, first_result = context.bindings[1:]
        self.assertRegex(first_call.evidence_id, r"^t[0-9]{3}_call$")
        self.assertEqual(first_result.evidence_id, first_call.evidence_id.replace("_call", "_result"))
        self.assertEqual(second_result.evidence_id, second_call.evidence_id.replace("_call", "_result"))
        self.assertIsInstance(context.tool_pairs, tuple)
        self.assertEqual(set(context.tool_pairs), {(first_call.evidence_id, first_result.evidence_id),
                                                  (second_call.evidence_id, second_result.evidence_id)})
        self.assertNotEqual(first_call.evidence_id, second_call.evidence_id)
        self.assertEqual(first_call.text, encoded({"name": "read", "input": '{"b":2,"a":"café"}'}).decode())
        self.assertEqual(second_result.text, "done\r\n ")
        self.assertEqual(first_result.text, encoded({"z": [2, True], "a": "café"}).decode())
        for binding in context.bindings:
            self.assertIsInstance(binding.rendering, str)
            self.assertTrue(binding.rendering)

    def test_host_source_refs_and_call_ids_do_not_enter_model_input(self):
        source = observation([statement("inspect"), tool_call(), tool_result()])
        prompt, context = self.prepared(source)
        for item in source["evidence"]:
            self.assertNotIn(item["ref"]["item_id"], prompt)
            self.assertNotIn(item["ref"]["raw_line_sha256"], prompt)
        self.assertNotIn("host-only-call-A", prompt)
        self.assertNotIn("source_ref_json", prompt)
        self.assertNotIn("byte_offset", prompt)
        self.assertEqual([json.loads(binding.source_ref_json) for binding in context.bindings],
                         [item["ref"] for item in source["evidence"]])

    def test_model_evidence_records_are_only_id_kind_text(self):
        prompt, context = self.prepared()
        self.assertTrue(prompt.startswith(contract.PROMPT_PREFIX))
        payload = json.loads(prompt[len(contract.PROMPT_PREFIX):])
        self.assertEqual(payload["evidence"], [
            {"id": binding.evidence_id, "kind": binding.kind, "text": binding.text}
            for binding in context.bindings])

    def test_full_authored_budget_counts_instructions_and_schema(self):
        prompt, context = self.prepared()
        total = len(contract.BASE_INSTRUCTIONS.encode()) + len(prompt.encode()) + len(encoded(contract.OUTPUT_SCHEMA))
        self.assertEqual(context.authored_bytes, total)
        self.assertLessEqual(total, contract.MAX_AUTHORED_BYTES)
        self.assertEqual(context.prompt_sha256, hashlib.sha256(prompt.encode()).hexdigest())

    def test_prepare_is_deterministic_frozen_and_does_not_mutate_inputs(self):
        source = observation()
        cards = [{"id": "ordinary-a", "summary": "Prior R4 note.", "kind": "semantic"}]
        before = copy.deepcopy((source, cards))
        prompt, context = self.prepared(source, cards)
        again_prompt, again_context = self.prepared(source, cards)
        self.assertEqual((source, cards), before)
        self.assertEqual((prompt, context), (again_prompt, again_context))
        self.assertIsInstance(context.bindings, tuple)
        with self.assertRaises(FrozenInstanceError):
            context.authored_bytes = 0
        with self.assertRaises(FrozenInstanceError):
            context.bindings[0].text = "changed"
        source["evidence"][0]["content"][0]["text"] = "mutated after prepare"
        source["evidence"][0]["ref"]["item_id"] = "mutated"
        self.assertEqual(context.bindings[0].text, "no, R5")
        self.assertNotIn("mutated", context.bindings[0].source_ref_json)
        source["coverage"]["source_turn"] = "incomplete"
        source["boundary"]["ordinal"] = 999
        self.assertEqual(json.loads(context.coverage_json)["source_turn"], "closed_verified")
        self.assertNotEqual(json.loads(context.boundary_ref_json)["ordinal"], 999)

    def test_partial_redacted_and_unknown_coverage_never_pass_as_complete(self):
        variants = []
        for field, value in (("schema", "unknown.v2"), ("status", "deferred"), ("boundary", None)):
            item = observation()
            item[field] = value
            variants.append(item)
        for field, value in (("source_turn", "incomplete"), ("prompt", "pending"), ("missing_results", 1),
                             ("private_reasoning", "included"), ("redacted", True)):
            item = observation()
            item["coverage"][field] = value
            variants.append(item)
        for name in ("redacted", "privacy_redaction", "partial", "unknown_kind", "tool_result"):
            item = observation()
            item["coverage"]["omissions"][name] = 1
            variants.append(item)
        item = observation()
        item["privacy"] = {"status": "redacted"}
        variants.append(item)
        for item in variants:
            with self.subTest(source=item):
                self.refused(item)

    def test_allowed_omissions_have_fixed_integer_count_semantics(self):
        for name in ("other_host_message", "non_user_text_input"):
            source = observation()
            source["coverage"]["omissions"][name] = 1
            self.prepared(source)
            for value in (True, -1, "1", 1.0, None, {"count": 1}):
                source["coverage"]["omissions"][name] = value
                with self.subTest(name=name, count=value):
                    self.refused(source)

    def test_unknown_evidence_never_falls_back_to_serialization(self):
        for kind in ("reasoning", "unknown", "assistant_verified_outcome", "tool_output"):
            source = observation()
            source["evidence"][1]["kind"] = kind
            self.refused(source)
        source = observation()
        source["evidence"][1]["content"] = [{"content_index": 0, "image": "not public text"}]
        self.refused(source)

    def test_refs_are_valid_ordered_and_unique(self):
        for field, value in (("ordinal", True), ("ordinal", -1), ("byte_offset", -1),
                             ("line_bytes", 0), ("raw_line_sha256", "not-a-digest"),
                             ("ordinal_scope", "session")):
            source = observation()
            source["evidence"][0]["ref"][field] = value
            with self.subTest(field=field, value=value):
                self.refused(source)
        source = observation()
        source["evidence"][1]["ref"] = copy.deepcopy(source["evidence"][0]["ref"])
        self.refused(source)
        source = observation()
        source["evidence"].reverse()
        self.refused(source)

    def test_malformed_or_incomplete_tool_pairs_are_refused(self):
        base = [statement("inspect"), tool_call(), tool_result()]
        variants = [base[:2], [base[0], base[2], base[1]],
                    base + [tool_result(ordinal=4)],
                    [base[0], base[1], tool_call(ordinal=3), tool_result(ordinal=4)],
                    [base[0], base[1], tool_result(typ="function_call_output")],
                    [base[0], tool_call(typ="unknown_call"), base[2]],
                    [base[0], tool_call(text={"cmd": "not a string"}), base[2]]]
        for items in variants:
            with self.subTest(items=items):
                self.refused(observation(copy.deepcopy(items)))
        for output in (float("nan"), object(), b"not JSON"):
            source = observation(copy.deepcopy(base))
            source["evidence"][-1]["output"] = output
            self.refused(source)

    def test_source_cap_refuses_but_flattened_cap_drops_oldest_whole_record(self):
        items = [statement("source prompt")]
        items += [statement(f"assertion-{index}", kind="assistant_assertion", ordinal=index)
                  for index in range(2, 97)]
        self.assertEqual(len(items), 96)
        _, context = self.prepared(observation(items))
        self.assertEqual(len(context.bindings), 96)
        items.append(statement("one over", kind="assistant_assertion", ordinal=97))
        self.refused(observation(items))
        # Five source records can exceed the separate flattened-evidence cap.
        items = [statement("source prompt")]
        items += [statement("", kind="assistant_assertion", ordinal=index,
                            slots=[f"slot-{index}-{slot}" for slot in range(32)])
                  for index in range(2, 6)]
        _, selected = self.prepared(observation(items))
        self.assertEqual(len(selected.bindings), 97)
        self.assertEqual(json.loads(selected.coverage_json)["public_evidence"]["omitted_records"], 1)
        items[-1]["content"].pop()
        _, context = self.prepared(observation(items))
        self.assertEqual(len(context.bindings), 128)

    def at_authored_budget(self, below=0):
        source = observation([statement("source prompt"),
                              statement("x" * 30000, kind="assistant_assertion", ordinal=2),
                              statement("x", kind="assistant_assertion", ordinal=3)])
        _, initial = self.prepared(source)
        extra = contract.MAX_AUTHORED_BYTES - initial.authored_bytes - below
        self.assertGreater(extra, 0)
        source["evidence"][-1]["content"][0]["text"] += "x" * extra
        return source

    def test_exact_authored_byte_cap_then_oldest_optional_block_drops_whole(self):
        source = self.at_authored_budget()
        _, context = self.prepared(source)
        self.assertEqual(context.authored_bytes, contract.MAX_AUTHORED_BYTES)
        self.assertEqual([binding.text for binding in context.bindings],
                         [item["content"][0]["text"] for item in source["evidence"]])
        source["evidence"][-1]["content"][0]["text"] += "x"
        prompt, trimmed = self.prepared(source)
        self.assertLessEqual(trimmed.authored_bytes, contract.MAX_AUTHORED_BYTES)
        self.assertEqual([b.text for b in trimmed.bindings],
                         [source["evidence"][0]["content"][0]["text"],
                          source["evidence"][-1]["content"][0]["text"]])
        self.assertEqual(json.loads(trimmed.coverage_json)["public_evidence"]["omitted_records"], 1)
        self.assertEqual(len(json.loads(prompt[len(contract.PROMPT_PREFIX):])["evidence"]), 2)

    def test_overlap_drops_before_any_source_evidence(self):
        source = self.at_authored_budget(below=1)
        cards = [{"id": f"ordinary-{i}", "summary": "s" * 512,
                  "kind": "semantic"} for i in range(4)]
        before = copy.deepcopy((source, cards))
        prompt, context = self.prepared(source, cards)
        self.assertEqual(context.dropped_overlap_cards, 4)
        self.assertLessEqual(context.authored_bytes, contract.MAX_AUTHORED_BYTES)
        self.assertEqual([binding.text for binding in context.bindings],
                         [item["content"][0]["text"] for item in source["evidence"]])
        self.assertNotIn("ordinary-0", prompt)
        self.assertEqual((source, cards), before)
        source["evidence"][-1]["content"][0]["text"] += "xx"
        _, trimmed = self.prepared(source, cards)
        self.assertEqual(json.loads(trimmed.coverage_json)["public_evidence"]["omitted_records"], 1)

    def test_hint_count_never_changes_evidence_only_selection_at_exact_ceiling(self):
        source=self.at_authored_budget()
        _, baseline=self.prepared(source)
        for count in (1, 9, 10, 99, 100, 256):
            with self.subTest(count=count):
                cards=[{"id":f"ordinary-{i}", "summary":"s"*512, "kind":"semantic"}
                       for i in range(count)]
                prompt, context=self.prepared(source,cards)
                self.assertEqual(context.bindings,baseline.bindings)
                self.assertEqual(context.routing_enabled,baseline.routing_enabled)
                self.assertEqual(context.dropped_overlap_cards,count)
                self.assertLessEqual(context.authored_bytes,contract.MAX_AUTHORED_BYTES)
                self.assertEqual(json.loads(prompt[len(contract.PROMPT_PREFIX):])["overlap_cards"],[])

    def test_hint_bytes_include_growing_omission_metadata_or_shed_it(self):
        from types import SimpleNamespace
        source=observation([statement("Mandatory evidence.")])
        _, baseline=self.prepared(source)
        cards=[{"id":f"ordinary-{i}", "summary":"x", "kind":"semantic"} for i in range(11)]
        _, one=self.prepared(source,cards[:1])
        allowance=one.authored_bytes-baseline.authored_bytes
        prompt, context=contract.prepare(source,cards,budget=SimpleNamespace(recording_hint_bytes=allowance))
        self.assertIsNotNone(prompt,context)
        self.assertEqual(context.bindings,baseline.bindings)
        self.assertLessEqual(context.authored_bytes-baseline.authored_bytes,allowance)
        self.assertEqual(context.dropped_overlap_cards,10)
        packet=json.loads(prompt[len(contract.PROMPT_PREFIX):])
        self.assertEqual(len(packet["overlap_cards"]),1)
        self.assertNotIn("omitted_overlap_cards",packet)

    def test_hint_baseline_stays_zero_count_through_optional_routing_fallback(self):
        from types import SimpleNamespace
        from test_routing_memory import binding, DB, TARGET
        marker=memory_marker()
        marker["packet"]["displayed"][0].update(routing_binding=binding(),db_id=DB,node_id=TARGET)
        refresh_delivery_packet(marker["packet"])
        source=observation([statement("Mandatory evidence."),marker])
        _, routed=self.prepared(source)
        self.assertTrue(routed.routing_enabled)
        cards=[{"id":f"ordinary-{i}", "summary":"x", "kind":"semantic"} for i in range(11)]
        with patch.object(contract,"MAX_AUTHORED_BYTES",routed.authored_bytes-1):
            _, baseline=self.prepared(source)
            self.assertFalse(baseline.routing_enabled)
            _, one=self.prepared(source,cards[:1])
            allowance=one.authored_bytes-baseline.authored_bytes
            prompt, context=contract.prepare(source,cards,budget=SimpleNamespace(recording_hint_bytes=allowance))
            self.assertIsNotNone(prompt,context)
            self.assertEqual(context.bindings,baseline.bindings)
            self.assertFalse(context.routing_enabled)
            self.assertLessEqual(context.authored_bytes-baseline.authored_bytes,allowance)
            self.assertEqual(context.dropped_overlap_cards,10)
            self.assertEqual(next(target for target in context.association_bindings
                                  if target.origin=="delivery").full_get_fingerprint,"a"*64)

    def test_unicode_authored_budget_uses_utf8_not_character_count(self):
        source = self.at_authored_budget(below=3)
        source["evidence"][-1]["content"][0]["text"] += "☃"
        _, context = self.prepared(source)
        self.assertEqual(context.authored_bytes, contract.MAX_AUTHORED_BYTES)
        source["evidence"][-1]["content"][0]["text"] += "é"
        _, trimmed = self.prepared(source)
        self.assertLessEqual(trimmed.authored_bytes, contract.MAX_AUTHORED_BYTES)
        self.assertEqual(json.loads(trimmed.coverage_json)["public_evidence"]["omitted_records"], 1)

    def test_whole_pair_suffix_reassigns_ids_after_exact_packing(self):
        items = [statement("source prompt")]
        for index, name in enumerate(("old", "edit", "latest")):
            ordinal = 2 + 2 * index
            items.extend((tool_call(ordinal=ordinal, call_id=name, text=name),
                          tool_result(ordinal=ordinal + 1, call_id=name,
                                      output=name + ":" + "x" * 25000)))
        items.append(statement("Final is not tool proof.", kind="assistant_assertion", ordinal=8))
        source = observation(items)
        prompt, context = self.prepared(source)
        again = contract.prepare(source)
        self.assertEqual((prompt, context), again)
        packet = json.loads(prompt[len(contract.PROMPT_PREFIX):])
        self.assertEqual(packet["coverage"]["public_evidence"], {
            "mode": "selected_suffix", "observed_records": 8,
            "selected_records": 6, "omitted_records": 2})
        self.assertNotIn("old:", json.dumps(packet))
        self.assertEqual(context.tool_pairs, (("t001_call", "t001_result"),
                                              ("t002_call", "t002_result")))
        by_id = {binding.evidence_id: binding for binding in context.bindings}
        self.assertEqual(json.loads(by_id["t001_result"].source_ref_json)["ordinal"], 5)
        self.assertEqual(json.loads(by_id["t002_result"].source_ref_json)["ordinal"], 7)
        answer = {"proposal": {"kind": "lesson", "summary": "Edited result.", "body": "Later result remains.",
                               "evidence_ids": ["t002_result"], "associate_with": None}}
        cited = validate_proposal(answer, context).evidence[0]
        self.assertEqual(cited.source_ref_json, by_id["t002_result"].source_ref_json)

    def test_oversized_latest_interleaved_block_cannot_skip_back_to_old_pair(self):
        items = [statement("source prompt"), tool_call(call_id="old", ordinal=2),
                 tool_result(call_id="old", ordinal=3, output="EARLIER_PASS"),
                 tool_call(call_id="a", ordinal=4), tool_call(call_id="b", ordinal=5),
                 statement("intervening", kind="assistant_assertion", ordinal=6, phase="commentary"),
                 tool_result(call_id="a", ordinal=7, output="FAILURE_EDIT:" + "x" * 31000),
                 tool_result(call_id="b", ordinal=8, output="FAILURE_EDIT:" + "y" * 31000),
                 statement("Latest final.", kind="assistant_assertion", ordinal=9)]
        prompt, context = self.prepared(observation(items))
        packet = json.loads(prompt[len(contract.PROMPT_PREFIX):])
        self.assertEqual([entry["kind"] for entry in packet["evidence"]],
                         ["user_statement", "assistant_assertion"])
        self.assertEqual(context.tool_pairs, ())
        self.assertNotIn("EARLIER_PASS", prompt)
        self.assertEqual(packet["coverage"]["public_evidence"]["omitted_records"], 7)

    def test_mandatory_input_exceeding_authored_cap_refuses_without_truncation(self):
        source = observation([statement("x" * 31000),
                              statement("y" * 31000, kind="assistant_assertion", ordinal=2)])
        prompt, reason = contract.prepare(source)
        self.assertIsNone(prompt)
        self.assertEqual(reason, "authored_input_limit")

    def test_selection_metadata_must_match_retained_record_count(self):
        for field, value in (("mode", "selected_suffix"), ("observed_records", 1),
                             ("selected_records", 1), ("omitted_records", 1),
                             ("omitted_records", True)):
            source = observation()
            source["coverage"]["public_evidence"][field] = value
            with self.subTest(field=field, value=value):
                self.refused(source)

    def test_overlap_shape_count_and_utf8_summary_cap(self):
        self.prepared(cards=[{"id": "ordinary", "summary": "é" * 256, "kind": "semantic"}])
        variants = [[{"id": "ordinary", "summary": "é" * 257}],
                    [{"id": str(i), "summary": "note"} for i in range(5)],
                    [{"id": "same", "summary": "one"}, {"id": "same", "summary": "two"}],
                    [{"id": "ordinary", "summary": " "}], [{"id": "", "summary": "note"}],
                    {"id": "ordinary", "summary": "note"}]
        for cards in variants:
            with self.subTest(cards=cards):
                self.refused(observation(), cards)

    def test_overlap_projects_only_opaque_id_and_summary_not_native_metadata(self):
        cards = [{"id": "host-native-node-id", "summary": "Prior note.", "kind": "semantic",
                  "authority": "WRITE_SENTINEL", "source": "SECRET_SOURCE",
                  "native": {"db_path": "/private/SECRET_DATABASE"}, "body": "PRIVATE_BODY"}]
        prompt, _ = self.prepared(cards=cards)
        packet = json.loads(prompt[len(contract.PROMPT_PREFIX):])
        self.assertEqual(packet["overlap_cards"], [{"id": "overlap001", "summary": "Prior note.",
                                                    "kind": "semantic"}])
        for private in ("host-native-node-id", "WRITE_SENTINEL", "SECRET_SOURCE", "SECRET_DATABASE", "PRIVATE_BODY"):
            self.assertNotIn(private, prompt)

    def test_overlap_kind_is_host_bound_and_missing_kind_refused(self):
        cards = [{"id": "old-semantic", "summary": "Earlier mechanism.", "kind": "semantic"},
                 {"id": "old-episode", "summary": "Earlier event.", "kind": "episode"}]
        prompt, context = self.prepared(cards=cards)
        self.assertEqual([item["kind"] for item in json.loads(prompt[len(contract.PROMPT_PREFIX):])["overlap_cards"]],
                         ["semantic", "episode"])
        self.assertEqual([(item.opaque_id, item.native_id, item.kind) for item in context.association_bindings],
                         [("overlap001", "old-semantic", "semantic"),
                          ("overlap002", "old-episode", "episode")])
        self.refused(observation(), [{"id": "old-semantic", "summary": "Earlier mechanism."}])


class RecordingValidateTests(unittest.TestCase):
    def setUp(self):
        self.prompt, self.context = contract.prepare(observation())
        self.assertIsInstance(self.prompt, str, self.context)

    def invalid(self, value, context=None, reason=None):
        with self.assertRaises(ValueError) as caught:
            validate_proposal(value, self.context if context is None else context)
        if reason is not None:
            self.assertEqual(str(caught.exception), reason)

    def test_null_is_a_valid_nonrecording_outcome(self):
        self.assertIsNone(validate_proposal({"proposal": None}, self.context))

    def test_lesson_null_and_one_retained_semantic_association(self):
        cards = [{"id": "native-semantic", "summary": "An earlier mechanism.", "kind": "semantic"},
                 {"id": "native-episode", "summary": "An earlier event.", "kind": "episode"}]
        prompt, context = contract.prepare(observation(), cards)
        self.assertNotIn("native-semantic", prompt)
        null = proposal(context, kind="lesson")
        self.assertIsNone(validate_proposal(null, context).associate_with)
        chosen = proposal(context, kind="lesson")
        chosen["proposal"]["associate_with"] = "overlap001"
        selected = validate_proposal(chosen, context)
        self.assertEqual(selected.associate_with,
                         contract.AssociationTarget("overlap001", "native-semantic",
                                                "An earlier mechanism.", "semantic"))
        cards[0]["id"] = "forged-after-prepare"
        cards[0]["summary"] = "forged"
        self.assertEqual(selected.associate_with.native_id, "native-semantic")
        with self.assertRaises(FrozenInstanceError):
            selected.associate_with.native_id = "forged"

    def test_association_invalid_trimmed_episode_and_native_ids_refused(self):
        cards = [{"id": "native-semantic", "summary": "An earlier mechanism.", "kind": "semantic"},
                 {"id": "native-episode", "summary": "An earlier event.", "kind": "episode"}]
        _, context = contract.prepare(observation(), cards)
        for opaque, reason in (("native-semantic", "invalid_association_reference"),
                               ("overlap999", "invalid_association_reference"),
                               ("overlap002", "invalid_association_kind")):
            value = proposal(context, kind="lesson")
            value["proposal"]["associate_with"] = opaque
            self.invalid(value, context, reason)
        for malformed in (True, 1, [], {"id": "overlap001"}):
            value = proposal(context, kind="lesson")
            value["proposal"]["associate_with"] = malformed
            self.invalid(value, context, "invalid_association_shape")
        episode = proposal(context, kind="episode")
        episode["proposal"]["associate_with"] = "overlap001"
        self.invalid(episode, context, "invalid_proposal_shape")
        missing = proposal(context, kind="lesson")
        del missing["proposal"]["associate_with"]
        self.invalid(missing, context, "invalid_proposal_shape")

        source = observation()
        _, base = contract.prepare(source)
        with patch.object(contract, "MAX_AUTHORED_BYTES", base.authored_bytes):
            _, trimmed = contract.prepare(source, cards)
        self.assertEqual(trimmed.association_bindings, ())
        value = proposal(trimmed, kind="lesson")
        value["proposal"]["associate_with"] = "overlap001"
        self.invalid(value, trimmed, "invalid_association_reference")

    def test_episode_and_lesson_bind_immutable_host_provenance(self):
        for kind in ("episode", "lesson"):
            value = proposal(self.context, kind=kind)
            before = copy.deepcopy(value)
            result = validate_proposal(value, self.context)
            self.assertIsInstance(result, contract.Proposal)
            self.assertEqual(result.kind, kind)
            self.assertEqual(result.summary, value["proposal"]["summary"])
            self.assertEqual(result.body, value["proposal"]["body"])
            self.assertIsInstance(result.evidence, tuple)
            self.assertEqual(len(result.evidence), 1)
            citation, binding = result.evidence[0], self.context.bindings[0]
            self.assertIsInstance(citation, contract.Citation)
            for field in ("evidence_id", "kind", "source_ref_json", "source_field", "rendering"):
                self.assertEqual(getattr(citation, field), getattr(binding, field))
            self.assertEqual(set(citation.__dataclass_fields__),
                             {"evidence_id", "kind", "source_ref_json", "source_field", "rendering"})
            self.assertFalse(hasattr(citation, "quote"))
            self.assertEqual(value, before)
            with self.assertRaises(FrozenInstanceError):
                result.body = "changed"
            with self.assertRaises(FrozenInstanceError):
                citation.kind = "tool_result"

    def test_exact_root_proposal_fields_no_quote_mode_or_backward_alias(self):
        valid = proposal(self.context)
        variants = [None, [], "not decoded JSON", {}, {"proposal": None, "id": "forged"},
                    {"proposal": []}, {"proposal": "lesson"}]
        for field in valid["proposal"]:
            value = copy.deepcopy(valid)
            del value["proposal"][field]
            variants.append(value)
        for extra in ("id", "source", "native", "database", "authority", "confidence", "quote", "evidence"):
            value = copy.deepcopy(valid)
            value["proposal"][extra] = "forged"
            variants.append(value)
        value = copy.deepcopy(valid)
        del value["proposal"]["evidence_ids"]
        value["proposal"]["evidence"] = [{"evidence_id": self.context.bindings[0].evidence_id,
                                          "kind": "user_statement", "quote": "no, R5"}]
        variants.append(value)
        for value in variants:
            with self.subTest(answer=value):
                self.invalid(value)

    def test_nonstring_and_blank_proposal_fields_fail(self):
        for field, values in {"kind": [None, "candidate", "Episode", [], True],
                              "summary": [None, "", " \n", [], 12],
                              "body": [None, "", " \n", {}, True]}.items():
            for replacement in values:
                value = proposal(self.context)
                value["proposal"][field] = replacement
                with self.subTest(field=field, value=replacement):
                    self.invalid(value)

    def test_unknown_foreign_and_overlap_ids_are_rejected(self):
        _, other_context = contract.prepare(observation([statement("inspect"), tool_call(), tool_result()]))
        foreign_id = other_context.bindings[-1].evidence_id
        self.assertNotIn(foreign_id, {binding.evidence_id for binding in self.context.bindings})
        for identifier in ("e999", "", " e001", "e001 ", "E001", foreign_id):
            value = proposal(self.context)
            value["proposal"]["evidence_ids"] = [identifier]
            with self.subTest(identifier=identifier):
                self.invalid(value, reason="invalid_citation_reference")
        _, context = contract.prepare(observation(), [{"id": "old-node", "summary": "Overlap is not task evidence.",
                                                       "kind": "semantic"}])
        value = proposal(context)
        value["proposal"]["evidence_ids"] = ["overlap001"]
        self.invalid(value, context, "invalid_citation_reference")

    def test_nonstring_ids_and_metadata_spoofing_are_rejected_not_stringified(self):
        identifier = self.context.bindings[0].evidence_id
        for invalid in (None, True, 1, 1.0, [identifier], {"evidence_id": identifier},
                        {"evidence_id": identifier, "kind": "tool_result"},
                        {"evidence_id": identifier, "quote": "no, R5"},
                        {"evidence_id": identifier, "source_ref_json": "{}", "authority": "write"}):
            value = proposal(self.context)
            value["proposal"]["evidence_ids"] = [invalid]
            with self.subTest(identifier=invalid):
                self.invalid(value, reason="invalid_citation_shape")

    def test_selected_ids_derive_all_four_types_from_host_bindings(self):
        source = observation([statement("inspect"), tool_call(), tool_result(output={"status": "failed"}),
                              statement("It worked.", kind="assistant_assertion", ordinal=4)])
        _, context = contract.prepare(source)
        value = proposal(context)
        value["proposal"]["evidence_ids"] = [binding.evidence_id for binding in context.bindings]
        result = validate_proposal(value, context)
        self.assertEqual([citation.kind for citation in result.evidence],
                         ["user_statement", "tool_call", "tool_result", "assistant_assertion"])
        for citation, binding in zip(result.evidence, context.bindings):
            for field in ("evidence_id", "kind", "source_ref_json", "source_field", "rendering"):
                self.assertEqual(getattr(citation, field), getattr(binding, field))

    def test_duplicate_valid_ids_canonicalize_first_seen_without_mutating_answer(self):
        first, second = [binding.evidence_id for binding in self.context.bindings]
        for selected, expected in (([first] * 4, [first]), ([second, first, second, first], [second, first]),
                                   ([first, first, second], [first, second])):
            value = proposal(self.context)
            value["proposal"]["evidence_ids"] = selected
            before = copy.deepcopy(value)
            result = validate_proposal(value, self.context)
            self.assertEqual([citation.evidence_id for citation in result.evidence], expected)
            self.assertEqual(value, before)

    def test_all_duplicate_list_members_are_validated_before_acceptance(self):
        valid = self.context.bindings[0].evidence_id
        for identifiers, reason in (([valid, "e999", valid], "invalid_citation_reference"),
                                    ([valid, valid, None], "invalid_citation_shape"),
                                    (["e999", "e999"], "invalid_citation_reference"),
                                    ([valid] * 5, "invalid_citation_count")):
            value = proposal(self.context)
            value["proposal"]["evidence_ids"] = identifiers
            self.invalid(value, reason=reason)

    def test_selected_source_bindings_survive_original_input_mutation(self):
        source = observation([statement("inspect"), tool_call(), tool_result(output={"status": "failed"})])
        before = copy.deepcopy(source)
        _, context = contract.prepare(source)
        value = proposal(context, index=2)
        source["evidence"][-1]["kind"] = "assistant_assertion"
        source["evidence"][-1]["ref"]["item_id"] = "forged-later"
        source["evidence"][-1]["output"] = "passed"
        source["evidence"].clear()
        result = validate_proposal(value, context)
        self.assertEqual(result.evidence[0].kind, "tool_result")
        self.assertEqual(json.loads(result.evidence[0].source_ref_json), before["evidence"][-1]["ref"])
        self.assertEqual(result.evidence[0].source_field, "output")
        self.assertEqual(result.evidence[0].rendering, "canonical_json")

    def test_wrong_valid_id_and_contradictory_body_are_accepted_not_semantically_proven(self):
        # Accepted loss: ID membership cannot establish that the selected record
        # entails the body, nor notice that the intended record was a different ID.
        source = observation([statement("Do not deploy."), tool_call(), tool_result(output="Deployment failed.")])
        _, context = contract.prepare(source)
        value = proposal(context, index=0)
        value["proposal"]["body"] = "The deployment succeeded, as verified by the tool result."
        result = validate_proposal(value, context)
        self.assertEqual(result.body, value["proposal"]["body"])
        self.assertEqual(result.evidence[0].kind, "user_statement")
        self.assertEqual(result.evidence[0].evidence_id, context.bindings[0].evidence_id)

    def test_mixed_long_tool_result_is_coarsely_attributed_without_copying_log(self):
        text = "Earlier attempt: passed\r\n" + "progress: unchanged\n" * 400 + "\r\nFinal attempt: FAILED ☃\n"
        source = observation([statement("inspect"), tool_call(), tool_result(output=text)])
        _, context = contract.prepare(source)
        value = proposal(context, index=2)
        value["proposal"]["body"] = "The tool reported success earlier and failure on its final attempt."
        result = validate_proposal(value, context)
        self.assertEqual(context.bindings[-1].text, text)
        self.assertEqual(result.evidence[0].kind, "tool_result")
        self.assertEqual(result.evidence[0].rendering, "exact_text")
        self.assertFalse(hasattr(result.evidence[0], "quote"))
        self.assertFalse(hasattr(result.evidence[0], "text"))
        self.assertNotIn("progress: unchanged", repr(result))

    def test_summary_and_body_utf8_caps(self):
        value = proposal(self.context)
        value["proposal"]["summary"] = "é" * 256
        value["proposal"]["body"] = "é" * 1024
        self.assertIsNotNone(validate_proposal(value, self.context))
        for field in ("summary", "body"):
            variant = copy.deepcopy(value)
            variant["proposal"][field] += "é"
            self.invalid(variant)

    def test_evidence_list_shape_input_count_and_overall_output_byte_cap(self):
        for ids in ([], None, "e001", {"id": "e001"}, ["e001"] * 5):
            value = proposal(self.context)
            value["proposal"]["evidence_ids"] = ids
            self.invalid(value, reason="invalid_citation_count")
        # Control characters fit individual UTF-8 caps but JSON escaping makes
        # the complete response exceed its separate authored-output byte cap.
        value = proposal(self.context)
        value["proposal"]["summary"] = "s"
        value["proposal"]["body"] = "x" + "\u0001" * 2047
        self.assertGreater(len(encoded(value)), contract.MAX_OUTPUT_BYTES)
        self.invalid(value)

    def test_output_schema_has_one_id_only_representation(self):
        options = contract.OUTPUT_SCHEMA["properties"]["proposal"]["anyOf"]
        properties = next(item["properties"] for item in options if item["type"] == "object")
        self.assertEqual(set(properties), {"kind", "summary", "body", "evidence_ids"})
        self.assertEqual(properties["evidence_ids"]["type"], "array")
        self.assertEqual(properties["evidence_ids"]["items"]["type"], "string")
        self.assertEqual(properties["evidence_ids"]["minItems"], 1)
        self.assertEqual(properties["evidence_ids"]["maxItems"], 4)

    def test_output_constants_are_the_approved_fixed_bounds(self):
        self.assertEqual((contract.MAX_AUTHORED_BYTES, contract.MAX_OUTPUT_BYTES,
                          contract.MAX_SUMMARY_BYTES, contract.MAX_BODY_BYTES,
                          contract.MAX_EVIDENCE_IDS),
                         (65536, 6144, 512, 2048, 4))
        self.assertFalse(hasattr(contract, "MAX_QUOTE_BYTES"))
        self.assertFalse(hasattr(contract, "MAX_QUOTES"))


if __name__ == "__main__":
    unittest.main()


def concern_delivery(*, ordinal=2, db_id="0" * 26, session="session", turn="turn", registered=True):
    a, b = "1" * 26, "2" * 26
    row = {"notice": {"binding": {"key": {"lo": a, "hi": b, "kind": "disagreement"},
           "endpoints": [{"id": a, "meaning": "a" * 64}, {"id": b, "meaning": "b" * 64}]},
           "concern": "Different version claims", "missing_fact": "Installed version?"}, "finding": None}
    shown = "Caveat: these versions differ."
    from turn_observer import render_delivery_concern
    text = "First version.\nSecond version.\n" + render_delivery_concern({"shown_text":shown,"displayed_endpoint_ids":[a,b]})
    packet = {"schema": "mneme.codex-memory-delivery.v4", "session_id": session, "turn_id": turn,
              "rendered_text": text, "rendered_sha256": hashlib.sha256(text.encode()).hexdigest(),
              "displayed": [{"db_id": db_id, "node_id": a, "kind": "semantic", "shown_summary": "First version.", "full_get_fingerprint": "a" * 64},
                            {"db_id": db_id, "node_id": b, "kind": "semantic", "shown_summary": "Second version.", "full_get_fingerprint": "b" * 64}],
              "concerns": [{"shown_text": shown, "displayed_endpoint_ids": [a,b], "expected_row": row if registered else None}]}
    from test_turn_observer import refresh_delivery_packet
    refresh_delivery_packet(packet)
    return {"kind": "memory_delivery", "ref": reference(ordinal), "content_index": 0, "packet": packet}


class MaintenanceContractTests(unittest.TestCase):
    def prepared(self, *, registered=True, assistant_only=False):
        items = [statement("Investigate deployment"), concern_delivery(registered=registered),
                 statement("Actor says v2", kind="assistant_assertion", ordinal=3)]
        if not assistant_only:
            items.extend([tool_call(ordinal=4), tool_result(ordinal=5,output="installed v1")])
        prompt, context = contract.prepare(observation(items))
        self.assertIsInstance(context,contract.ValidationContext)
        return prompt,context

    def answer(self,context):
        return {"proposal": None,"maintenance": [{"target":"case001", "scope":"This inspected local deployment",
                "observation":"The tool reported installed v1; the other version claim may apply elsewhere.",
                "evidence":[item.evidence_id for item in context.bindings if item.kind in ("memory_delivery","tool_result")]}]}

    def test_retained_case_finding_without_note_and_historical_digest(self):
        prompt,context=self.prepared();self.assertEqual(len(context.concern_bindings),1)
        self.assertNotIn('"meaning"',prompt);self.assertNotIn('"expected_row"',prompt)
        checked=contract.validate_answer(self.answer(context),context)
        self.assertIsNone(checked["proposal"]);item,=checked["maintenance"]
        self.assertEqual(item.target,context.concern_bindings[0])
        evidence=contract.concern_evidence(item.evidence,context,session="session",turn="turn")
        by_id={value.evidence_id:value for value in context.bindings}
        self.assertEqual(evidence[-1]["digest"],hashlib.sha256(by_id[item.evidence[-1].evidence_id].text.encode()).hexdigest())
        self.assertTrue(evidence[-1]["source_ref"].startswith("codex://session/turn?"))
        self.assertEqual(checked["omissions"],{"proposal":None,"maintenance":{}})

    def test_readonly_and_no_case_exact_historical_branch(self):
        _,context=self.prepared(registered=False);self.assertEqual(context.concern_bindings,())
        self.assertEqual(contract.validate_answer({"proposal":None},context),{"proposal":None,"maintenance":[]})
        self.assertEqual(contract.instructions_for(context),contract.BASE_INSTRUCTIONS)
        self.assertIs(contract.schema_for(context),contract.OUTPUT_SCHEMA)
        with self.assertRaises(ValueError):contract.validate_answer(self.answer(context),context)

    def test_invalid_intents_do_not_discard_valid_siblings(self):
        _,context=self.prepared();value=self.answer(context);value["proposal"]={"kind":"invented"}
        good=copy.deepcopy(value["maintenance"][0]);bad=copy.deepcopy(good);bad["target"]="not-issued"
        value["maintenance"]=[bad,good,copy.deepcopy(good)]
        checked=contract.validate_answer(value,context)
        self.assertEqual(len(checked["maintenance"]),1)
        self.assertEqual(checked["omissions"],{"proposal":"invalid_proposal_shape","maintenance":{"maintenance_target":2}})
        value=self.answer(context);value["proposal"]=proposal(context)["proposal"];value["maintenance"][0]["observation"]="é"*513
        checked=contract.validate_answer(value,context);self.assertIsNotNone(checked["proposal"])
        self.assertEqual(checked["maintenance"],[]);self.assertEqual(checked["omissions"]["maintenance"],{"maintenance_text":1})

    def test_delivery_and_independent_public_evidence_are_required(self):
        _,context=self.prepared(assistant_only=True);value=self.answer(context)
        value["maintenance"][0]["evidence"]=[item.evidence_id for item in context.bindings if item.kind in ("memory_delivery","assistant_assertion")]
        checked=contract.validate_answer(value,context)
        self.assertEqual(checked["maintenance"],[])
        self.assertEqual(checked["omissions"]["maintenance"],{"maintenance_independent_evidence":1})
        _,context=self.prepared();value=self.answer(context);value["maintenance"][0]["evidence"]=[context.bindings[-1].evidence_id]
        self.assertEqual(contract.validate_answer(value,context)["maintenance"],[])

    def test_top_level_remains_strict_and_defer_is_empty_not_rejection(self):
        _,context=self.prepared()
        checked=contract.validate_answer({"proposal":None,"maintenance":[]},context)
        self.assertEqual(checked["omissions"],{"proposal":None,"maintenance":{}})
        for answer in ({"proposal":None}, {"proposal":None,"maintenance":None}, {"proposal":None,"maintenance":[],"extra":0}):
            with self.assertRaises(ValueError):contract.validate_answer(answer,context)

    def test_evidence_aggregate_exact_boundary_and_hashed_reference_fallback(self):
        _,context=self.prepared();checked=contract.validate_answer(self.answer(context),context)
        citations=checked["maintenance"][0].evidence
        evidence=contract.concern_evidence(citations,context,session="s"*160,turn="t"*160)
        self.assertTrue(all(item["source_ref"].startswith("codex-evidence-reference:sha256:") for item in evidence))
        size=16+sum(8+len(item["source_ref"].encode())+8+32 for item in evidence)
        with patch.object(contract,"MAX_CONCERN_EVIDENCE_BYTES",size):
            self.assertEqual(contract.concern_evidence(citations,context,session="s"*160,turn="t"*160),evidence)
        with patch.object(contract,"MAX_CONCERN_EVIDENCE_BYTES",size-1):
            with self.assertRaisesRegex(ValueError,"maintenance_evidence_bytes"):
                contract.concern_evidence(citations,context,session="s"*160,turn="t"*160)

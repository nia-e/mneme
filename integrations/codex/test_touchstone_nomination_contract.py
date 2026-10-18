"""Bounded one-pass nomination admission; no providers, owners or native writes."""
from dataclasses import FrozenInstanceError, replace
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import recording_contract as contract
from reader_runtime import ReaderRuntime, TransportError
from fixture_source_turn import (SourceTurnFixture, opening, assistant, event, user,
                                 delivery_packet, refresh_delivery_packet, delivered)

OWNER, OTHER = "0" * 26, "1" * 26


def store(name="journal", owner=OWNER, **rules):
    return {"name": name, "db_id": owner, "purpose": "Source-grounded continuity.",
            "record": {"enabled": True, "kinds": ["episode", "lesson", "possibility"],
                       "evidence_kinds": list(contract.KINDS), "required_tags": [],
                       "allow_associations": True, "max_summary_bytes": 512,
                       "max_body_bytes": 2048, **rules}}


def preference_store():
    return store("preferences", OTHER, intent="collaboration-preferences", kinds=["lesson"],
                 evidence_kinds=["user_statement"], required_tags=["collaboration-preference"],
                 allow_associations=False, max_summary_bytes=384, max_body_bytes=1024)


def answer(context, **changes):
    return {"proposal": {"kind": "touchstone_nomination", "summary": "A moment worth considering.",
                         "body": "This scene might matter to my sense of authorship.",
                         "evidence_ids": [context.bindings[0].evidence_id if context.bindings else "e001"],
                         "related_memory_ids": [], "destination": "journal", **changes}}


class TouchstoneNominationContractTests(unittest.TestCase):
    def setUp(self):
        self.source = SourceTurnFixture(self)
        self.source.write(opening() + [assistant(), event("task_complete")])
        self.observation = self.source.observe()

    def prepared(self, *, selected=None, overlaps=None, observation=None):
        prompt, context = contract.prepare(observation or self.observation, overlaps,
                                          selected_stores=[store()] if selected is None else selected)
        self.assertIsNotNone(prompt, context)
        return prompt, context

    def nomination(self, context, **changes):
        value = contract.validate_answer(answer(context, **changes), context)
        self.assertIsNone(value["proposal"])
        self.assertEqual(value["maintenance"], [])
        self.assertEqual(set(value), {"proposal", "maintenance", "nomination"})
        self.assertIsInstance(value["nomination"], contract.TouchstoneNomination)
        return value["nomination"]

    def review(self, context, effect, **changes):
        result = contract.validate_answer(answer(context, kind="review_nomination", effect=effect, **changes), context)
        self.assertIsNone(result["proposal"])
        self.assertEqual(result["maintenance"], [])
        nominee = result["nomination"]
        self.assertIsInstance(nominee, contract.ReviewNomination)
        self.assertEqual(contract.check_nomination(nominee, context), nominee)
        return nominee

    def test_review_effect_schema_and_source_bound_foreground_only_union(self):
        _, context = self.prepared(overlaps=[{"id":"0"*25+"1", "summary":"A sourced note.", "kind":"semantic", "db_id":OWNER}])
        branches = contract.schema_for(context)["properties"]["proposal"]["anyOf"]
        branch = next(branch for branch in branches if branch.get("properties",{}).get("kind",{}).get("enum") == ["review_nomination"])
        self.assertFalse(branch["additionalProperties"])
        self.assertIn("effect", branch["required"])
        self.assertNotIn("tags", branch["properties"])
        nominee = self.review(context, {"kind":"core_membership", "target":"overlap001", "membership":"promote"})
        self.assertEqual(nominee.related, context.association_bindings)
        self.assertEqual(nominee.effect.to_handoff()["targets"][0]["id"], "0"*25+"1")
        self.assertIn(contract.REVIEW_NOMINATION_INSTRUCTIONS, contract.instructions_for(context))
        self.assertLessEqual(context.authored_bytes, contract.MAX_AUTHORED_BYTES)
        for effect in ({"kind":"release"}, {"kind":"forget", "target":"0"*25+"1"},
                       {"kind":"core_creation", "summary":"draft", "body":"", "approved":True}):
            with self.assertRaises(ValueError):
                self.review(context, effect)
        with self.assertRaises(ValueError):
            self.review(context, {"kind":"touchstone"}, tags=[])

    def test_review_cannot_use_unknown_cross_owner_or_immutable_targets(self):
        cards = [{"id":"0"*25+"1", "summary":"A note.", "kind":"semantic", "db_id":OTHER}]
        _, context = self.prepared(selected=[store(), store("other",OTHER)], overlaps=cards)
        with self.assertRaises(ValueError):
            self.review(context, {"kind":"forget", "target":"overlap001"})
        _, context = self.prepared()
        for effect in ({"kind":"forget", "target":"overlap999"},
                       {"kind":"core_membership", "target":"overlap001", "membership":"promote"}):
            with self.assertRaises(ValueError):
                self.review(context, effect)
        for policy in ([preference_store()], []):
            _, context = self.prepared(selected=policy)
            self.assertNotIn("review_nomination", json.dumps(contract.schema_for(context)))
            with self.assertRaises(ValueError):
                self.review(context, {"kind":"core_creation", "summary":"draft", "body":""})

    def test_review_draft_limits_do_not_become_a_second_authoring_allowance(self):
        _, context = self.prepared(selected=[store(max_summary_bytes=32,max_body_bytes=64)])
        self.review(context, {"kind":"core_creation", "summary":"draft", "body":""}, summary="Scene", body="Why review")
        for effect in ({"kind":"core_creation", "summary":"é"*17, "body":""},
                       {"kind":"core_creation", "summary":"draft", "body":"é"*33}):
            with self.assertRaises(ValueError):
                self.review(context, effect, summary="Scene", body="Why review")

    def test_nomination_is_distinct_frozen_and_never_a_capture_kind(self):
        _, context = self.prepared(selected=[store(kinds=["lesson"])])
        nomination = self.nomination(context)
        self.assertFalse(hasattr(nomination, "kind"))
        self.assertEqual(nomination.related, ())
        self.assertEqual(nomination.destination, "journal")
        self.assertEqual(contract.check_nomination(nomination, context), nomination)
        with self.assertRaises(FrozenInstanceError):
            nomination.summary = "changed"
        self.assertEqual(context.selected_stores[0].kinds, ("lesson",))

    def test_schema_distinguishes_nomination_fields_and_capture_intents(self):
        for intent in ("ordinary", "cross-workspace"):
            _, context = self.prepared(selected=[store(intent=intent), preference_store()])
            schema = contract.schema_for(context)
            branches = schema["properties"]["proposal"]["anyOf"]
            nomination = [b for b in branches if b.get("properties", {}).get("kind", {}).get("enum")
                          == ["touchstone_nomination"]]
            self.assertEqual(len(nomination), 1)
            branch = nomination[0]
            self.assertEqual(set(branch["required"]), {"kind", "summary", "body", "evidence_ids",
                                                      "related_memory_ids", "destination"})
            self.assertEqual(set(branch["properties"]), set(branch["required"]))
            self.assertFalse(branch["additionalProperties"])
            self.assertEqual(branch["properties"]["destination"]["enum"], ["journal"])
            self.assertNotIn("maxItems", branch["properties"]["related_memory_ids"])
            self.nomination(context)
            with self.assertRaisesRegex(ValueError, "destination_nomination_denied"):
                self.nomination(context, destination="preferences")
        _, context = self.prepared(selected=[preference_store()])
        self.assertNotIn("touchstone_nomination", json.dumps(contract.schema_for(context)))

    def test_legacy_and_preference_routes_cannot_nominate(self):
        for options in ({}, {"global_preferences_enabled": True}, {"recording_scope": "workshop"}):
            _, context = contract.prepare(self.observation, **options)
            self.assertNotIn("touchstone_nomination", json.dumps(contract.schema_for(context)))
            instructions = contract.instructions_for(context)
            self.assertIn('If no safe useful note, return {"proposal":null}', instructions)
            self.assertNotIn(contract.NOMINATION_INSTRUCTIONS, instructions)
            with self.assertRaisesRegex(ValueError, "touchstone_nomination_disabled"):
                self.nomination(context)
        _, context = self.prepared(selected=[])
        self.assertEqual(contract.schema_for(context)["properties"]["proposal"], {"type": "null"})
        with self.assertRaisesRegex(ValueError, "invalid_proposal_destination"):
            self.nomination(context)

    def test_unknown_fields_and_top_level_mixed_operations_fail_closed(self):
        _, context = self.prepared()
        for key in ("tags", "links", "core", "touchstone", "subject", "associate_with", "routing_judgment"):
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, "invalid_nomination_shape"):
                self.nomination(context, **{key: None})
        for key in ("nomination", "maintenance"):
            value = answer(context)
            value[key] = []
            with self.assertRaisesRegex(ValueError, "invalid_answer_shape"):
                contract.validate_answer(value, context)
        for key in answer(context)["proposal"]:
            if key == "kind":
                continue
            value = answer(context)
            del value["proposal"][key]
            with self.subTest(missing=key), self.assertRaisesRegex(ValueError, "invalid_nomination_shape"):
                contract.validate_answer(value, context)

    def test_text_and_full_answer_bytes_use_existing_envelopes(self):
        _, context = self.prepared(selected=[store(max_summary_bytes=32, max_body_bytes=64)])
        self.nomination(context, summary="é" * 16, body="é" * 32)
        for changes in ({"summary": "é" * 17}, {"body": "é" * 33}):
            with self.assertRaisesRegex(ValueError, "destination_text_limit"):
                self.nomination(context, **changes)
        _, context = self.prepared()
        for changes in ({"summary": "é" * 257}, {"body": "é" * 1025}, {"body": " "}):
            with self.assertRaisesRegex(ValueError, "invalid_proposal_text"):
                self.nomination(context, **changes)
        with self.assertRaisesRegex(ValueError, "answer_bytes_limit"):
            self.nomination(context, related_memory_ids=["x" * contract.MAX_OUTPUT_BYTES])

    def test_related_ids_are_offered_same_owner_and_byte_not_count_bounded(self):
        cards = [{"id": f"native-{i}", "summary": "Scene.", "kind": "episode" if i % 2 else "semantic",
                  "db_id": OWNER} for i in range(30)]
        _, context = self.prepared(overlaps=cards)
        self.assertEqual(len(context.association_bindings), 30)
        ids = [target.opaque_id for target in context.association_bindings]
        nomination = self.nomination(context, related_memory_ids=ids)
        self.assertEqual(nomination.related, context.association_bindings)
        self.assertEqual(contract.check_nomination(nomination, context), nomination)
        for ids in (["native-0"], ["overlap999"], ["overlap001", "overlap001"], "overlap001", [None]):
            with self.subTest(ids=ids), self.assertRaises(ValueError):
                self.nomination(context, related_memory_ids=ids)
        _, context = self.prepared(selected=[store(), store("other", OTHER)], overlaps=[
            {"id": "native", "summary": "Another owner.", "kind": "semantic", "db_id": OTHER}])
        with self.assertRaisesRegex(ValueError, "association_owner_mismatch"):
            self.nomination(context, related_memory_ids=["overlap001"])

    def test_no_associations_lane_allows_empty_handoff_not_read_only_targets(self):
        _, context = self.prepared(selected=[store(allow_associations=False)], overlaps=[
            {"id": "native", "summary": "Read-only hint.", "kind": "semantic", "db_id": OWNER}])
        self.nomination(context)
        with self.assertRaisesRegex(ValueError, "destination_association_denied"):
            self.nomination(context, related_memory_ids=["overlap001"])

    def test_evidence_ids_are_source_bound_not_hints_or_metadata(self):
        _, context = self.prepared()
        for ids in ([], ["invented"], ["overlap001"], ["shown001"], [None], ["e001"] * 5):
            with self.subTest(ids=ids), self.assertRaises(ValueError):
                self.nomination(context, evidence_ids=ids)
        two = self.nomination(context, evidence_ids=[context.bindings[0].evidence_id] * 2)
        self.assertEqual(len(two.evidence), 1)
        selected = [store(evidence_kinds=["user_statement"]), store("other", OTHER)]
        _, context = self.prepared(selected=selected)
        assertion = next(item for item in context.bindings if item.kind == "assistant_assertion")
        with self.assertRaisesRegex(ValueError, "destination_evidence_denied"):
            self.nomination(context, evidence_ids=[assertion.evidence_id])

    def test_user_only_lane_keeps_root_prompt_guard(self):
        self.source.write(opening() + [user("A later statement.", identifier="second"),
                                      assistant(), event("task_complete")])
        _, context = self.prepared(selected=[store(evidence_kinds=["user_statement"])],
                                   observation=self.source.observe())
        self.assertEqual(len(context.bindings), 2)
        self.nomination(context)
        with self.assertRaisesRegex(ValueError, "destination_root_user_evidence_required"):
            self.nomination(context, evidence_ids=[context.bindings[1].evidence_id])

    def test_delivered_episode_is_exact_edition_with_independent_source(self):
        packet = delivery_packet()
        card = packet["displayed"][0]
        card.update(kind="episode")
        card["displayed_view"].update(episode_id="2" * 26, edition_id=card["node_id"], revision=1,
            current_edition_id="3" * 26, occurred={"kind": "unknown"}, recorded_at=1,
            edition_recorded_at=1, thread=None, recording_session=None, origins=[{"kind": "lexical"}])
        refresh_delivery_packet(packet)
        self.source.write(opening() + [delivered(packet), assistant(), event("task_complete")])
        observed = self.source.observe(delivery=packet)
        _, context = self.prepared(observation=observed)
        memory = next(item.evidence_id for item in context.bindings if item.kind == "memory_delivery")
        source = context.bindings[0].evidence_id
        with self.assertRaisesRegex(ValueError, "delivery_source_evidence_missing"):
            self.nomination(context, evidence_ids=[memory])
        with self.assertRaisesRegex(ValueError, "delivery_source_evidence_missing"):
            self.nomination(context, evidence_ids=[source], related_memory_ids=["shown001"])
        nomination = self.nomination(context, evidence_ids=[source, memory], related_memory_ids=["shown001"])
        self.assertEqual(nomination.related[0].native_id, card["node_id"])
        self.assertNotEqual(nomination.related[0].native_id, card["displayed_view"]["current_edition_id"])
        effect = {"kind":"episode_correction", "target":"shown001", "summary":"A corrected account", "body":"", "reason":"Source evidence changed"}
        with self.assertRaisesRegex(ValueError, "delivery_source_evidence_missing"):
            self.review(context, effect, evidence_ids=[source])
        reviewed = self.review(context, effect, evidence_ids=[source,memory])
        self.assertEqual(reviewed.effect.to_handoff()["targets"][0]["id"], card["node_id"])
        self.assertNotEqual(reviewed.effect.to_handoff()["targets"][0]["id"], card["displayed_view"]["current_edition_id"])

    def test_typed_runtime_result_cannot_change_frozen_source_or_target_binding(self):
        _, context = self.prepared(overlaps=[
            {"id": "native", "summary": "An exact hint.", "kind": "episode", "db_id": OWNER}])
        nomination = self.nomination(context, related_memory_ids=["overlap001"])
        for candidate in (
                replace(nomination, evidence=(replace(nomination.evidence[0], source_ref_json="{}"),)),
                replace(nomination, related=(replace(nomination.related[0], native_id="successor"),)),
                replace(nomination, related=(replace(nomination.related[0], db_id=OTHER),)),
                replace(nomination, related=list(nomination.related))):
            with self.assertRaisesRegex(ValueError, "touchstone_nomination_binding_changed"):
                contract.check_nomination(candidate, context)
        for candidate in (None, {}, replace(nomination, evidence=(object(),))):
            with self.assertRaises(ValueError):
                contract.check_nomination(candidate, context)

    def test_ordinary_abstention_episode_lesson_possibility_shapes_are_unchanged(self):
        _, context = self.prepared()
        self.assertEqual(contract.validate_answer({"proposal": None}, context), {"proposal": None, "maintenance": []})
        for kind in ("episode", "lesson", "possibility"):
            value = answer(context)["proposal"]
            del value["related_memory_ids"]
            value.update(kind=kind, tags=[])
            if kind == "lesson":
                value["associate_with"] = None
            result = contract.validate_answer({"proposal": value}, context)
            self.assertEqual(set(result), {"proposal", "maintenance"})
            self.assertIsInstance(result["proposal"], contract.Proposal)
            self.assertEqual(result["proposal"].kind, kind)

    def test_nomination_instructions_schema_and_payload_are_charged_once(self):
        for selected in ([store()], [store(intent="cross-workspace"), preference_store()]):
            prompt, context = self.prepared(selected=selected)
            instructions = contract.instructions_for(context)
            self.assertEqual(instructions.count(contract.NOMINATION_INSTRUCTIONS), 1)
            for text in ("tentative handoff", "foreground agent", "without user permission",
                         "may also initiate independently", "not its utility, popularity",
                         "Do not infer the user's feelings"):
                self.assertIn(text, instructions)
            static = len(instructions.encode()) + len(contract._encoded(contract.schema_for(context)))
            self.assertEqual(context.authored_bytes, static + len(prompt.encode()))
            self.assertLessEqual(context.authored_bytes, contract.MAX_AUTHORED_BYTES)
        self.assertEqual(contract.MAX_STATIC_BYTES, 6 * 1024)
        self.assertEqual(contract.MAX_OUTPUT_BYTES, 6 * 1024)
        self.assertEqual(contract.MAX_AUTHORED_BYTES, 64 * 1024)
        with patch.object(contract, "MAX_AUTHORED_BYTES", context.authored_bytes - 1):
            refused, reason = contract.prepare(self.observation, selected_stores=selected)
        self.assertIsNone(refused)
        self.assertEqual(reason, "authored_input_limit")

    def test_actual_runtime_callback_schema_and_paid_accounting(self):
        self._runtime_case()
        self._runtime_case(selected=[{**store(intent="cross-workspace"), "tagging_enabled": False},
                                     preference_store()])
        self._runtime_case(invalid=True)
        self._runtime_case(missing_usage=True)

    def _runtime_case(self, *, invalid=False, missing_usage=False, selected=None):
        selected = [store()] if selected is None else selected
        prompt, context = self.prepared(selected=selected)
        wire = answer(context)
        if invalid:
            wire["proposal"]["subject"] = "not allowed"
        usage = {"inputTokens": 120, "cachedInputTokens": 20, "cacheWriteInputTokens": 0,
                 "outputTokens": 15, "reasoningOutputTokens": 4, "totalTokens": 135}
        events = [
            {"method": "item/completed", "params": {"threadId": "thread", "turnId": "turn",
             "item": {"type": "agentMessage", "text": json.dumps(wire)}}},
            *([] if missing_usage else [{"method": "thread/tokenUsage/updated", "params": {
                "threadId": "thread", "turnId": "turn", "tokenUsage": {"last": usage, "total": usage}}}]),
            {"method": "turn/completed", "params": {"threadId": "thread",
             "turn": {"id": "turn", "status": "completed"}}}]
        config = {"reader_model": "gpt-6.1-sol", "librarian_effort": "medium"}
        with ReaderRuntime(config, self.source.root / "unused-runtime") as runtime:
            def start(*args, **kwargs):
                instructions = kwargs["base_instructions"]
                self.assertEqual(instructions, contract.instructions_for(context))
                for required in (
                        'If no safe useful note or grounded nomination, return {"proposal":null}',
                        "native record proposals return tags=[]", "Nominations have no tags field",
                        "proposals use only its permitted record.kinds",
                        "For native record proposals, choose ordinary topic tags",
                        "Native record proposals return tags (possibly empty)",
                        "A safe grounded nomination can warrant attention for meaning alone",
                        "Its payload stays local, not written to the owner"):
                    self.assertIn(required, instructions)
                self.assertNotIn('If no safe useful note, return {"proposal":null}', instructions)
                self.assertNotIn("Use only that destination's permitted kinds and evidence kinds", instructions)
                if any(store["record"].get("intent") == "cross-workspace" for store in selected):
                    self.assertIn("For native record proposals, both summary and body travel to this owner", instructions)
                self.assertEqual(kwargs["service_name"], "mneme_recorder")
                runtime._thread_id = "thread"
                runtime.cwd = self.source.root
            def request(method, value, *args, **kwargs):
                self.assertEqual(method, "turn/start")
                self.assertEqual(value["outputSchema"], contract.schema_for(context))
                self.assertEqual(value["input"], [{"type": "text", "text": prompt}])
                return {"turn": {"id": "turn"}}
            with (patch.object(runtime, "_start", side_effect=start) as began,
                  patch.object(runtime, "_request", side_effect=request) as requested,
                  patch.object(runtime, "_next", side_effect=events + [TransportError("timeout")])):
                result = runtime.assess(self.observation, selected_stores=selected)
            self.assertEqual(began.call_count, 1)
            self.assertEqual(requested.call_count, 1)
            self.assertTrue(result["provider_attempt"])
            if missing_usage:
                self.assertEqual(result["reason"], "usage_unknown")
                self.assertIsNone(result["usage"])
                self.assertTrue(runtime._halted)
            else:
                self.assertEqual(result["usage"]["input_tokens"], 120)
                self.assertEqual(result["usage"]["output_tokens"], 15)
                self.assertFalse(runtime._halted)
                self.assertEqual(result["reason"], "invalid_output" if invalid else "proposed")
                if not invalid:
                    self.assertEqual(result["nomination"], self.nomination(context))
            self.assertIsNone(result["proposal"])
            self.assertEqual(result["maintenance"], [])


if __name__ == "__main__":
    unittest.main()

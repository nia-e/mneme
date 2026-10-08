"""Offline touchstone wire, salience, scope and exact-delivery regressions."""
import copy
import hashlib
import json
from types import SimpleNamespace
import unittest

import hook_recall
import hooks
import reader_contract
import recording_contract
from touchstone_contract import (validate_touchstone_view, validate_touchstone_retrieval,
                                delivery_cache_fingerprint)
from turn_observer import validate_delivery_packet, DELIVERY_SCHEMA

DB = "1" * 26
OWNER = "2" * 26
TARGET = "3" * 26


def touchstone(**changes):
    value = {"schema": "mneme.touchstone-view.v1", "subject": "Example Agent's remembered warmth",
             "coverage": "summary_only", "references": [{"db_id": DB, "id": TARGET,
                 "snapshot_sha256": "a" * 64, "summary": {
                     "text": "Original disagreement", "complete": True, "source_bytes": 21},
                 "resolution": "changed_snapshot"}], "references_omitted": 2,
             "origins": [{"kind": "referrer", "anchor_id": TARGET}]}
    return {**value, **changes}


def card(**changes):
    return {"id": OWNER, "kind": "semantic", "summary": "This conversation mattered",
            "status": "active", "source": "codex:authored", "fingerprint": "b" * 64,
            "touchstone": touchstone(), **changes}


def coverage(**changes):
    return {"searched": False, "read_limit": 0, "catalog_reads": 0, "record_reads": 0,
            "referrer_page_reads": 0, "target_reads": 0, "anchors_total": 0,
            "anchors_examined": 0, "owners_discovered": 0, "further_tail_unknown": False,
            "stop_reason": None, **changes}


class TouchstoneLibrarianTests(unittest.TestCase):
    dialogue = [{"role": "user", "text": "Recall the design disagreement in this project"}]

    def test_strict_facet_preserves_subject_historical_summary_and_resolution(self):
        original = touchstone()
        clean = validate_touchstone_view(original, expected_db_id=DB)
        self.assertEqual(clean, original)
        self.assertIsNot(clean["references"], original["references"])
        prompt, context = reader_contract.prepare(self.dialogue, [card()])
        self.assertIsInstance(prompt, str, context)
        shown = json.loads(prompt.split("PAYLOAD:\n")[1])["cards"][0]
        self.assertEqual(shown["touchstone"], original)
        self.assertNotIn("body", prompt)

    def test_no_tag_inference_and_no_personal_meaning_or_core_authority(self):
        plain = card()
        plain.pop("touchstone")
        plain["tags"] = ["touchstone"]
        prompt, _ = reader_contract.prepare(self.dialogue, [plain])
        self.assertNotIn('"touchstone"', prompt)
        self.assertIn("never author/rewrite personal meaning", reader_contract.BASE_INSTRUCTIONS)
        self.assertIn("not popularity", recording_contract.BASE_INSTRUCTIONS)
        self.assertIn("merge owners", recording_contract.BASE_INSTRUCTIONS)
        self.assertNotIn("touchstone", recording_contract.OUTPUT_SCHEMA["properties"])

    def test_unknown_fields_invalid_status_scope_and_duplicate_refs_fail_closed(self):
        bad = [touchstone(extra=True), touchstone(coverage="full_body"), touchstone(subject=""),
               touchstone(references_omitted=True), touchstone(origins=[])]
        for field, value in (("resolution", "archived"), ("snapshot_sha256", "A" * 64),
                             ("summary", {"text": "x", "complete": True, "source_bytes": 9})):
            current = touchstone()
            current["references"][0][field] = value
            bad.append(current)
        duplicate = touchstone()
        duplicate["references"] *= 2
        bad.append(duplicate)
        for value in bad:
            with self.subTest(value=value), self.assertRaises(ValueError):
                validate_touchstone_view(value)
        with self.assertRaises(ValueError):
            validate_touchstone_view(touchstone(), expected_db_id="4" * 26)
        with self.assertRaises(ValueError):
            reader_contract._project_card(card(kind="episode"))

    def test_atomic_facet_budget_omission_does_not_demote_owner(self):
        later = {"id": "later", "kind": "semantic", "summary": "Small lesson"}
        baseline = card()
        baseline.pop("touchstone")
        prompt, _ = reader_contract.prepare(self.dialogue, [baseline, later])
        output, context = reader_contract.prepare(self.dialogue, [card(), later], budget=SimpleNamespace(
            selector_prompt_bytes=len(prompt.encode()), selector_answer_bytes=4096))
        self.assertEqual(context["ids"], ["later"])
        self.assertEqual(context["prompt_omitted_count"], 1)
        self.assertNotIn("This conversation mattered", output)
        self.assertNotIn("Original disagreement", output)

    def test_delivery_identity_binds_actual_caveats_not_only_owner_summary(self):
        original = card()
        rendered, displayed = hooks._render_cards_with_display([original], DB)
        packet = {"schema": DELIVERY_SCHEMA, "session_id": "session", "turn_id": "turn",
                  "rendered_text": rendered, "rendered_sha256": hashlib.sha256(rendered.encode()).hexdigest(),
                  "displayed": displayed, "concerns": []}
        clean = validate_delivery_packet(packet)
        self.assertEqual(clean["displayed"][0]["displayed_view"]["touchstone"], original["touchstone"])
        changed = copy.deepcopy(original)
        changed["touchstone"]["references"][0]["resolution"] = "missing"
        _, changed_display = hooks._render_cards_with_display([changed], DB)
        self.assertNotEqual(displayed[0]["displayed_view_sha256"], changed_display[0]["displayed_view_sha256"])
        self.assertEqual(displayed[0]["full_get_fingerprint"], changed_display[0]["full_get_fingerprint"])
        self.assertNotEqual(delivery_cache_fingerprint(original), delivery_cache_fingerprint(changed))
        ordinary = dict(original)
        ordinary.pop("touchstone")
        self.assertEqual(delivery_cache_fingerprint(ordinary), ordinary["fingerprint"])
        forged = copy.deepcopy(packet)
        forged["displayed"] = changed_display
        with self.assertRaises(ValueError):
            validate_delivery_packet(forged)
        with self.assertRaises(ValueError):
            hooks._pack_async_delivery([original], "session", "turn", "4" * 26)

    def test_recording_overlap_and_delivered_projection_keep_authored_facet(self):
        overlap, bindings = recording_contract._overlap([card()])
        self.assertEqual(overlap[0]["touchstone"], touchstone())
        self.assertEqual(json.loads(bindings[0].touchstone_json), touchstone())
        malformed = card(touchstone={"subject": "not enough"})
        with self.assertRaises(ValueError):
            recording_contract._overlap([malformed])
        from test_recording_contract import observation, statement, reference
        rendered, displayed = hooks._render_cards_with_display([card()], DB)
        marker = {"kind": "memory_delivery", "ref": reference(2), "content_index": 0,
                  "packet": {"schema": DELIVERY_SCHEMA, "session_id": "session", "turn_id": "turn",
                             "rendered_text": rendered, "rendered_sha256": hashlib.sha256(rendered.encode()).hexdigest(),
                             "displayed": displayed, "concerns": []}}
        prompt, context = recording_contract.prepare(observation([statement("A new scoped choice"), marker]))
        self.assertIsInstance(prompt, str, context)
        delivered = json.loads(prompt.split("PAYLOAD:\n")[1])["delivered_cards"]
        self.assertEqual(delivered[0]["touchstone"], touchstone())
        self.assertEqual(json.loads(context.association_bindings[0].touchstone_json), touchstone())

    def test_v6_cannot_smuggle_new_native_facet(self):
        context = {"schema": "mneme.context.v6", "core": [], "primary": [card()],
                   "expansions": [], "episodes": []}
        with self.assertRaises(ValueError):
            hook_recall._candidates(context, reader=True)
        context["schema"] = "mneme.context.v7"
        context["touchstone_retrieval"] = coverage()
        self.assertEqual(hook_recall._candidates(context, reader=True)[0]["touchstone"], touchstone())

    def test_operation_coverage_retains_one_hop_bounds_and_explicit_unknown_tail(self):
        value = coverage(searched=True, read_limit=4, catalog_reads=1, record_reads=1,
                         referrer_page_reads=1, target_reads=1, anchors_total=3,
                         anchors_examined=1, owners_discovered=1,
                         further_tail_unknown=True, stop_reason="budget")
        self.assertEqual(validate_touchstone_retrieval(value), value)
        for bad in ({**value, "target_reads": 2}, {**value, "anchors_examined": 4},
                    {**value, "searched": False}, {**value, "recursive_reads": 1}):
            with self.subTest(value=bad), self.assertRaises(ValueError):
                validate_touchstone_retrieval(bad)
        from test_hook_recall import discovery_metadata
        native = hook_recall._native_discovery({**discovery_metadata(), "schema": "mneme.context.v7",
                                               "touchstone_retrieval": value})
        self.assertEqual(native["touchstone_retrieval"], value)
        bounded = hook_recall._bounded_discovery({"scope": "bounded_native_window",
                "continuation": "unavailable", "native": native, "adapter": None})
        self.assertEqual(bounded["native"]["touchstone_retrieval"], value)

    def test_get_identity_binds_authored_record_but_not_resolution_or_routes(self):
        node = {"id": OWNER, "status": "active", "summary": "This conversation mattered",
                "summary_truncated": False, "provenance": {"type": "conversation", "session": "fixture", "turn": 1},
                "memory_kind": {"kind": "semantic"}, "touchstone": {"owner": OWNER,
                    "subject": touchstone()["subject"], "references": [{"db_id": DB, "id": TARGET,
                        "summary": "Original disagreement", "provenance": {"type": "conversation"},
                        "created": 12, "memory_kind": {"kind": "semantic"}},
                        {"db_id": DB, "id": "4" * 26, "summary": "Other", "provenance": {},
                         "created": 13, "memory_kind": {"kind": "semantic"}},
                        {"db_id": DB, "id": "5" * 26, "summary": "Third", "provenance": {},
                         "created": 14, "memory_kind": {"kind": "semantic"}}]}}
        first = hook_recall._card(node, OWNER, card())
        candidate = card()
        candidate["touchstone"]["references"][0]["resolution"] = "missing"
        candidate["touchstone"]["origins"] = [{"kind": "direct"}]
        second = hook_recall._card(node, OWNER, candidate)
        self.assertEqual(first["fingerprint"], second["fingerprint"])
        changed = copy.deepcopy(node)
        changed["touchstone"]["references"][1]["summary"] = "Different original"
        self.assertNotEqual(first["fingerprint"], hook_recall._card(changed, OWNER, candidate)["fingerprint"])
        with self.assertRaises(ValueError):
            hook_recall._card(node, OWNER, {"kind": "semantic"})

    def test_incoming_owner_is_not_a_direct_hit_or_learning_route(self):
        incoming = card(entry_kind="conditional", conditional_binding={"forged": "binding"})
        plain = {"id": "4" * 26, "kind": "semantic"}
        context = {"schema": "mneme.context.v7", "core": [], "primary": [incoming, plain],
                   "expansions": [], "episodes": [], "touchstone_retrieval": coverage(),
                   "observation": {"schema": 1, "learning": "disabled", "cards": [
                       {"node_id": OWNER, "card_sha256": "a" * 64, "lane": "primary"},
                       {"node_id": plain["id"], "card_sha256": "b" * 64, "lane": "primary"}]}}
        self.assertEqual([c["id"] for c in hook_recall._candidates(context, reader=True)], [plain["id"], OWNER])
        _, displayed = hooks._render_cards_with_display([incoming], DB)
        self.assertNotIn("entry_kind", displayed[0])
        self.assertNotIn("conditional_binding", displayed[0])


if __name__ == "__main__":
    unittest.main()

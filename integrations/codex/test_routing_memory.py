import copy
import hashlib
import json
import unittest
from unittest.mock import patch

import routing_memory as routing


DB, PREVIOUS, TARGET, WITNESS = ["0" * 25 + str(i) for i in range(1, 5)]


def binding():
    return {"db_id": DB, "route": {"previous": PREVIOUS, "target": TARGET,
            "from": PREVIOUS, "to": TARGET, "previous_fingerprint": "a" * 64,
            "target_fingerprint": "b" * 64, "edge_fingerprint": "c" * 64}}


def body(**changes):
    args = {"note": "Keep the journal when the snapshot omits pending writes.",
            "binding": binding(), "sign": "weaken", "conditions": "Snapshot omits pending writes.",
            "rationale": "Discarding the journal lost the pending write in the shown readback.",
            "shown_summary": "Discard the journal after taking a snapshot.",
            "session": "session1", "turn": "turn1", "evidence": [
                {"kind": "memory_delivery", "reference": "source-record:2"},
                {"kind": "tool_result", "reference": "source-record:5"}]}
    return routing.encode_witness(**{**args, **changes})


def node(text=None):
    text = body() if text is None else text
    return {"id": WITNESS, "db_id": DB, "status": "active", "tags": [routing.TAG],
            "memory_kind": {"kind": "semantic"}, "provenance": {"type": "external", "source": {
                "namespace": routing.NAMESPACE, "session": "session1",
                "reference": "codex://session1/turn1"}}, "body": text,
            "body_range": {"source_start": 0, "source_end": len(text.encode()),
                           "has_more": False, "next_offset": None}}


class RoutingMemoryTests(unittest.TestCase):
    def test_conditional_binding_is_exact_target_and_database_not_a_path(self):
        self.assertEqual(routing.validate_conditional_binding(binding(), TARGET, expected_db_id=DB), binding())
        for target, db in ((PREVIOUS, DB), (TARGET, WITNESS)):
            with self.assertRaises(ValueError):
                routing.validate_conditional_binding(binding(), target, expected_db_id=db)
        with self.assertRaisesRegex(ValueError, "binding_path"):
            routing.validate_observed_binding(binding(), [])

    def test_conditional_witness_annotation_is_optional_and_legacy_bytes_unchanged(self):
        old = body()
        self.assertNotIn("entry_kind", json.loads(old))
        self.assertEqual(body(entry_kind=None), old)
        conditional = body(entry_kind="conditional")
        decoded = routing.decode_witness(node(conditional), expected_db_id=DB)
        self.assertEqual(decoded["witness"]["entry_kind"], "conditional")
        self.assertEqual(decoded["witness"]["binding"], binding())
        for invalid in ("graph", False, {}, ""):
            with self.assertRaises(ValueError):
                body(entry_kind=invalid)

    def test_round_trip_keeps_exact_conditions_and_native_binding(self):
        original = node()
        decoded = routing.decode_witness(original, expected_db_id=DB)
        self.assertEqual(decoded["witness"]["binding"], binding())
        self.assertEqual(decoded["witness"]["sign"], "weaken")
        self.assertEqual(decoded["body_sha256"], hashlib.sha256(original["body"].encode()).hexdigest())
        self.assertIn("omits pending writes", decoded["witness"]["conditions"])

    def test_binding_copies_input_and_rejects_unrelated_or_wrong_database(self):
        value = binding()
        copied = routing.validate_binding(value, expected_db_id=DB)
        value["route"]["target_fingerprint"] = "d" * 64
        self.assertEqual(copied["route"]["target_fingerprint"], "b" * 64)
        with self.assertRaises(ValueError):
            routing.validate_binding(value, expected_db_id=WITNESS)
        value["route"]["target"] = WITNESS
        with self.assertRaises(ValueError):
            routing.validate_binding(value)

    def test_reverse_orientation_stays_distinct(self):
        forward = binding()
        reverse = copy.deepcopy(forward)
        reverse["route"]["previous"], reverse["route"]["target"] = TARGET, PREVIOUS
        self.assertNotEqual(routing.validate_binding(forward), routing.validate_binding(reverse))

    def test_ordinary_note_or_forged_label_is_not_automatically_feedback(self):
        for change in ({"body": "Ordinary prose."}, {"tags": []}, {"status": "archived"},
                       {"db_id": WITNESS}, {"memory_kind": {"kind": "episode"}},
                       {"provenance": {"type": "conversation"}}):
            with self.subTest(change=change):
                self.assertIsNone(routing.decode_witness({**node(), **change}, expected_db_id=DB))

    def test_partial_readback_and_changed_source_are_misses(self):
        for key, value in (("source_start", 1), ("source_end", 2), ("has_more", True), ("next_offset", 1)):
            n = node(); n["body_range"][key] = value
            self.assertIsNone(routing.decode_witness(n, expected_db_id=DB))
        n = node(); n["provenance"]["source"]["reference"] = "codex://another/turn"
        self.assertIsNone(routing.decode_witness(n, expected_db_id=DB))

    def test_correction_is_an_exact_reference_not_automatic_supersession(self):
        correction = {"node_id": WITNESS, "body_sha256": "e" * 64}
        result = routing.decode_witness(node(body(corrects=correction)), expected_db_id=DB)
        self.assertEqual(result["witness"]["corrects"], correction)
        self.assertEqual(result["witness"]["sign"], "weaken")
        with self.assertRaises(ValueError):
            body(corrects={"node_id": WITNESS, "newest_wins": True})

    def test_no_extra_vote_or_scalar_and_duplicate_sources_rejected(self):
        value = json.loads(body())
        value["confidence"] = 1
        with self.assertRaises(ValueError):
            routing.validate_witness(value)
        with self.assertRaises(ValueError):
            body(evidence=[{"kind": "memory_delivery", "reference": "r1"}] * 2)
        with self.assertRaises(ValueError):
            routing.hint(binding(), 0.9)

    def test_bounds_are_utf8_bytes_and_never_truncate_a_binding(self):
        body(conditions="é" * (routing.MAX_CONDITIONS_BYTES // 2))
        with self.assertRaises(ValueError):
            body(conditions="é" * (routing.MAX_CONDITIONS_BYTES // 2 + 1))
        bad = binding(); bad["route"]["edge_fingerprint"] += "0"
        with self.assertRaises(ValueError):
            routing.validate_binding(bad)

    def test_duplicate_keys_and_malformed_optional_data_are_misses(self):
        raw = body().replace('"schema":', '"schema":"other","schema":', 1)
        self.assertIsNone(routing.decode_witness(node(raw), expected_db_id=DB))
        for value in (None, [], {}, {"id": []}):
            self.assertIsNone(routing.decode_witness(value, expected_db_id=DB))

    def test_private_binding_survives_delivery_without_changing_actor_text(self):
        import hooks
        from turn_observer import DELIVERY_SCHEMA, validate_delivery_packet
        card = {"id": TARGET, "summary": "Check the snapshot's journal coverage.",
                "source": "codex:example", "kind": "semantic", "status": "active",
                "fingerprint": "f" * 64}
        baseline, _ = hooks._render_cards_with_display([card], DB)
        text, displayed = hooks._render_cards_with_display([{**card, "routing_binding": binding()}], DB)
        self.assertEqual(text, baseline)
        self.assertEqual(displayed[0]["routing_binding"], binding())
        packet = {"schema": DELIVERY_SCHEMA, "session_id": "s", "turn_id": "t",
                  "rendered_text": text, "rendered_sha256": hashlib.sha256(text.encode()).hexdigest(),
                  "displayed": displayed, "concerns": []}
        retained = validate_delivery_packet(packet)
        displayed[0]["routing_binding"]["route"]["target_fingerprint"] = "d" * 64
        self.assertEqual(retained["displayed"][0]["routing_binding"], binding())
        # Invalid optional routing metadata loses learning, not historical delivery.
        packet["displayed"][0]["routing_binding"]["db_id"] = WITNESS
        clean = validate_delivery_packet(packet)
        self.assertNotIn("routing_binding", clean["displayed"][0])
        self.assertEqual(clean["rendered_text"], text)

    def test_native_observation_reaches_only_selected_card_not_reader_prompt(self):
        import hook_recall
        import reader_worker
        card = {"id": TARGET, "summary": "Check the snapshot's journal coverage.",
                "source": "codex:example", "kind": "semantic", "status": "active",
                "fingerprint": "f" * 64}
        path = [{"previous": PREVIOUS, "target": TARGET, "from": PREVIOUS, "to": TARGET,
                 "kind": "associative", "anchor": None}]
        native = {"schema": 1, "learning": "disabled", "cards": [{"node_id": TARGET,
                  "card_sha256": "d" * 64, "lane": "primary", "graph_path": path,
                  "routing_binding": binding()}]}
        observation = hook_recall._observed_cards({"observation": native}, [card], reader=True)
        self.assertEqual(observation["cards"][0]["routing_binding"], binding())

        class Runtime:
            def select(self, dialogue, cards):
                self.cards = cards
                return {"reason": "selected", "selected_ids": [TARGET], "provider_attempt": True}

        runtime = Runtime()
        response = {"outcome": "ok", "cards": [card], "observation": observation, "db_id": DB}
        with patch("hook_recall.collect_reader", return_value=response):
            result = reader_worker._execute({"service_config": "unused", "project_root": "unused",
                                            "reader_model": "gpt-6.1-sol", "librarian_effort": "medium"},
                                             "snapshot", runtime, lambda: True, set())
        self.assertEqual(result["cards"][0]["routing_binding"], binding())
        self.assertNotIn("routing_binding", runtime.cards[0])
        self.assertNotIn("routing_binding", card)
        # A valid binding for another incoming edge is not this observation.
        wrong = native["cards"][0]["routing_binding"]["route"]
        wrong["previous"] = wrong["from"] = WITNESS
        ordinary = hook_recall._observed_cards({"observation": native}, [card], reader=True)
        self.assertNotIn("routing_binding", ordinary["cards"][0])


if __name__ == "__main__":
    unittest.main()


class RoutingRecordingTests(unittest.TestCase):
    """No provider: complete source-bound delivery -> note, including reversal."""

    def prepared(self, *, overlap=None, route=True, conditional=False):
        import recording_contract as c
        from test_recording_contract import memory_marker, observation, statement
        from test_turn_observer import refresh_delivery_packet
        marker = memory_marker()
        card = marker['packet']['displayed'][0]
        card.update(db_id=DB, node_id=TARGET)
        if route:
            if conditional:
                card.update(entry_kind='conditional', conditional_binding=binding())
            else:
                card['routing_binding'] = binding()
        refresh_delivery_packet(marker['packet'])
        source = observation([statement('The snapshot omitted pending writes.'), marker,
                              statement('The readback lost the pending write.', ordinal=3)])
        prompt, context = c.prepare(source, overlap)
        self.assertIsInstance(prompt, str, context)
        return source, prompt, context

    def answer(self, **changes):
        return {'proposal': {'kind': 'lesson', 'summary': 'A snapshot can omit pending writes.',
                'body': 'Keep the journal if it holds writes excluded from the snapshot.',
                'evidence_ids': ['e002', 'e003'], 'associate_with': None,
                'routing_judgment': {'target': 'shown001', 'sign': 'weaken',
                'conditions': 'The snapshot omits pending writes.',
                'rationale': 'The shown advice discarded the pending write.', 'corrects': None,
                **changes}}}

    def prior(self, **changes):
        return {'id': WITNESS, 'kind': 'semantic', 'summary': 'Earlier routing opinion.',
                'routing_witness': routing.decode_witness(node(body(**changes)), expected_db_id=DB)}

    def test_conditional_feedback_and_correction_preserve_origin_and_exact_sources(self):
        import recording_contract as c
        import recording_jobs as jobs
        old = self.prior(sign='boost', entry_kind='conditional')
        _, prompt, context = self.prepared(conditional=True, overlap=[old])
        self.assertIn('"entry_kind":"conditional"', prompt)
        checked = c.validate_answer(self.answer(corrects='overlap001'), context)['proposal']
        self.assertEqual(checked.routing_judgment.target.entry_kind, 'conditional')
        value, reason = jobs._routing_payload(checked, context, db_id=DB, session='s', turn='t')
        self.assertEqual(reason, 'prepared')
        witness = json.loads(value['body'])
        self.assertEqual(witness['entry_kind'], 'conditional')
        self.assertEqual(witness['corrects'], {k:old['routing_witness'][k] for k in ('node_id','body_sha256')})
        self.assertIsNone(jobs._routing_payload(checked, context, db_id=WITNESS, session='s', turn='t')[0])
        answer = self.answer()
        answer['proposal']['evidence_ids'] = ['e003']
        self.assertEqual(c.validate_answer(answer, context)['proposal'].routing_reason, 'routing_evidence')
        answer['proposal']['evidence_ids'] = ['e002']
        with self.assertRaisesRegex(ValueError, 'delivery_source_evidence_missing'):
            c.validate_answer(answer, context)

    def test_dynamic_contract_only_for_retained_bound_delivery(self):
        import recording_contract as c
        from test_recording_contract import proposal
        source, prompt, context = self.prepared()
        self.assertTrue(context.routing_enabled)
        self.assertIn('route_bound', prompt)
        packet = json.loads(prompt[len(c.PROMPT_PREFIX):])
        # The native ID was genuinely visible in final-display JSON. Preserve
        # that historical text; the separate assessment target stays opaque.
        self.assertIn(TARGET, next(e["text"] for e in packet["evidence"] if e["kind"] == "memory_delivery"))
        self.assertNotIn(TARGET, json.dumps(packet["delivered_cards"]))
        self.assertNotIn('edge_fingerprint', prompt)
        self.assertIn('routing_judgment', json.dumps(c.schema_for(context)))
        self.assertEqual(context.authored_bytes, len(c.instructions_for(context).encode())
                         + len(c._encoded(c.schema_for(context))) + len(prompt.encode()))
        self.assertLessEqual(len(c.instructions_for(context).encode())
                            + len(c._encoded(c.schema_for(context))) + len(c.PROMPT_PREFIX.encode()),
                            c.MAX_STATIC_BYTES)
        normalized = c.validate_answer(self.answer(), context)
        self.assertEqual(normalized["maintenance"], [])
        checked = normalized["proposal"]
        self.assertEqual(checked.routing_judgment.sign, 'weaken')
        source['evidence'][1]['packet']['displayed'][0]['routing_binding']['route']['edge_fingerprint'] = 'd'*64
        self.assertEqual(json.loads(checked.routing_judgment.target.routing_binding_json), binding())
        _, _, plain = self.prepared(route=False)
        self.assertFalse(plain.routing_enabled)
        self.assertEqual(c.instructions_for(plain), c.BASE_INSTRUCTIONS)
        self.assertEqual(c.schema_for(plain), c.OUTPUT_SCHEMA)
        self.assertIsNone(c.validate_answer(proposal(plain), plain)["proposal"].routing_judgment)

    def test_optional_routing_never_displaces_source_evidence(self):
        import recording_contract as c
        source, _, _ = self.prepared()
        ordinary = copy.deepcopy(source)
        ordinary['evidence'][1]['packet']['displayed'][0].pop('routing_binding')
        plain_prompt, plain = c.prepare(ordinary)
        with patch.object(c, 'MAX_AUTHORED_BYTES', plain.authored_bytes):
            actual, context = c.prepare(source)
        self.assertEqual(actual, plain_prompt)
        self.assertFalse(context.routing_enabled)
        self.assertEqual(context.bindings, plain.bindings)
        self.assertEqual(context.authored_bytes, plain.authored_bytes)

    def test_explicit_correction_binds_body_and_exact_oriented_meaning(self):
        import recording_contract as c
        import recording_jobs as jobs
        old = self.prior(sign='boost')
        _, prompt, context = self.prepared(overlap=[old])
        self.assertIn('prior_judgment', prompt)
        self.assertNotIn(WITNESS, prompt)
        answer = self.answer(corrects='overlap001')
        normalized = c.validate_answer(answer, context)
        self.assertEqual(normalized["maintenance"], [])
        checked = normalized["proposal"]
        self.assertEqual(checked.routing_judgment.corrects.native_id, WITNESS)
        payload, reason = jobs._routing_payload(checked, context, db_id=DB, session='session2', turn='turn2')
        self.assertEqual(reason, 'prepared')
        witness = json.loads(payload['body'])
        self.assertEqual(witness['corrects'], {k: old['routing_witness'][k] for k in ('node_id','body_sha256')})
        self.assertEqual(witness['binding'], binding())
        self.assertEqual(witness['note'], checked.body)
        self.assertEqual(witness['source_turn'], {'session':'session2','turn':'turn2'})
        # Same target, altered edge meaning does not become a valid correction.
        changed = binding(); changed['route']['edge_fingerprint'] = 'e'*64
        _, prompt, different = self.prepared(overlap=[self.prior(binding=changed)])
        self.assertNotIn('prior_judgment', prompt)
        normalized = c.validate_answer(answer, different)
        self.assertEqual(normalized["maintenance"], [])
        rejected = normalized["proposal"]
        self.assertIsNone(rejected.routing_judgment)
        self.assertEqual(rejected.routing_reason, 'routing_correction')
        self.assertEqual(rejected.body, checked.body)

    def test_unknown_or_invalid_optional_judgment_keeps_ordinary_lesson(self):
        import recording_contract as c
        _, _, context = self.prepared()
        for change in ({'target':'overlap001'}, {'sign':1}, {'rationale':''},
                       {'conditions':'é'*257}, {'corrects':'missing'}):
            with self.subTest(change=change):
                answer = self.answer(**change)
                normalized = c.validate_answer(answer, context)
                self.assertEqual(normalized["maintenance"], [])
                checked = normalized["proposal"]
                self.assertIsNone(checked.routing_judgment)
                self.assertTrue(checked.routing_reason.startswith('routing_'))
                self.assertEqual(checked.body, answer['proposal']['body'])
        _, _, plain = self.prepared(route=False)
        self.assertIsNone(c.validate_answer(self.answer(), plain)["proposal"].routing_judgment)
        missing = self.answer(); missing['proposal']['evidence_ids'] = ['e003']
        self.assertEqual(c.validate_answer(missing, context)["proposal"].routing_reason, 'routing_evidence')

    def test_writer_rechecks_context_and_database_without_new_native_calls(self):
        from dataclasses import replace
        import recording_contract as c
        import recording_jobs as jobs
        _, _, context = self.prepared()
        normalized = c.validate_answer(self.answer(), context)
        self.assertEqual(normalized["maintenance"], [])
        checked = normalized["proposal"]
        value, reason = jobs._routing_payload(checked, context, db_id=WITNESS, session='s', turn='t')
        self.assertIsNone(value)
        self.assertEqual(reason, 'routing_invalid')
        forged = replace(checked, routing_judgment=replace(checked.routing_judgment,
                         target=replace(checked.routing_judgment.target, summary='Unseen meaning')))
        self.assertIsNone(jobs._routing_payload(forged, context, db_id=DB, session='s', turn='t')[0])
        forged = replace(checked, evidence=(replace(checked.evidence[0], source_field='invented'),
                                          checked.evidence[1]))
        self.assertIsNone(jobs._routing_payload(forged, context, db_id=DB, session='s', turn='t')[0])

    def test_invalid_delivery_metadata_cannot_reappear_in_recording_context(self):
        import recording_contract as c
        source, _, _ = self.prepared()
        source['evidence'][1]['packet']['displayed'][0]['routing_binding']['db_id'] = WITNESS
        prompt, context = c.prepare(source)
        self.assertIsInstance(prompt, str)
        self.assertFalse(context.routing_enabled)
        self.assertIsNone(context.association_bindings[0].routing_binding_json)

class HintArrayBudgetTests(unittest.TestCase):
    def test_exact_whole_wire_boundary_and_no_prefix(self):
        value = routing.hint(binding(),"weaken")
        values = [copy.deepcopy(value) for _ in range(34)]
        self.assertEqual(len(routing.encoded(value)),476)
        self.assertEqual(len(routing.normalize_hints(values)),34)
        self.assertGreater(len(routing.encoded(values+[value])),routing.MAX_HINT_BYTES)
        original = copy.deepcopy(values)
        with self.assertRaises(ValueError):routing.normalize_hints(values+[value])
        self.assertEqual(values,original)
        raw_bytes = len(routing.encoded(values))
        with patch.object(routing,"MAX_HINT_BYTES",raw_bytes):
            self.assertEqual(routing.normalize_hints(values),values)
        with patch.object(routing,"MAX_HINT_BYTES",raw_bytes-1),self.assertRaises(ValueError):
            routing.normalize_hints(values)

    def test_invalid_late_item_rejects_whole_array_and_copies_bindings(self):
        good = routing.hint(binding(),"boost")
        for bad in ({**good,"extra":1},{**good,"sign":"neutral"},None):
            with self.subTest(bad=bad),self.assertRaises(ValueError):
                routing.normalize_hints([good,bad])
        with self.assertRaises(ValueError):routing.normalize_hints((good,))
        normalized = routing.normalize_hints([good])
        normalized[0]["route"]["edge_fingerprint"] = "d"*64
        self.assertEqual(good["route"]["edge_fingerprint"],"c"*64)
        self.assertEqual(routing.normalize_hints([]),[])

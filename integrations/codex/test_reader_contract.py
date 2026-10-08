"""Offline variable-card selector contract; historical evaluation stays frozen."""
import unittest
import json
import reader_contract as contract


class ReaderContractTests(unittest.TestCase):
    def test_any_supplied_unique_subset_preserves_source_order(self):
        ids = [str(i) for i in range(20)]
        self.assertEqual(contract.validate_answer({"selected_ids": ids[::-1], "concerns": []}, {"ids": ids})["selected_ids"], ids)
        self.assertEqual(contract.validate_answer({"selected_ids": ["7", "2", "5"], "concerns": []}, {"ids": ids})["selected_ids"], ["2", "5", "7"])
        self.assertEqual(contract.validate_answer({"selected_ids": [], "concerns": []}, {"ids": ids})["selected_ids"], [])
        self.assertNotIn("maxItems", contract.OUTPUT_SCHEMA["properties"]["selected_ids"])
        self.assertNotIn("two-card", contract.BASE_INSTRUCTIONS)

    def test_invalid_or_foreign_ids_fail_closed_without_typeerror(self):
        for answer in ({"selected_ids": ["a", "a"]}, {"selected_ids": ["foreign"]},
                       {"selected_ids": [{}]}, {"selected_ids": "a"},
                       {"selected_ids": ["a"], "extra": True}, None):
            with self.subTest(answer=answer), self.assertRaises(ValueError):
                contract.validate_answer(answer, {"ids": ["a", "b", "c"]})

    def test_discovery_and_prompt_budgets_remain_bounded(self):
        cards = [{"id": str(i), "summary": "A useful prior warning"} for i in range(8)]
        prompt, ids = contract.prepare([{"role": "user", "text": "Design this repository feature"}], cards)
        self.assertEqual(ids["ids"], [str(i) for i in range(8)])
        self.assertLessEqual(len(prompt.encode()), contract.MAX_PROMPT_BYTES)
        _, more = contract.prepare([{"role": "user", "text": "Design this feature"}],
                                   cards + [{"id": "extra", "summary": "More"}])
        self.assertEqual(len(more["ids"]), 9)


class EpisodeProjectionTests(unittest.TestCase):
    dialogue = [{"role": "user", "text": "Investigate this repository failure"}]

    def episode(self, **changes):
        card = {"id": "3" * 26, "kind": "episode", "summary": "Historical repair attempt",
                "source": "fixture", "fingerprint": "a" * 64,
                "episode_id": "1" * 26, "edition_id": "3" * 26, "revision": 2,
                "current_edition_id": "3" * 26, "occurred": {"kind": "point", "at": 12},
                "recorded_at": 15, "edition_recorded_at": 20, "thread": "opaque 🐙 label",
                "recording_session": None, "origins": [{"kind": "lexical"}]}
        return {**card, **changes}

    def payload(self, cards, **kwargs):
        prompt, context = contract.prepare(self.dialogue, cards, **kwargs)
        self.assertIsInstance(prompt, str, context)
        return json.loads(prompt.split("PAYLOAD:\n", 1)[1]), prompt, context

    def test_real_collector_card_preserves_complete_episode_projection(self):
        from hook_recall import _card
        candidate = self.episode()
        node = {"id": candidate["id"], "status": "active", "summary": candidate["summary"],
                "summary_truncated": False, "created": candidate["edition_recorded_at"],
                "provenance": {"type": "conversation", "session": "fixture-session", "turn": 1},
                "memory_kind": {"kind": "episode", "episode": {
                    key: candidate[key] for key in ("episode_id", "revision", "occurred", "recorded_at", "thread")}}}
        collector_card = _card(node, candidate["id"], candidate)
        payload, _, context = self.payload([collector_card])
        self.assertEqual(payload["cards"], [{key: value for key, value in collector_card.items()
                                             if key != "status"}])
        self.assertEqual(context["ids"], [candidate["id"]])
        self.assertIn("historical account, not present advice", contract.BASE_INSTRUCTIONS)
        self.assertIn("recall-time observed editorial head, not a fresh head read", contract.BASE_INSTRUCTIONS)

    def test_semantic_controls_have_no_episode_or_scalar_confidence_fields(self):
        basic = {"id": "lesson", "summary": "Scoped reusable lesson"}
        for kind in ({}, {"kind": "semantic"}):
            payload, _, _ = self.payload([{**basic, **kind, "confidence": .99,
                                          "stability": 1, "status": "active"}])
            self.assertEqual(payload["cards"], [{**basic, "kind": "semantic"}])

    def test_linked_historical_scene_keeps_typed_origins_without_teaching_a_lesson(self):
        from test_hook_recall import reference_origin
        edition = "3" * 26
        origins = [reference_origin(edition, episode_anchor=True), reference_origin(edition)]
        origins[0].update(**{"from": edition, "to": origins[0]["anchor"]["identity"]["edition_id"],
                             "edge_kind": "Bridge", "body_anchor": {"start": 2, "end": 9}})
        scene = self.episode(summary="The workshop goose reappeared in the release joke.",
                             current_edition_id="4" * 26, recording_session="late recap 🐙",
                             origins=origins)
        payload, _, _ = self.payload([scene])
        self.assertEqual(payload["cards"], [scene])
        self.assertIsNot(payload["cards"][0]["origins"], origins)
        self.assertEqual(contract.validate_answer({"selected_ids": [edition], "concerns": []},
                         {"ids": [edition]})["selected_ids"], [edition])
        self.assertIn("without teaching a lesson", contract.BASE_INSTRUCTIONS)
        self.assertNotIn("lesson", payload["cards"][0])

    def test_malformed_origins_and_recording_coordinates_fail_closed(self):
        from test_hook_recall import reference_origin
        origin = reference_origin("3" * 26)
        for origins in (None, [], {}, [{"kind": "unknown"}], [{"kind": "lexical", "extra": "x"}],
                        [{"kind": "lexical"}] * 2, [{**origin, "to": "wrong-edition"}],
                        [{**origin, "from": "3" * 26, "to": "3" * 26,
                          "anchor": {"kind": "semantic", "node_id": "3" * 26}}],
                        [{**origin, "anchor": {"kind": "episode", "identity": None}}],
                        [{**origin, "body_anchor": {"start": True, "end": 3}}],
                        [{**origin, "edge_kind": "causes"}]):
            with self.subTest(origins=origins):
                self.assertEqual(contract.prepare(self.dialogue, [self.episode(origins=origins)]),
                                 (None, "invalid_input"))
        for recording_session in (False, {}, [], ""):
            with self.subTest(recording_session=recording_session):
                self.assertEqual(contract.prepare(self.dialogue, [
                    self.episode(recording_session=recording_session)]), (None, "invalid_input"))

    def test_only_origins_and_recording_session_overflow_omit_the_complete_card(self):
        from test_hook_recall import reference_origin
        from types import SimpleNamespace
        episode = self.episode()
        later = {"id": "later", "kind": "semantic", "summary": "Small lesson"}
        _, baseline, _ = self.payload([episode, later])
        episode["recording_session"] = "r" * 512
        episode["origins"] = [reference_origin(episode["id"], anchor_id=f"anchor-{i}")
                              for i in range(12)]
        payload, prompt, context = self.payload([episode, later], budget=SimpleNamespace(
            selector_prompt_bytes=len(baseline.encode()), selector_answer_bytes=4096))
        self.assertEqual(payload["cards"], [later])
        self.assertEqual(context["prompt_omitted_count"], 1)
        self.assertNotIn("origins", prompt)
        self.assertNotIn("recording_session", prompt)
        self.assertNotIn(episode["summary"], prompt)

    def test_occurrence_contexts_preserve_multiple_settings_not_recorder_identity(self):
        refs = [{"namespace": "session", "key": "mac", "label": "Design discussion"},
                {"namespace": "session", "key": "pi", "label": "Experiment 🐙"}]
        episode = self.episode(source="codex:later-recap", occurrence_contexts=refs)
        payload, _, _ = self.payload([episode])
        self.assertEqual(payload["cards"][0], episode)
        self.assertIsNot(payload["cards"][0]["occurrence_contexts"], refs)
        self.assertNotIn("occurrence_contexts", self.payload([self.episode()])[0]["cards"][0])
        for value in (None, [], {}, [{"namespace": "s"}], [{"namespace": "s", "key": "k", "label": None}],
                      [{"namespace": "s", "key": "k", "extra": "x"}],
                      [{"namespace": "s", "key": "x" * 1024}]):
            with self.subTest(value=value):
                self.assertEqual(contract.prepare(self.dialogue, [self.episode(occurrence_contexts=value)]),
                                 (None, "invalid_input"))
        self.assertEqual(contract.prepare(self.dialogue, [{"id": "note", "summary": "A lesson",
                                                          "occurrence_contexts": refs}]),
                         (None, "invalid_input"))

    def test_missing_malformed_and_orphan_episode_data_fail_closed(self):
        episode = self.episode()
        malformed = [{key: value for key, value in episode.items() if key != missing}
                     for missing in ("kind", *contract.EPISODE_FIELDS)]
        malformed += [self.episode(**change) for change in (
            {"kind": "semantic"}, {"kind": "future"}, {"kind": None},
            {"edition_id": "different"}, {"episode_id": None}, {"revision": True},
            {"recorded_at": "15"}, {"edition_recorded_at": False}, {"thread": {}},
            {"occurred": None}, {"occurred": {"kind": "unknown", "at": 12}},
            {"occurred": {"kind": "point"}}, {"occurred": {"kind": "point", "at": True}},
            {"occurred": {"kind": "range", "start": 1}}, {"occurred": {"kind": []}},
        )]
        for card in malformed:
            with self.subTest(card=card):
                self.assertEqual(contract.prepare(self.dialogue, [card]), (None, "invalid_input"))
        for kind in ({}, {"kind": "semantic"}):
            for field in contract.EPISODE_FIELDS:
                with self.subTest(kind=kind, orphan=field):
                    self.assertEqual(contract.prepare(self.dialogue, [
                        {"id": "lesson", "summary": "Scoped lesson", **kind, field: episode[field]}]),
                        (None, "invalid_input"))

    def test_unknown_range_times_and_opaque_threads_remain_exact(self):
        cards = [self.episode(occurred={"kind": "unknown"}, thread=None),
                 self.episode(id="4" * 26, edition_id="4" * 26,
                              current_edition_id="4" * 26,
                              occurred={"kind": "range", "start": 1, "end": 5},
                              recorded_at=4, edition_recorded_at=6)]
        payload, _, context = self.payload(cards)
        self.assertEqual(payload["cards"], cards)
        self.assertEqual(context["ids"], [card["id"] for card in cards])
        self.assertNotIn("machine", payload["cards"][1])
        self.assertNotIn("session", payload["cards"][1])

    def test_episode_metadata_and_summary_are_omitted_together_before_later_card(self):
        from hook_recall import EPISODE_FIELDS
        from types import SimpleNamespace
        episode = self.episode(summary="x" * 650)
        later = {"id": "later", "summary": "Small scoped lesson", "kind": "semantic"}
        # Fit the historical summary without its coordinates, but not the full
        # episode. Coordinates are not optional controls to shed for more cards.
        semantic_twin = {key: value for key, value in episode.items()
                         if key not in EPISODE_FIELDS and key != "kind"}
        _, baseline, _ = self.payload([semantic_twin, later])
        payload, prompt, context = self.payload([episode, later], budget=SimpleNamespace(
            selector_prompt_bytes=len(baseline.encode()), selector_answer_bytes=4096))
        self.assertEqual(payload["cards"], [later])
        self.assertEqual(context["ids"], ["later"])
        self.assertEqual(context["prompt_omitted_count"], 1)
        self.assertNotIn(episode["summary"], prompt)
        self.assertLessEqual(len(prompt.encode()), len(baseline.encode()))

    def test_native_capacity_minima_charge_the_discriminant(self):
        semantic = {"id": "0" * 26, "summary": "x", "source": "x",
                    "fingerprint": "0" * 64, "kind": "semantic"}
        payload, _, _ = self.payload([semantic, self.episode()])
        self.assertEqual(contract.MIN_PROJECTED_CARD_BYTES, len(contract._encode(semantic)))
        self.assertEqual(contract.MIN_VERIFIED_CARD_BYTES,
                         len(contract._encode({**semantic, "status": "active"})))
        self.assertTrue(all(len(contract._encode(card)) >= contract.MIN_PROJECTED_CARD_BYTES
                            for card in payload["cards"]))

    def test_only_added_occurrence_context_causes_whole_card_omission(self):
        from types import SimpleNamespace
        episode = self.episode()
        later = {"id": "later", "kind": "semantic", "summary": "Small lesson"}
        _, baseline, _ = self.payload([episode, later])
        episode["occurrence_contexts"] = [{"namespace": "s", "key": "pi", "label": "x" * 800}]
        payload, prompt, context = self.payload([episode, later], budget=SimpleNamespace(
            selector_prompt_bytes=len(baseline.encode()), selector_answer_bytes=4096))
        self.assertEqual(payload["cards"], [later])
        self.assertEqual(context["prompt_omitted_count"], 1)
        self.assertNotIn(episode["summary"], prompt)
        self.assertNotIn("occurrence_contexts", prompt)


class ConcernSelectionTests(unittest.TestCase):
    def test_variable_cases_dedup_and_selected_endpoint_binding(self):
        ids = [str(i) for i in range(5)]
        cases = [{"kind": "disagreement", "left_id": "0", "right_id": str(i),
                  "caveat": "These scopes differ 🐙", "missing_fact": "Which scope applies?"}
                 for i in range(1, 5)]
        answer = {"selected_ids": ids[::-1], "concerns": cases}
        self.assertEqual(contract.validate_answer(answer, {"ids": ids})["concerns"], cases)
        for bad in (cases + [{**cases[0], "left_id": "1", "right_id": "0"}],
                    [{**cases[0], "right_id": "foreign"}],
                    [{**cases[0], "caveat": "🐙" * 129}]):
            with self.assertRaises(ValueError):
                contract.validate_answer({**answer, "concerns": bad}, {"ids": ids})
        with self.assertRaises(ValueError):
            contract.validate_answer({"selected_ids": ids}, {"ids": ids})

    def test_historical_finding_projection_hides_meaning_hashes(self):
        from test_turn_observer import concern_row
        finding = {"scope": "Earlier environment", "observation": "Historical outcome",
                   "evidence": [{"source_ref": "fixture", "digest": "c" * 64}]}
        row = concern_row(finding=finding)
        cards = [{"id": str(i) * 26, "summary": "Useful note"} for i in (1, 2)]
        prompt, _ = contract.prepare([{"role": "user", "text": "Resolve this repository issue"}],
                                     cards, concern_rows=[row])
        self.assertIn("Historical outcome", prompt)
        self.assertNotIn("a" * 64, prompt)
        self.assertNotIn("c" * 64, prompt)

class EffortAdmissionTests(unittest.TestCase):
    def test_byte_derived_capacity_and_actual_prompt_admission(self):
        from librarian_policy import LibrarianBudget
        dialogue = [{"role": "user", "text": "Investigate " + "🐙" * 600}]
        cards = [{"id": str(i), "summary": ('\\"' * 300 if i < 18 else "short warning"),
                  "source": "fixture", "fingerprint": "a" * 64} for i in range(24)]
        low = LibrarianBudget(effort="low")
        work, _ = contract.plan(dialogue, budget=low)
        self.assertGreater(work["max_nodes"], 8)
        self.assertEqual(work["k"], min(64, work["max_nodes"]))
        _,short_ctx = contract.prepare(work["dialogue"], [{**c,"summary":"short warning"} for c in cards],budget=low)
        self.assertGreater(len(short_ctx["ids"]),8)
        prompt, offered = contract.prepare(work["dialogue"], cards, budget=low)
        self.assertLessEqual(len(prompt.encode()), low.selector_prompt_bytes)
        self.assertGreater(len(offered["ids"]), 0)
        self.assertLess(len(offered["ids"]), len(cards))
        self.assertIn("18", offered["ids"])  # Skip a large card and still fit later short ones.
        self.assertEqual(offered["prompt_omitted_count"], len(cards)-len(offered["ids"]))
        with self.assertRaises(ValueError):
            contract.validate_answer({"selected_ids": [next(i for i in map(str,range(24)) if i not in offered["ids"])], "concerns": []}, offered)

    def test_optional_paths_lose_before_baseline_cards(self):
        from librarian_policy import LibrarianBudget
        cards = [{"id": str(i), "summary": "x"*600, "native": {"graph_path": ["p"*1000]}}
                 for i in range(10)]
        prompt, ctx = contract.prepare([{"role":"user","text":"Analyze this project"}], cards,
                                       budget=LibrarianBudget(effort="low"))
        self.assertEqual(ctx["ids"], [str(i) for i in range(10)])
        self.assertGreater(len(ctx["path_omitted_ids"]), 0)
        self.assertLessEqual(len(prompt.encode()), 8192)

    def test_answer_policy_is_not_prompt_policy(self):
        from librarian_policy import LibrarianBudget
        cases = [{"kind":"disagreement","left_id":"0","right_id":str(i),
                  "caveat":"x"*450,"missing_fact":"y"*200} for i in range(1,5)]
        answer = {"selected_ids":[str(i) for i in range(5)],"concerns":cases}
        with self.assertRaises(ValueError):
            contract.validate_answer(answer, {"ids":answer["selected_ids"],"answer_bytes":LibrarianBudget(effort="low").selector_answer_bytes})
        self.assertEqual(len(contract.validate_answer(answer, {"ids":answer["selected_ids"],"answer_bytes":LibrarianBudget(effort="high").selector_answer_bytes})["concerns"]),4)


class ConditionalEntryPromptTests(unittest.TestCase):
    dialogue = [{"role": "user", "text": "Investigate this repository failure"}]

    def test_conditional_marker_is_optional_and_fingerprints_stay_private(self):
        import json
        cards = [{"id": "b", "summary": "Try the bounded journal", "native": {
            "entry_kind": "conditional", "conditional_binding": {"secret": "native-only"}}}]
        prompt, offered = contract.prepare(self.dialogue, cards)
        payload = json.loads(prompt.split("PAYLOAD:\n", 1)[1])
        self.assertEqual(payload["cards"][0]["entry_kind"], "conditional")
        self.assertNotIn("native-only", prompt)
        self.assertNotIn("graph_path", prompt)
        self.assertEqual(offered["entry_omitted_ids"], [])
        self.assertEqual(offered["path_omitted_ids"], [])

    def test_marker_sheds_without_card_or_path_omission(self):
        from types import SimpleNamespace
        ordinary = [{"id": "b", "summary": "Try the bounded journal"}]
        baseline, _ = contract.prepare(self.dialogue, ordinary)
        cards = [{**ordinary[0], "native": {"entry_kind": "conditional"}}]
        prompt, offered = contract.prepare(self.dialogue, cards, budget=SimpleNamespace(
            selector_prompt_bytes=len(baseline.encode()), selector_answer_bytes=4096))
        self.assertEqual(prompt, baseline)
        self.assertEqual(offered["ids"], ["b"])
        self.assertEqual(offered["entry_omitted_ids"], ["b"])
        self.assertEqual(offered["path_omitted_ids"], [])
        self.assertEqual(offered["prompt_omitted_count"], 0)

    def test_conditional_conflict_never_projects_a_graph_path(self):
        for path in ([{"target": "b"}], "oversized" * 1000, {}):
            prompt, _ = contract.prepare(self.dialogue, [{"id": "b", "summary": "Useful note",
                "native": {"entry_kind": "conditional", "graph_path": path}}])
            self.assertNotIn("graph_path", prompt)
            self.assertIn('"entry_kind":"conditional"', prompt)
        ordinary, offered = contract.prepare(self.dialogue, [{"id": "b", "summary": "Useful note"}])
        self.assertNotIn("entry_kind", ordinary)
        self.assertEqual(offered["entry_omitted_ids"], [])

    def test_unknown_or_orphan_metadata_does_not_salvage_path_or_refuse_card(self):
        baseline, _ = contract.prepare(self.dialogue, [{"id": "b", "summary": "Useful note"}])
        for fields in ({"entry_kind": "future"}, {"entry_kind": None},
                       {"conditional_binding": {"opaque": "native-only"}}):
            for path in ([{"target": "b"}], "x" * 2000):
                prompt, offered = contract.prepare(self.dialogue, [{"id": "b", "summary": "Useful note",
                    "native": {**fields, "graph_path": path}}])
                self.assertEqual(prompt, baseline)
                self.assertEqual(offered["ids"], ["b"])
                self.assertEqual(offered["entry_omitted_ids"], [])
                self.assertEqual(offered["path_omitted_ids"], [])

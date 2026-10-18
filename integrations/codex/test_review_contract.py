"""Pure foreground-effect admission: no owner, provider, or queue."""
from dataclasses import replace
from types import SimpleNamespace
import copy
import unittest

import review_contract as contract

DB = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
A = "01ARZ3NDEKTSV4RRFFQ69G5FAA"
B = "01ARZ3NDEKTSV4RRFFQ69G5FAB"
C = "01ARZ3NDEKTSV4RRFFQ69G5FAC"
LIMITS = dict(destination_db_id=DB, max_bytes=6144,
              max_summary_bytes=512, max_body_bytes=2048)


def target(opaque="overlap001", identifier=A, **extra):
    return SimpleNamespace(opaque_id=opaque, native_id=identifier, db_id=DB,
                           summary="Observed account, not present-day verification.",
                           kind="semantic", origin="overlap", touchstone_json=None,
                           **extra)


class ReviewEffectTests(unittest.TestCase):
    def setUp(self):
        self.targets = [target(), target("shown002", B)]
        self.targets[1].origin = "delivery"
        self.episode = target("overlap003", C)
        self.episode.kind = "episode"
        self.targets.append(self.episode)
        self.effects = [
            {"kind": "touchstone"},
            {"kind": "core_creation", "summary": "A proposed core note", "body": "draft"},
            {"kind": "core_membership", "target": "overlap001", "membership": "promote"},
            {"kind": "core_membership", "target": "overlap001", "membership": "demote"},
            {"kind": "core_content_change", "target": "overlap001", "field": "summary", "proposed_text": "Changed meaning"},
            {"kind": "core_content_change", "target": "overlap001", "field": "body", "proposed_text": ""},
            {"kind": "supersede", "winner": "overlap001", "loser": "shown002"},
            {"kind": "forget", "target": "overlap001"},
            {"kind": "reconcile", "a": "overlap001", "b": "shown002", "resolution": "context-dependent"},
            {"kind": "reconcile", "a": "overlap001", "b": "shown002", "resolution": "unresolved"},
            {"kind": "merge", "mode": "full", "winner": "overlap001", "loser": "shown002"},
            {"kind": "merge", "mode": "keep", "a": "overlap001", "b": "shown002"},
            {"kind": "episode_correction", "target": "overlap003", "summary": "Corrected account", "body": "", "reason": "New evidence"},
        ]

    def admit(self, effect, targets=None, **limits):
        return contract.validate_effect(effect, self.targets if targets is None else targets,
                                        **{**LIMITS, **limits})

    def test_closed_union_roundtrips_and_binds_exact_reference_editions(self):
        seen = set()
        for effect in self.effects:
            with self.subTest(effect=effect):
                checked = self.admit(effect)
                seen.add(checked.kind)
                self.assertEqual(checked.to_model(), effect)
                self.assertEqual(contract.check_effect(checked, self.targets, **LIMITS), checked)
                offered = {item.opaque_id: item for item in self.targets}
                for ref in checked.to_handoff()["targets"]:
                    role = ref["role"]
                    self.assertEqual(ref["db_id"], DB)
                    self.assertEqual(ref["id"], offered[effect[role]].native_id)
                    self.assertNotIn("expected_snapshot_sha256", ref)
        self.assertEqual(seen, set(contract.EFFECT_KINDS))
        self.assertEqual(self.admit(self.effects[-1]).to_handoff()["targets"][0]["id"], C)

    def test_not_native_requests_tokens_or_operational_actions(self):
        for effect in self.effects:
            for key in ("db", "expected_db_id", "expected_tags", "action", "approved", "token"):
                with self.subTest(kind=effect["kind"], key=key), self.assertRaises(ValueError):
                    self.admit({**effect, key: "arbitrary"})
        for kind in ("release", "resume", "snapshot_create", "decay", "prune", "migrate", "feedback", "save"):
            with self.assertRaises(ValueError):
                self.admit({"kind": kind})
        schema = contract.effect_schema(max_summary_bytes=512, max_body_bytes=2048)
        self.assertTrue(all(branch["additionalProperties"] is False for branch in schema["oneOf"]))

    def test_targets_must_be_offered_same_owner_and_distinct(self):
        for opaque in (A, "unseen", True, None):
            with self.assertRaises(ValueError):
                self.admit({"kind": "forget", "target": opaque})
        for change in ({"db_id": C}, {"native_id": "not-an-id"}, {"origin": "read_only"}, {"kind": "episode"}):
            altered = copy.deepcopy(self.targets)
            vars(altered[0]).update(change)
            with self.assertRaises(ValueError):
                self.admit({"kind": "forget", "target": "overlap001"}, altered)
        altered = copy.deepcopy(self.targets)
        altered[1].native_id = A
        with self.assertRaises(ValueError):
            self.admit({"kind": "supersede", "winner": "overlap001", "loser": "shown002"}, altered)
        with self.assertRaises(ValueError):
            self.admit({"kind": "touchstone"}, [self.targets[0], self.targets[0]])

    def test_known_touchstone_immutability_is_not_overridden(self):
        self.targets[0].touchstone_json = "{}"
        for effect in (self.effects[2], self.effects[4], self.effects[10]):
            with self.assertRaises(ValueError):
                self.admit(effect)
        # Supersession, forgetting the annotation, and declining a merge do not
        # rewrite its meaning; foreground still needs authority/native checks.
        for effect in (self.effects[6], self.effects[7], self.effects[11]):
            self.admit(effect)

    def test_wrong_variant_and_fields_never_degrade_into_another_operation(self):
        cases = [
            {"kind": "core_membership", "target": "overlap001", "membership": "cleanup"},
            {"kind": "core_content_change", "target": "overlap001", "field": "tags", "proposed_text": "x"},
            {"kind": "merge", "mode": "keep", "winner": "overlap001", "loser": "shown002"},
            {"kind": "reconcile", "a": "overlap001", "b": "shown002", "resolution": "supersede"},
            {"kind": "episode_correction", "target": "overlap001", "summary": "x", "body": "", "reason": "x"},
        ]
        for effect in cases:
            with self.assertRaises(ValueError):
                self.admit(effect)
        for effect in self.effects:
            for field in tuple(effect):
                broken = {key: value for key, value in effect.items() if key != field}
                with self.assertRaises(ValueError):
                    self.admit(broken)

    def test_authored_byte_limits_and_handoff_overhead_are_charged(self):
        for text in ("", " \n", None, True, "é" * 257):
            with self.assertRaises(ValueError):
                self.admit({"kind": "core_creation", "summary": text, "body": ""})
        with self.assertRaises(ValueError):
            self.admit({"kind": "core_content_change", "target": "overlap001", "field": "body", "proposed_text": "x" * 2049})
        with self.assertRaises(ValueError):
            self.admit({"kind": "forget", "target": "overlap001"}, max_bytes=80, max_summary_bytes=32, max_body_bytes=32)
        for bad in (0, -1, True):
            with self.assertRaises(ValueError):
                self.admit({"kind": "touchstone"}, max_bytes=bad)

    def test_injected_typed_effect_is_rechecked_against_frozen_bindings(self):
        checked = self.admit({"kind": "forget", "target": "overlap001"})
        changed = replace(checked, references=(replace(checked.references[0], id=B),))
        with self.assertRaises(ValueError):
            contract.check_effect(changed, self.targets, **LIMITS)
        self.targets[0].summary = "A changed observed target"
        with self.assertRaises(ValueError):
            contract.check_effect(checked, self.targets, **LIMITS)


if __name__ == "__main__":
    unittest.main()

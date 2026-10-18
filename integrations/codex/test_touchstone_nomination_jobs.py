"""Real closed-turn recorder state, stub assessor/owners, local durable handoff.

No provider, live owner, hook installation or lifecycle reload is contacted.
"""
from dataclasses import replace
import copy
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import recording_contract as contract
import recording_jobs as jobs
import reader_worker as reader
import hooks
import touchstone_handoff as handoff
from fixture_recording import RecordingFixture, SESSION, DB_ID
from store_registry import resolve


SUMMARY = "Possible meaning in revising a shared decision."
BODY = "A tentative connection for the foreground to consider, not authored significance."


class NominationRuntime:
    def __init__(self, fixture, *, after=None, transform=None, effect=None):
        self.fixture, self.after, self.transform, self.effect = fixture, after, transform, effect
        self.calls = []

    def assess(self, observation, overlap, *, timeout, **options):
        self.fixture.assertEqual(self.fixture.state()["jobs"][0]["phase"], "assessing")
        prompt, context = contract.prepare(observation, overlap, **options)
        self.fixture.assertIsNotNone(prompt, context)
        self.calls.append((observation, context, timeout))
        wire = {"kind":"touchstone_nomination", "summary":SUMMARY, "body":BODY,
                "evidence_ids":[context.bindings[0].evidence_id],
                "related_memory_ids":[], "destination":"shared-work"}
        if self.effect is not None:
            wire.update(kind="review_nomination", effect=self.effect)
        answer = contract.validate_answer({"proposal":wire}, context)
        result = {"reason": "proposed", "provider_attempt": True,
                  "usage": {"input_tokens": 20, "output_tokens": 5}, **answer}
        if self.transform:
            result = self.transform(result)
        if self.after:
            self.after()
        return result


class NominationJobsTests(unittest.TestCase, RecordingFixture):
    def setUp(self):
        RecordingFixture.__init__(self, self)
        self.root = self.root.resolve()
        self.registry = self.root / "stores.json"
        self.service.write_text(json.dumps({"mode": "connect", "database_name": "user",
            "database_path": "/srv/mneme/shared.db", "url": "http://127.0.0.1:18766/"}))
        self.entry = {"name": "shared-work", "service_config": str(self.service),
            "db_alias": "user", "database_path": "/srv/mneme/shared.db", "db_id": DB_ID,
            "purpose": "Generally applicable knowledge and agent continuity.",
            "read": {"enabled": True}, "record": {"enabled": True, "intent": "cross-workspace",
                "kinds": ["lesson", "episode", "possibility"], "evidence_kinds": list(contract.KINDS),
                "allow_associations": True}, "maintenance": {"enabled": False}}
        self.write_registry()
        self.config.pop("service_config")
        self.config.update(schema="mneme.codex-hooks.config.v12", memory_scope="registry",
                           project_root=self.root, state_dir=self.root / "state",
                           memory_mode="async", ttl_seconds=600)
        self.refresh()
        self.identity.side_effect = lambda *args, **kwargs: {"outcome": "ok", "db_id": DB_ID,
            "native_work": {"decoded_bytes": kwargs["spent_native_bytes"] + 100}}
        self.overlap.side_effect = lambda *args, **kwargs: {"outcome": "empty", "db_id": DB_ID,
            "cards": [], "native_work": {"decoded_bytes": kwargs["spent_native_bytes"] + 100}}
        self.native = self._cleanup.enter_context(patch.object(jobs, "_native_write",
            side_effect=AssertionError("nomination must not acquire a native writer")))
        self.maintenance = self._cleanup.enter_context(patch.object(jobs, "_native_maintenance",
            side_effect=AssertionError("nomination must not run maintenance")))

    def write_registry(self):
        self.registry.write_text(json.dumps({"schema": "mneme.stores.v1", "stores": [self.entry]}))

    def refresh(self):
        self.config["frozen_selection"] = resolve(self.root, registry_path=self.registry).to_dict()

    def outbox(self):
        return json.loads((self.config["state_dir"] / "touchstone-handoff" / "state.json").read_text())

    def run_one(self, runtime=None, **callbacks):
        runtime = runtime or NominationRuntime(self)
        worked = jobs.step(self.config, SESSION, runtime,
            callbacks.get("reserve", self.reserve), callbacks.get("account", self.account))
        self.native.assert_not_called()
        self.maintenance.assert_not_called()
        return worked, runtime

    def receipt(self):
        return self.state()["receipts"][-1]

    def frozen(self):
        self.closed()
        with patch.object(jobs, "_publish_nomination") as publish:
            _, runtime = self.run_one()
        publish.assert_called_once()
        self.assertEqual(self.state()["jobs"][0]["phase"], "nomination_frozen")
        return runtime

    def test_closed_turn_persists_before_enqueue_one_assessment_no_native_write(self):
        self.closed()
        original = handoff.enqueue

        def enqueue(config, envelope):
            persisted = self.state()["jobs"][0]
            self.assertEqual(persisted["phase"], "nomination_frozen")
            self.assertEqual(persisted["nomination_envelope"], envelope)
            self.assertFalse(set(persisted) & {"payload", "maintenance", "save_outcome"})
            self.assertLessEqual(len(jobs.encoded(persisted)), jobs.MAX_JOB_BYTES)
            return original(config, envelope)

        with patch.object(handoff, "enqueue", side_effect=enqueue):
            _, runtime = self.run_one()
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.reservations), 1)
        self.assertEqual(len(self.accounts), 1)
        self.assertEqual(self.identity.call_count, 1)
        self.assertEqual(self.overlap.call_count, 1)
        self.assertEqual(self.state()["jobs"], [])
        receipt = self.receipt()
        self.assertEqual((receipt["outcome"], receipt["reason"]), ("deferred", "foreground_review_pending"))
        self.assertEqual(receipt["nomination"]["outcome"], "queued")
        self.assertEqual(receipt["nomination"]["status"], "pending")
        self.assertEqual(receipt["target"]["db_id"], DB_ID)
        self.assertEqual(self.state()["counts"]["verified"], 0)
        exported = jobs.export_receipts(self.config, [SESSION])
        for text in (SUMMARY, BODY, "nomination_envelope", '"native"', '"proposal"'):
            self.assertNotIn(text, json.dumps(exported))
        self.assertIn(BODY, json.dumps(self.outbox()))

    def test_real_next_prompt_hook_offers_once_without_memory_delivery_or_native_save(self):
        hook_config = self.root / "hook-config.json"
        hook_config.write_text("{}")
        self.config["_config_path"] = hook_config
        self.closed()
        self.run_one()
        identifier = self.receipt()["nomination"]["id"]
        prompt = "Review the next design change with me."
        self.source_start("turn-2", prompt=prompt)
        event = {**self.event("turn-2", prompt=prompt), "cwd": str(self.root),
                 "hook_event_name": "UserPromptSubmit"}
        # Use actual hook admission, actual dynamically loaded reader.notice,
        # actual recorder notice, and actual outbox consume. Only detached
        # launch is replaced; this test must never start a process or provider.
        with patch.object(hooks, "_wake_async") as launch, \
             patch.object(jobs, "attach_delivery") as attach:
            first = hooks.handle_event(event, self.config)
            second = hooks.handle_event(event, self.config)
        self.assertEqual(launch.call_count, 2)
        attach.assert_not_called()
        context = first["hookSpecificOutput"]["additionalContext"]
        self.assertIn(SUMMARY, context)
        self.assertIn(BODY, context)
        self.assertIn(identifier, context)
        self.assertIn("not read, approved or saved", context)
        self.assertNotIn(SUMMARY, json.dumps(second))
        self.assertLessEqual(len(context.encode()), hooks.MAX_CONTEXT_BYTES)
        entry = self.outbox()["entries"][0]
        self.assertEqual(entry["status"], "emitted")
        self.assertNotIn("envelope", entry)
        self.assertEqual(self.outbox()["emitted"], 1)
        ok, state = reader._state(self.config, SESSION, lambda value: (copy.deepcopy(value), False), create=False)
        self.assertTrue(ok)
        self.assertEqual(state["active"]["turn"], "turn-2")
        self.assertFalse(state["active"]["closed"])
        self.assertIsNone(state["ready"])
        self.native.assert_not_called()
        self.maintenance.assert_not_called()

    def test_crash_after_enqueue_exact_replay_without_new_provider_or_owner_work(self):
        self.closed()
        original = jobs._write

        def fail_terminal_once(path, data):
            if data["receipts"] and "nomination" in data["receipts"][-1]:
                raise OSError("synthetic receipt persistence failure after durable enqueue")
            original(path, data)

        with patch.object(jobs, "_write", side_effect=fail_terminal_once):
            _, runtime = self.run_one()
        frozen = copy.deepcopy(self.state()["jobs"][0])
        self.assertEqual(frozen["phase"], "nomination_frozen")
        before = copy.deepcopy(self.outbox())
        self.run_one(runtime)
        self.assertEqual(self.outbox(), before)
        self.assertEqual(self.receipt()["nomination"], {
            "outcome": "replay", "id": frozen["nomination_envelope"]["id"], "status": "pending"})
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.reservations), 1)
        self.assertEqual(len(self.accounts), 1)
        self.assertEqual(self.identity.call_count, 1)

    def test_real_reader_accounting_is_charged_once_and_not_reset_by_replay(self):
        self.closed()
        reader.notice(self.config, self.event(), False)
        reader.close_turn(self.config, SESSION, "turn-1")
        reserve = lambda key: reader._recording_reserve(self.config, SESSION, key)
        account = lambda key, result: reader._recording_account(self.config, SESSION, key, result)
        with patch.object(jobs, "_publish_nomination"):
            _, runtime = self.run_one(reserve=reserve, account=account)
        self.run_one(runtime, reserve=reserve, account=account)
        ok, accounting = reader._state(self.config, SESSION, lambda data: (copy.deepcopy(data), False), create=False)
        self.assertTrue(ok)
        self.assertEqual((accounting["attempts"], accounting["input_tokens"], accounting["output_tokens"]), (1, 20, 5))
        self.assertFalse(accounting["unknown_usage"])
        self.assertIsNone(accounting["reservation"])
        self.assertEqual(len(runtime.calls), 1)

    def test_interrupt_after_assessment_prevents_handoff_without_refund(self):
        self.closed()
        runtime = NominationRuntime(self, after=lambda: jobs.close_turn(self.config, SESSION, "turn-1", cancel=True))
        with patch.object(handoff, "enqueue") as enqueue:
            self.run_one(runtime)
        enqueue.assert_not_called()
        self.assertEqual(self.receipt()["outcome"], "cancelled")
        self.assertEqual(len(self.accounts), 1)
        self.assertEqual(len(self.reservations), 1)

    def test_interrupt_of_frozen_intent_never_enqueues(self):
        runtime = self.frozen()
        jobs._signal_cancel(self.config, SESSION, "turn-1")
        with patch.object(handoff, "enqueue") as enqueue:
            self.run_one(runtime)
        enqueue.assert_not_called()
        self.assertEqual(self.receipt()["outcome"], "cancelled")
        self.assertEqual(len(runtime.calls), 1)

    def test_config_rebind_after_assessment_never_enqueues(self):
        self.closed()
        def change():
            self.entry["db_id"] = "01ARZ3NDEKTSV4RRFFQ69G5FAW"
            self.write_registry()
        with patch.object(handoff, "enqueue") as enqueue:
            _, runtime = self.run_one(NominationRuntime(self, after=change))
        enqueue.assert_not_called()
        self.assertEqual(self.receipt()["outcome"], "deferred")
        self.assertEqual(len(self.accounts), 1)
        self.refresh()
        self.assertFalse(self.run_one(runtime)[0])

    def test_config_change_after_freeze_never_reinterprets_intent(self):
        runtime = self.frozen()
        self.entry["purpose"] = "Changed capture purpose."
        self.write_registry()
        with patch.object(handoff, "enqueue") as enqueue:
            self.run_one(runtime)
        enqueue.assert_not_called()
        self.assertEqual(self.receipt()["reason"], "config_changed")
        self.assertEqual(len(runtime.calls), 1)

    def test_unknown_usage_keeps_unresolved_paid_intent_without_handoff_or_retry(self):
        self.closed()
        runtime = NominationRuntime(self, transform=lambda result: {**result, "usage": None})
        with patch.object(handoff, "enqueue") as enqueue:
            self.run_one(runtime)
            self.assertFalse(self.run_one(runtime)[0])
        enqueue.assert_not_called()
        state = self.state()
        self.assertTrue(state["usage_unknown"])
        self.assertEqual((state["jobs"][0]["phase"], state["jobs"][0]["reason"]), ("unresolved", "usage_unknown"))
        self.assertEqual(len(runtime.calls), 1)
        self.assertNotIn(BODY, json.dumps(state))

    def test_accounting_failure_never_hands_off_or_retries(self):
        self.closed()
        with patch.object(handoff, "enqueue") as enqueue:
            _, runtime = self.run_one(account=lambda key, result: False)
            self.assertFalse(self.run_one(runtime)[0])
        enqueue.assert_not_called()
        self.assertEqual(self.state()["jobs"][0]["reason"], "accounting_unavailable")
        self.assertTrue(self.state()["usage_unknown"])
        self.assertEqual(len(runtime.calls), 1)

    def test_review_nomination_uses_same_frozen_job_and_never_acquires_native_writer(self):
        self.closed()
        effect = {"kind":"core_creation","summary":"Tentative core wording","body":""}
        worked, runtime = self.run_one(NominationRuntime(self, effect=effect))
        self.assertTrue(worked)
        self.assertEqual(len(runtime.calls), 1)
        queued = self.outbox()["entries"][0]["envelope"]
        self.assertEqual(queued["schema"], handoff.REVIEW_ENVELOPE_SCHEMA)
        self.assertEqual(queued["effect"]["kind"], "core_creation")
        self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.receipt()["nomination"]["outcome"], "queued")

    def test_review_effect_secrets_or_forged_typed_binding_never_queue_or_save(self):
        self.closed()
        effect = {"kind":"core_creation","summary":"Tentative", "body":"bearer " + "a"*32}
        with patch.object(handoff, "enqueue") as enqueue:
            self.run_one(NominationRuntime(self, effect=effect))
        enqueue.assert_not_called()
        self.assertEqual(self.receipt()["reason"], "nomination_refused")

    def test_mixed_and_forged_typed_results_never_fall_through_to_ordinary_save(self):
        self.closed()
        def mixed(result):
            nominee = result["nomination"]
            return {**result, "proposal": contract.Proposal("lesson", SUMMARY, BODY,
                nominee.evidence, destination=nominee.destination)}
        with patch.object(handoff, "enqueue") as enqueue:
            self.run_one(NominationRuntime(self, transform=mixed))
        enqueue.assert_not_called()
        self.assertEqual(self.receipt()["reason"], "nomination_refused")
        self.assertNotIn(BODY, json.dumps(self.receipt()))

    def test_forged_citation_binding_cannot_become_a_handoff(self):
        self.closed()
        def forged(result):
            nominee = result["nomination"]
            citation = replace(nominee.evidence[0], source_ref_json="{}")
            return {**result, "nomination": replace(nominee, evidence=(citation,))}
        with patch.object(handoff, "enqueue") as enqueue:
            self.run_one(NominationRuntime(self, transform=forged))
        enqueue.assert_not_called()
        self.assertEqual(self.receipt()["reason"], "nomination_refused")

    def test_nomination_disguised_as_proposal_is_not_an_ordinary_save(self):
        self.closed()
        def disguised(result):
            nominee = result.pop("nomination")
            return {**result, "proposal": contract.Proposal("touchstone_nomination", SUMMARY, BODY,
                nominee.evidence, destination=nominee.destination)}
        with patch.object(handoff, "enqueue") as enqueue:
            self.run_one(NominationRuntime(self, transform=disguised))
        enqueue.assert_not_called()
        self.assertEqual(self.receipt()["reason"], "nomination_refused")

    def test_mixed_replayed_job_cannot_enter_native_write_or_export_body(self):
        runtime = self.frozen()
        self.mutate(lambda data: data["jobs"][0].update(payload={"summary": SUMMARY, "body": BODY},
            proposal_kind="lesson", citations=[]))
        with patch.object(handoff, "enqueue") as enqueue:
            self.run_one(runtime)
        enqueue.assert_not_called()
        self.assertEqual(self.receipt()["reason"], "nomination_intent_refused")
        self.assertNotIn(BODY, json.dumps(self.receipt()))

    def test_source_rebind_of_frozen_envelope_is_rejected(self):
        runtime = self.frozen()
        self.mutate(lambda data: data["jobs"][0].update(turn_id="turn-other"))
        with patch.object(handoff, "enqueue") as enqueue:
            self.run_one(runtime)
        enqueue.assert_not_called()
        self.assertEqual(self.receipt()["reason"], "nomination_intent_refused")

    def test_frozen_envelope_capacity_refusal_does_not_leave_assessing_intent(self):
        self.closed()
        with patch.object(handoff, "freeze", return_value={"oversized": "x" * jobs.MAX_JOB_BYTES}), \
             patch.object(handoff, "enqueue") as enqueue:
            self.run_one()
        enqueue.assert_not_called()
        self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.receipt()["reason"], "nomination_intent_cap")
        self.assertFalse(self.state()["usage_unknown"])

    def test_outbox_refusal_is_honest_terminal_and_not_a_new_provider_attempt(self):
        runtime = self.frozen()
        with patch.object(handoff, "enqueue", return_value={"outcome": "capacity", "body": BODY}) as enqueue:
            self.run_one(runtime)
            self.assertFalse(self.run_one(runtime)[0])
        self.assertEqual(enqueue.call_count, 1)
        self.assertEqual(self.receipt()["nomination"], {"outcome": "capacity"})
        self.assertEqual(self.receipt()["reason"], "nomination_handoff_capacity")
        self.assertNotIn(BODY, json.dumps(self.receipt()))
        self.assertEqual(len(runtime.calls), 1)

    def test_unavailable_outbox_does_not_claim_delivery_or_verified_save(self):
        runtime = self.frozen()
        with patch.object(handoff, "enqueue", side_effect=OSError("synthetic inaccessible outbox")):
            self.run_one(runtime)
        self.assertEqual(self.receipt()["nomination"], {"outcome": "unavailable"})
        self.assertEqual(self.receipt()["reason"], "nomination_handoff_unavailable")
        self.assertEqual(self.state()["counts"]["verified"], 0)

    def test_replayed_emission_is_not_pending_approval_or_verified_save(self):
        runtime = self.frozen()
        identifier = self.state()["jobs"][0]["nomination_envelope"]["id"]
        with patch.object(handoff, "enqueue", return_value={"outcome": "replay", "id": identifier,
                                                          "status": "emitted", "body": BODY}):
            self.run_one(runtime)
        self.assertEqual(self.receipt()["reason"], "foreground_review_emitted")
        self.assertEqual(self.receipt()["outcome"], "deferred")
        self.assertEqual(self.receipt()["nomination"], {"outcome": "replay", "id": identifier, "status": "emitted"})
        self.assertEqual(self.state()["counts"]["verified"], 0)
        self.assertNotIn(BODY, json.dumps(self.receipt()))

    def test_replayed_suppression_is_not_an_approval_or_foreground_decline(self):
        runtime = self.frozen()
        identifier = self.state()["jobs"][0]["nomination_envelope"]["id"]
        with patch.object(handoff, "enqueue", return_value={"outcome": "replay", "id": identifier,
                                                          "status": "suppressed"}):
            self.run_one(runtime)
        self.assertEqual(self.receipt()["reason"], "nomination_handoff_suppressed")
        self.assertEqual(self.receipt()["outcome"], "deferred")
        self.assertEqual(self.state()["counts"]["verified"], 0)

    def test_forged_unresolved_nomination_cannot_export_candidate_through_partial_receipt(self):
        runtime = self.frozen()
        self.mutate(lambda data: data["jobs"][0].update(phase="write_intent", active_branch="save",
            maintenance=[{"status": "pending", "target": "opaque"}],
            save_outcome={"status": "pending", "body": BODY}))
        self.run_one(runtime)
        exported = jobs.export_receipts(self.config, [SESSION])
        self.assertNotIn(BODY, json.dumps(exported))
        self.assertNotIn("maintenance", exported["receipts"][-1])

    def test_unavailable_owner_does_not_get_a_nomination_assessor(self):
        self.identity.side_effect = lambda *args, **kwargs: {"outcome": "unavailable",
            "native_work": {"decoded_bytes": kwargs["spent_native_bytes"] + 100}}
        self.closed()
        with patch.object(handoff, "enqueue") as enqueue:
            _, runtime = self.run_one()
        enqueue.assert_not_called()
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.receipt()["reason"], "no_available_destination")

    def test_unclosed_source_never_assessed(self):
        self.begin()
        jobs.close_turn(self.config, SESSION, "turn-1")
        with patch.object(handoff, "enqueue") as enqueue:
            _, runtime = self.run_one()
        enqueue.assert_not_called()
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.reservations, [])


class NominationNativeDenialTests(unittest.TestCase):
    def test_direct_native_entrypoints_reject_nomination_markers_before_config_or_io(self):
        for marker in ({"nomination_envelope": {}}, {"nomination_outcome": {}},
                       {"phase": "nomination_frozen"}, {"proposal_kind": "touchstone_nomination"},
                       {"proposal_kind": "touchstone"}, {"proposal_kind": "review_nomination"}):
            with self.subTest(marker=marker):
                with self.assertRaisesRegex(ValueError, "nomination_is_not_native_write_authority"):
                    jobs._native_write({}, marker, 1)
                with self.assertRaisesRegex(ValueError, "nomination_is_not_native_write_authority"):
                    jobs._native_maintenance({}, marker, {}, 1)


if __name__ == "__main__":
    unittest.main()

"""Disposable recorder authority/receipt checks; no models or native stores."""
import copy
from dataclasses import replace
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import recording_contract as contract
import recording_jobs as jobs
from fixture_recording import DB_ID, OTHER_DB, SESSION, RecordingFixture
from target_policy import GLOBAL_PREFERENCE_NAMESPACE, GLOBAL_PREFERENCE_TAG
from turn_observer import observe_source_turn


class PreferenceRuntime:
    def __init__(self, destination="global_preference"):
        self.destination = destination
        self.calls = 0

    def assess(self, observation, overlap, *, timeout, **options):
        self.calls += 1
        _, context = contract.prepare(observation, overlap, **options)
        evidence = next(item for item in context.bindings if item.kind == "user_statement")
        result = contract.validate_answer({"proposal": {
            "kind": "lesson", "destination": self.destination,
            "summary": "The user prefers concise collaboration.",
            "body": "Keep replies brief unless detail is necessary for the work.",
            "evidence_ids": [evidence.evidence_id], "associate_with": None}}, context)
        return {"reason": "proposed", "provider_attempt": True,
                "usage": {"input_tokens": 30, "output_tokens": 10}, **result}


class GlobalPreferenceRecordingTests(unittest.TestCase, RecordingFixture):
    def setUp(self):
        RecordingFixture.__init__(self, self)
        self.config.update(schema="mneme.codex-hooks.config.v10", memory_mode="async")
        self.preference_service = self.root / "personal-service.json"
        store = self.root / "personal.db"
        store.write_bytes(b"not opened")
        self.preference_service.write_text(json.dumps({"binary": "/bin/true", "database_name": "user",
            "database_path": str(store), "working_directory": str(self.root),
            "state_dir": str(self.root / "personal-state"), "port": 18766}))
        self.config["global_preferences"] = {"service_config": str(self.preference_service),
            "database_path": str(store), "db_id": OTHER_DB}
        self.identity.side_effect = lambda service, root, **kw: {"outcome": "ok",
            "db_id": OTHER_DB if "store_target" in kw else DB_ID,
            "native_work": {"decoded_bytes": 100}}
        self.overlap.return_value["native_work"] = {"decoded_bytes": 1000}

    def observed(self, delivery=None):
        if delivery is None:
            job = self.closed()
        else:
            import fixture_source_turn as source
            job = self.begin()
            self.append([source.delivered(delivery, turn="turn-1")])
            self.source_complete()
            jobs.close_turn(self.config, SESSION, "turn-1")
        return observe_source_turn(jobs._admission(job["admission"]), delivery=delivery)

    def answer(self, context, **changes):
        evidence = next(item for item in context.bindings if item.kind == "user_statement")
        proposal = {"kind": "lesson", "destination": "global_preference",
            "summary": "The user prefers concise collaboration.", "body": "Keep replies brief.",
            "evidence_ids": [evidence.evidence_id], "associate_with": None}
        proposal.update(changes)
        return {"proposal": proposal}

    def test_direct_notice_refuses_child_before_source_or_local_state_work(self):
        for field in ("agent_id", "agent_type"):
            with self.subTest(field=field), patch("recording_jobs.admit_source_turn") as admission, \
                    patch("recording_jobs._cancel_marker") as marker, patch("recording_jobs._transaction") as transaction:
                result = jobs.notice(self.config, {**self.event(), field: "child"})
                self.assertEqual(result, {"outcome": "skipped"})
                admission.assert_not_called()
                marker.assert_not_called()
                transaction.assert_not_called()
                self.assertFalse(self.config["state_dir"].exists())
                self.assertFalse(self.path().exists())

    def test_opt_in_schema_is_explicit_and_project_only_schema_unchanged(self):
        observation = self.observed()
        _, old = contract.prepare(observation)
        self.assertIs(contract.schema_for(old), contract.OUTPUT_SCHEMA)
        _, enabled = contract.prepare(observation, global_preferences_enabled=True)
        self.assertIn("destination", contract.schema_for(enabled)["properties"]["proposal"]["anyOf"][2]["required"])
        with self.assertRaises(ValueError):
            contract.validate_answer(self.answer(enabled), old)
        missing = self.answer(enabled)
        missing["proposal"].pop("destination")
        with self.assertRaisesRegex(ValueError, "invalid_proposal_destination"):
            contract.validate_answer(missing, enabled)
        self.assertLessEqual(enabled.authored_bytes, contract.MAX_AUTHORED_BYTES)

    def test_observer_collaboration_omission_reaches_assessor_without_child_authority(self):
        import fixture_source_turn as source
        job = self.begin()
        self.append([source.row("response_item", {
            "type": "agent_message", "id": "child-message", "author": "/root/reviewer", "recipient": "/root",
            "content": [{"type": "input_text", "text": "UNTRUSTED_CHILD_PREFERENCE"},
                        {"type": "encrypted_content", "encrypted_content": "OPAQUE_CHILD_CONTENT"}],
            source.META: {"turn_id": "turn-1", "create_time": 1790867655.506312}}),
            source.user("Across projects, keep collaboration replies concise.", identifier="root-preference", turn="turn-1")])
        self.source_complete()
        jobs.close_turn(self.config, SESSION, "turn-1")
        before = self.path().read_bytes()
        observed = observe_source_turn(jobs._admission(job["admission"]))
        self.assertEqual(observed["status"], "complete", observed)
        self.assertEqual(observed["coverage"]["omissions"]["collaboration_input"], 1)
        prompt, context = contract.prepare(observed, global_preferences_enabled=True)
        self.assertIsInstance(context, contract.ValidationContext)
        self.assertNotIn("UNTRUSTED_CHILD_PREFERENCE", prompt)
        self.assertNotIn("OPAQUE_CHILD_CONTENT", prompt)
        root = next(item for item in context.bindings if "Across projects" in item.text)
        self.assertEqual(root.kind, "user_statement")
        proposal = contract.validate_answer(self.answer(context, evidence_ids=[root.evidence_id]), context)["proposal"]
        self.assertEqual(proposal.evidence[0].source_ref_json, root.source_ref_json)
        self.assertEqual(self.path().read_bytes(), before)
        for omissions in ({"collaboration_input": 4097}, {"unknown_authority": 1}, {"collaboration_input": True}):
            with self.subTest(omissions=omissions):
                invalid = copy.deepcopy(observed)
                invalid["coverage"]["omissions"] = omissions
                self.assertEqual(contract.prepare(invalid, global_preferences_enabled=True),
                                 (None, "unsupported_public_omissions"))

    def test_global_operation_matrix_denies_non_user_and_project_operations(self):
        _, context = contract.prepare(self.observed(), global_preferences_enabled=True)
        assistant = next(item for item in context.bindings if item.kind == "assistant_assertion")
        bad = [{"kind": "episode"}, {"associate_with": "overlap001"},
               {"evidence_ids": [assistant.evidence_id]}, {"body": "x" * 1025},
               {"routing_judgment": {"target": "shown001"}}, {"destination": "user"}]
        for change in bad:
            with self.subTest(change=change), self.assertRaises(ValueError):
                contract.validate_answer(self.answer(context, **change), context)
        proposal = contract.validate_answer(self.answer(context), context)["proposal"]
        tampered = replace(proposal, evidence=(replace(proposal.evidence[0], source_field="other"),))
        with self.assertRaisesRegex(ValueError, "global_preference_evidence"):
            contract.validate_global_preference(tampered, context)

    def test_foreign_delivered_memory_remains_read_only_not_association_authority(self):
        packet = self.delivery_packet()
        _, context = contract.prepare(self.observed(packet), expected_db_id=OTHER_DB,
                                      global_preferences_enabled=True)
        self.assertIsInstance(context, contract.ValidationContext)
        self.assertEqual(context.association_bindings, ())
        self.assertFalse(context.routing_enabled)
        with self.assertRaises(ValueError):
            contract.validate_answer(self.answer(context, destination="project", associate_with="shown001"), context)
        memory = next(item for item in context.bindings if item.kind == "memory_delivery")
        with self.assertRaises(ValueError):
            contract.validate_answer(self.answer(context, evidence_ids=[memory.evidence_id]), context)

    def complete(self, runtime=None, *, write=None):
        self.closed()
        return self.step(runtime or PreferenceRuntime(), native_write=write or self.preference_write)

    def preference_write(self, config, job, timeout):
        self.writes.append(copy.deepcopy(job))
        return {"db": job.get("db_alias", "project"), "db_id": job["db_id"], "id": DB_ID,
                "readback_status": "verified", "replayed": False}

    def test_global_save_is_pinned_distilled_and_scope_bound_locally(self):
        _, runtime = self.complete()
        self.assertEqual(runtime.calls, 1)
        self.assertEqual(len(self.identity.call_args_list), 2)
        self.assertEqual(self.identity.call_args_list[-1].args[0], self.preference_service)
        self.assertEqual(len(self.writes), 1)
        job = self.writes[0]
        self.assertEqual((job["db_alias"], job["db_id"]), ("user", OTHER_DB))
        self.assertEqual(job["store_target"]["scope"], "global_preference")
        self.assertEqual(job["payload"]["source"]["namespace"], GLOBAL_PREFERENCE_NAMESPACE)
        self.assertEqual(job["payload"]["tags"], [GLOBAL_PREFERENCE_TAG])
        self.assertNotIn(str(self.root), json.dumps(job["payload"]))
        self.assertEqual(set(job["payload"]), {"source", "summary", "body", "tags"})
        self.assertTrue(job["citations"])
        receipt = self.state()["receipts"][0]
        self.assertEqual(receipt["outcome"], "verified")
        self.assertEqual(receipt["destination"], "global_preference")
        self.assertEqual(receipt["target"], job["store_target"])

    def test_project_choice_never_contacts_global_owner(self):
        self.complete(PreferenceRuntime("project"))
        self.assertEqual(len(self.identity.call_args_list), 1)
        self.assertEqual(self.writes[0]["db_id"], DB_ID)
        self.assertEqual(self.state()["receipts"][0]["destination"], "project")

    def test_global_identity_failure_never_falls_back_to_project(self):
        self.identity.side_effect = [{"outcome": "ok", "db_id": DB_ID, "native_work": {"decoded_bytes": 100}},
                                     {"outcome": "ok", "db_id": DB_ID, "native_work": {"decoded_bytes": 100}}]
        self.complete()
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["receipts"][0]["reason"], "global_identity_unavailable")

    def test_aggregate_identity_bytes_are_charged(self):
        from librarian_policy import resolve
        self.overlap.return_value["native_work"]["decoded_bytes"] = resolve(self.config).native_read_bytes - 50
        self.complete()
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["receipts"][0]["reason"], "global_identity_unavailable")

    def test_global_native_deadline_covers_identity_and_save(self):
        def identity(service, root, **options):
            if "store_target" in options:
                self.monotonic += 100
            return {"outcome": "ok", "db_id": OTHER_DB if "store_target" in options else DB_ID,
                    "native_work": {"decoded_bytes": 100}}
        self.identity.side_effect = identity
        self.complete()
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["receipts"][0]["reason"], "native_deadline")

    def test_global_proposal_cannot_accompany_project_maintenance(self):
        _, context = contract.prepare(self.observed(), global_preferences_enabled=True)
        context = replace(context, concern_bindings=(contract.ConcernTarget("case001", DB_ID, "{}", "fixture"),))
        answer = self.answer(context)
        answer["maintenance"] = [{"target": "case001"}]
        with self.assertRaisesRegex(ValueError, "global_preference_operation"):
            contract.validate_answer(answer, context)

    def test_config_digest_fences_global_target_and_service_bytes(self):
        original = jobs._config_digest(self.config)
        self.preference_service.write_text(self.preference_service.read_text() + "\n")
        self.assertNotEqual(original, jobs._config_digest(self.config))
        config = copy.deepcopy(self.config)
        config["global_preferences"]["db_id"] = DB_ID
        self.assertNotEqual(jobs._config_digest(config), jobs._config_digest(self.config))

    def test_ambiguous_global_write_is_not_replayed(self):
        def fail(*args):
            self.writes.append("ambiguous")
            raise TimeoutError("fixture")
        self.complete(write=fail)
        self.assertEqual(self.state()["jobs"][0]["phase"], "unresolved")
        self.step(PreferenceRuntime())
        self.assertEqual(self.writes, ["ambiguous"])

    def test_native_save_uses_global_owner_and_verified_expected_identity(self):
        from target_policy import global_preferences_policy
        policy = global_preferences_policy(self.config)
        job = {"destination": "global_preference", "store_target": policy.canonical(), "db_alias": "user",
               "db_id": OTHER_DB, "config_sha256": jobs._config_digest(self.config),
               "proposal_kind": "lesson", "payload": {"source": {"namespace": GLOBAL_PREFERENCE_NAMESPACE,
               "key": "a" * 64, "reference": "codex-global-preference://sha256/" + "a" * 64},
               "summary": "User prefers brevity.", "body": "Keep replies brief.", "tags": [GLOBAL_PREFERENCE_TAG]}}
        with patch("mcp_client.McpClient") as factory:
            client = factory.return_value
            client.call_tool.return_value = [{"db": "user", "name": "user", "state": "open",
                "configured_path": policy.database_path, "db_id": OTHER_DB}]
            client.save_verified.return_value = {"readback_status": "verified"}
            jobs._native_write(self.config, job, 2)
            client.save_verified.assert_called_once_with("user", {**job["payload"], "kind": "note"}, expected_db_id=OTHER_DB)
            for field, value in (("links", [{"to": DB_ID}]), ("action", "append"), ("tags", ["core"]),
                                 ("source", {**job["payload"]["source"], "namespace": "codex-acquisition.v1"})):
                with self.subTest(field=field), self.assertRaises(ValueError):
                    jobs._native_write(self.config, {**job, "payload": {**job["payload"], field: value}}, 2)
            self.assertEqual(client.save_verified.call_count, 1)
        with self.assertRaises(ValueError):
            jobs._native_maintenance(self.config, job, {}, 2)


if __name__ == "__main__":
    unittest.main()
